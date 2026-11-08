//! `.bazelproject` parser: port of `ProjectViewParser` and its sections (bazelbuild/intellij, ijwb branch).
//!
//! Sources: `base/.../projectview/parser/{ProjectViewParser,ParseContext}.java`,
//! `base/.../projectview/section/{ListSectionParser,ScalarSectionParser,GlobSectionParser,LabelSectionParser}.java`,
//! `base/.../projectview/section/sections/*.java`, `base/.../projectview/ProjectViewSet.java` (merging) and the
//! Java sections `java/.../java/projectview/*.java` registered by `BlazeJavaSyncPlugin.getSections`.
//!
//! IntelliJ refuses to load a project view with any error; here every problem becomes a line in
//! `ProjectView::warnings` (`file:line: message`) and parsing resumes at the next line or section.

use std::collections::HashSet;
use std::fmt::Display;

use crate::types::{DirectoryEntry, ProjectView, TargetExpression};

/// `SectionParser.INDENT`: list items are indented by exactly this many spaces.
const INDENT: usize = 2;

const WORKSPACE_TYPES: &[&str] = &[
    "android",
    "c",
    "java",
    "dart",
    "python",
    "javascript",
    "go",
    "intellij_plugin",
    "none",
];
const LANGUAGES: &[&str] = &[
    "generic",
    "c",
    "java",
    "android",
    "javascript",
    "typescript",
    "dart",
    "go",
    "python",
    "scala",
    "kotlin",
];

/// Parses `text` (the file `file`, workspace-relative) and, recursively, its `import`/`try_import`s.
/// `read_import` returns the contents of a workspace-relative path, or `None` when it is missing.
pub fn parse(
    text: &str,
    file: &str,
    read_import: &mut dyn FnMut(&str) -> Option<String>,
) -> ProjectView {
    let root = normalize_workspace_path(file).unwrap_or_else(|_| file.to_owned());
    let mut parser = Parser {
        view: ProjectView::default(),
        visited: HashSet::new(),
        stack: Vec::new(),
        read_import,
    };
    parser.visited.insert(root.clone());
    parser.stack.push(root.clone());
    parser.parse_file(text, &root);
    parser.view
}

/// `PathUtil.getCanonicalPath` followed by `WorkspacePath.validate`: drops `.` and empty components (so a
/// trailing `/` and `./` go away, and `.` is the root `""`) and resolves `..`.
pub fn normalize_workspace_path(path: &str) -> Result<String, String> {
    if path.starts_with('/') {
        return Err(format!(
            "Workspace path must be relative; cannot start with '/': {path}"
        ));
    }
    if path.contains(':') {
        return Err(format!("Workspace path may not contain ':': {path}"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!(
                        "Workspace path must be inside the workspace; cannot start with '../': {path}"
                    ));
                }
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

/// `Label.validate`: `None` for a valid `//pkg:name` or `@repo//pkg:name`, else the error.
pub fn label_error(label: &str) -> Option<String> {
    let Some(colon) = label.find(':') else {
        return Some(format!("Not a valid label, no target name found: {label}"));
    };
    if let Some(rest) = label.strip_prefix("//") {
        return package_name_error(&rest[..colon - 2])
            .or_else(|| target_name_error(&label[colon + 1..]));
    }
    if label.starts_with('@')
        && let Some(slashes) = label.find("//")
    {
        return label_error(&label[slashes..]);
    }
    Some(format!("Not a valid label, no target name found: {label}"))
}

/// `TargetExpression.validate`: `None` for a valid (possibly `-`excluded, possibly wildcard) target pattern.
pub fn target_expression_error(pattern: &str) -> Option<String> {
    let mut rest = pattern.strip_prefix('-').unwrap_or(pattern);
    if rest.is_empty() {
        return Some("Target should be non-empty.".to_owned());
    }
    if rest.starts_with('@') {
        let Some(slashes) = rest.find("//") else {
            return Some(format!(
                "Invalid target expression '{rest}': Couldn't find package path"
            ));
        };
        let repo = &rest[..slashes];
        let name = repo
            .strip_prefix("@@")
            .or_else(|| repo.strip_prefix('@'))
            .unwrap_or(repo);
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Some(format!(
                "Invalid target expression '{rest}': workspace names may contain only A-Z, a-z, 0-9, '-', '_' and '.'"
            ));
        }
        rest = &rest[slashes..];
    }
    let rest = strip_wildcard_suffix(rest);
    let colon = rest.find(':');
    let prefix = if rest.starts_with("//") { 2 } else { 0 };
    let package = &rest[prefix..colon.unwrap_or(rest.len())];
    package_name_error(package).or_else(|| colon.and_then(|i| target_name_error(&rest[i + 1..])))
}

/// `WildcardTargetPattern.stripWildcardSuffix`.
fn strip_wildcard_suffix(pattern: &str) -> &str {
    let pattern = [":all", ":*", ":all-targets"]
        .iter()
        .find_map(|s| pattern.strip_suffix(s))
        .unwrap_or(pattern);
    if pattern == "//..." {
        ""
    } else {
        pattern.strip_suffix("/...").unwrap_or(pattern)
    }
}

/// `PackagePathValidator.validatePackageName`.
fn package_name_error(package: &str) -> Option<String> {
    let error = |reason: &str| Some(format!("Invalid package name '{package}': {reason}"));
    if package.is_empty() {
        return None;
    }
    if package.starts_with('/') {
        return error("package names may not start with '/'");
    }
    if !package
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/-._ $()".contains(c))
    {
        return error(
            "package names may contain only A-Z, a-z, 0-9, '/', '-', '.', ' ', '$', '(', ')' and '_'",
        );
    }
    if package.ends_with('/') {
        return error("package names may not end with '/'");
    }
    for segment in package.split('/') {
        if segment.is_empty() {
            return error("package names may not contain '//' path separators");
        }
        if segment.bytes().all(|b| b == b'.') {
            return error("package name component contains only '.' characters");
        }
    }
    None
}

/// `TargetName.validate`.
fn target_name_error(name: &str) -> Option<String> {
    let error = |reason: &str| Some(format!("Invalid target name: {name}\n{reason}"));
    if name.is_empty() {
        return Some("target names cannot be empty".to_owned());
    }
    if name.starts_with('/') {
        return error("target names may not start with \"/\"");
    }
    if name == "." {
        return None;
    }
    if name == ".." || name.starts_with("../") {
        return error("target names may not contain up-level references \"..\"");
    }
    if name.starts_with("./") {
        return error("target names may not contain \".\" as a path segment");
    }
    let mut slashes_checked = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || "!%-@^_\"#$&'()*+,;<=>?[]{|}~. ".contains(c) {
            continue;
        }
        if c != '/' {
            return error(&format!("target names may not contain {c}"));
        }
        if !slashes_checked {
            slashes_checked = true;
            if name.contains("/../") {
                return error("target names may not contain up-level references \"..\"");
            }
            if name.contains("/./") {
                return error("target names may not contain \".\" as a path segment");
            }
            if name.contains("//") {
                return error("target names may not contain \"//\" path separators");
            }
        }
    }
    if name.ends_with("/..") {
        return error("target names may not contain up-level references \"..\"");
    }
    if name.ends_with('/') {
        return error("target names may not end with \"/\"");
    }
    None
}

