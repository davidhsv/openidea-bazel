//! `ImportRoots`: which workspace directories the project covers (port of `ImportRoots.java`).

use crate::types::{ImportRoots, ProjectView};

/// The project's roots from `directories:`, `.bazelignore` (its text, if the file exists), Bazel's
/// convenience symlinks (`bazel-bin`, `bazel-out`, `bazel-testlogs`, `bazel-genfiles`,
/// `bazel-<workspace_dir_name>`) and the plugin's own data directory `project_data_dir`.
pub fn import_roots(view: &ProjectView, bazelignore: Option<&str>, workspace_dir_name: &str, project_data_dir: &str) -> ImportRoots {
    let _ = (view, bazelignore, workspace_dir_name, project_data_dir);
    unimplemented!("agent A")
}

/// Whether a workspace-relative path is under an include and under no exclude (path components).
pub fn contains(roots: &ImportRoots, path: &str) -> bool {
    let _ = (roots, path);
    unimplemented!("agent A")
}
