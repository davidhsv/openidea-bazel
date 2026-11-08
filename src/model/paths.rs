//! Absolute paths of artifacts: port of `ArtifactLocationDecoderImpl.decode` and
//! `ExecutionPathHelper.parse` (bazelbuild/intellij, ijwb branch).

use crate::types::{ArtifactLocation, BazelInfo};

/// `BuildSystemProvider.buildArtifactDirectories` for Bazel: top-level execution-root
/// directories that hold build outputs rather than main-repository sources.
const OUTPUT_DIRS: [&str; 4] = ["bazel-out", "bazel-bin", "bazel-genfiles", "bazel-testlogs"];

/// Whether `path` is `ancestor` or below it, by whole path components (`""` contains everything).
pub(crate) fn is_under(ancestor: &str, path: &str) -> bool {
    ancestor.is_empty()
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The directory of a `/`-separated path (`""` for a top-level file).
pub(crate) fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// Resolves artifacts against the workspace, the execution root and the output base.
pub(crate) struct PathDecoder<'a> {
    workspace_root: &'a str,
    execution_root: &'a str,
    output_base: &'a str,
}

impl<'a> PathDecoder<'a> {
    pub(crate) fn new(workspace_root: &'a str, info: &'a BazelInfo) -> Self {
        Self {
            workspace_root: trim_dir(workspace_root),
            execution_root: trim_dir(&info.execution_root),
            output_base: trim_dir(&info.output_base),
        }
    }

    /// `ArtifactLocationDecoderImpl.decode`: main-repository sources live in the workspace,
    /// everything else under the execution root. External repositories are read from
    /// `<output_base>/external` because the execution root links only some of them.
    pub(crate) fn artifact(&self, loc: &ArtifactLocation) -> String {
        if loc.is_main_repo_source() {
            return join(self.workspace_root, &loc.relative_path);
        }
        let exec_path = loc.exec_path();
        self.outside_workspace(&exec_path)
            .unwrap_or_else(|| join(self.execution_root, &exec_path))
    }

    /// An execution-root-relative path as jdeps files list them, classified like
    /// `ExecutionPathHelper.parse`: `external/…` is an external repository, a path under an
    /// output directory is generated, anything else is a main-repository source.
    pub(crate) fn exec_path(&self, exec_path: &str) -> String {
        if let Some(path) = self.outside_workspace(exec_path) {
            return path;
        }
        if is_output_path(exec_path) {
            join(self.execution_root, exec_path)
        } else {
            join(self.workspace_root, exec_path)
        }
    }

    fn outside_workspace(&self, exec_path: &str) -> Option<String> {
        if exec_path.starts_with("external/") {
            return Some(join(self.output_base, exec_path));
        }
        // --experimental_sibling_repository_layout puts external repositories next to the
        // main repository's execution root.
        exec_path
            .strip_prefix("../")
            .map(|rest| join(parent(self.execution_root), rest))
    }
}

/// The relative part `ExecutionPathHelper.parse` derives from an execution-root path:
/// `bazel-out/<config>/bin/<relative>` loses its three output components.
pub(crate) fn exec_relative_path(exec_path: &str) -> &str {
    if !is_output_path(exec_path) {
        return exec_path;
    }
    let mut separators = exec_path.match_indices('/').map(|(i, _)| i);
    match separators.nth(2) {
        Some(third) => &exec_path[third + 1..],
        None => "",
    }
}

fn is_output_path(exec_path: &str) -> bool {
    let first = exec_path.split('/').next().unwrap_or_default();
    exec_path.contains('/') && OUTPUT_DIRS.contains(&first)
}

fn trim_dir(dir: &str) -> &str {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() && dir.starts_with('/') {
        "/"
    } else {
        trimmed
    }
}

fn join(base: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches('/');
    match (base, rel) {
        (_, "") => base.to_owned(),
        ("/", _) => format!("/{rel}"),
        _ => format!("{base}/{rel}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> BazelInfo {
        BazelInfo {
            execution_root: "/ob/execroot/_main/".into(),
            output_base: "/ob".into(),
            ..BazelInfo::default()
        }
    }

    fn loc(fragment: &str, rel: &str, is_source: bool, is_external: bool) -> ArtifactLocation {
        ArtifactLocation {
            root_execution_path_fragment: fragment.into(),
            relative_path: rel.into(),
            is_source,
            is_external,
        }
    }

    #[test]
    fn is_under_compares_whole_components() {
        assert!(is_under("", "a/b"));
        assert!(is_under("a/b", "a/b"));
        assert!(is_under("a/b", "a/b/c"));
        assert!(!is_under("a/b", "a/bc"));
        assert!(!is_under("a/b/c", "a/b"));
    }

    #[test]
    fn parent_of_top_level_file_is_root() {
        assert_eq!(parent("Foo.java"), "");
        assert_eq!(parent("a/b/Foo.java"), "a/b");
    }

    #[test]
    fn decodes_artifact_locations() {
        let info = info();
        let d = PathDecoder::new("/ws/", &info);
        assert_eq!(
            d.artifact(&loc("", "libs/a/Foo.java", true, false)),
            "/ws/libs/a/Foo.java"
        );
        assert_eq!(
            d.artifact(&loc(
                "bazel-out/k8-fastbuild/bin",
                "libs/a/liba.jar",
                false,
                false
            )),
            "/ob/execroot/_main/bazel-out/k8-fastbuild/bin/libs/a/liba.jar"
        );
        // A generated jar of an external repository is still a build output.
        assert_eq!(
            d.artifact(&loc(
                "bazel-out/k8-fastbuild/bin",
                "external/maven/x/header_x.jar",
                false,
                true
            )),
            "/ob/execroot/_main/bazel-out/k8-fastbuild/bin/external/maven/x/header_x.jar"
        );
        assert_eq!(
            d.artifact(&loc(
                "external/javax_json",
                "jar/downloaded.jar",
                true,
                true
            )),
            "/ob/external/javax_json/jar/downloaded.jar"
        );
        assert_eq!(
            d.artifact(&loc("../repo", "lib.jar", true, true)),
            "/ob/execroot/repo/lib.jar"
        );
    }

    #[test]
    fn decodes_exec_paths() {
        let info = info();
        let d = PathDecoder::new("/ws", &info);
        assert_eq!(
            d.exec_path("bazel-out/k8/bin/a/liba-hjar.jar"),
            "/ob/execroot/_main/bazel-out/k8/bin/a/liba-hjar.jar"
        );
        assert_eq!(d.exec_path("external/x/lib.jar"), "/ob/external/x/lib.jar");
        assert_eq!(
            d.exec_path("third_party/lib.jar"),
            "/ws/third_party/lib.jar"
        );
    }

    #[test]
    fn relative_path_of_exec_paths() {
        assert_eq!(exec_relative_path("bazel-out/k8/bin/a/b.jar"), "a/b.jar");
        assert_eq!(exec_relative_path("bazel-out/k8/bin"), "");
        assert_eq!(exec_relative_path("external/m/b.jar"), "external/m/b.jar");
        assert_eq!(exec_relative_path("third_party/b.jar"), "third_party/b.jar");
    }
}
