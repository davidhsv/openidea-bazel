//! The Java project model: port of `JavaSourceFilter`, `SourceDirectoryCalculator` and
//! `BlazeJavaWorkspaceImporter` (bazelbuild/intellij, ijwb branch).

use crate::types::{BazelInfo, ImportRoots, JavaModel, ProjectView, TargetMap};

/// Files the model reads while it is computed.
pub trait SyncFiles {
    /// A workspace file (a Java source, to read its `package`), workspace-relative.
    fn read_workspace_file(&mut self, path: &str) -> Option<String>;
    /// A file under the execution root (jdeps), exec-root-relative.
    fn read_exec_bytes(&mut self, exec_path: &str) -> Option<Vec<u8>>;
    /// Whether an absolute path exists (choosing a class jar over an interface jar).
    fn exists(&mut self, abs_path: &str) -> bool;
}

pub fn build_java_model(
    targets: &TargetMap,
    view: &ProjectView,
    roots: &ImportRoots,
    info: &BazelInfo,
    workspace_root: &str,
    files: &mut dyn SyncFiles,
) -> JavaModel {
    let _ = (targets, view, roots, info, workspace_root, files);
    unimplemented!("agent C")
}
