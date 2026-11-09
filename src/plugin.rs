//! The plugin itself: commands, status item and BUILD-file change tracking over [`crate::sync`].

use openidea_plugin::host::{self, Project};
use openidea_plugin::{
    Context, Error, Event, EventKind, Invocation, LogLevel, NotificationAction, NotificationLevel,
    PickItem, Plugin, StatusItem, register_plugin,
};

use openidea_plugin::details::ProcessLog;
use openidea_plugin::language_features::{Document, GutterAction, LanguageFeatures};

use std::collections::BTreeSet;

use crate::fxhash::FxHashSet;
use crate::ide;
use crate::sync::{self, SYNC_LOG, SyncError, SyncSummary};

/// Edits that change what a sync produces (IntelliJ's "BUILD files changed" check).
const BUILD_GLOBS: &[&str] = &[
    "**/BUILD",
    "**/BUILD.bazel",
    "**/*.bzl",
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    ".bazelproject",
    ".bazelrc",
];
const STATUS: &str = "sync";
/// Setting: sync a Bazel workspace that was never synced when it opens.
const SYNC_ON_OPEN: &str = "syncOnOpen";
/// The `runDetectors` entry tests run with (`bazel test`, debugged with `--java_debug`).
const BAZEL_TEST_CONFIG: &str = "bazel-test";
/// Setting: `NAME=value` entries the binaries of Run Target and Debug Target start with.
const RUN_ENVIRONMENT: &str = "runEnvironment";

#[derive(Default)]
pub struct BazelPlugin {
    /// The granted Bazel launcher: `bazelisk` (preferred, honours `.bazelversion`) or `bazel`.
    bazel: Option<String>,
    /// Project roots already checked for a sync on open in this session.
    opened: FxHashSet<String>,
    /// Roots of the workspaces synced at least once: the ones whose tests the gutter offers.
    synced: FxHashSet<String>,
    /// Project roots the gutter already looked for project data in.
    checked: FxHashSet<String>,
    /// Roots of the workspaces whose BUILD files changed since their last sync. The status item
    /// is shared by every window, so it names them, and clicking it syncs them, not the focused one.
    changed: BTreeSet<String>,
}

impl Plugin for BazelPlugin {
    fn activate(&mut self, ctx: &Context) -> Result<(), Error> {
        self.bazel = ["bazelisk", "bazel"]
            .into_iter()
            .find(|p| {
                ctx.capabilities
                    .iter()
                    .any(|c| c == &format!("process:{p}"))
            })
            .map(String::from);
        if ctx.capabilities.iter().any(|c| c == "events") {
            host::subscribe_paths("files-changed", BUILD_GLOBS)?;
            // Windows that open later report their focused editor: their project has opened.
            host::subscribe(EventKind::EditorFocused)?;
            // A checkout rewrites many BUILD files at once; the file events of such a batch can
            // miss, the repository change does not.
            host::subscribe(EventKind::VcsChanged)?;
        }
        // Projects already open when the plugin starts; the ones a window opens later come in
        // through its first event (see `handle_events`). A synced one is only remembered here: the
        // check of its BUILD files waits for that event, so that the gutter request that may have
        // activated the plugin is answered within the editor's budget.
        for project in host::projects().unwrap_or_default() {
            if is_bazel_workspace(&project) && has_project_data(&project) {
                if let Ok(root) = project.root() {
                    self.synced.insert(root);
                }
                continue;
            }
            self.on_project_open(&project);
        }
        Ok(())
    }

    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        let project = ctx.project().ok_or("no project is open")?;
        match id {
            "sync" => self.sync(&project),
            "sync-changed" => self.sync_changed(&project),
            "open-sync-log" => open(&project, SYNC_LOG),
            "test-file" => self.test_file(&project, ctx, false),
            "gutter-test" => {
                let (debug, line, path) =
                    parse_gutter_argument(arg.unwrap_or_default()).ok_or("not a test line")?;
                self.run_test(&project, path, Some(line), debug)
            }
            "run-target" => self.run_target(&project, false),
            "debug-target" => self.run_target(&project, true),
            "debug-test-file" => self.test_file(&project, ctx, true),
            "open-project-view" => {
                if !project.exists(sync::PROJECT_VIEW)? {
                    project.write_file(sync::PROJECT_VIEW, DEFAULT_PROJECT_VIEW)?;
                }
                open(&project, sync::PROJECT_VIEW)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], ctx: &Invocation) {
        // Any event from a window means its project is open. A window's first `editor-focused`
        // (no editor yet) is reported before plugins load, so the first one that arrives is
        // usually from opening a file.
        let project = ctx.project();
        if let Some(project) = &project {
            self.on_project_open(project);
        }
        let ours = |p: &String| p.contains("/.openidea/") || p.contains("/bazel-");
        // Events wait while a sync runs, so a batch can arrive after the sync that already read
        // those files: only an edit newer than the last sync counts.
        let synced = project.as_ref().and_then(|p| modified(p, SYNC_LOG));
        let build_changed = events.iter().any(|e| match e {
            Event::FilesChanged(c) => {
                c.removed.iter().any(|p| !ours(p))
                    || c.created.iter().chain(&c.modified).any(|p| {
                        !ours(p)
                            && edited_after(project.as_ref().and_then(|pr| modified(pr, p)), synced)
                    })
            }
            _ => false,
        }) || (events.iter().any(|e| matches!(e, Event::VcsChanged(_)))
            && project
                .as_ref()
                .is_some_and(|p| synced_build_files_edited(p, synced)));
        if build_changed && let Some(project) = &project {
            self.mark_changed(project);
        }
    }
}

