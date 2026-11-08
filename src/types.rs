//! Data shared by the sync pipeline: the aspect's target map (a subset of
//! `intellij_ide_info.proto`), the project view, and the Java model handed to
//! the Eclipse emitter.

use std::collections::BTreeMap;

/// `blaze.ArtifactLocation`: `exec_root + root_execution_path_fragment + relative_path`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactLocation {
    pub root_execution_path_fragment: String,
    pub relative_path: String,
    pub is_source: bool,
    pub is_external: bool,
}

impl ArtifactLocation {
    /// The path relative to the execution root, the form jdeps files use.
    pub fn exec_path(&self) -> String {
        if self.root_execution_path_fragment.is_empty() {
            self.relative_path.clone()
        } else {
            format!("{}/{}", self.root_execution_path_fragment, self.relative_path)
        }
    }

    /// A source file of the main repository (resolved against the workspace).
    pub fn is_main_repo_source(&self) -> bool {
        self.is_source && !self.is_external
    }
}

/// `blaze.LibraryArtifact`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryArtifact {
    pub jar: Option<ArtifactLocation>,
    pub interface_jar: Option<ArtifactLocation>,
    /// `source_jars`, falling back to the deprecated `source_jar`.
    pub source_jars: Vec<ArtifactLocation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyType {
    /// `dependency_type: 0`: deps, exports, jars, toolchains…
    CompileTime,
    /// `dependency_type: 1`: runtime_deps.
    Runtime,
}

/// `blaze.Dependency`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub key: TargetKey,
    pub dependency_type: DependencyType,
}

/// `blaze.TargetKey`. Labels keep their form from the info file: `//pkg:name`
/// in the main repository, `@@canonical_repo//pkg:name` outside it.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetKey {
    pub label: String,
    pub aspect_ids: Vec<String>,
}

/// `blaze.JavaIdeInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JavaIdeInfo {
    pub jars: Vec<LibraryArtifact>,
    pub generated_jars: Vec<LibraryArtifact>,
    pub sources: Vec<ArtifactLocation>,
    pub package_manifest: Option<ArtifactLocation>,
    pub jdeps: Option<ArtifactLocation>,
    pub filtered_gen_jar: Option<LibraryArtifact>,
    pub main_class: String,
    pub test_class: String,
    pub plugin_processor_jars: Vec<LibraryArtifact>,
}

/// `blaze.JavaToolchainIdeInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JavaToolchainIdeInfo {
    pub source_version: String,
    pub target_version: String,
}

/// `blaze.TargetIdeInfo`, the fields the Java sync reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetIdeInfo {
    pub key: TargetKey,
    pub kind_string: String,
    pub build_file_artifact_location: Option<ArtifactLocation>,
    pub deps: Vec<Dependency>,
    pub tags: Vec<String>,
    pub java_ide_info: Option<JavaIdeInfo>,
    pub java_toolchain_ide_info: Option<JavaToolchainIdeInfo>,
}

/// Every target read for a sync, by key.
pub type TargetMap = BTreeMap<TargetKey, TargetIdeInfo>;

/// One `directories:` entry: `path` is workspace-relative (`""` is the root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: String,
    pub included: bool,
}

/// One `targets:` entry; `-//x:y` excludes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetExpression {
    pub pattern: String,
    pub excluded: bool,
}

/// The merged project view (`.bazelproject` and its imports). Scalars take the
/// last occurrence, lists concatenate in file order (imports first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectView {
    pub directories: Vec<DirectoryEntry>,
    pub targets: Vec<TargetExpression>,
    pub derive_targets_from_directories: bool,
    pub allow_manual_targets_sync: bool,
    pub workspace_type: Option<String>,
    pub additional_languages: Vec<String>,
    /// `java_language_level: 17` or `JDK_17`, as a feature version.
    pub java_language_level: Option<u32>,
    pub test_sources: Vec<String>,
    pub build_flags: Vec<String>,
    pub sync_flags: Vec<String>,
    pub exclude_target: Vec<String>,
    pub import_target_output: Vec<String>,
    pub excluded_libraries: Vec<String>,
    /// Sections this port doesn't use and lines it couldn't parse.
    pub warnings: Vec<String>,
}

/// `ImportRoots`: workspace-relative directories (`""` is the root), minimized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportRoots {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    /// Excludes that go into the target-derivation query (not `.bazelignore`d ones).
    pub query_excludes: Vec<String>,
}

/// `bazel info` values the sync needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BazelInfo {
    pub execution_root: String,
    pub output_base: String,
    pub bazel_bin: String,
    /// `release 7.7.1` → 7; 0 for a development build.
    pub release_major: u32,
    pub release: String,
}

/// A source folder of the generated project.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceRoot {
    /// Workspace-relative directory.
    pub path: String,
    /// The package of the folder itself; empty when it matches the directory layout from a root.
    pub package_prefix: String,
    pub is_test: bool,
}

/// A library jar of the generated project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    /// Absolute path of the jar put on the classpath.
    pub jar: String,
    pub source_jar: Option<String>,
    /// Only test sources need it.
    pub test_only: bool,
}

/// The project the Eclipse emitter writes, in classpath order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JavaModel {
    pub workspace_root: String,
    pub source_roots: Vec<SourceRoot>,
    pub libraries: Vec<Library>,
    /// Workspace-relative directories hidden from the IDE (ImportRoots excludes and Bazel's symlinks).
    pub excludes: Vec<String>,
    pub language_level: u32,
    pub warnings: Vec<String>,
}
