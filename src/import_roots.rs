//! `ImportRoots`: which workspace directories the project covers (port of `ImportRoots.java`).
//!
//! Sources: `base/.../sync/projectview/{ImportRoots,BazelIgnoreParser}.java`, `base/.../util/WorkspacePathUtil.java`
//! and `BazelBuildSystemProvider.buildArtifactDirectories`.

use std::borrow::Cow;
use std::collections::HashSet;

use crate::projectview::normalize_workspace_path;
use crate::types::{ImportRoots, ProjectView};

/// The project's roots from `directories:`, `.bazelignore` (its text, if the file exists), Bazel's
/// convenience symlinks (`bazel-bin`, `bazel-out`, `bazel-testlogs`, `bazel-genfiles`,
/// `bazel-<workspace_dir_name>`) and the plugin's own data directory `project_data_dir`.
///
/// As in IntelliJ, the symlinks and the data directory are excluded only when the workspace root itself is
/// included, `.bazelignore` entries always. Excludes are minimized (duplicates and paths under another
/// exclude dropped, insertion order kept); includes drop duplicates, paths under another include and paths
/// under or equal to an exclude. Excludes outside every include are kept, as in IntelliJ. `query_excludes`
/// leaves out what Bazel already skips: `.bazelignore`d paths and the convenience symlinks.
/// Invalid `.bazelignore` lines are skipped; [`parse_bazelignore`] reports them.
pub fn import_roots(
    view: &ProjectView,
    bazelignore: Option<&str>,
    workspace_dir_name: &str,
    project_data_dir: &str,
) -> ImportRoots {
    let roots: Vec<Cow<str>> = view
        .directories
        .iter()
        .filter(|d| d.included)
        .map(|d| clean_path(&d.path))
        .collect();
    let mut excludes: Vec<String> = view
        .directories
        .iter()
        .filter(|d| !d.included)
        .map(|d| clean_path(&d.path).into_owned())
        .collect();

    let symlinks = build_artifact_directories(workspace_dir_name);
    if roots.iter().any(|root| root.is_empty()) {
        excludes.extend(symlinks.iter().cloned());
        // An empty data directory would exclude the whole workspace.
        if let Ok(data_dir) = normalize_workspace_path(project_data_dir)
            && !data_dir.is_empty()
        {
            excludes.push(data_dir);
        }
    }
    let ignored = bazelignore
        .map(|text| parse_bazelignore(text).paths)
        .unwrap_or_default();
    excludes.extend(ignored.iter().cloned());

    let excludes = minimal_paths(excludes.iter().map(String::as_str), &HashSet::new());
    let exclude_set: HashSet<&str> = excludes.iter().copied().collect();
    let includes = minimal_paths(roots.iter().map(|root| &**root), &exclude_set);

    let skipped_by_bazel: HashSet<&str> = ignored
        .iter()
        .chain(&symlinks)
        .map(String::as_str)
        .collect();
    let query_excludes = excludes
        .iter()
        .filter(|e| !skipped_by_bazel.contains(*e))
        .map(|e| (*e).to_owned())
        .collect();
    ImportRoots {
        includes: includes.into_iter().map(str::to_owned).collect(),
        excludes: excludes.into_iter().map(str::to_owned).collect(),
        query_excludes,
    }
}

/// Whether a workspace-relative path is under an include and under no exclude (path components).
pub fn contains(roots: &ImportRoots, path: &str) -> bool {
    let path = clean_path(path);
    roots
        .includes
        .iter()
        .any(|include| is_ancestor(include, &path, false))
        && !roots
            .excludes
            .iter()
            .any(|exclude| is_ancestor(exclude, &path, false))
}