impl BazelPlugin {
    /// IntelliJ's `BlazeSyncStartupActivity`, once per project and session: a Bazel workspace
    /// without project data syncs when it opens; one with data only restores it, which here is
    /// already on disk (the Eclipse project).
    fn on_project_open(&mut self, project: &Project) {
        let Ok(root) = project.root() else { return };
        if !self.opened.insert(root) || !is_bazel_workspace(project) {
            return;
        }
        if has_project_data(project) {
            self.synced.insert(project.root().unwrap_or_default());
            // A checkout made while the IDE was closed has no file events.
            if synced_build_files_edited(project, modified(project, SYNC_LOG)) {
                self.mark_changed(project);
            }
            return;
        }
        if host::setting_bool(SYNC_ON_OPEN).unwrap_or(true) && self.bazel.is_some() {
            // A cancelled or failed sync has told the user already.
            if let Err(e) = self.sync(project) {
                host::log(LogLevel::Info, &format!("sync on open: {e}"));
            }
        } else {
            let _ = host::notify_with_actions(
                NotificationLevel::Info,
                "Bazel workspace",
                "Sync the project with its BUILD files to resolve Java code from Bazel.",
                &[NotificationAction::new("Sync", "sync")],
            );
        }
    }

    /// Test / Debug Test File: the focused file's tests, or the test method under the caret
    /// (with `editor.read`).
    fn test_file(
        &mut self,
        project: &Project,
        ctx: &Invocation,
        debug: bool,
    ) -> Result<Option<String>, Error> {
        let editor = ctx
            .editor()
            .ok_or("open the test file in an editor first")?;
        let line = editor
            .selections()
            .ok()
            .and_then(|s| s.first().map(|r| r.start.line as usize));
        self.run_test(project, &editor.path(), line, debug)
    }

