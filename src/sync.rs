//! "Sync Project with BUILD Files": the order of the IntelliJ plugin's `SyncPhaseCoordinator`
//! (project view → `bazel info` → aspect files → target derivation → aspect build → target map →
//! Java model), ending in an Eclipse project and the jdtls settings instead of an IntelliJ module.

use std::collections::{BTreeSet, VecDeque};

use crate::fxhash::{FxHashSet, key_fingerprint};

use crate::eclipse;
use crate::ideinfo;
use crate::import_roots;
use crate::model::{self, SyncFiles};
use crate::projectview;
use crate::query;
use crate::settings;
use crate::types::{BazelInfo, DirectoryEntry, ProjectView, TargetKey, TargetMap};
use crate::{aspect, types::JavaModel};

/// The plugin's own directory in the workspace (IntelliJ's `.ijwb`).
pub const DATA_DIR: &str = ".openidea/bazel";
pub const QUERY_FILE: &str = ".openidea/bazel/query.txt";
const TARGETS_FILE: &str = ".openidea/bazel/targets.txt";
pub const PROJECT_VIEW: &str = ".bazelproject";
pub const SYNC_LOG: &str = ".openidea/bazel/last-sync.txt";
/// Which targets build each source file (IntelliJ's `SourceToTargetMap`), for running and
/// debugging a file's tests through Bazel: `path\tlabel\tkind` lines.
pub const TARGET_INDEX: &str = ".openidea/bazel/targets.tsv";
const SETTINGS: &str = ".openidea/settings.toml";
const GIT_EXCLUDE: &str = ".git/info/exclude";
/// The aspect's info files are read for the synced targets and their dependencies up to this
/// depth: the `-direct-deps` output groups build the targets' and their direct (and forwarded)
/// dependencies' info files; files deeper than that may be stale.
const INFO_DEPTH: u32 = 2;
/// Exit codes Bazel uses for a usable, possibly partial result with `--keep_going`.
const BUILD_OK: [i32; 3] = [0, 1, 3];

/// What the sync needs from the IDE; [`crate::host`] implements it over the plugin API.
pub trait SyncHost {
    fn workspace_root(&mut self) -> Result<String, String>;
    /// A workspace file, workspace-relative; `None` when it doesn't exist.
    fn read_workspace(&mut self, path: &str) -> Result<Option<String>, String>;
    fn write_workspace(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn is_workspace_dir(&mut self, path: &str) -> bool;
    /// A file outside the workspace (Bazel's output base), absolute; `None` when it doesn't exist.
    fn read_text(&mut self, path: &str) -> Result<Option<String>, String>;
    fn read_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, String>;
    fn exists(&mut self, path: &str) -> bool;
    /// The plugin's private directory (absolute).
    fn private_dir(&mut self) -> Result<String, String>;
    fn write_private(&mut self, path: &str, text: &str) -> Result<(), String>;
    /// Runs Bazel in the workspace to its end, reporting its progress; `step` names it for the user.
    fn run_bazel(&mut self, args: &[String], step: &str) -> Result<BazelRun, SyncError>;
    /// Progress for the user (and the host's watchdog).
    fn report(&mut self, message: &str);
    fn cancelled(&mut self) -> bool;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BazelRun {
    pub exit_code: i32,
    pub stdout: String,
    /// The last lines Bazel printed on stderr, for errors.
    pub stderr_tail: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(m) => f.write_str(m),
        }
    }
}

