//! Bazel command lines and their output: `bazel info`, target derivation (`bazel query`) and the sync build.

use crate::types::{BazelInfo, ImportRoots, ProjectView};

/// Parses `bazel info` output (`key: value` lines).
pub fn parse_bazel_info(output: &str) -> BazelInfo {
    let _ = output;
    unimplemented!("agent A")
}

/// The query of `BlazeQueryDirectoryToTargetProvider` for the project's directories.
pub fn derive_targets_query(roots: &ImportRoots, allow_manual_targets: bool) -> String {
    let _ = (roots, allow_manual_targets);
    unimplemented!("agent A")
}

/// Parses `bazel query --output=label_kind` lines (`<kind> rule <label>`) into (kind, label).
pub fn parse_label_kind(output: &str) -> Vec<(String, String)> {
    let _ = output;
    unimplemented!("agent A")
}

/// The labels to build: derived targets of the Java and generic kinds the IDE knows (filegroups dropped),
/// then the explicit `targets:`, minus `-` targets and `exclude_target`.
pub fn sync_targets(derived: &[(String, String)], view: &ProjectView) -> Vec<String> {
    let _ = (derived, view);
    unimplemented!("agent A")
}

/// Arguments after `bazel` for the sync build (`build … --aspects=… --output_groups=…`), reading the
/// labels from `target_pattern_file`.
pub fn sync_build_args(view: &ProjectView, info: &BazelInfo, aspect: &str, target_pattern_file: &str) -> Vec<String> {
    let _ = (view, info, aspect, target_pattern_file);
    unimplemented!("agent A")
}

/// Parses Bazel's `[done / total]` progress from a stderr line.
pub fn parse_progress(line: &str) -> Option<(u32, u32)> {
    let _ = line;
    unimplemented!("agent A")
}