    /// Runs a test file's Bazel test target in the Run tool window, as IntelliJ's Bazel plugin
    /// runs tests: only the test method at `line` when there is one there. With `debug`, the IDE
    /// adds the `bazel-test` configuration's `debugArgs` (`--java_debug`) and attaches the Java
    /// debugger to the test JVM.
    fn run_test(
        &mut self,
        project: &Project,
        path: &str,
        line: Option<usize>,
        debug: bool,
    ) -> Result<Option<String>, Error> {
        if self.bazel.is_none() {
            return Err(Error::Denied("process:bazelisk".into()));
        }
        let root = project.root()?;
        let rel = path
            .strip_prefix(&format!("{}/", root.trim_end_matches('/')))
            .unwrap_or(path)
            .to_string();
        let index = project
            .read_file(sync::TARGET_INDEX)
            .map_err(|_| Error::NotFound("the target index (sync the project first)".into()))?;
        // The class the file declares, as the test target names it: its `package` and its name.
        let text = project.read_file(&rel).unwrap_or_default();
        let class = qualified_class(&rel, &text);
        let label = sync::test_targets_of(&index, &rel, Some(&class))
            .first()
            .map(|l| l.to_string())
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "a Bazel test target for {rel} (sync after adding it)"
                ))
            })?;
        let mut args = vec![label];
        // The JUnit runners Bazel uses match `Class#method` against the filter.
        if let Some(method) = line.and_then(|l| test_method_at(&text, l)) {
            args.push(format!("--test_filter={class}#{method}$"));
        }
        host::start_run(BAZEL_TEST_CONFIG, &args, debug).map(Some)
    }

    /// Whether `path` is in a synced workspace. A plugin activated before its windows registered
    /// their projects learns of them here: each project is checked once, then remembered.
    fn is_synced(&mut self, path: &str) -> bool {
        let inside = |root: &str| path.starts_with(&format!("{}/", root.trim_end_matches('/')));
        if self.synced.iter().any(|r| inside(r)) {
            return true;
        }
        for project in host::projects().unwrap_or_default() {
            let Ok(root) = project.root() else { continue };
            if inside(&root) && self.checked.insert(root.clone()) && has_project_data(&project) {
                self.synced.insert(root);
                return true;
            }
        }
        false
    }

    fn mark_changed(&mut self, project: &Project) {
        if let Ok(root) = project.root() {
            self.changed.insert(root);
            self.show_changed();
        }
    }

    /// Shows the out-of-sync workspaces in the status item; false when there are none.
    fn show_changed(&self) -> bool {
        let names: Vec<&str> = self.changed.iter().map(|r| workspace_name(r)).collect();
        let (text, tooltip) = match names.as_slice() {
            [] => return false,
            [one] => (
                format!("Bazel: {one} BUILD files changed"),
                format!("BUILD files of {one} changed since its last sync — click to sync it"),
            ),
            many => (
                format!("Bazel: {} workspaces out of sync", many.len()),
                format!(
                    "BUILD files of {} changed since their last sync — click to sync them",
                    many.join(", ")
                ),
            ),
        };
        set_status_command(&text, &tooltip, "sync-changed");
        true
    }

    /// Syncs the workspaces whose BUILD files changed, in whichever window they are open; with
    /// none, the focused one.
    fn sync_changed(&mut self, focused: &Project) -> Result<Option<String>, Error> {
        if self.changed.is_empty() {
            return self.sync(focused);
        }
        for project in host::projects()? {
            if project.root().is_ok_and(|r| self.changed.contains(&r)) {
                self.sync(&project)?;
            }
        }
        Ok(None)
    }

    /// `bazel run` of a binary the user picks, in a log tab, as IntelliJ's Bazel run
    /// configurations do; `debug` lists `java_binary` targets and passes their launcher
    /// `--debug`, so the JVM waits on port 5005 for Attach to Process.
    fn run_target(&mut self, project: &Project, debug: bool) -> Result<Option<String>, Error> {
        let bazel = self
            .bazel
            .clone()
            .ok_or_else(|| Error::Denied("process:bazelisk".into()))?;
        let query = project.read_file(sync::QUERY_FILE).map_err(|_| {
            Error::NotFound("the sync's target list (sync the project first)".into())
        })?;
        let targets = sync::runnable_targets(&query, debug);
        if targets.is_empty() {
            return Err(Error::NotFound(
                "a binary target in the synced directories".into(),
            ));
        }
        // A workspace has dozens of binaries and the list has no search: a filter first, which
        // starts as the last target run in this workspace.
        let last_key = format!("last-run:{}", project.root()?);
        let last = host::storage::get(&last_key).ok().flatten();
        let verb = if debug { "Debug" } else { "Run" };
        let Some(filter) = host::input(
            &format!("Bazel: {verb} Target (a label, or part of one)"),
            last.as_deref(),
        )?
        else {
            return Ok(None);
        };
        let matching = matching_targets(&targets, &filter);
        let label = match matching.as_slice() {
            [] => {
                return Err(Error::NotFound(format!(
                    "a binary target matching `{}`",
                    filter.trim()
                )));
            }
            [(label, _)] => label.to_string(),
            several => {
                let items: Vec<PickItem> = several
                    .iter()
                    .map(|(label, kind)| PickItem::new(label, label).description(kind))
                    .collect();
                let Some(label) = host::pick(&format!("Bazel: {verb} Target"), &items)? else {
                    return Ok(None);
                };
                label
            }
        };
        // Without the `storage` grant the order is simply not remembered.
        let _ = host::storage::set(&last_key, &label);
        let env = host::setting(RUN_ENVIRONMENT)
            .map(|json| json_strings(&json))
            .unwrap_or_default();
        let run_under = run_under_env(&env).map_err(Error::Invalid)?;
        let mut args = vec!["run", &label, "--color=no", "--curses=no"];
        if let Some(run_under) = &run_under {
            args.push(run_under);
        }
        if debug {
            args.extend(["--", "--debug"]);
        }
        host::open_log_tab(
            &format!("{verb} {label}"),
            &format!("Bazel {verb}: {label}"),
            &ProcessLog::new(&bazel, &args),
        )?;
        if debug {
            let _ = host::notify_with_actions(
                NotificationLevel::Info,
                "Bazel binary waiting for the debugger",
                "The JVM listens on port 5005 once Bazel starts it: Run › Attach to Process… (⌥⇧F5) and pick it.",
                &[],
            );
        }
        Ok(None)
    }

    fn sync(&mut self, project: &Project) -> Result<Option<String>, Error> {
        let bazel = self
            .bazel
            .clone()
            .ok_or_else(|| Error::Denied("process:bazelisk".into()))?;
        let root = project.root()?;
        self.changed.remove(&root);
        let name = workspace_name(&root).to_string();
        set_status(
            &format!("Bazel: syncing {name}…"),
            "Syncing the project with its BUILD files",
        );
        let task = host::begin_task("Bazel: Sync Project with BUILD Files", true);
        let mut ide = ide::IdeHost {
            project,
            task: &task,
            bazel: &bazel,
        };
        let result = sync::sync(&mut ide);
        drop(task);
        let changed_elsewhere = self.show_changed();
        let set_status = |text: &str, tooltip: &str| {
            if !changed_elsewhere {
                set_status(text, tooltip);
            }
        };
        match result {
            Ok(summary) => {
                self.synced.insert(root.clone());
                set_status(
                    "Bazel: synced",
                    "Synced with the BUILD files — click to sync again",
                );
                notify_done(&summary);
                Ok(None)
            }
            Err(SyncError::Cancelled) => {
                set_status(
                    "Bazel: sync cancelled",
                    "The last sync was cancelled — click to sync",
                );
                Err(Error::Cancelled)
            }
            Err(SyncError::Failed(message)) => {
                set_status(
                    "Bazel: sync failed",
                    "The last sync failed — click to sync again",
                );
                host::log(LogLevel::Error, &message);
                let _ = host::notify_with_actions(
                    NotificationLevel::Error,
                    "Bazel sync failed",
                    &first_lines(&message, 12),
                    &[NotificationAction::new("Show Log", "open-sync-log")],
                );
                Ok(None)
            }
        }
    }
}

fn notify_done(s: &SyncSummary) {
    let mut body = format!(
        "{} source roots and {} libraries from {} targets.",
        s.source_roots, s.libraries, s.targets
    );
    if !s.warnings.is_empty() {
        body.push_str(&format!(" {} warnings.", s.warnings.len()));
    }
    let actions = [NotificationAction::new("Show Log", "open-sync-log")];
    let level = if s.warnings.is_empty() {
        NotificationLevel::Info
    } else {
        NotificationLevel::Warning
    };
    // jdtls reads the Eclipse project when it starts, and the IDE reads `.openidea/settings.toml`
    // when the project opens; plugins can trigger neither.
    if s.settings_changed {
        body.push_str(" Restart OpenIDEA (Find Action › Restart IDE) so the Java language server loads the Bazel project.");
    } else if s.project_changed {
        body.push_str(" Restart the Java language server (language servers in the status bar › jdtls › Restart) to load the new classpath.");
    }
    let _ = host::notify_with_actions(level, "Bazel sync finished", &body, &actions);
}

/// Whether a sync already wrote the project (IntelliJ's "has project data").
fn has_project_data(project: &Project) -> bool {
    project
        .read_file(".project")
        .is_ok_and(|text| text.contains(crate::eclipse::GENERATED_MARKER))
}