/// `WorkspacePath.validate` (no normalization), as `ImportSection` checks the imported path.
fn workspace_path_error(path: &str) -> Option<String> {
    if path.starts_with('/') {
        Some(format!(
            "Workspace path must be relative; cannot start with '/': {path}"
        ))
    } else if path.starts_with("../") {
        Some(format!(
            "Workspace path must be inside the workspace; cannot start with '../': {path}"
        ))
    } else if path.ends_with('/') {
        Some(format!("Workspace path may not end with '/': {path}"))
    } else if path.contains(':') {
        Some(format!("Workspace path may not contain ':': {path}"))
    } else {
        None
    }
}

/// `JavaLanguageLevelSection.getLanguageLevel`: an IntelliJ `LanguageLevel` name (`JDK_1_8`, `JDK_17`,
/// `JDK_21_PREVIEW`, any case) or a positive feature version, as the feature version.
fn java_language_level(text: &str) -> Option<u32> {
    let canonical = |digits: &str| -> Option<u32> {
        if digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || digits.starts_with('0')
        {
            return None;
        }
        digits.parse().ok()
    };
    let upper = text.to_ascii_uppercase();
    if let Some(version) = upper.strip_prefix("JDK_") {
        if let Some(minor) = version.strip_prefix("1_") {
            return canonical(minor).filter(|n| (3..=9).contains(n));
        }
        let version = version.strip_suffix("_PREVIEW").unwrap_or(version);
        return canonical(version).filter(|&n| n >= 10);
    }
    // Integer.parseInt accepts a sign and leading zeros; levels below JDK_1_3 don't exist.
    text.parse::<u32>().ok().filter(|&n| n >= 3)
}

/// Java's `String.trim`: strips every char up to U+0020.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// Java regex `\s`.
fn is_regex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r')
}

/// `TextBlockSection`'s `^\s*#.*$`.
fn is_comment(raw: &str) -> bool {
    raw.trim_start_matches(is_regex_space).starts_with('#')
}

/// `TextBlockSection`'s `^\s*$`.
fn is_blank(raw: &str) -> bool {
    raw.chars().all(is_regex_space)
}

/// `ParseContext.Line`.
#[derive(Clone, Copy)]
struct Line<'a> {
    raw: &'a str,
    text: &'a str,
    indent: usize,
}

