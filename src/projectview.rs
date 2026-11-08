//! `.bazelproject` parser: port of `ProjectViewParser` and its sections (bazelbuild/intellij, ijwb branch).

use crate::types::ProjectView;

/// Parses `text` (the file `file`, workspace-relative) and, recursively, its `import`/`try_import`s.
/// `read_import` returns the contents of a workspace-relative path, or `None` when it is missing.
pub fn parse(text: &str, file: &str, read_import: &mut dyn FnMut(&str) -> Option<String>) -> ProjectView {
    let _ = (text, file, read_import);
    unimplemented!("agent A")
}
