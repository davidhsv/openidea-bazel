//! The IntelliJ aspect (bazelbuild/intellij, ijwb branch), shipped with the plugin and written into the workspace.

/// Workspace-relative package the aspect is written to.
pub const ASPECT_DIR: &str = ".openidea/bazel/aspects";

/// `--aspects` value.
pub fn aspect_flag() -> String {
    format!("//{ASPECT_DIR}:intellij_info_bundled.bzl%intellij_info_aspect")
}

/// The aspect's files (name, text) realized for a Bazel major version.
pub fn aspect_files(release_major: u32) -> Vec<(&'static str, String)> {
    let _ = release_major;
    unimplemented!("agent D")
}