/// `ParseContext`: the file split on `\n`, and the current line.
struct Cursor<'a> {
    lines: Vec<Line<'a>>,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        let lines = text
            .split('\n')
            .map(|raw| Line {
                raw,
                text: java_trim(raw),
                indent: raw.bytes().take_while(|&b| b == b' ').count(),
            })
            .collect();
        Cursor { lines, pos: 0 }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.lines.len()
    }

    fn current(&self) -> Line<'a> {
        self.lines[self.pos]
    }

    /// `TextBlockSection.parseTextBlock`: consumes a run of comment lines, or of blank lines, of equal indent.
    fn skip_text_block(&mut self) -> bool {
        let first = self.current();
        let matches: fn(&str) -> bool = if is_comment(first.raw) {
            is_comment
        } else if is_blank(first.raw) {
            is_blank
        } else {
            return false;
        };
        while !self.at_end() && self.current().indent == first.indent && matches(self.current().raw)
        {
            self.pos += 1;
        }
        true
    }

    /// `ListSectionParser.parse` after the header: the item lines (index, text) and whether anything indented
    /// (an item or a comment) followed the header. Unindented comments are part of the list only when an item
    /// follows them; otherwise the cursor goes back to them so they are read as top-level text.
    fn scan_list(&mut self) -> (Vec<(usize, &'a str)>, bool) {
        let mut items = Vec::new();
        let mut has_content = false;
        let mut correct_indentation_run = true;
        let mut saved = None;
        while !self.at_end() {
            let line = self.current();
            let index = self.pos;
            let indented = line.indent == INDENT;
            if !indented && correct_indentation_run {
                saved = Some(index);
            }
            correct_indentation_run = indented;
            if !self.skip_text_block() {
                if !indented {
                    break;
                }
                items.push((index, line.text));
                self.pos += 1;
            }
            if indented {
                has_content = true;
                saved = None;
            }
        }
        if let Some(position) = saved {
            self.pos = position;
        }
        (items, has_content)
    }

    /// `ProjectViewParser.skipSection`: skips the indented lines that follow.
    fn skip_section(&mut self) {
        while !self.at_end() && self.current().indent != 0 {
            self.pos += 1;
        }
    }
}

/// One file's scalar sections; the last occurrence in the file wins.
#[derive(Default)]
struct Scalars {
    derive_targets_from_directories: Option<bool>,
    allow_manual_targets_sync: Option<bool>,
    workspace_type: Option<String>,
    java_language_level: Option<u32>,
}

struct Parser<'r> {
    /// The merged view: each imported file is merged when its parse completes, the importing file after its imports.
    view: ProjectView,
    visited: HashSet<String>,
    /// The files being parsed, outermost first.
    stack: Vec<String>,
    read_import: &'r mut dyn FnMut(&str) -> Option<String>,
}