/// `FileUtil.isAncestor` on workspace-relative paths, component-wise: `""` is the ancestor of every path,
/// `foo` of `foo/bar` but not of `foobar`. With `strict`, a path isn't its own ancestor.
pub fn is_ancestor(ancestor: &str, path: &str, strict: bool) -> bool {
    if ancestor.is_empty() {
        return !(strict && path.is_empty());
    }
    match path.strip_prefix(ancestor) {
        Some("") => !strict,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// `BazelBuildSystemProvider.buildArtifactDirectories`.
pub fn build_artifact_directories(workspace_dir_name: &str) -> Vec<String> {
    let mut dirs: Vec<String> = ["bazel-bin", "bazel-genfiles", "bazel-out", "bazel-testlogs"]
        .map(String::from)
        .into();
    if !workspace_dir_name.is_empty() && !workspace_dir_name.contains('/') {
        dirs.push(format!("bazel-{workspace_dir_name}"));
    }
    dirs
}

/// A parsed `.bazelignore`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BazelIgnore {
    /// Workspace-relative directories, normalized.
    pub paths: Vec<String>,
    /// Lines that aren't workspace-relative paths (`line N: ...`), skipped.
    pub warnings: Vec<String>,
}

/// `BazelIgnoreParser.getIgnoredPaths`: one path per line, `#` comments and blank lines skipped, a trailing
/// `/` allowed. Lines are also trimmed and normalized (`./a`, `a//b`), as Bazel reads them.
pub fn parse_bazelignore(text: &str) -> BazelIgnore {
    let mut ignore = BazelIgnore::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match normalize_workspace_path(line) {
            Ok(path) if !path.is_empty() => ignore.paths.push(path),
            Ok(_) => ignore.warnings.push(format!(
                "line {}: '{line}' would ignore the whole workspace; skipped",
                index + 1
            )),
            Err(error) => ignore
                .warnings
                .push(format!("line {}: {error}; skipped", index + 1)),
        }
    }
    ignore
}

/// `WorkspacePathUtil.calculateMinimalWorkspacePaths`: the distinct `paths`, in order, that aren't under or
/// equal to an `excluded` path nor strictly under another of `paths`.
fn minimal_paths<'a>(
    paths: impl Iterator<Item = &'a str> + Clone,
    excluded: &HashSet<&str>,
) -> Vec<&'a str> {
    let all: HashSet<&str> = paths.clone().collect();
    let mut seen = HashSet::new();
    paths
        .filter(|path| {
            !excluded.contains(path)
                && !strict_ancestors(path).any(|a| excluded.contains(a) || all.contains(a))
                && seen.insert(*path)
        })
        .collect()
}

/// `""` (for a non-root path), then each proper prefix ending before a `/`.
fn strict_ancestors(path: &str) -> impl Iterator<Item = &str> {
    let root = (!path.is_empty()).then_some("");
    root.into_iter()
        .chain(path.match_indices('/').map(move |(i, _)| &path[..i]))
}