impl From<String> for SyncError {
    fn from(m: String) -> Self {
        Self::Failed(m)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub targets: usize,
    pub info_files: usize,
    pub source_roots: usize,
    pub libraries: usize,
    pub build_exit_code: i32,
    /// The Eclipse project changed: jdtls needs a restart to see it.
    pub project_changed: bool,
    /// `.openidea/settings.toml` changed: it applies when the project is opened again.
    pub settings_changed: bool,
    pub warnings: Vec<String>,
}

pub fn sync(host: &mut dyn SyncHost) -> Result<SyncSummary, SyncError> {
    let mut log = Vec::new();
    let result = run(host, &mut log);
    let mut text = log.join("\n");
    match &result {
        Ok(s) => text.push_str(&format!(
            "\n\nSynced {} targets ({} info files): {} source roots, {} libraries. Bazel exit code {}.\n",
            s.targets, s.info_files, s.source_roots, s.libraries, s.build_exit_code
        )),
        Err(e) => text.push_str(&format!("\n\nSync failed: {e}\n")),
    }
    // The log is a convenience: a failure to write it must not hide the sync's own outcome.
    let _ = host.write_workspace(SYNC_LOG, &text);
    result
}

fn run(host: &mut dyn SyncHost, log: &mut Vec<String>) -> Result<SyncSummary, SyncError> {
    let root = host.workspace_root()?;
    let mut summary = SyncSummary::default();
    let mut timing = Timing::new();

    host.report("Reading the project view");
    let view = read_project_view(host)?;
    for w in &view.warnings {
        log.push(format!("project view: {w}"));
    }
    let workspace_name = root
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("workspace")
        .to_string();
    // A workspace's first sync takes minutes, and the IDE has opened it as a Maven project:
    // jdtls' Maven import and the Maven run configurations go off now, not when it ends.
    if !host
        .read_workspace(SETTINGS)?
        .is_some_and(|t| settings::is_managed(&t))
    {
        let private = host.private_dir()?;
        summary.settings_changed = write_settings(host, &jdtls_data_dir(&private, &root, ""), log)?;
    }
    let bazelignore = host.read_workspace(".bazelignore")?;
    let roots =
        import_roots::import_roots(&view, bazelignore.as_deref(), &workspace_name, DATA_DIR);
    check_cancel(host)?;

    let mut flags = view.build_flags.clone();
    flags.extend(view.sync_flags.iter().cloned());
    let mut info_args = vec!["info".to_string()];
    info_args.extend(flags.iter().cloned());
    info_args.extend(["execution_root", "output_base", "bazel-bin", "release"].map(String::from));
    let run = host.run_bazel(&info_args, "bazel info")?;
    if run.exit_code != 0 {
        return Err(failure("bazel info", &run));
    }
    timing.mark("bazel info");
    let info = query::parse_bazel_info(&run.stdout);
    if info.execution_root.is_empty() || info.bazel_bin.is_empty() {
        return Err(SyncError::Failed(format!(
            "bazel info printed no execution_root/bazel-bin:\n{}",
            run.stdout
        )));
    }
    log.push(format!(
        "{} — execution root {}",
        info.release, info.execution_root
    ));

    host.report("Writing the aspect");
    write_if_changed(host, &format!("{DATA_DIR}/.gitignore"), "*\n")?;
    for (name, text) in aspect::aspect_files(info.release_major) {
        write_if_changed(host, &format!("{}/{name}", aspect::ASPECT_DIR), &text)?;
    }

    let private = host.private_dir()?;
    let targets = derive_targets(host, &view, &roots, &root)?;
    timing.mark("query");
    if targets.is_empty() {
        return Err(SyncError::Failed(
            "no targets to sync: check `directories:` and `targets:` in .bazelproject".into(),
        ));
    }
    summary.targets = targets.len();
    log.push(format!("{} targets to sync", targets.len()));

    // Bazel's scratch files live in the workspace, so syncs of different worktrees never share them.
    host.write_workspace(TARGETS_FILE, &(targets.join("\n") + "\n"))?;
    let target_file = join(&root, TARGETS_FILE);
    let build_args = query::sync_build_args(&view, &info, &aspect::aspect_flag(), &target_file);
    let run = host.run_bazel(&build_args, "Building with the IntelliJ aspect")?;
    timing.mark("build");
    summary.build_exit_code = run.exit_code;
    if !BUILD_OK.contains(&run.exit_code) {
        return Err(failure("the aspect build", &run));
    }
    if run.exit_code != 0 {
        let message = "the build had errors; the project reflects the targets that built";
        log.push(format!(
            "warning: {message}\n{}",
            run.stderr_tail.join("\n")
        ));
        summary.warnings.push(message.into());
    }

    let labels: Vec<String> = targets
        .iter()
        .filter(|t| is_single_label(t))
        .cloned()
        .collect();
    let target_map = read_target_map(host, &info, &labels, log)?;
    summary.info_files = target_map.len();
    write_if_changed(host, TARGET_INDEX, &target_index(&target_map))?;
    timing.mark("info files");
    check_cancel(host)?;

    host.report("Computing the Java project");
    let model = {
        let mut files = HostFiles {
            host: &mut *host,
            info: &info,
            reads: 0,
        };
        model::build_java_model(&target_map, &view, &roots, &info, &root, &mut files)
    };
    for w in &model.warnings {
        log.push(format!("model: {w}"));
    }
    timing.mark("model");
    summary.source_roots = model.source_roots.len();
    summary.libraries = model.libraries.len();
    summary.warnings.extend(model.warnings.iter().cloned());

    host.report("Writing the Eclipse project");
    let (project_changed, links_key) =
        write_eclipse(host, &model, &workspace_name, log, &mut summary.warnings)?;
    summary.project_changed = project_changed;
    let data_dir = jdtls_data_dir(&private, &root, &links_key);
    summary.settings_changed |= write_settings(host, &data_dir, log)?;
    timing.mark("project files");
    log.push(timing.line());
    Ok(summary)
}

/// Wall-clock time of each step of a sync, for its log (IntelliJ's sync log reports them too).
struct Timing {
    last: std::time::Instant,
    steps: Vec<(&'static str, std::time::Duration)>,
}

impl Timing {
    fn new() -> Self {
        Timing {
            last: std::time::Instant::now(),
            steps: Vec::new(),
        }
    }

    /// Ends `step` now.
    fn mark(&mut self, step: &'static str) {
        let now = std::time::Instant::now();
        self.steps.push((step, now - self.last));
        self.last = now;
    }

    fn line(&self) -> String {
        let steps: Vec<String> = self
            .steps
            .iter()
            .map(|(step, d)| format!("{step} {}", format_duration(*d)))
            .collect();
        format!("timing: {}", steps.join(" · "))
    }
}

/// `0.4 s`, `12 s`, `8 min 15 s`.
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1} s")
    } else if secs < 60.0 {
        format!("{} s", secs.round() as u64)
    } else {
        let whole = secs.round() as u64;
        format!("{} min {} s", whole / 60, whole % 60)
    }
}

fn read_project_view(host: &mut dyn SyncHost) -> Result<ProjectView, SyncError> {
    match host.read_workspace(PROJECT_VIEW)? {
        Some(text) => {
            let mut read_import = |path: &str| host.read_workspace(path).ok().flatten();
            Ok(projectview::parse(&text, PROJECT_VIEW, &mut read_import))
        }
        // IntelliJ's default for a new project: the whole workspace, targets derived from it.
        None => Ok(ProjectView {
            directories: vec![DirectoryEntry {
                path: String::new(),
                included: true,
            }],
            derive_targets_from_directories: true,
            warnings: vec![format!("no {PROJECT_VIEW}: syncing the whole workspace")],
            ..ProjectView::default()
        }),
    }
}

