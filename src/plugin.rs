//! The plugin itself: commands, status item and BUILD-file change tracking over [`crate::sync`].

use openidea_plugin::host::{self, Project};
use openidea_plugin::{
    Context, Error, Event, EventKind, Invocation, LogLevel, NotificationAction, NotificationLevel,
    PickItem, Plugin, StatusItem, register_plugin,
};

use openidea_plugin::details::ProcessLog;

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
/// Setting: `NAME=value` entries the binaries of Run Target and Debug Target start with.
const RUN_ENVIRONMENT: &str = "runEnvironment";

#[derive(Default)]
pub struct BazelPlugin {
    /// The granted Bazel launcher: `bazelisk` (preferred, honours `.bazelversion`) or `bazel`.
    bazel: Option<String>,
    /// Project roots already checked for a sync on open in this session.
    opened: FxHashSet<String>,
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
        // through its first event (see `handle_events`).
        for project in host::projects().unwrap_or_default() {
            self.on_project_open(&project);
        }
        Ok(())
    }

    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        let project = ctx.project().ok_or("no project is open")?;
        match id {
            "sync" => self.sync(&project),
            "sync-changed" => self.sync_changed(&project),
            "open-sync-log" => open(&project, SYNC_LOG),
            "test-file" => self.test_file(&project, ctx, false),
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

    /// Runs the focused file's test target through Bazel in a log tab, as IntelliJ's Bazel plugin
    /// runs tests (`bazel test`); `debug` adds `--java_debug`, so the test JVM waits for a debugger
    /// on port 5005, which Attach to Process lists as a JDWP process.
    fn test_file(
        &mut self,
        project: &Project,
        ctx: &Invocation,
        debug: bool,
    ) -> Result<Option<String>, Error> {
        let bazel = self
            .bazel
            .clone()
            .ok_or_else(|| Error::Denied("process:bazelisk".into()))?;
        let editor = ctx
            .editor()
            .ok_or("open the test file in an editor first")?;
        let root = project.root()?;
        let path = editor.path();
        let rel = path
            .strip_prefix(&format!("{}/", root.trim_end_matches('/')))
            .unwrap_or(&path)
            .to_string();
        let index = project
            .read_file(sync::TARGET_INDEX)
            .map_err(|_| Error::NotFound("the target index (sync the project first)".into()))?;
        // The class the file declares, as the test target names it: its `package` and its name.
        let text = project.read_file(&rel).unwrap_or_default();
        let stem = rel
            .rsplit('/')
            .next()
            .unwrap_or(&rel)
            .trim_end_matches(".java");
        let class = match crate::model::declared_package(&text) {
            Some(pkg) if !pkg.is_empty() => format!("{pkg}.{stem}"),
            _ => stem.to_string(),
        };
        let label = sync::test_targets_of(&index, &rel, Some(&class))
            .first()
            .map(|l| l.to_string())
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "a Bazel test target for {rel} (sync after adding it)"
                ))
            })?;
        // A log tab keeps the output (and the exit code) after Bazel ends, unlike a terminal tab.
        let mode = if debug {
            "--java_debug"
        } else {
            "--test_output=streamed"
        };
        let log = ProcessLog::new(&bazel, &["test", &label, "--color=no", "--curses=no", mode]);
        let verb = if debug { "Debug" } else { "Test" };
        host::open_log_tab(
            &format!("{verb} {label}"),
            &format!("Bazel {verb}: {label}"),
            &log,
        )?;
        if debug {
            let _ = host::notify_with_actions(
                NotificationLevel::Info,
                "Bazel test waiting for the debugger",
                "The test JVM listens on port 5005 once Bazel starts it: Run › Attach to Process… (⌥⇧F5) and pick it.",
                &[],
            );
        }
        Ok(None)
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

register_plugin!(BazelPlugin);

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
        activated(ALL);
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