/// The path without empty and `.` components, borrowed when already clean.
fn clean_path(path: &str) -> Cow<'_, str> {
    if path.split('/').all(|part| !part.is_empty() && part != ".") {
        return Cow::Borrowed(path);
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    Cow::Owned(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DirectoryEntry;

    const ALLOCADIA_VIEW: &str = include_str!("../tests/data/allocadia.bazelproject");
    const ALLOCADIA_IGNORE: &str = include_str!("../tests/data/allocadia.bazelignore");

    fn view(directories: &[&str]) -> ProjectView {
        let directories = directories
            .iter()
            .map(|d| match d.strip_prefix('-') {
                Some(path) => DirectoryEntry {
                    path: path.to_owned(),
                    included: false,
                },
                None => DirectoryEntry {
                    path: (*d).to_owned(),
                    included: true,
                },
            })
            .collect();
        ProjectView {
            directories,
            ..ProjectView::default()
        }
    }

    #[test]
    fn allocadia_roots() {
        let view = crate::projectview::parse(ALLOCADIA_VIEW, ".bazelproject", &mut |_| None);
        let roots = import_roots(
            &view,
            Some(ALLOCADIA_IGNORE),
            "Allocadia",
            ".openidea/bazel",
        );
        assert_eq!(roots.includes, [""]);
        // IntelliJ's .workspace.iml has 62 excludeFolders: the 61 .bazelignore entries (which cover the 42
        // project view excludes and the bazel-* symlinks) and its own .ijwb, here .openidea/bazel.
        assert_eq!(roots.excludes.len(), 62);
        for expected in [
            ".git",
            ".bazelignore",
            "bazel-Allocadia",
            "bazel-genfiles",
            ".openidea/bazel",
            "tools/a11y/node_modules",
        ] {
            assert!(
                roots.excludes.iter().any(|e| e == expected),
                "{expected} missing"
            );
        }
        assert!(!roots.excludes.iter().any(|e| e == ".ijwb"));
        assert_eq!(&roots.excludes[..2], [".claude/worktrees", "node_modules"]);
        assert_eq!(roots.query_excludes, [".openidea/bazel"]);
    }

    #[test]
    fn symlinks_and_data_dir_only_with_the_workspace_root() {
        let roots = import_roots(&view(&["java", "-java/gen"]), None, "ws", ".openidea/bazel");
        assert_eq!(roots.includes, ["java"]);
        assert_eq!(roots.excludes, ["java/gen"]);
        assert_eq!(roots.query_excludes, ["java/gen"]);

        let roots = import_roots(&view(&["."]), None, "ws", ".openidea/bazel/");
        assert_eq!(roots.includes, [""]);
        assert_eq!(
            roots.excludes,
            [
                "bazel-bin",
                "bazel-genfiles",
                "bazel-out",
                "bazel-testlogs",
                "bazel-ws",
                ".openidea/bazel"
            ]
        );
        // Bazel never descends into its convenience symlinks, so they stay out of the query.
        assert_eq!(roots.query_excludes, [".openidea/bazel"]);
    }

    #[test]
    fn empty_data_dir_does_not_exclude_the_workspace() {
        let roots = import_roots(&view(&[""]), None, "", "");
        assert_eq!(roots.includes, [""]);
        assert_eq!(
            roots.excludes,
            ["bazel-bin", "bazel-genfiles", "bazel-out", "bazel-testlogs"]
        );
    }

    #[test]
    fn minimization() {
        let roots = import_roots(
            &view(&[
                "a",
                "a/b",
                "c",
                "a",
                "x/y",
                "-x",
                "-c/d",
                "-c/d/e",
                "-c/d",
                "-elsewhere",
                "-cc",
            ]),
            Some("# comment\n\n  c/ignored/  \r\nc/ignored/deeper\n"),
            "ws",
            ".openidea/bazel",
        );
        assert_eq!(roots.includes, ["a", "c"]);
        // An exclude outside every include is kept, as in IntelliJ.
        assert_eq!(roots.excludes, ["x", "c/d", "elsewhere", "cc", "c/ignored"]);
        assert_eq!(roots.query_excludes, ["x", "c/d", "elsewhere", "cc"]);
    }

    #[test]
    fn excluded_root_drops_everything() {
        let roots = import_roots(&view(&["a", "-"]), None, "ws", ".openidea/bazel");
        assert!(roots.includes.is_empty());
        assert_eq!(roots.excludes, [""]);
    }

    #[test]
    fn bazelignore_parsing() {
        let parsed =
            parse_bazelignore("# c\n.git/\n  node_modules  \r\n./a//b/\n/abs\n../up\nc:d\n.\n\n");
        assert_eq!(parsed.paths, [".git", "node_modules", "a/b"]);
        assert_eq!(parsed.warnings.len(), 4, "{:?}", parsed.warnings);
        assert!(parsed.warnings[0].starts_with("line 5: Workspace path must be relative"));
        assert!(parsed.warnings[3].starts_with("line 8: '.' would ignore the whole workspace"));
        assert_eq!(parse_bazelignore(ALLOCADIA_IGNORE).paths.len(), 61);
        assert!(parse_bazelignore(ALLOCADIA_IGNORE).warnings.is_empty());
    }

    #[test]
    fn contains_is_component_wise() {
        let roots = ImportRoots {
            includes: vec!["foo".into(), "lib/x".into()],
            excludes: vec!["foo/gen".into()],
            query_excludes: vec![],
        };
        assert!(contains(&roots, "foo"));
        assert!(contains(&roots, "foo/a/B.java"));
        assert!(!contains(&roots, "foobar"));
        assert!(!contains(&roots, "foobar/a"));
        assert!(!contains(&roots, "foo/gen"));
        assert!(!contains(&roots, "foo/gen/x"));
        assert!(contains(&roots, "foo/generated"));
        assert!(contains(&roots, "./foo//a/"));
        assert!(!contains(&roots, "lib"));
        assert!(!contains(&roots, ""));

        let root = ImportRoots {
            includes: vec!["".into()],
            excludes: vec!["bazel-bin".into()],
            query_excludes: vec![],
        };
        assert!(contains(&root, ""));
        assert!(contains(&root, "anything/at/all"));
        assert!(!contains(&root, "bazel-bin/x"));
        assert!(!contains(&ImportRoots::default(), "a"));
    }

    #[test]
    fn ancestry() {
        assert!(is_ancestor("", "", false));
        assert!(!is_ancestor("", "", true));
        assert!(is_ancestor("", "a", true));
        assert!(is_ancestor("a", "a", false));
        assert!(!is_ancestor("a", "a", true));
        assert!(is_ancestor("a", "a/b", true));
        assert!(!is_ancestor("a", "ab", false));
        assert!(!is_ancestor("a/b", "a", false));
    }
}