fn derive_targets(
    host: &mut dyn SyncHost,
    view: &ProjectView,
    roots: &crate::types::ImportRoots,
    root: &str,
) -> Result<Vec<String>, SyncError> {
    let mut derived = Vec::new();
    let query_expr = query::derive_targets_query(roots, view.allow_manual_targets_sync);
    if view.derive_targets_from_directories && !query_expr.is_empty() {
        let output = join(root, QUERY_FILE);
        let mut args = vec!["query".to_string()];
        args.extend(view.build_flags.iter().cloned());
        args.extend(
            [
                "--output=label_kind",
                "--keep_going",
                &format!("--output_file={output}"),
                "--",
            ]
            .map(String::from),
        );
        args.push(query_expr);
        let run = host.run_bazel(&args, "Finding the targets")?;
        if !BUILD_OK.contains(&run.exit_code) {
            return Err(failure("bazel query", &run));
        }
        let text = host.read_workspace(QUERY_FILE)?.unwrap_or_default();
        derived = query::parse_label_kind(&text);
    }
    Ok(query::sync_targets(&derived, view))
}

/// The aspect's info files of the synced targets and, through their deps, of what they depend on.
/// Paths are computed like the aspect names them, so no build event file has to be read.
fn read_target_map(
    host: &mut dyn SyncHost,
    info: &BazelInfo,
    targets: &[String],
    log: &mut Vec<String>,
) -> Result<TargetMap, SyncError> {
    let mut map = TargetMap::new();
    let mut queue: VecDeque<(TargetKey, u32)> = targets
        .iter()
        .map(|l| {
            (
                TargetKey {
                    label: l.clone(),
                    aspect_ids: vec![],
                },
                0,
            )
        })
        .collect();
    let mut seen: FxHashSet<u64> = queue.iter().map(|(k, _)| key_fingerprint(k)).collect();
    let mut missing_top_level = 0usize;
    while let Some((key, depth)) = queue.pop_front() {
        if map.len().is_multiple_of(100) {
            check_cancel(host)?;
            host.report(&format!("Reading target info ({} read)", map.len()));
        }
        let path = ideinfo::info_file_path(&info.bazel_bin, &key);
        // Raw bytes: the aspect writes ASCII, which needs none of the host's text decoding.
        let Some(bytes) = host.read_bytes(&path)? else {
            if depth == 0 {
                missing_top_level += 1;
            }
            continue;
        };
        let text = String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        // A file Bazel (or IntelliJ, which shares the output base) is rewriting reads truncated:
        // skip the target rather than the sync.
        let target = match ideinfo::parse_target_ide_info(&text) {
            Ok(target) => target,
            Err(e) => {
                log.push(format!("warning: skipped {path}: {e}"));
                continue;
            }
        };
        if depth < INFO_DEPTH {
            for dep in &target.deps {
                if seen.insert(key_fingerprint(&dep.key)) {
                    queue.push_back((dep.key.clone(), depth + 1));
                }
            }
        }
        map.insert(target.key.clone(), target);
    }
    if missing_top_level > 0 {
        log.push(format!(
            "{missing_top_level} synced targets have no aspect output (not Java, incompatible or failed to build)"
        ));
    }
    Ok(map)
}

