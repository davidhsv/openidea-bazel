//! `.intellij-info.txt` files: parsing into [`TargetIdeInfo`] and finding them under `bazel-bin`.

use crate::types::{TargetIdeInfo, TargetKey};

/// Parses one info file (protobuf text format of `blaze.TargetIdeInfo`), ignoring unknown fields.
pub fn parse_target_ide_info(text: &str) -> Result<TargetIdeInfo, String> {
    let _ = text;
    unimplemented!("agent B")
}

/// Java's `String.hashCode`, which Starlark's `hash()` is.
pub fn java_string_hash(s: &str) -> i32 {
    let _ = s;
    unimplemented!("agent B")
}

/// Where the aspect writes the info file of `key` (see `intellij_info_impl_bundled.bzl`):
/// `<bazel_bin>/[external/<repo>/]<package>/<name>-<hash(name)>[-<hash(".".join(aspect_ids))>].intellij-info.txt`.
pub fn info_file_path(bazel_bin: &str, key: &TargetKey) -> String {
    let _ = (bazel_bin, key);
    unimplemented!("agent B")
}