fn is_bazel_workspace(project: &Project) -> bool {
    [
        "MODULE.bazel",
        "WORKSPACE",
        "WORKSPACE.bazel",
        sync::PROJECT_VIEW,
    ]
    .iter()
    .any(|f| project.exists(f).unwrap_or(false))
}

fn open(project: &Project, path: &str) -> Result<Option<String>, Error> {
    if !project.exists(path)? {
        return Err(Error::NotFound(format!("{path} (sync the project first)")));
    }
    host::open_location(
        &format!("{}/{path}", project.root()?.trim_end_matches('/')),
        0,
        0,
    )?;
    Ok(None)
}

/// Bazel files at the workspace root that change what a sync produces.
const ROOT_FILES: &[&str] = &[
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    ".bazelproject",
    ".bazelrc",
];

/// Whether a BUILD file of a synced package, or a root Bazel file, was edited or removed after
/// the last sync. New packages are not seen here, only through `files-changed`.
fn synced_build_files_edited(project: &Project, synced: Option<u64>) -> bool {
    // Never synced: nothing to compare with (a sync on open covers it).
    if !project.exists(SYNC_LOG).unwrap_or(false) {
        return false;
    }
    let Ok(index) = project.read_file(sync::TARGET_INDEX) else {
        return false;
    };
    let edited = |path: &str| edited_after(modified(project, path), synced);
    sync::indexed_packages(&index).into_iter().any(|package| {
        let build = ["BUILD.bazel", "BUILD"]
            .map(|f| {
                if package.is_empty() {
                    f.to_string()
                } else {
                    format!("{package}/{f}")
                }
            })
            .into_iter()
            .find(|f| project.exists(f).unwrap_or(false));
        build.is_none_or(|f| edited(&f))
    }) || ROOT_FILES
        .iter()
        .any(|f| project.exists(f).unwrap_or(false) && edited(f))
}

/// The targets whose label contains `filter` (ignoring case); an exact label alone when one is.
fn matching_targets<'a>(targets: &[(&'a str, &'a str)], filter: &str) -> Vec<(&'a str, &'a str)> {
    let filter = filter.trim();
    if let Some(&exact) = targets.iter().find(|(label, _)| *label == filter) {
        return vec![exact];
    }
    let filter = filter.to_lowercase();
    targets
        .iter()
        .filter(|(label, _)| label.to_lowercase().contains(&filter))
        .copied()
        .collect()
}

/// The JUnit test method whose declaration or body holds 0-based `line`: the closest method
/// declared at or above it and annotated `@Test` (or a JUnit 5 test annotation) right above.
fn test_method_at(text: &str, line: usize) -> Option<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = line.min(lines.len().checked_sub(1)?);
    loop {
        if let Some(name) = method_name(lines[i]) {
            let annotated = lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim())
                .take_while(|l| l.starts_with('@') || l.is_empty() || l.starts_with("//"))
                .any(|l| {
                    [
                        "@Test",
                        "@ParameterizedTest",
                        "@RepeatedTest",
                        "@TestFactory",
                    ]
                    .iter()
                    .any(|a| {
                        l == *a
                            || l.starts_with(&format!("{a}("))
                            || l.starts_with(&format!("{a} "))
                    })
                });
            return annotated.then_some(name);
        }
        // A class or another member's end above the caret: not inside a method.
        if lines[i].trim_start().starts_with("class ") || lines[i].contains(" class ") {
            return None;
        }
        i = i.checked_sub(1)?;
    }
}

/// The name of the method a line declares (`void name(`, with any modifiers).
fn method_name(line: &str) -> Option<&str> {
    let before = line.split('(').next()?.trim_end();
    if !line.contains('(') || before.contains('=') || before.contains("new ") {
        return None;
    }
    let mut words = before.split_whitespace().rev();
    let name = words.next()?;
    let return_type = words.next()?;
    let valid = |w: &str| w.chars().all(|c| c.is_alphanumeric() || c == '_');
    (valid(name)
        && ![
            "if",
            "for",
            "while",
            "switch",
            "catch",
            "return",
            "synchronized",
        ]
        .contains(&name)
        && !["return", "new", "throw", "else"].contains(&return_type)
        && return_type
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && !return_type.ends_with([';', ',']))
    .then_some(name)
}

/// The `package`-qualified class a Java source declares, named after its file.
fn qualified_class(rel: &str, text: &str) -> String {
    let stem = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".java");
    match crate::model::declared_package(text) {
        Some(pkg) if !pkg.is_empty() => format!("{pkg}.{stem}"),
        _ => stem.to_string(),
    }
}

/// The lines to run tests from: the file's class (all its tests), then each test method.
fn test_lines<'a>(text: &'a str, stem: &str) -> Vec<(usize, Option<&'a str>)> {
    let lines: Vec<&str> = text.lines().collect();
    let declares_class = |l: &str| {
        l.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w[0] == "class" && w[1] == stem)
    };
    let mut out: Vec<(usize, Option<&str>)> = lines
        .iter()
        .position(|l| declares_class(l))
        .map(|i| (i, None))
        .into_iter()
        .collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(name) = method_name(line)
            && test_method_at(text, i) == Some(name)
        {
            out.push((i, Some(name)));
        }
    }
    out
}

/// A gutter action's argument: `run`/`debug`, the 0-based line and the file.
fn gutter_argument(debug: bool, line: usize, path: &str) -> String {
    format!("{}\t{line}\t{path}", if debug { "debug" } else { "run" })
}

fn parse_gutter_argument(arg: &str) -> Option<(bool, usize, &str)> {
    let mut parts = arg.splitn(3, '\t');
    let debug = match parts.next()? {
        "run" => false,
        "debug" => true,
        _ => return None,
    };
    let line = parts.next()?.parse().ok()?;
    Some((debug, line, parts.next().filter(|p| !p.is_empty())?))
}