fn write_eclipse(
    host: &mut dyn SyncHost,
    model: &JavaModel,
    name: &str,
    log: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(bool, String), SyncError> {
    let files = eclipse::emit(model, name);
    for w in &files.warnings {
        log.push(format!("eclipse: {w}"));
        warnings.push(w.clone());
    }
    // jdtls adds its own resource filter to `.project` after importing it; that alone is no change.
    // The new file keeps that filter: a running jdtls that finds it missing writes `.project` back
    // from its own description, links of the previous sync included (measured with jdtls 1.63).
    let existing = host.read_workspace(".project")?;
    if existing.as_deref().map(strip_jdtls_filters) != Some(files.project.clone()) {
        let project = match existing.as_deref().map(jdtls_filters) {
            Some(kept) if !kept.is_empty() => files.project.replacen(
                "\t</filteredResources>",
                &format!("{kept}\t</filteredResources>"),
                1,
            ),
            _ => files.project,
        };
        host.write_workspace(".project", &project)
            .map_err(|e| format!("writing .project: {e}"))?;
    }
    // What jdtls must reload: the classpath and the compiler settings (the filters only hide folders).
    let mut changed = write_if_changed(host, ".classpath", &files.classpath)?;
    changed |= write_if_changed(
        host,
        ".settings/org.eclipse.jdt.core.prefs",
        &files.jdt_core_prefs,
    )?;
    Ok((changed, files.links_key))
}

fn write_settings(
    host: &mut dyn SyncHost,
    jdtls_data_dir: &str,
    log: &mut Vec<String>,
) -> Result<bool, SyncError> {
    let existing = host.read_workspace(SETTINGS)?;
    let mut merged = settings::merge_openidea_settings(existing.as_deref(), jdtls_data_dir)
        .map_err(|e| format!("{SETTINGS} could not be updated: {e}"))?;
    // Bazel builds the workspace, as in IntelliJ's Bazel projects: a leftover root POM must not
    // turn editor test runs into Maven builds.
    if host.read_workspace("pom.xml")?.is_some() {
        merged = settings::unlink_maven_poms(&merged, &["pom.xml"])
            .map_err(|e| format!("{SETTINGS} could not be updated: {e}"))?;
        log.push("the root pom.xml is unlinked from OpenIDEA's Maven model".into());
    }
    let changed = write_if_changed(host, SETTINGS, &merged)?;
    let patterns = [
        "/.openidea/settings.toml",
        "/.project",
        "/.classpath",
        "/.settings/",
    ];
    if host.is_workspace_dir(".git") {
        let exclude = host.read_workspace(GIT_EXCLUDE)?;
        if let Some(text) = settings::ensure_git_excludes(exclude.as_deref(), &patterns) {
            host.write_workspace(GIT_EXCLUDE, &text)?;
        }
    } else if host
        .read_workspace(".git")?
        .is_some_and(|t| t.starts_with("gitdir:"))
    {
        // A linked worktree shares the main checkout's `.git/info/exclude`, which is outside this
        // project and so out of the plugin's reach.
        log.push(format!(
            "note: this is a Git worktree; to keep the generated files out of `git status`, add these lines to the main checkout's .git/info/exclude: {}",
            patterns.join(" ")
        ));
    }
    Ok(changed)
}

/// `.project` without the `<filter>` jdtls adds (its arguments end in
/// `__CREATED_BY_JAVA_LANGUAGE_SERVER__`), and without `<filteredResources>` if that leaves it empty.
fn strip_jdtls_filters(project: &str) -> String {
    let mut out = project.to_string();
    loop {
        let Some(range) = jdtls_filter_ranges(&out).next() else {
            return out;
        };
        out.replace_range(range, "");
    }
}

/// The resource filters jdtls added to `.project`, as they are written there.
fn jdtls_filters(project: &str) -> String {
    jdtls_filter_ranges(project).map(|r| &project[r]).collect()
}

/// Where each filter jdtls added is, its own lines (indentation and line break) included.
fn jdtls_filter_ranges(project: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    const MARK: &str = "__CREATED_BY_JAVA_LANGUAGE_SERVER__";
    project.match_indices(MARK).filter_map(|(mark, _)| {
        let start = project[..mark].rfind("<filter>")?;
        let end = mark + project[mark..].find("</filter>")? + "</filter>".len();
        let start = project[..start].rfind('\n').map_or(start, |i| i + 1);
        let end = project[end..].find('\n').map_or(end, |i| end + i + 1);
        Some(start..end)
    })
}

fn write_if_changed(host: &mut dyn SyncHost, path: &str, text: &str) -> Result<bool, SyncError> {
    if host.read_workspace(path)?.as_deref() == Some(text) {
        return Ok(false);
    }
    host.write_workspace(path, text)
        .map_err(|e| format!("writing {path}: {e}"))?;
    Ok(true)
}

fn failure(step: &str, run: &BazelRun) -> SyncError {
    SyncError::Failed(format!(
        "{step} failed (exit code {}):\n{}",
        run.exit_code,
        run.stderr_tail.join("\n")
    ))
}

/// Main-repository sources and the targets building them, sorted (`path\tlabel\tkind`), plus
/// `class:<test class>` lines for test targets that name their class instead of listing sources
/// (one `java_test` per class over a shared library; IntelliJ's `TestTargetHeuristic` matches both).
pub fn target_index(targets: &TargetMap) -> String {
    let mut lines: Vec<String> = targets
        .values()
        .filter(|t| t.key.aspect_ids.is_empty())
        .flat_map(|t| {
            let java = t.java_ide_info.as_ref();
            let sources = java
                .into_iter()
                .flat_map(|j| &j.sources)
                .filter(|s| s.is_main_repo_source());
            let class = java
                .filter(|j| !j.test_class.is_empty())
                .map(|j| format!("class:{}", j.test_class));
            sources
                .map(|s| s.relative_path.clone())
                .chain(class)
                .map(move |key| format!("{key}\t{}\t{}", t.key.label, t.kind_string))
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines.join("\n") + "\n"
}

/// The test targets for `path` (workspace-relative) whose class is `class` (`a.b.CTest`), from a
/// [`target_index`]: test targets building the file, then those naming its class.
pub fn test_targets_of<'a>(index: &'a str, path: &str, class: Option<&str>) -> Vec<&'a str> {
    let class_key = class.map(|c| format!("class:{c}"));
    let rows: Vec<(&str, &str, &str)> = index
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            Some((f.next()?, f.next()?, f.next()?))
        })
        .collect();
    let is_test = |kind: &str| kind.ends_with("_test") || kind == "test_suite";
    let by_source = rows.iter().filter(|(p, _, k)| *p == path && is_test(k));
    let by_class = rows
        .iter()
        .filter(|(p, _, _)| Some(*p) == class_key.as_deref());
    let mut labels: Vec<&str> = by_source.chain(by_class).map(|(_, l, _)| *l).collect();
    labels.dedup();
    labels
}

