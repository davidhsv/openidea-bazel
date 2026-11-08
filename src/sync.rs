//! "Sync Project with BUILD Files": the order of the IntelliJ plugin's `SyncPhaseCoordinator`
//! (project view → `bazel info` → aspect files → target derivation → aspect build → target map →
//! Java model), ending in an Eclipse project and the jdtls settings instead of an IntelliJ module.

use std::collections::VecDeque;

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
pub const PROJECT_VIEW: &str = ".bazelproject";
pub const SYNC_LOG: &str = ".openidea/bazel/last-sync.txt";
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
    let targets = derive_targets(host, &view, &roots, &private)?;
    if targets.is_empty() {
        return Err(SyncError::Failed(
            "no targets to sync: check `directories:` and `targets:` in .bazelproject".into(),
        ));
    }
    summary.targets = targets.len();
    log.push(format!("{} targets to sync", targets.len()));

    let target_file = join(&private, "targets.txt");
    host.write_private(&target_file, &(targets.join("\n") + "\n"))?;
    let build_args = query::sync_build_args(&view, &info, &aspect::aspect_flag(), &target_file);
    let run = host.run_bazel(&build_args, "Building with the IntelliJ aspect")?;
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
    summary.source_roots = model.source_roots.len();
    summary.libraries = model.libraries.len();
    summary.warnings.extend(model.warnings.iter().cloned());

    host.report("Writing the Eclipse project");
    summary.project_changed =
        write_eclipse(host, &model, &workspace_name, log, &mut summary.warnings)?;
    let data_dir = join(&private, &format!("jdtls/{:016x}", fnv1a(root.as_bytes())));
    summary.settings_changed = write_settings(host, &data_dir)?;
    Ok(summary)
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
    private: &str,
) -> Result<Vec<String>, SyncError> {
    let mut derived = Vec::new();
    let query_expr = query::derive_targets_query(roots, view.allow_manual_targets_sync);
    if view.derive_targets_from_directories && !query_expr.is_empty() {
        let output = join(private, "query.txt");
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
        let text = host.read_text(&output)?.unwrap_or_default();
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
        let Some(text) = host.read_text(&path)? else {
            if depth == 0 {
                missing_top_level += 1;
            }
            continue;
        };
        let target = ideinfo::parse_target_ide_info(&text).map_err(|e| format!("{path}: {e}"))?;
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
) -> Result<bool, SyncError> {
    let files = eclipse::emit(model, name);
    for w in &files.warnings {
        log.push(format!("eclipse: {w}"));
        warnings.push(w.clone());
    }
    let mut changed = false;
    changed |= write_if_changed(host, ".project", &files.project)?;
    changed |= write_if_changed(host, ".classpath", &files.classpath)?;
    changed |= write_if_changed(
        host,
        ".settings/org.eclipse.jdt.core.prefs",
        &files.jdt_core_prefs,
    )?;
    Ok(changed)
}

fn write_settings(host: &mut dyn SyncHost, jdtls_data_dir: &str) -> Result<bool, SyncError> {
    let existing = host.read_workspace(SETTINGS)?;
    let merged = settings::merge_openidea_settings(existing.as_deref(), jdtls_data_dir)
        .map_err(|e| format!("{SETTINGS} could not be updated: {e}"))?;
    let changed = write_if_changed(host, SETTINGS, &merged)?;
    if host.is_workspace_dir(".git") {
        let exclude = host.read_workspace(GIT_EXCLUDE)?;
        let patterns = [
            "/.openidea/settings.toml",
            "/.project",
            "/.classpath",
            "/.settings/",
        ];
        if let Some(text) = settings::ensure_git_excludes(exclude.as_deref(), &patterns) {
            host.write_workspace(GIT_EXCLUDE, &text)?;
        }
    }
    Ok(changed)
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