/// `--run_under=env NAME=value…`: Bazel 7 has no `--run_env`, and the binary inherits only the
/// Bazel client's environment. `None` without entries; an entry without `=` is an error.
fn run_under_env(env: &[String]) -> Result<Option<String>, String> {
    if env.is_empty() {
        return Ok(None);
    }
    let mut out = String::from("--run_under=env");
    for entry in env {
        let valid = entry
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && !name.contains(char::is_whitespace));
        if !valid {
            return Err(format!(
                "`{entry}` in the {RUN_ENVIRONMENT} setting is not NAME=value"
            ));
        }
        out.push(' ');
        // Bazel splits `--run_under` as a shell would.
        if entry
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_=-./:@,+%".contains(&c))
        {
            out.push_str(entry);
        } else {
            out.push('\'');
            out.push_str(&entry.replace('\'', "'\\''"));
            out.push('\'');
        }
    }
    Ok(Some(out))
}

/// The strings of a JSON array of strings, as the host returns an array setting.
fn json_strings(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(start) = rest.find('"') {
        let body = &rest[start + 1..];
        let mut end = None;
        let mut escaped = false;
        for (i, c) in body.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        out.push(openidea_plugin::text::unescape_json(&body[..end]));
        rest = &body[end + 1..];
    }
    out
}

/// When a project file was last modified (`path` absolute or relative to the root).
fn modified(project: &Project, path: &str) -> Option<u64> {
    let root = project.root().ok()?;
    let rel = path
        .strip_prefix(root.trim_end_matches('/'))
        .unwrap_or(path);
    project.stat(rel.trim_start_matches('/')).ok()?.modified
}

/// Whether a file was edited after the last sync; unknown times count as an edit.
fn edited_after(file: Option<u64>, synced: Option<u64>) -> bool {
    match (file, synced) {
        (Some(file), Some(synced)) => file > synced,
        _ => true,
    }
}

fn set_status(text: &str, tooltip: &str) {
    set_status_command(text, tooltip, "sync");
}

fn set_status_command(text: &str, tooltip: &str, command: &str) {
    host::set_status_item(
        STATUS,
        &StatusItem {
            text: text.into(),
            tooltip: Some(tooltip.into()),
            command: Some(command.into()),
        },
    );
}