/// The targets `bazel run` can start, from the sync's query result (`<kind> rule <label>` lines),
/// sorted by label: rules whose kind ends in `_binary`, except Starlark-private ones
/// (`_run_binary` is a build action). `java_only` keeps `java_binary`, whose launcher takes
/// `--debug`.
pub fn runnable_targets(query: &str, java_only: bool) -> Vec<(&str, &str)> {
    let mut targets: Vec<(&str, &str)> = query
        .lines()
        .filter_map(|line| {
            let (kind, rest) = line.split_once(' ')?;
            let label = rest.strip_prefix("rule ")?.trim();
            let runnable = if java_only {
                kind == "java_binary"
            } else {
                kind.ends_with("_binary") && !kind.starts_with('_')
            };
            runnable.then_some((label, kind))
        })
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// The workspace packages whose targets the index lists, as BUILD-file directories ("" is the root).
pub fn indexed_packages(index: &str) -> BTreeSet<&str> {
    index
        .lines()
        .filter_map(|l| l.split('\t').nth(1)?.strip_prefix("//")?.split_once(':'))
        .map(|(package, _)| package)
        .collect()
}

/// A target pattern file line naming one target (not `-//x`, `//x/...` or `//x:all`).
fn is_single_label(pattern: &str) -> bool {
    let p = pattern.trim();
    !p.is_empty()
        && !p.starts_with('-')
        && !p.ends_with("...")
        && !p.contains("/...:")
        && !p.ends_with(":all")
        && !p.ends_with(":*")
        && !p.ends_with(":all-targets")
}

fn check_cancel(host: &mut dyn SyncHost) -> Result<(), SyncError> {
    if host.cancelled() {
        Err(SyncError::Cancelled)
    } else {
        Ok(())
    }
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// The workspace's jdtls data directory: one per workspace and set of linked resources, stable
/// across syncs that keep them (jdtls rereads the classpath when it starts, but creates linked
/// resources only when it imports the project), whatever form the host gives the root in.
fn jdtls_data_dir(private: &str, root: &str, links_key: &str) -> String {
    let root = root.trim_end_matches('/');
    let key = if links_key.is_empty() {
        fnv1a(root.as_bytes())
    } else {
        fnv1a(format!("{root}\n{links_key}").as_bytes())
    };
    join(private, &format!("jdtls/{key:016x}"))
}

/// A stable name for the workspace's jdtls data directory.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// The model's file access, over the host; keeps the host's watchdog fed while it reads.
struct HostFiles<'a> {
    host: &'a mut dyn SyncHost,
    info: &'a BazelInfo,
    reads: u32,
}

impl HostFiles<'_> {
    fn tick(&mut self) {
        self.reads += 1;
        if self.reads.is_multiple_of(200) {
            self.host.report(&format!(
                "Computing the Java project ({} files read)",
                self.reads
            ));
        }
    }
}