impl Parser<'_> {
    fn warn(&mut self, file: &str, index: usize, message: impl Display) {
        self.view
            .warnings
            .push(format!("{file}:{}: {message}", index + 1));
    }

    /// `ProjectViewParser.parseProjectView(ParseContext, ...)`.
    fn parse_file(&mut self, text: &str, file: &str) {
        let mut cx = Cursor::new(text);
        let mut own = ProjectView::default();
        let mut scalars = Scalars::default();
        while !cx.at_end() {
            if cx.skip_text_block() {
                continue;
            }
            let line = cx.current();
            let index = cx.pos;
            if let Some((path, mandatory)) = import_line(line.text) {
                cx.pos += 1;
                self.import(path, file, index, mandatory);
                continue;
            }
            let Some((name, rest)) = section_header(line.text) else {
                if line.indent != 0 {
                    self.warn(
                        file,
                        index,
                        format_args!("Invalid indentation on line: '{}'", line.text),
                    );
                } else {
                    self.warn(
                        file,
                        index,
                        format_args!("Could not parse: '{}'", line.text),
                    );
                    cx.pos += 1;
                }
                cx.skip_section();
                continue;
            };
            cx.pos += 1;
            if let Some(kind) = list_kind(name) {
                if !rest.is_empty() {
                    self.warn(
                        file,
                        index,
                        format_args!("Could not parse: '{}'", line.text),
                    );
                    cx.skip_section();
                    continue;
                }
                let (items, has_content) = cx.scan_list();
                if !has_content {
                    self.warn(file, index, format_args!("Empty section: '{name}'"));
                }
                self.list(kind, &items, file, index, &mut own);
            } else if matches!(name, "import" | "try_import") {
                self.warn(
                    file,
                    index,
                    format_args!("Could not parse: '{}'", line.text),
                );
                cx.skip_section();
            } else if !self.scalar(name, java_trim(rest), file, index, &mut scalars) {
                self.warn(
                    file,
                    index,
                    format_args!("Section '{name}' is not supported; ignored"),
                );
                if rest.is_empty() {
                    cx.scan_list();
                }
            }
        }
        self.merge(own, scalars);
    }

    /// `ImportSection`/`TryImportSection`: parses the imported file once, before the importing file's sections.
    fn import(&mut self, path: &str, file: &str, index: usize, mandatory: bool) {
        if let Some(error) = workspace_path_error(path) {
            self.warn(file, index, error);
            return;
        }
        let normalized = match normalize_workspace_path(path) {
            Ok(normalized) if !normalized.is_empty() => normalized,
            Ok(_) => {
                self.warn(
                    file,
                    index,
                    format_args!("Could not load project view file: '{path}'"),
                );
                return;
            }
            Err(error) => {
                self.warn(file, index, error);
                return;
            }
        };
        if self.stack.contains(&normalized) {
            let cycle = self.stack.join(" -> ");
            self.warn(
                file,
                index,
                format_args!("Import cycle: {cycle} -> {normalized}; import skipped"),
            );
            return;
        }
        if self.visited.contains(&normalized) {
            return;
        }
        let Some(text) = (self.read_import)(&normalized) else {
            let kind = if mandatory { "import" } else { "try_import" };
            self.warn(
                file,
                index,
                format_args!("Could not load project view file: '{normalized}' ({kind})"),
            );
            return;
        };
        self.visited.insert(normalized.clone());
        self.stack.push(normalized.clone());
        self.parse_file(&text, &normalized);
        self.stack.pop();
    }

    /// The `ScalarSectionParser`s this port reads; false when `name` isn't one of them.
    fn scalar(
        &mut self,
        name: &str,
        value: &str,
        file: &str,
        index: usize,
        scalars: &mut Scalars,
    ) -> bool {
        match name {
            "derive_targets_from_directories" | "allow_manual_targets_sync" => {
                let parsed = match value {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
                match parsed {
                    None => self.warn(
                        file,
                        index,
                        format_args!(
                            "'{name}' must be set to 'true' or 'false' (e.g. '{name}: true')"
                        ),
                    ),
                    Some(flag) if name == "derive_targets_from_directories" => {
                        scalars.derive_targets_from_directories = Some(flag)
                    }
                    Some(flag) => scalars.allow_manual_targets_sync = Some(flag),
                }
            }
            "workspace_type" => {
                if WORKSPACE_TYPES.contains(&value) {
                    scalars.workspace_type = Some(value.to_owned());
                } else {
                    self.warn(file, index, format_args!("Invalid workspace type: {value}"));
                }
            }
            "java_language_level" => match java_language_level(value) {
                Some(level) => scalars.java_language_level = Some(level),
                None => self.warn(
                    file,
                    index,
                    format_args!("Illegal java language level: {value}"),
                ),
            },
            _ => return false,
        }
        true
    }

    /// The `ListSectionParser`s' `parseItem`, for the item lines of a section.
    fn list(
        &mut self,
        kind: ListKind,
        items: &[(usize, &str)],
        file: &str,
        header: usize,
        own: &mut ProjectView,
    ) {
        if kind == ListKind::ExcludedLibraries {
            self.warn(
                file,
                header,
                "excluded_libraries has been deprecated. Please use 'exclude_library' instead.",
            );
        }
        for &(index, text) in items {
            match kind {
                ListKind::Directories => {
                    let (included, path) = match text.strip_prefix('-') {
                        Some(path) => (false, path),
                        None => (true, text),
                    };
                    match normalize_workspace_path(path) {
                        Ok(path) => own.directories.push(DirectoryEntry { path, included }),
                        Err(error) => self.warn(file, index, error),
                    }
                }
                ListKind::Targets => match target_expression_error(text) {
                    None => own.targets.push(match text.strip_prefix('-') {
                        Some(pattern) => TargetExpression {
                            pattern: pattern.to_owned(),
                            excluded: true,
                        },
                        None => TargetExpression {
                            pattern: text.to_owned(),
                            excluded: false,
                        },
                    }),
                    Some(error) => self.warn(file, index, error),
                },
                ListKind::AdditionalLanguages => {
                    if LANGUAGES.contains(&text) {
                        own.additional_languages.push(text.to_owned());
                    } else {
                        self.warn(file, index, format_args!("Invalid language: {text}"));
                    }
                }
                ListKind::TestSources => {
                    self.glob_warnings("test_sources", text, file, index);
                    own.test_sources.push(text.to_owned());
                }
                ListKind::ExcludeLibrary | ListKind::ExcludedLibraries => {
                    let name = if kind == ListKind::ExcludeLibrary {
                        "exclude_library"
                    } else {
                        "excluded_libraries"
                    };
                    self.glob_warnings(name, text, file, index);
                    own.excluded_libraries.push(text.to_owned());
                }
                ListKind::BuildFlags => own.build_flags.push(text.to_owned()),
                ListKind::SyncFlags => own.sync_flags.push(text.to_owned()),
                ListKind::ExcludeTarget | ListKind::ImportTargetOutput => match label_error(text) {
                    None if kind == ListKind::ExcludeTarget => {
                        own.exclude_target.push(text.to_owned())
                    }
                    None => own.import_target_output.push(text.to_owned()),
                    Some(error) => self.warn(file, index, error),
                },
            }
        }
    }

    /// `GlobSectionParser.parseItem`'s diagnostics; the glob is kept either way, as there.
    fn glob_warnings(&mut self, name: &str, text: &str, file: &str, index: usize) {
        if text.starts_with("//") {
            self.warn(
                file,
                index,
                format_args!("{name} is a list of file path globs, not target patterns."),
            );
        }
        if text.contains("...") {
            self.warn(
                file,
                index,
                format_args!("The \"...\" wildcard is not supported in {name}. Use '*' instead."),
            );
        }
        if text.contains("**") {
            self.warn(
                file,
                index,
                format_args!("The \"**\" wildcard is not supported in {name}. Use '*' instead."),
            );
        }
    }

    /// `ProjectViewSet`: lists concatenate in file order, the last scalar wins.
    fn merge(&mut self, own: ProjectView, scalars: Scalars) {
        let view = &mut self.view;
        view.directories.extend(own.directories);
        view.targets.extend(own.targets);
        view.additional_languages.extend(own.additional_languages);
        view.test_sources.extend(own.test_sources);
        view.build_flags.extend(own.build_flags);
        view.sync_flags.extend(own.sync_flags);
        view.exclude_target.extend(own.exclude_target);
        view.import_target_output.extend(own.import_target_output);
        view.excluded_libraries.extend(own.excluded_libraries);
        if let Some(flag) = scalars.derive_targets_from_directories {
            view.derive_targets_from_directories = flag;
        }
        if let Some(flag) = scalars.allow_manual_targets_sync {
            view.allow_manual_targets_sync = flag;
        }
        if scalars.workspace_type.is_some() {
            view.workspace_type = scalars.workspace_type;
        }
        if scalars.java_language_level.is_some() {
            view.java_language_level = scalars.java_language_level;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Directories,
    Targets,
    AdditionalLanguages,
    TestSources,
    BuildFlags,
    SyncFlags,
    ExcludeTarget,
    ImportTargetOutput,
    ExcludeLibrary,
    ExcludedLibraries,
}

/// The list sections this port reads (`Sections.PARSERS` and `BlazeJavaSyncPlugin.getSections`).
fn list_kind(name: &str) -> Option<ListKind> {
    Some(match name {
        "directories" => ListKind::Directories,
        "targets" => ListKind::Targets,
        "additional_languages" => ListKind::AdditionalLanguages,
        "test_sources" => ListKind::TestSources,
        "build_flags" => ListKind::BuildFlags,
        "sync_flags" => ListKind::SyncFlags,
        "exclude_target" => ListKind::ExcludeTarget,
        "import_target_output" => ListKind::ImportTargetOutput,
        "exclude_library" => ListKind::ExcludeLibrary,
        "excluded_libraries" => ListKind::ExcludedLibraries,
        _ => return None,
    })
}

/// `import <path>` / `try_import <path>` (`ScalarSectionParser` with a space divider): (path, mandatory).
fn import_line(text: &str) -> Option<(&str, bool)> {
    [("import", true), ("try_import", false)]
        .into_iter()
        .find_map(|(name, mandatory)| {
            Some((
                java_trim(text.strip_prefix(name)?.strip_prefix(' ')?),
                mandatory,
            ))
        })
}

/// `name:rest` where `name` is a section-like identifier.
fn section_header(text: &str) -> Option<(&str, &str)> {
    let (name, rest) = text.split_once(':')?;
    let identifier =
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    identifier.then_some((name, rest))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const ALLOCADIA: &str = include_str!("../tests/data/allocadia.bazelproject");

    fn parse_text(text: &str) -> ProjectView {
        parse(text, ".bazelproject", &mut |path| {
            panic!("unexpected import of {path}")
        })
    }

    fn parse_files(files: &[(&str, &str)]) -> (ProjectView, Vec<String>) {
        let files: HashMap<&str, &str> = files.iter().copied().collect();
        let mut reads = Vec::new();
        let view = parse(files[".bazelproject"], ".bazelproject", &mut |path| {
            reads.push(path.to_owned());
            files.get(path).map(|text| (*text).to_owned())
        });
        (view, reads)
    }

    fn dirs(view: &ProjectView) -> Vec<String> {
        view.directories
            .iter()
            .map(|d| format!("{}{}", if d.included { "" } else { "-" }, d.path))
            .collect()
    }

    #[test]
    fn allocadia_project_view() {
        let view = parse_text(ALLOCADIA);
        let includes: Vec<&str> = view
            .directories
            .iter()
            .filter(|d| d.included)
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(includes, [""]);
        let excludes: Vec<&str> = view
            .directories
            .iter()
            .filter(|d| !d.included)
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(excludes.len(), 42);
        assert_eq!(excludes[0], ".claude/worktrees");
        assert_eq!(excludes[41], "projects/uptempo/web-client/node_modules");
        assert!(view.derive_targets_from_directories);
        assert!(!view.allow_manual_targets_sync);
        assert_eq!(view.java_language_level, Some(17));
        assert_eq!(view.workspace_type.as_deref(), Some("java"));
        assert_eq!(view.additional_languages, ["typescript", "javascript"]);
        assert_eq!(view.test_sources, ["*/src/test/java*"]);
        assert!(view.targets.is_empty());
        // `ts_config_rules` (a JavaScript-plugin section) is the only thing skipped; `targets:` holding only a
        // comment isn't empty for ListSectionParser.
        assert_eq!(
            view.warnings,
            [".bazelproject:64: Section 'ts_config_rules' is not supported; ignored"]
        );
    }

    #[test]
    fn root_directory_and_normalized_paths() {
        let view =
            parse_text("directories:\n  .\n  ./foo/\n  foo//bar/./baz\n  a/../b\n  -./x/\n  -.\n");
        assert_eq!(dirs(&view), ["", "foo", "foo/bar/baz", "b", "-x", "-"]);
        assert!(view.warnings.is_empty(), "{:?}", view.warnings);
    }

    #[test]
    fn invalid_directories_are_skipped_with_warnings() {
        let view = parse_text(
            "directories:\n  good\n  /abs\n  ../out\n  a/../../out\n  c:d\n  also_good\n",
        );
        assert_eq!(dirs(&view), ["good", "also_good"]);
        assert_eq!(view.warnings.len(), 4, "{:?}", view.warnings);
        assert!(view.warnings[0].starts_with(".bazelproject:3: Workspace path must be relative"));
        assert!(view.warnings[1].contains("inside the workspace"));
        assert!(view.warnings[3].contains("may not contain ':'"));
    }

    /// `ProjectViewParserTest.testListParserAcceptsWhitespace`.
    #[test]
    fn comments_and_blank_lines_inside_lists() {
        let text = "directories:\n  dir0\n  \n\n  dir1\n  \n  \n# comment\n  dir2\n\n  # commented out dir\n  \n# comment\n# comment";
        let view = parse_text(text);
        assert_eq!(dirs(&view), ["dir0", "dir1", "dir2"]);
        assert!(view.warnings.is_empty(), "{:?}", view.warnings);
    }

    #[test]
    fn unindented_comment_after_a_list_ends_it() {
        let view = parse_text("targets:\n  //a:b\n# trailing\n\nworkspace_type: java\n");
        assert_eq!(
            view.targets,
            [TargetExpression {
                pattern: "//a:b".into(),
                excluded: false
            }]
        );
        assert_eq!(view.workspace_type.as_deref(), Some("java"));
        assert!(view.warnings.is_empty(), "{:?}", view.warnings);
    }

    #[test]
    fn empty_section_warns() {
        let view = parse_text("directories:\n\ntargets:\n  # only a comment\n");
        assert_eq!(
            view.warnings,
            [".bazelproject:1: Empty section: 'directories'"]
        );
    }

    /// `testMinimumIndentRequired` and `testIncorrectIndentationResultsInIssue`.
    #[test]
    fn list_items_need_exactly_two_spaces() {
        let view = parse_text("directories:\n  java/com/google\njava/com/google2\n");
        assert_eq!(dirs(&view), ["java/com/google"]);
        assert_eq!(
            view.warnings,
            [".bazelproject:3: Could not parse: 'java/com/google2'"]
        );

        let view = parse_text("directories:\n  a\n    b\n  c\n\ntargets:\n  //x:y\n");
        assert_eq!(dirs(&view), ["a"]);
        assert_eq!(
            view.warnings,
            [".bazelproject:3: Invalid indentation on line: 'b'"]
        );
        assert_eq!(view.targets.len(), 1);
    }

    #[test]
    fn malformed_lines_and_unknown_sections_are_skipped() {
        let text = "directories\n  ignored\nbogus line\nshard_sync: true\ntest_flags:\n  --foo\n\n  # c\n  --bar\nimport: x\nderive_targets_from_directories: maybe\nworkspace_type: cobol\njava_language_level: 1.8\ndirectories: inline\n  skipped\nadditional_languages:\n  java\n  klingon\nallow_manual_targets_sync: true\n";
        let view = parse_text(text);
        assert!(view.directories.is_empty());
        assert!(!view.derive_targets_from_directories);
        assert!(view.allow_manual_targets_sync);
        assert_eq!(view.workspace_type, None);
        assert_eq!(view.java_language_level, None);
        assert_eq!(view.additional_languages, ["java"]);
        assert_eq!(
            view.warnings,
            [
                ".bazelproject:1: Could not parse: 'directories'",
                ".bazelproject:3: Could not parse: 'bogus line'",
                ".bazelproject:4: Section 'shard_sync' is not supported; ignored",
                ".bazelproject:5: Section 'test_flags' is not supported; ignored",
                ".bazelproject:10: Could not parse: 'import: x'",
                ".bazelproject:11: 'derive_targets_from_directories' must be set to 'true' or 'false' (e.g. 'derive_targets_from_directories: true')",
                ".bazelproject:12: Invalid workspace type: cobol",
                ".bazelproject:13: Illegal java language level: 1.8",
                ".bazelproject:14: Could not parse: 'directories: inline'",
                ".bazelproject:18: Invalid language: klingon",
            ]
        );
    }

    #[test]
    fn java_language_levels() {
        for (text, level) in [
            ("17", Some(17)),
            ("8", Some(8)),
            ("017", Some(17)),
            ("JDK_17", Some(17)),
            ("jdk_1_8", Some(8)),
            ("JDK_1_3", Some(3)),
            ("JDK_21_PREVIEW", Some(21)),
            ("JDK_8", None),
            ("JDK_1_10", None),
            ("JDK_1_08", None),
            ("JDK_X", None),
            ("1.8", None),
            ("0", None),
            ("-5", None),
            ("2", None),
            ("", None),
        ] {
            assert_eq!(java_language_level(text), level, "{text:?}");
        }
        let view = parse_text("java_language_level: JDK_11\njava_language_level:21\n");
        assert_eq!(view.java_language_level, Some(21));
    }

    #[test]
    fn targets_keep_exclusions_and_reject_invalid_patterns() {
        let view = parse_text(
            "targets:\n  //a/...\n  -//a/b:c\n  @@repo//x:y\n  //bad:\n  -\n  //a:all-targets\n  //p/...:*\n  //x:a:b\n",
        );
        let targets: Vec<(&str, bool)> = view
            .targets
            .iter()
            .map(|t| (t.pattern.as_str(), t.excluded))
            .collect();
        assert_eq!(
            targets,
            [
                ("//a/...", false),
                ("//a/b:c", true),
                ("@@repo//x:y", false),
                ("//a:all-targets", false),
                ("//p/...:*", false)
            ]
        );
        assert_eq!(view.warnings.len(), 3, "{:?}", view.warnings);
        assert!(view.warnings[0].contains("target names cannot be empty"));
        assert!(view.warnings[1].contains("Target should be non-empty."));
        assert!(view.warnings[2].contains("target names may not contain :"));
    }

    #[test]
    fn label_and_target_validation() {
        assert_eq!(label_error("//a/b:c"), None);
        assert_eq!(label_error("//:root"), None);
        assert_eq!(
            label_error("@@rules_jvm_external~~maven~maven//:com_google_guava_guava"),
            None
        );
        assert_eq!(label_error("//a/b:c/d.java"), None);
        assert!(label_error("//a/b").is_some());
        assert!(label_error("a:b").is_some());
        assert!(label_error("//a//b:c").is_some());
        assert!(label_error("//a/./b:c").is_some());
        assert!(label_error("//a b%:c").is_some());
        assert!(label_error("//a:b/../c").is_some());
        assert!(label_error("//a:b/").is_some());
        assert_eq!(target_expression_error("//..."), None);
        assert_eq!(target_expression_error("-//...:all"), None);
        assert_eq!(target_expression_error("java/com/..."), None);
        assert!(target_expression_error("@bad repo//x").is_some());
        assert!(target_expression_error("@repo").is_some());
    }

    #[test]
    fn label_sections_and_globs() {
        let text = "exclude_target:\n  //a:b\n  //bad\nimport_target_output:\n  //c:d\ntest_sources:\n  //javatests\n  a/.../b\n  **/test\nexcluded_libraries:\n  guava*\nexclude_library:\n  junit*\nbuild_flags:\n  --config=ci\nsync_flags:\n  --define=x=1\n";
        let view = parse_text(text);
        assert_eq!(view.exclude_target, ["//a:b"]);
        assert_eq!(view.import_target_output, ["//c:d"]);
        assert_eq!(view.test_sources, ["//javatests", "a/.../b", "**/test"]);
        assert_eq!(view.excluded_libraries, ["guava*", "junit*"]);
        assert_eq!(view.build_flags, ["--config=ci"]);
        assert_eq!(view.sync_flags, ["--define=x=1"]);
        assert_eq!(view.warnings.len(), 5, "{:?}", view.warnings);
        assert!(view.warnings[0].starts_with(".bazelproject:3: Not a valid label"));
        assert!(
            view.warnings[1]
                .ends_with("test_sources is a list of file path globs, not target patterns.")
        );
        assert!(view.warnings[2].contains("\"...\" wildcard"));
        assert!(view.warnings[3].contains("\"**\" wildcard"));
        assert_eq!(
            view.warnings[4],
            ".bazelproject:10: excluded_libraries has been deprecated. Please use 'exclude_library' instead."
        );
    }

    #[test]
    fn scalars_last_occurrence_wins_and_crlf_is_accepted() {
        let view = parse_text(
            "workspace_type: java\r\nworkspace_type: android\r\ndirectories:\r\n  a\r\n  # c\r\n  b/\r\n\r\nderive_targets_from_directories:true\r\n",
        );
        assert_eq!(view.workspace_type.as_deref(), Some("android"));
        assert_eq!(dirs(&view), ["a", "b"]);
        assert!(view.derive_targets_from_directories);
        assert!(view.warnings.is_empty(), "{:?}", view.warnings);
    }

    /// `ProjectViewParserTest.testMultipleImports`: imports come first wherever the `import` line is, and a
    /// file imported twice is read once.
    #[test]
    fn imports_merge_before_the_importing_file() {
        let (view, reads) = parse_files(&[
            (
                ".bazelproject",
                "workspace_type: java\nimport tools/mother.bazelproject\ndirectories:\n  child1\nimport ./tools/father.bazelproject\ndirectories:\n  child2\n",
            ),
            (
                "tools/grandparent.bazelproject",
                "directories:\n  grandparent\njava_language_level: 11\n",
            ),
            (
                "tools/mother.bazelproject",
                "import tools/grandparent.bazelproject\ndirectories:\n  mother\nworkspace_type: python\n",
            ),
            (
                "tools/father.bazelproject",
                "import tools/grandparent.bazelproject\ndirectories:\n  father\n",
            ),
        ]);
        assert_eq!(
            dirs(&view),
            ["grandparent", "mother", "father", "child1", "child2"]
        );
        assert_eq!(view.workspace_type.as_deref(), Some("java"));
        assert_eq!(view.java_language_level, Some(11));
        assert_eq!(
            reads,
            [
                "tools/mother.bazelproject",
                "tools/grandparent.bazelproject",
                "tools/father.bazelproject"
            ]
        );
        assert!(view.warnings.is_empty(), "{:?}", view.warnings);
    }

    #[test]
    fn missing_imports_warn() {
        let (view, _) = parse_files(&[(
            ".bazelproject",
            "try_import user.bazelproject\nimport shared.bazelproject\nimport /abs.bazelproject\nimport dir/\nimport ../up.bazelproject\ndirectories:\n  a\n",
        )]);
        assert_eq!(dirs(&view), ["a"]);
        assert_eq!(
            view.warnings,
            [
                ".bazelproject:1: Could not load project view file: 'user.bazelproject' (try_import)",
                ".bazelproject:2: Could not load project view file: 'shared.bazelproject' (import)",
                ".bazelproject:3: Workspace path must be relative; cannot start with '/': /abs.bazelproject",
                ".bazelproject:4: Workspace path may not end with '/': dir/",
                ".bazelproject:5: Workspace path must be inside the workspace; cannot start with '../': ../up.bazelproject",
            ]
        );
    }

    #[test]
    fn import_cycles_warn_and_stop() {
        let (view, reads) = parse_files(&[
            (
                ".bazelproject",
                "import a.bazelproject\ndirectories:\n  root\n",
            ),
            (
                "a.bazelproject",
                "import b.bazelproject\ndirectories:\n  a\n",
            ),
            (
                "b.bazelproject",
                "import a.bazelproject\nimport .bazelproject\ndirectories:\n  b\n",
            ),
        ]);
        assert_eq!(dirs(&view), ["b", "a", "root"]);
        assert_eq!(reads, ["a.bazelproject", "b.bazelproject"]);
        assert_eq!(
            view.warnings,
            [
                "b.bazelproject:1: Import cycle: .bazelproject -> a.bazelproject -> b.bazelproject -> a.bazelproject; import skipped",
                "b.bazelproject:2: Import cycle: .bazelproject -> a.bazelproject -> b.bazelproject -> .bazelproject; import skipped",
            ]
        );
    }

    #[test]
    fn empty_text_is_an_empty_view() {
        assert_eq!(parse_text(""), ProjectView::default());
    }

    #[test]
    fn normalize_workspace_paths() {
        assert_eq!(normalize_workspace_path(".").unwrap(), "");
        assert_eq!(normalize_workspace_path("").unwrap(), "");
        assert_eq!(normalize_workspace_path("./a//b/./c/").unwrap(), "a/b/c");
        assert_eq!(normalize_workspace_path("a/b/../c").unwrap(), "a/c");
        assert!(normalize_workspace_path("a/../..").is_err());
        assert!(normalize_workspace_path("/a").is_err());
        assert!(normalize_workspace_path("a:b").is_err());
    }
}