/// A workspace's folder name, as the window title shows it.
fn workspace_name(root: &str) -> &str {
    root.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(root)
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

const DEFAULT_PROJECT_VIEW: &str = "directories:\n  .\n\nderive_targets_from_directories: true\n";

impl LanguageFeatures for BazelPlugin {
    /// Run and Debug beside a synced workspace's test classes and methods, as IntelliJ's Bazel
    /// plugin offers them in the gutter.
    fn gutter_actions(&mut self, doc: &Document) -> Vec<GutterAction> {
        if !doc.path.ends_with(".java") || !doc.text.contains("@Test") || !self.is_synced(&doc.path)
        {
            return Vec::new();
        }
        let stem = doc
            .path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches(".java");
        test_lines(&doc.text, stem)
            .into_iter()
            .flat_map(|(line, method)| {
                let what = match method {
                    Some(m) => format!("{stem}.{m}"),
                    None => stem.to_string(),
                };
                [(false, "run", "Run"), (true, "debug", "Debug")].map(|(debug, icon, verb)| {
                    GutterAction {
                        line: line as u32,
                        icon: Some(icon.into()),
                        tooltip: format!("{verb} 'Bazel test {what}'"),
                        command: "gutter-test".into(),
                        argument: Some(gutter_argument(debug, line, &doc.path)),
                    }
                })
            })
            .collect()
    }
}

register_plugin!(BazelPlugin, language_features);

#[cfg(test)]
mod tests {
    use openidea_plugin::mock::MockHost;
    use openidea_plugin::{Context, Event, FilesChanged};

    use super::*;

    const ALL: &[&str] = &[
        "project.read",
        "project.write",
        "process:bazelisk",
        "fs.read:/private/var/tmp",
        "events",
    ];

    fn activated(caps: &[&str]) -> BazelPlugin {
        let mut plugin = BazelPlugin::default();
        let ctx = Context {
            plugin_id: "davidhsv.bazel".into(),
            plugin_version: "0.1.0".into(),
            api_version: "1.3".into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
        };
        plugin.activate(&ctx).unwrap();
        plugin
    }

    fn read(host: &MockHost, path: &str) -> Option<String> {
        host.invocation()
            .project()
            .and_then(|p| p.read_file(path).ok())
    }

    #[test]
    fn sync_needs_a_bazel_grant() {
        let host = MockHost::install().with_capabilities(&["project.read", "project.write"]);
        host.add_file(".bazelproject", "directories:\n  .\n");
        let mut plugin = activated(&["project.read", "project.write"]);
        assert_eq!(
            plugin.run_command("sync", &host.invocation(), None),
            Err(Error::Denied("process:bazelisk".into()))
        );
        assert!(host.tasks().is_empty(), "nothing started");
    }

    #[test]
    fn a_failed_sync_runs_in_a_task_and_points_at_its_log() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file(
            ".bazelproject",
            "directories:\n  .\n\nderive_targets_from_directories: true\n",
        );
        let mut plugin = activated(ALL);
        // The mock's processes print no `bazel info` values, so the sync stops after it.
        assert_eq!(
            plugin.run_command("sync", &host.invocation(), None),
            Ok(None)
        );

        let tasks = host.tasks();
        assert_eq!(tasks[0].title, "Bazel: Sync Project with BUILD Files");
        assert!(
            tasks[0]
                .reports
                .iter()
                .any(|(_, _, m)| m.as_deref() == Some("bazel info"))
        );
        let failed = host
            .notifications_with_actions()
            .into_iter()
            .find(|n| n.notification.title == "Bazel sync failed")
            .expect("failure notification");
        assert!(
            failed
                .notification
                .body
                .contains("bazel info printed no execution_root"),
            "{}",
            failed.notification.body
        );
        assert_eq!(failed.actions[0].command, "open-sync-log");
        assert_eq!(host.status_items()[STATUS].text, "Bazel: sync failed");
        assert!(read(&host, SYNC_LOG).unwrap().contains("Sync failed"));
        assert!(
            host.logs()
                .iter()
                .any(|(level, _)| *level == LogLevel::Error)
        );
    }

    #[test]
    fn a_never_synced_workspace_syncs_when_it_opens() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "module(name = \"demo\")\n");
        activated(ALL);
        assert_eq!(
            host.tasks()[0].title,
            "Bazel: Sync Project with BUILD Files"
        );
        assert!(read(&host, SYNC_LOG).is_some());
    }

    #[test]
    fn a_window_opening_a_never_synced_workspace_syncs_it_once() {
        let host = MockHost::install().with_capabilities(ALL);
        host.set_project_root(None);
        let mut plugin = activated(ALL);
        assert!(host.tasks().is_empty(), "no project open yet");
        host.set_project_root(Some("/project"));
        host.add_file("MODULE.bazel", "");
        // The window's first event can be any kind: here, a file it opened.
        plugin.handle_events(
            &[Event::FileOpened("/project/BUILD".into())],
            &host.invocation(),
        );
        assert_eq!(host.tasks().len(), 1);
        let focused = [Event::EditorFocused(None)];
        plugin.handle_events(&focused, &host.invocation());
        assert_eq!(host.tasks().len(), 1, "once per project and session");
    }

    #[test]
    fn a_synced_workspace_only_loads_when_it_opens() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "");
        host.add_file(
            ".project",
            "<projectDescription><comment>Generated from Bazel by openidea-bazel</comment></projectDescription>",
        );
        activated(ALL);
        assert!(host.tasks().is_empty());
        assert!(host.notifications_with_actions().is_empty());
        assert!(!host.status_items().contains_key(STATUS));
    }

    #[test]
    fn a_synced_workspace_whose_build_files_changed_while_closed_says_so_when_it_opens() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "");
        host.add_file(
            ".project",
            "<projectDescription><comment>Generated from Bazel by openidea-bazel</comment></projectDescription>",
        );
        host.add_file(SYNC_LOG, "Synced");
        host.add_file(sync::TARGET_INDEX, "app/A.java\t//app:lib\tjava_library\n");
        let mut plugin = activated(ALL);
        assert!(
            !host.status_items().contains_key(STATUS),
            "checked on the window's first event, not during activation"
        );
        plugin.handle_events(&[Event::EditorFocused(None)], &host.invocation());
        assert!(host.tasks().is_empty(), "no sync without asking");
        assert_eq!(
            host.status_items()[STATUS].text,
            "Bazel: project BUILD files changed"
        );
    }

    #[test]
    fn with_sync_on_open_off_or_no_bazel_grant_a_new_workspace_gets_a_hint() {
        for (setting, caps) in [
            ("false", ALL),
            ("true", &["project.read", "project.write"][..]),
        ] {
            let host = MockHost::install().with_capabilities(caps);
            host.set_setting(SYNC_ON_OPEN, setting);
            host.add_file("MODULE.bazel", "");
            activated(caps);
            assert!(host.tasks().is_empty());
            let hint = host.notifications_with_actions();
            assert!(hint.iter().any(
                |n| n.notification.title == "Bazel workspace" && n.actions[0].command == "sync"
            ));
        }
    }

    #[test]
    fn activation_prefers_bazelisk_and_watches_build_files() {
        let host = MockHost::install().with_capabilities(ALL);
        let plugin = activated(&[
            "project.read",
            "project.write",
            "process:bazel",
            "process:bazelisk",
            "events",
        ]);
        assert_eq!(plugin.bazel.as_deref(), Some("bazelisk"));
        assert_eq!(host.path_subscriptions()["files-changed"], BUILD_GLOBS);

        let plugin = activated(&["project.read", "project.write", "process:bazel"]);
        assert_eq!(plugin.bazel.as_deref(), Some("bazel"));
    }

    #[test]
    fn a_checkout_marks_a_synced_project_out_of_sync() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        let checkout = [Event::VcsChanged("/project".into())];
        plugin.handle_events(&checkout, &host.invocation());
        assert!(
            !host.status_items().contains_key(STATUS),
            "never synced: nothing to compare"
        );
        host.add_file(SYNC_LOG, "Synced");
        host.add_file(sync::TARGET_INDEX, "app/A.java\t//app:lib\tjava_library\n");
        host.add_file("app/BUILD.bazel", "java_library(name = \"lib\")\n");
        plugin.handle_events(&checkout, &host.invocation());
        // MockHost files have no modification time, which counts as an edit.
        assert_eq!(
            host.status_items()[STATUS].text,
            "Bazel: project BUILD files changed"
        );
    }

    #[test]
    fn build_file_edits_mark_the_project_out_of_sync_but_its_own_files_do_not() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        let changed = |path: &str| {
            [Event::FilesChanged(FilesChanged {
                created: vec![],
                modified: vec![path.into()],
                removed: vec![],
            })]
        };
        plugin.handle_events(
            &changed("/project/.openidea/bazel/aspects/java_info.bzl"),
            &host.invocation(),
        );
        assert!(!host.status_items().contains_key(STATUS));
        plugin.handle_events(&changed("/project/app/BUILD.bazel"), &host.invocation());
        let item = &host.status_items()[STATUS];
        assert_eq!(item.text, "Bazel: project BUILD files changed");
        assert_eq!(item.command.as_deref(), Some("sync-changed"));

        // The click syncs the changed workspace, whichever window it comes from.
        plugin
            .run_command("sync-changed", &host.invocation(), None)
            .unwrap();
        assert_eq!(host.tasks().len(), 1);
        assert!(plugin.changed.is_empty());
        assert!(!host.status_items()[STATUS].text.contains("changed"));
    }

    #[test]
    fn workspaces_are_named_by_their_folder() {
        assert_eq!(workspace_name("/code/worktrees/pr-22138"), "pr-22138");
        assert_eq!(workspace_name("/code/Allocadia/"), "Allocadia");
        assert_eq!(workspace_name("/"), "/");
    }

    #[test]
    fn only_edits_newer_than_the_last_sync_are_out_of_sync() {
        assert!(edited_after(Some(20), Some(10)));
        assert!(!edited_after(Some(10), Some(10)), "read by that sync");
        assert!(!edited_after(Some(5), Some(10)));
        assert!(edited_after(None, Some(10)));
        assert!(edited_after(Some(5), None), "never synced");
    }

    #[test]
    fn run_target_runs_the_picked_binary_and_remembers_it() {
        let host = MockHost::install().with_capabilities(&[ALL, &["storage"]].concat());
        let mut plugin = activated(ALL);
        host.add_file(
            sync::QUERY_FILE,
            "java_binary rule //app:run\njava_library rule //app:lib\nsh_binary rule //tools:fmt\n",
        );
        // One match runs without a list.
        host.answer_next_prompt(Some(&["app:run"]));
        plugin
            .run_command("run-target", &host.invocation(), None)
            .unwrap();
        let tab = &host.log_tabs()[0];
        assert_eq!(tab.title, "Bazel Run: //app:run");
        assert_eq!(tab.log.program, "bazelisk");
        assert_eq!(
            tab.log.args,
            ["run", "//app:run", "--color=no", "--curses=no"]
        );
        assert_eq!(host.storage()["last-run:/project"], "//app:run");

        // Several matches go to the list.
        host.add_file(
            sync::QUERY_FILE,
            "java_binary rule //app:run\njava_binary rule //app:run_other\nsh_binary rule //tools:fmt\n",
        );
        host.answer_next_prompt(Some(&["app:run"]));
        host.answer_next_prompt(Some(&["//app:run"]));
        plugin
            .run_command("debug-target", &host.invocation(), None)
            .unwrap();
        let tab = &host.log_tabs()[1];
        assert_eq!(
            tab.log.args,
            [
                "run",
                "//app:run",
                "--color=no",
                "--curses=no",
                "--",
                "--debug"
            ]
        );

        // `sh_binary` has no `--debug`: not offered for debugging.
        host.answer_next_prompt(Some(&["tools:fmt"]));
        assert!(
            plugin
                .run_command("debug-target", &host.invocation(), None)
                .is_err()
        );
    }

    #[test]
    fn the_run_environment_becomes_a_run_under_env_prefix() {
        let env = |e: &[&str]| e.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run_under_env(&[]), Ok(None));
        assert_eq!(
            run_under_env(&env(&["ALLOCADIA_ENV=local", "URL=http://localhost:8888"])),
            Ok(Some(
                "--run_under=env ALLOCADIA_ENV=local URL=http://localhost:8888".into()
            ))
        );
        assert_eq!(
            run_under_env(&env(&["MSG=it's on"])),
            Ok(Some("--run_under=env 'MSG=it'\\''s on'".into()))
        );
        assert!(run_under_env(&env(&["NOVALUE"])).is_err());
        assert!(run_under_env(&env(&["=x"])).is_err());
        assert_eq!(
            json_strings(r#"["A=1", "B=\"q\"", "C=x,y"]"#),
            ["A=1", "B=\"q\"", "C=x,y"]
        );
        assert!(json_strings("[]").is_empty());
    }

    #[test]
    fn the_caret_finds_its_test_method() {
        let text = "package x;\n\n@RunWith(JUnit4.class)\npublic class IsoInstantTest {\n\n  @Test\n  public void mysqlTimestampToInstant() {\n    assertEquals(\n        1L, f(\"a\"));\n  }\n\n  private static long f(String s) {\n    return 1;\n  }\n\n  @ParameterizedTest\n  @ValueSource(ints = {1})\n  void each(int i) {}\n}\n";
        assert_eq!(test_method_at(text, 6), Some("mysqlTimestampToInstant"));
        assert_eq!(test_method_at(text, 8), Some("mysqlTimestampToInstant"));
        assert_eq!(test_method_at(text, 12), None, "a helper is not a test");
        assert_eq!(test_method_at(text, 17), Some("each"));
        assert_eq!(
            test_method_at(text, 3),
            None,
            "the class line runs the class"
        );
        assert_eq!(test_method_at(text, 0), None);
        assert_eq!(test_method_at("", 0), None);
    }

    const ISO_TEST: &str = "package com.x;\n\n@RunWith(JUnit4.class)\npublic class IsoTest {\n\n  @Test\n  public void parses() {\n    assertEquals(1, 1);\n  }\n\n  private void helper() {}\n\n  @Test\n  public void formats() {}\n}\n";

    #[test]
    fn the_gutter_finds_a_synced_workspace_the_plugin_activated_without() {
        let host = synced_test_host();
        let mut plugin = BazelPlugin::default();
        let doc = Document {
            path: "/project/lib/IsoTest.java".into(),
            language: "java".into(),
            version: 1,
            text: ISO_TEST.into(),
        };
        assert_eq!(plugin.gutter_actions(&doc).len(), 6);
        drop(host);
    }

    #[test]
    fn test_lines_are_the_class_then_each_test_method() {
        assert_eq!(
            test_lines(ISO_TEST, "IsoTest"),
            [(3, None), (6, Some("parses")), (13, Some("formats"))]
        );
        assert!(test_lines("class Other {}", "IsoTest").is_empty());
    }

    #[test]
    fn gutter_arguments_round_trip() {
        let arg = gutter_argument(true, 6, "/project/a b/IsoTest.java");
        assert_eq!(
            parse_gutter_argument(&arg),
            Some((true, 6, "/project/a b/IsoTest.java"))
        );
        assert_eq!(parse_gutter_argument("walk\t1\t/x"), None);
        assert_eq!(parse_gutter_argument("run\tx\t/x"), None);
    }

    fn synced_test_host() -> MockHost {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "");
        host.add_file(
            ".project",
            "<projectDescription><comment>Generated from Bazel by openidea-bazel</comment></projectDescription>",
        );
        host.add_file(
            sync::TARGET_INDEX,
            "class:com.x.IsoTest\t//lib:IsoTest\tjava_test\n",
        );
        host.add_file("lib/IsoTest.java", ISO_TEST);
        host.set_run_configurations(vec![openidea_plugin::PluginRunConfiguration {
            id: BAZEL_TEST_CONFIG.into(),
            name: "bazel test".into(),
            program: "bazelisk".into(),
            test: true,
            ..Default::default()
        }]);
        host.set_run_configuration_debugger(BAZEL_TEST_CONFIG);
        host
    }

    #[test]
    fn the_gutter_runs_or_debugs_one_test_method_or_the_class() {
        let host = synced_test_host();
        let mut plugin = activated(ALL);
        let path = "/project/lib/IsoTest.java";
        for (debug, line) in [(false, 6), (true, 13), (false, 3)] {
            plugin
                .run_command(
                    "gutter-test",
                    &host.invocation(),
                    Some(&gutter_argument(debug, line, path)),
                )
                .unwrap();
        }
        let runs: Vec<(String, Vec<String>, bool)> = host
            .configuration_runs()
            .into_iter()
            .map(|(_, config, args, debug)| (config, args, debug))
            .collect();
        assert_eq!(
            runs,
            [
                (
                    "bazel-test".to_string(),
                    vec![
                        "//lib:IsoTest".to_string(),
                        "--test_filter=com.x.IsoTest#parses$".to_string()
                    ],
                    false
                ),
                (
                    "bazel-test".to_string(),
                    vec![
                        "//lib:IsoTest".to_string(),
                        "--test_filter=com.x.IsoTest#formats$".to_string()
                    ],
                    true
                ),
                (
                    "bazel-test".to_string(),
                    vec!["//lib:IsoTest".to_string()],
                    false
                ),
            ]
        );
    }

    #[test]
    fn gutter_actions_offer_run_and_debug_on_test_lines_of_a_synced_workspace() {
        let host = synced_test_host();
        let mut plugin = activated(ALL);
        let doc = |path: &str| Document {
            path: path.into(),
            language: "java".into(),
            version: 1,
            text: ISO_TEST.into(),
        };
        let actions = plugin.gutter_actions(&doc("/project/lib/IsoTest.java"));
        let shown: Vec<(u32, String)> = actions
            .iter()
            .map(|a| (a.line, a.tooltip.clone()))
            .collect();
        assert_eq!(
            shown,
            [
                (3, "Run 'Bazel test IsoTest'".to_string()),
                (3, "Debug 'Bazel test IsoTest'".to_string()),
                (6, "Run 'Bazel test IsoTest.parses'".to_string()),
                (6, "Debug 'Bazel test IsoTest.parses'".to_string()),
                (13, "Run 'Bazel test IsoTest.formats'".to_string()),
                (13, "Debug 'Bazel test IsoTest.formats'".to_string()),
            ]
        );
        assert!(actions.iter().all(|a| a.command == "gutter-test"));
        assert!(
            plugin
                .gutter_actions(&doc("/elsewhere/IsoTest.java"))
                .is_empty(),
            "not in a synced project"
        );
        drop(host);
    }

    #[test]
    fn a_filter_matches_labels_or_one_exact_label() {
        let targets = [
            ("//app:run", "java_binary"),
            ("//app:run_other", "java_binary"),
            ("//tools:fmt", "sh_binary"),
        ];
        assert_eq!(matching_targets(&targets, " //app:run "), [targets[0]]);
        assert_eq!(matching_targets(&targets, "APP"), targets[..2]);
        assert!(matching_targets(&targets, "nothing").is_empty());
        assert_eq!(matching_targets(&targets, ""), targets);
    }

    #[test]
    fn run_target_needs_a_sync() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        assert!(matches!(
            plugin.run_command("run-target", &host.invocation(), None),
            Err(Error::NotFound(m)) if m.contains("sync the project first")
        ));
    }

    #[test]
    fn open_project_view_creates_a_default_one() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        plugin
            .run_command("open-project-view", &host.invocation(), None)
            .unwrap();
        assert_eq!(
            read(&host, sync::PROJECT_VIEW).as_deref(),
            Some(DEFAULT_PROJECT_VIEW)
        );
        assert_eq!(host.opened_locations()[0].0, "/project/.bazelproject");
    }

    #[test]
    fn the_sync_log_needs_a_sync() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        assert!(matches!(
            plugin.run_command("open-sync-log", &host.invocation(), None),
            Err(Error::NotFound(m)) if m.contains("sync the project first")
        ));
    }
}