impl SyncFiles for HostFiles<'_> {
    fn read_workspace_file(&mut self, path: &str) -> Option<String> {
        self.tick();
        self.host.read_workspace(path).ok().flatten()
    }

    fn read_exec_bytes(&mut self, exec_path: &str) -> Option<Vec<u8>> {
        self.tick();
        let path = join(&self.info.execution_root, exec_path);
        self.host.read_bytes(&path).ok().flatten()
    }

    fn exists(&mut self, abs_path: &str) -> bool {
        self.tick();
        self.host.exists(abs_path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const EXEC_ROOT: &str = "/ob/execroot/_main";
    const BIN: &str = "/ob/execroot/_main/bazel-out/k8-fastbuild/bin";

    /// A workspace in memory and a Bazel that answers from canned output.
    struct Fake {
        workspace: BTreeMap<String, String>,
        outside: BTreeMap<String, Vec<u8>>,
        runs: Vec<Vec<String>>,
        query_output: String,
        info_exit: i32,
        build_exit: i32,
        cancelled: bool,
    }

    fn key(label: &str) -> TargetKey {
        TargetKey {
            label: label.into(),
            aspect_ids: vec![],
        }
    }

    const LIB_INFO: &str = r#"key {
  label: "//app:lib"
}
kind_string: "java_library"
deps {
  dependency_type: 0
  target {
    label: "@@maven//:guava"
  }
}
java_ide_info {
  jars {
    interface_jar {
      relative_path: "app/liblib-ijar.jar"
      root_execution_path_fragment: "bazel-out/k8-fastbuild/bin"
    }
    jar {
      relative_path: "app/liblib.jar"
      root_execution_path_fragment: "bazel-out/k8-fastbuild/bin"
    }
  }
  sources {
    is_source: true
    relative_path: "app/src/main/java/com/x/A.java"
    root_execution_path_fragment: ""
  }
}
"#;

    const GUAVA_INFO: &str = r#"key {
  label: "@@maven//:guava"
}
kind_string: "jvm_import"
java_ide_info {
  jars {
    interface_jar {
      is_external: true
      relative_path: "external/maven/guava/header_guava.jar"
      root_execution_path_fragment: "bazel-out/k8-fastbuild/bin"
    }
    jar {
      is_external: true
      relative_path: "external/maven/guava/processed_guava.jar"
      root_execution_path_fragment: "bazel-out/k8-fastbuild/bin"
    }
    source_jars {
      is_external: true
      relative_path: "external/maven/guava/guava-sources.jar"
      root_execution_path_fragment: "bazel-out/k8-fastbuild/bin"
    }
  }
}
"#;

    impl Fake {
        fn new() -> Self {
            let mut fake = Fake {
                workspace: BTreeMap::new(),
                outside: BTreeMap::new(),
                runs: vec![],
                query_output: "java_library rule //app:lib\nfilegroup rule //app:files\n".into(),
                info_exit: 0,
                build_exit: 0,
                cancelled: false,
            };
            fake.workspace.insert(PROJECT_VIEW.into(), "directories:\n  .\n\nderive_targets_from_directories: true\njava_language_level: 17\n".into());
            fake.workspace.insert(
                "app/src/main/java/com/x/A.java".into(),
                "package com.x;\n\nclass A {}\n".into(),
            );
            fake.workspace.insert(
                ".git/info/exclude".into(),
                "# git ls-files --others --exclude-from=.git/info/exclude\n".into(),
            );
            fake.outside.insert(
                ideinfo::info_file_path(BIN, &key("//app:lib")),
                LIB_INFO.into(),
            );
            fake.outside.insert(
                ideinfo::info_file_path(BIN, &key("@@maven//:guava")),
                GUAVA_INFO.into(),
            );
            for jar in [
                "processed_guava.jar",
                "header_guava.jar",
                "guava-sources.jar",
            ] {
                fake.outside
                    .insert(format!("{BIN}/external/maven/guava/{jar}"), vec![]);
            }
            fake
        }

        fn ran(&self, command: &str) -> Option<&Vec<String>> {
            self.runs.iter().find(|r| r[0] == command)
        }
    }

    impl SyncHost for Fake {
        fn workspace_root(&mut self) -> Result<String, String> {
            Ok("/ws/demo".into())
        }
        fn read_workspace(&mut self, path: &str) -> Result<Option<String>, String> {
            Ok(self.workspace.get(path).cloned())
        }
        fn write_workspace(&mut self, path: &str, text: &str) -> Result<(), String> {
            self.workspace.insert(path.into(), text.into());
            Ok(())
        }
        fn is_workspace_dir(&mut self, path: &str) -> bool {
            self.workspace
                .keys()
                .any(|k| k.starts_with(&format!("{path}/")))
        }
        fn read_text(&mut self, path: &str) -> Result<Option<String>, String> {
            Ok(self
                .outside
                .get(path)
                .map(|b| String::from_utf8_lossy(b).into_owned()))
        }
        fn read_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.outside.get(path).cloned())
        }
        fn exists(&mut self, path: &str) -> bool {
            self.outside.contains_key(path)
        }
        fn private_dir(&mut self) -> Result<String, String> {
            Ok("/private".into())
        }
        fn write_private(&mut self, path: &str, text: &str) -> Result<(), String> {
            self.outside.insert(path.into(), text.as_bytes().to_vec());
            Ok(())
        }
        fn run_bazel(&mut self, args: &[String], _step: &str) -> Result<BazelRun, SyncError> {
            if self.cancelled {
                return Err(SyncError::Cancelled);
            }
            self.runs.push(args.to_vec());
            match args[0].as_str() {
                "info" => Ok(BazelRun {
                    exit_code: self.info_exit,
                    stdout: format!(
                        "execution_root: {EXEC_ROOT}\noutput_base: /ob\nbazel-bin: {BIN}\nrelease: release 7.7.1\n"
                    ),
                    stderr_tail: vec!["ERROR: info failed".into()],
                }),
                "query" => {
                    let out = args
                        .iter()
                        .find_map(|a| a.strip_prefix("--output_file="))
                        .expect("--output_file");
                    let out = out.strip_prefix("/ws/demo/").expect("in the workspace");
                    self.workspace.insert(out.into(), self.query_output.clone());
                    Ok(BazelRun::default())
                }
                _ => Ok(BazelRun {
                    exit_code: self.build_exit,
                    stdout: String::new(),
                    stderr_tail: vec!["ERROR: build failed".into()],
                }),
            }
        }
        fn report(&mut self, _message: &str) {}
        fn cancelled(&mut self) -> bool {
            self.cancelled
        }
    }

    #[test]
    fn a_first_sync_turns_maven_off_before_bazel_runs() {
        let mut fake = Fake::new();
        fake.workspace.insert("pom.xml".into(), "<project/>".into());
        fake.info_exit = 1;
        assert!(sync(&mut fake).is_err(), "bazel info failed");
        let settings = &fake.workspace[SETTINGS];
        assert!(settings.contains("enabled = false"), "{settings}");
        assert!(
            settings.contains("unlinkedProjects = [\"pom.xml\"]"),
            "{settings}"
        );

        // Once managed, only the end of a sync writes them.
        fake.workspace
            .insert(SETTINGS.into(), "# Managed by openidea-bazel\n[x]\n".into());
        assert!(sync(&mut fake).is_err());
        assert_eq!(
            fake.workspace[SETTINGS],
            "# Managed by openidea-bazel\n[x]\n"
        );
    }

    #[test]
    fn a_root_pom_is_unlinked_from_maven() {
        let mut fake = Fake::new();
        sync(&mut fake).unwrap();
        assert!(!fake.workspace[SETTINGS].contains("unlinkedProjects"));
        fake.workspace.insert("pom.xml".into(), "<project/>".into());
        let s = sync(&mut fake).unwrap();
        assert!(s.settings_changed);
        assert!(
            fake.workspace[SETTINGS].contains("unlinkedProjects = [\"pom.xml\"]"),
            "{}",
            fake.workspace[SETTINGS]
        );
    }

    #[test]
    fn durations_read_like_a_sync_log() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(400)), "0.4 s");
        assert_eq!(format_duration(Duration::from_millis(12_400)), "12 s");
        assert_eq!(format_duration(Duration::from_secs(495)), "8 min 15 s");
    }

    #[test]
    fn runnable_targets_are_public_binaries() {
        let query = "java_binary rule //app:run\n\
                     _run_binary rule //gen:x\n\
                     sh_binary rule //tools:fmt\n\
                     java_library rule //app:lib\n\
                     alias rule //app:app\n\
                     java_binary rule //a:main\n";
        assert_eq!(
            runnable_targets(query, false),
            [
                ("//a:main", "java_binary"),
                ("//app:run", "java_binary"),
                ("//tools:fmt", "sh_binary")
            ]
        );
        assert_eq!(
            runnable_targets(query, true),
            [("//a:main", "java_binary"), ("//app:run", "java_binary")]
        );
    }

    #[test]
    fn indexed_packages_are_the_workspace_packages_of_the_index() {
        let index = "app/A.java\t//app:lib\tjava_library\n\
                     class:x.ATest\t//app/test:ATest\tjava_test\n\
                     MODULE.bazel\t//:root\tfilegroup\n\
                     x\t@maven//:guava\tjava_import\n";
        assert_eq!(
            indexed_packages(index).into_iter().collect::<Vec<_>>(),
            ["", "app", "app/test"]
        );
    }

    #[test]
    fn syncs_a_workspace_into_an_eclipse_project_for_jdtls() {
        let mut fake = Fake::new();
        let s = sync(&mut fake).unwrap();
        assert_eq!(
            (s.targets, s.info_files, s.source_roots, s.libraries),
            (1, 2, 1, 1)
        );
        assert!(s.project_changed && s.settings_changed);

        let build = fake.ran("build").unwrap();
        assert!(build.contains(&"--aspects=//.openidea/bazel/aspects:intellij_info_bundled.bzl%intellij_info_aspect".to_string()));
        assert!(
            build.contains(
                &"--target_pattern_file=/ws/demo/.openidea/bazel/targets.txt".to_string()
            )
        );
        assert_eq!(fake.workspace[TARGETS_FILE], "//app:lib\n");

        let classpath = &fake.workspace[".classpath"];
        assert!(
            classpath.contains(r#"path="app/src/main/java""#),
            "{classpath}"
        );
        assert!(
            classpath.contains(&format!("{BIN}/external/maven/guava/processed_guava.jar")),
            "{classpath}"
        );
        assert!(classpath.contains("JavaSE-17"));
        assert!(fake.workspace.contains_key(".project"));
        assert!(
            fake.workspace
                .contains_key(".settings/org.eclipse.jdt.core.prefs")
        );
        assert!(
            fake.workspace
                .contains_key(".openidea/bazel/aspects/intellij_info_bundled.bzl")
        );
        assert_eq!(fake.workspace[".openidea/bazel/.gitignore"], "*\n");

        let settings = &fake.workspace[".openidea/settings.toml"];
        assert!(settings.contains("/private/jdtls/"), "{settings}");
        assert!(fake.workspace[".git/info/exclude"].contains("/.openidea/settings.toml"));
        assert!(fake.workspace[SYNC_LOG].contains("Synced 1 targets"));
    }

    #[test]
    fn a_second_sync_changes_nothing_even_after_jdtls_added_its_filter() {
        let mut fake = Fake::new();
        sync(&mut fake).unwrap();
        let with_jdtls_filter = fake.workspace[".project"].replace(
            "\t</filteredResources>",
            "\t\t<filter>\n\t\t\t<id>1794168683519</id>\n\t\t\t<name></name>\n\t\t\t<type>30</type>\n\t\t\t<matcher>\n\t\t\t\t<id>org.eclipse.core.resources.regexFilterMatcher</id>\n\t\t\t\t<arguments>node_modules|\\.git|__CREATED_BY_JAVA_LANGUAGE_SERVER__</arguments>\n\t\t\t</matcher>\n\t\t</filter>\n\t</filteredResources>",
        );
        assert_ne!(with_jdtls_filter, fake.workspace[".project"]);
        fake.workspace
            .insert(".project".into(), with_jdtls_filter.clone());

        let s = sync(&mut fake).unwrap();
        assert!(!s.project_changed && !s.settings_changed);
        assert_eq!(
            fake.workspace[".project"], with_jdtls_filter,
            "jdtls' own filter is kept"
        );
    }

    #[test]
    fn a_changed_project_keeps_the_filter_jdtls_added() {
        let mut fake = Fake::new();
        sync(&mut fake).unwrap();
        let filter = "\t\t<filter>\n\t\t\t<id>1794168683519</id>\n\t\t\t<arguments>node_modules|__CREATED_BY_JAVA_LANGUAGE_SERVER__</arguments>\n\t\t</filter>\n";
        let with_jdtls_filter = fake.workspace[".project"].replace(
            "\t</filteredResources>",
            &format!("{filter}\t</filteredResources>"),
        );
        fake.workspace.insert(".project".into(), with_jdtls_filter);
        // An excluded directory changes the project's own filters.
        fake.workspace.insert(
            PROJECT_VIEW.into(),
            "directories:\n  .\n  -app/gen\n\nderive_targets_from_directories: true\njava_language_level: 17\n"
                .into(),
        );

        sync(&mut fake).unwrap();
        let project = &fake.workspace[".project"];
        assert!(project.contains("^gen$"), "{project}");
        assert!(project.contains(filter), "{project}");
        assert_eq!(
            project
                .matches("__CREATED_BY_JAVA_LANGUAGE_SERVER__")
                .count(),
            1
        );
    }

    #[test]
    fn strips_only_the_jdtls_filter() {
        let ours = "<filteredResources>\n\t\t<filter>\n\t\t\t<arguments>bazel-.*</arguments>\n\t\t</filter>\n\t</filteredResources>\n";
        let theirs = ours.replace(
            "\t</filteredResources>",
            "\t\t<filter>\n\t\t\t<arguments>x|__CREATED_BY_JAVA_LANGUAGE_SERVER__</arguments>\n\t\t</filter>\n\t</filteredResources>",
        );
        assert_eq!(strip_jdtls_filters(&theirs), ours);
        assert_eq!(strip_jdtls_filters(ours), ours);
    }

    #[test]
    fn a_malformed_info_file_skips_its_target_not_the_sync() {
        let mut fake = Fake::new();
        fake.outside.insert(
            ideinfo::info_file_path(BIN, &key("@@maven//:guava")),
            b"key { label: \"@@maven//:guava\"".to_vec(),
        );
        let s = sync(&mut fake).unwrap();
        assert_eq!((s.info_files, s.libraries), (1, 0));
        assert!(
            fake.workspace[SYNC_LOG].contains("warning: skipped"),
            "{}",
            fake.workspace[SYNC_LOG]
        );
    }

    #[test]
    fn bazel_failures_are_reported_with_bazel_output() {
        let mut fake = Fake::new();
        fake.info_exit = 2;
        let Err(SyncError::Failed(m)) = sync(&mut fake) else {
            panic!("expected a failure")
        };
        assert!(
            m.contains("bazel info failed") && m.contains("ERROR: info failed"),
            "{m}"
        );
        assert!(fake.workspace[SYNC_LOG].contains("Sync failed"));

        let mut fake = Fake::new();
        fake.build_exit = 2;
        let Err(SyncError::Failed(m)) = sync(&mut fake) else {
            panic!("expected a failure")
        };
        assert!(m.contains("aspect build failed"), "{m}");
    }

    #[test]
    fn a_partial_build_still_syncs_with_a_warning() {
        let mut fake = Fake::new();
        fake.build_exit = 1;
        let s = sync(&mut fake).unwrap();
        assert_eq!(s.build_exit_code, 1);
        assert!(!s.warnings.is_empty());
    }

    #[test]
    fn a_worktree_gets_a_note_instead_of_git_excludes() {
        let mut fake = Fake::new();
        fake.workspace.remove(".git/info/exclude");
        fake.workspace.insert(
            ".git".into(),
            "gitdir: /ws/main/.git/worktrees/demo\n".into(),
        );
        sync(&mut fake).unwrap();
        assert!(!fake.workspace.contains_key(".git/info/exclude"));
        assert!(
            fake.workspace[SYNC_LOG].contains("Git worktree"),
            "{}",
            fake.workspace[SYNC_LOG]
        );
    }

    #[test]
    fn cancelling_stops_the_sync() {
        let mut fake = Fake::new();
        fake.cancelled = true;
        assert_eq!(sync(&mut fake), Err(SyncError::Cancelled));
    }

    #[test]
    fn the_jdtls_data_dir_does_not_depend_on_a_trailing_slash() {
        assert_eq!(
            jdtls_data_dir("/p", "/ws/demo", ""),
            jdtls_data_dir("/p/", "/ws/demo/", "")
        );
        assert_ne!(
            jdtls_data_dir("/p", "/ws/demo", ""),
            jdtls_data_dir("/p", "/ws/other", "")
        );
    }

    #[test]
    fn a_new_set_of_linked_sources_gets_a_new_jdtls_workspace() {
        let plain = jdtls_data_dir("/p", "/ws/demo", "");
        let linked = jdtls_data_dir("/p", "/ws/demo", ".openidea-src\n.openidea-src/main/A.java");
        assert_ne!(plain, linked);
        assert_eq!(
            linked,
            jdtls_data_dir(
                "/p",
                "/ws/demo/",
                ".openidea-src\n.openidea-src/main/A.java"
            )
        );
        assert_ne!(
            linked,
            jdtls_data_dir("/p", "/ws/demo", ".openidea-src\n.openidea-src/main/B.java")
        );
    }

    #[test]
    fn the_target_index_finds_a_file_s_test_targets() {
        let mut fake = Fake::new();
        sync(&mut fake).unwrap();
        let index = &fake.workspace[TARGET_INDEX];
        assert_eq!(
            index,
            "app/src/main/java/com/x/A.java\t//app:lib\tjava_library\n"
        );
        let with_test = format!(
            "{index}app/src/test/java/ATest.java\t//app:ATest\tjava_test\nclass:x.BTest\t//app:x.BTest\tjava_test\n"
        );
        assert_eq!(
            test_targets_of(&with_test, "app/src/test/java/ATest.java", None),
            ["//app:ATest"]
        );
        assert_eq!(
            test_targets_of(
                &with_test,
                "app/src/test/java/x/BTest.java",
                Some("x.BTest")
            ),
            ["//app:x.BTest"]
        );
        assert!(
            test_targets_of(
                &with_test,
                "app/src/main/java/com/x/A.java",
                Some("com.x.A")
            )
            .is_empty()
        );
    }

    #[test]
    fn single_labels_are_told_from_patterns() {
        assert!(is_single_label("//app:lib"));
        for p in [
            "-//app:lib",
            "//app/...",
            "//app/...:all",
            "//app:all",
            "//app:*",
            "",
        ] {
            assert!(!is_single_label(p), "{p}");
        }
    }
}
