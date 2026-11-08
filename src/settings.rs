//! The project's `.openidea/settings.toml` (jdtls reads Eclipse metadata, not Maven) and `.git/info/exclude`.

/// `existing` with the jdtls settings the Bazel project needs; `jdtls_data_dir` is its `-data` workspace.
pub fn merge_openidea_settings(existing: Option<&str>, jdtls_data_dir: &str) -> Result<String, String> {
    let _ = (existing, jdtls_data_dir);
    unimplemented!("agent D")
}

/// `existing` (a `.git/info/exclude`) with `patterns` appended, or `None` when all are there.
pub fn ensure_git_excludes(existing: Option<&str>, patterns: &[&str]) -> Option<String> {
    let _ = (existing, patterns);
    unimplemented!("agent D")
}
