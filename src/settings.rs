//! The project's `.openidea/settings.toml` (jdtls reads Eclipse metadata, not Maven) and `.git/info/exclude`.

use std::collections::BTreeSet;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

const MANAGED_COMMENT: &str =
    "# Managed by openidea-bazel: jdtls reads the Bazel-derived Eclipse project instead of Maven.";
/// Recognizes the managed comment even when the rest of the line was edited.
const MANAGED_MARKER: &str = "# Managed by openidea-bazel";

/// `java.import.exclusions`: jdtls's own defaults plus Bazel's output symlinks and the folders of
/// other build tools and IDEs, so jdtls never imports a Maven/Gradle project next to the Bazel one.
/// `**/bazel-*` excludes the symlinks themselves: jdtls follows links while it looks for projects,
/// and Bazel's execroot (`bazel-<workspace>`) links the workspace's own `.project` and `.classpath`,
/// which would import the project a second time, at the execroot, sharing its output folder.
pub const JDTLS_IMPORT_EXCLUSIONS: &[&str] = &[
    "**/node_modules/**",
    "**/.metadata/**",
    "**/archetype-resources/**",
    "**/META-INF/maven/**",
    "**/bazel-*",
    "**/bazel-*/**",
    "**/.claude/**",
    "**/.worktrees/**",
    "**/target/**",
    "**/.ijwb/**",
    "**/.openidea/**",
];

/// `java.project.resourceFilters` (regexes on folder names, applied at every depth by jdtls).
pub const JDTLS_RESOURCE_FILTERS: &[&str] = &["node_modules", "\\.git", "bazel-.*", "\\.ijwb"];

/// Heads the lines `ensure_git_excludes` appends.
pub const GIT_EXCLUDES_COMMENT: &str = "# openidea-bazel";

/// `existing` with the jdtls settings the Bazel project needs; `jdtls_data_dir` is its `-data` workspace.
pub fn merge_openidea_settings(
    existing: Option<&str>,
    jdtls_data_dir: &str,
) -> Result<String, String> {
    let existing = existing.unwrap_or("");
    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| format!("settings.toml is not valid TOML: {e}"))?;

    let root = doc.as_item_mut();
    let servers = table(root, "languageServers", true)?;
    let jdtls = table(servers, "languageServers.jdtls", false)?;
    set(
        jdtls,
        "languageServers.jdtls.args",
        strings(&["-data", jdtls_data_dir]),
    )?;
    let settings = table(jdtls, "languageServers.jdtls.settings", true)?;
    let java = table(settings, "languageServers.jdtls.settings.java", true)?;
    let import = table(java, "languageServers.jdtls.settings.java.import", false)?;
    set(
        import,
        "languageServers.jdtls.settings.java.import.exclusions",
        strings(JDTLS_IMPORT_EXCLUSIONS),
    )?;
    let maven = table(
        import,
        "languageServers.jdtls.settings.java.import.maven",
        false,
    )?;
    set(
        maven,
        "languageServers.jdtls.settings.java.import.maven.enabled",
        Value::from(false),
    )?;
    let gradle = table(
        import,
        "languageServers.jdtls.settings.java.import.gradle",
        false,
    )?;
    set(
        gradle,
        "languageServers.jdtls.settings.java.import.gradle.enabled",
        Value::from(false),
    )?;
    let project = table(java, "languageServers.jdtls.settings.java.project", false)?;
    set(
        project,
        "languageServers.jdtls.settings.java.project.resourceFilters",
        strings(JDTLS_RESOURCE_FILTERS),
    )?;

    if !existing
        .lines()
        .any(|l| l.trim_start().starts_with(MANAGED_MARKER))
    {
        // The root's prefix is printed before anything else; parsed again it moves to the first
        // item's prefix, which prints at the same place.
        let decor = doc.as_table_mut().decor_mut();
        let prefix = decor.prefix().and_then(|p| p.as_str()).unwrap_or("");
        let prefix = format!("{MANAGED_COMMENT}\n{prefix}");
        decor.set_prefix(prefix);
    }
    Ok(doc.to_string())
}

fn last_key(path: &str) -> &str {
    path.rsplit_once('.').map_or(path, |(_, key)| key)
}

/// The table at `path` (its last key under `parent`), created when missing: inline inside an
/// inline table, dotted inside a dotted table, else a `[table]` (`implicit` hides its header while
/// it holds no values). Anything else already at `path` is an error: the user's value is kept.
fn table<'a>(parent: &'a mut Item, path: &str, implicit: bool) -> Result<&'a mut Item, String> {
    let inline = parent.is_inline_table();
    let parent = parent
        .as_table_like_mut()
        .ok_or_else(|| format!("settings.toml: no table above `{path}`"))?;
    let dotted = parent.is_dotted();
    let new_table = || {
        if inline {
            Item::Value(Value::InlineTable(InlineTable::new()))
        } else {
            let mut t = Table::new();
            t.set_implicit(implicit);
            t.set_dotted(dotted);
            Item::Table(t)
        }
    };
    let item = parent.entry(last_key(path)).or_insert_with(new_table);
    if item.is_none() {
        *item = new_table();
    }
    if item.is_table_like() {
        Ok(item)
    } else {
        Err(format!(
            "settings.toml: `{path}` is {}, not a table",
            article(item.type_name())
        ))
    }
}

/// Sets `path` (its last key in `table`) to `value`. An equal value is left as written; a different
/// one is replaced keeping its comments and spacing.
fn set(table: &mut Item, path: &str, value: Value) -> Result<(), String> {
    let table = table
        .as_table_like_mut()
        .ok_or_else(|| format!("settings.toml: no table above `{path}`"))?;
    let key = last_key(path);
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            if !same_value(old, &value) {
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
            }
        }
        Some(Item::None) | None => {
            table.insert(key, Item::Value(value));
        }
        Some(other) => {
            return Err(format!(
                "settings.toml: `{path}` is {}, not a value",
                article(other.type_name())
            ));
        }
    }
    Ok(())
}

fn article(type_name: &str) -> String {
    let an = type_name.starts_with(['a', 'e', 'i', 'o', 'u']);
    format!("{} {type_name}", if an { "an" } else { "a" })
}

fn strings(items: &[&str]) -> Value {
    Value::Array(items.iter().copied().collect::<Array>())
}

/// Equal values, ignoring how they are written (quotes, spacing, line breaks, comments).
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same_value(a, b))
        }
        _ => false,
    }
}

/// `existing` (a `.git/info/exclude`) with `patterns` appended, or `None` when all are there.
pub fn ensure_git_excludes(existing: Option<&str>, patterns: &[&str]) -> Option<String> {
    let existing = existing.unwrap_or("");
    let present: BTreeSet<&str> = existing.lines().map(str::trim).collect();
    let mut missing: Vec<&str> = Vec::new();
    let mut seen = BTreeSet::new();
    for pattern in patterns.iter().map(|p| p.trim()) {
        if !pattern.is_empty() && !present.contains(pattern) && seen.insert(pattern) {
            missing.push(pattern);
        }
    }
    if missing.is_empty() {
        return None;
    }
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = String::with_capacity(
        existing.len()
            + GIT_EXCLUDES_COMMENT.len()
            + missing.iter().map(|p| p.len() + 2).sum::<usize>()
            + 4,
    );
    out.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push_str(newline);
    }
    if !present.contains(GIT_EXCLUDES_COMMENT) {
        out.push_str(GIT_EXCLUDES_COMMENT);
        out.push_str(newline);
    }
    for pattern in missing {
        out.push_str(pattern);
        out.push_str(newline);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA_DIR: &str =
        "/Users/me/Library/Application Support/ai.l0ng.openidea/lsp/jdtls/bazel-877507b5c805";

    fn parse(text: &str) -> DocumentMut {
        text.parse()
            .unwrap_or_else(|e| panic!("output is not TOML: {e}\n{text}"))
    }

    fn strs(item: &Item) -> Vec<String> {
        item.as_array()
            .unwrap_or_else(|| panic!("not an array: {item}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    fn assert_managed(doc: &DocumentMut, data_dir: &str) {
        let jdtls = &doc["languageServers"]["jdtls"];
        assert_eq!(strs(&jdtls["args"]), ["-data", data_dir]);
        let java = &jdtls["settings"]["java"];
        assert_eq!(java["import"]["maven"]["enabled"].as_bool(), Some(false));
        assert_eq!(java["import"]["gradle"]["enabled"].as_bool(), Some(false));
        assert_eq!(strs(&java["import"]["exclusions"]), JDTLS_IMPORT_EXCLUSIONS);
        assert_eq!(
            strs(&java["project"]["resourceFilters"]),
            JDTLS_RESOURCE_FILTERS
        );
    }

    #[test]
    fn writes_the_jdtls_settings_into_a_new_file() {
        let out = merge_openidea_settings(None, DATA_DIR).unwrap();
        assert_managed(&parse(&out), DATA_DIR);
        assert_eq!(
            out,
            format!(
                "{MANAGED_COMMENT}
[languageServers.jdtls]
args = [\"-data\", \"{DATA_DIR}\"]

[languageServers.jdtls.settings.java.import]
exclusions = [\"**/node_modules/**\", \"**/.metadata/**\", \"**/archetype-resources/**\", \"**/META-INF/maven/**\", \"**/bazel-*\", \"**/bazel-*/**\", \"**/.claude/**\", \"**/.worktrees/**\", \"**/target/**\", \"**/.ijwb/**\", \"**/.openidea/**\"]

[languageServers.jdtls.settings.java.import.maven]
enabled = false

[languageServers.jdtls.settings.java.import.gradle]
enabled = false

[languageServers.jdtls.settings.java.project]
resourceFilters = [\"node_modules\", '\\.git', \"bazel-.*\", '\\.ijwb']
"
            )
        );
        // Parents without values of their own stay implicit: no bare headers.
        for header in [
            "[languageServers]",
            "[languageServers.jdtls.settings]",
            "[languageServers.jdtls.settings.java]",
        ] {
            assert!(!out.lines().any(|l| l == header), "{header} in\n{out}");
        }
        assert_eq!(merge_openidea_settings(Some(""), DATA_DIR).unwrap(), out);
    }

    #[test]
    fn merging_twice_gives_the_same_text() {
        let inputs = [
            None,
            Some(
                "[ui]\ntheme = \"dark\"\n\n[languageServers.jdtls]\nenabled = true\n\n[editor]\ntabSize = 2\n",
            ),
            Some("# mine\ntheme = \"x\"\n[languageServers]\njdtls.command = \"/opt/jdtls\"\n"),
        ];
        for input in inputs {
            let once = merge_openidea_settings(input, DATA_DIR).unwrap();
            let twice = merge_openidea_settings(Some(&once), DATA_DIR).unwrap();
            assert_eq!(twice, once, "input {input:?}");
            assert_eq!(once.matches(MANAGED_MARKER).count(), 1);
            assert_managed(&parse(&once), DATA_DIR);
        }
    }

    #[test]
    fn keeps_the_users_other_settings_comments_and_formatting() {
        let existing = "\
# My project settings
[ui]
theme = \"dark\"   # keep me

[languageServers.rust-analyzer]
enabled = false

[languageServers.jdtls]
enabled = true
command = \"/opt/jdtls/bin/jdtls\"
args = [\"-old\"]  # replaced value, kept comment
env = { JAVA_HOME = \"/jdk\" }

[languageServers.jdtls.settings.java.import]
generatesMetadataFilesAtProjectRoot = false
maven = { enabled = true, offline = true }

[languageServers.jdtls.settings.java.format]
tabSize = 4
";
        let out = merge_openidea_settings(Some(existing), "/new data").unwrap();
        let doc = parse(&out);
        assert_managed(&doc, "/new data");
        assert!(
            out.starts_with(&format!(
                "{MANAGED_COMMENT}\n# My project settings\n[ui]\ntheme = \"dark\"   # keep me\n"
            )),
            "{out}"
        );
        assert!(out.contains("[languageServers.rust-analyzer]\nenabled = false\n"));
        assert!(out.contains(
            "enabled = true\ncommand = \"/opt/jdtls/bin/jdtls\"\nargs = [\"-data\", \"/new data\"]  # replaced value, kept comment\nenv = { JAVA_HOME = \"/jdk\" }\n"
        ));
        assert!(
            out.contains("generatesMetadataFilesAtProjectRoot = false\nmaven = { enabled = false, offline = true }\n"),
            "{out}"
        );
        assert!(out.contains("[languageServers.jdtls.settings.java.format]\ntabSize = 4\n"));
        assert_eq!(
            doc["languageServers"]["jdtls"]["settings"]["java"]["import"]["maven"]["offline"]
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            merge_openidea_settings(Some(&out), "/new data").unwrap(),
            out
        );
    }

    #[test]
    fn equal_values_keep_their_formatting() {
        let existing = format!(
            "{MANAGED_COMMENT}\n[languageServers.jdtls]\nargs = [\n    '-data',\n    '{DATA_DIR}',\n]\n"
        );
        let out = merge_openidea_settings(Some(&existing), DATA_DIR).unwrap();
        assert!(out.starts_with(&existing), "{out}");
        assert_managed(&parse(&out), DATA_DIR);
    }

    #[test]
    fn dotted_and_inline_tables_are_extended_in_their_own_style() {
        let dotted = "[languageServers]\njdtls.command = \"jdtls\"\n";
        let out = merge_openidea_settings(Some(dotted), DATA_DIR).unwrap();
        assert_managed(&parse(&out), DATA_DIR);
        assert!(out.contains("jdtls.args = [\"-data\""), "{out}");
        assert!(
            out.contains("jdtls.settings.java.import.maven.enabled = false"),
            "{out}"
        );

        let inline = "[languageServers]\njdtls = { command = \"jdtls\" }\n";
        let out = merge_openidea_settings(Some(inline), DATA_DIR).unwrap();
        assert_managed(&parse(&out), DATA_DIR);
        assert!(
            out.contains("[languageServers]\njdtls = { command = \"jdtls\" , args = [\"-data\""),
            "{out}"
        );
        assert_eq!(merge_openidea_settings(Some(&out), DATA_DIR).unwrap(), out);
    }

    #[test]
    fn data_dirs_with_quotes_and_backslashes_round_trip() {
        let dir = r#"C:\Users\me\AppData\jdtls "bazel" 'x'"#;
        let out = merge_openidea_settings(None, dir).unwrap();
        assert_managed(&parse(&out), dir);
        assert_eq!(merge_openidea_settings(Some(&out), dir).unwrap(), out);
    }

    #[test]
    fn invalid_toml_is_an_error_not_an_overwrite() {
        let err =
            merge_openidea_settings(Some("[languageServers\nargs = 1\n"), DATA_DIR).unwrap_err();
        assert!(
            err.starts_with("settings.toml is not valid TOML: "),
            "{err}"
        );
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn a_non_table_where_a_table_is_needed_is_an_error() {
        let cases = [
            (
                "languageServers = \"x\"\n",
                "settings.toml: `languageServers` is a string, not a table",
            ),
            (
                "[languageServers]\njdtls = [1]\n",
                "settings.toml: `languageServers.jdtls` is an array, not a table",
            ),
            (
                "[[languageServers.jdtls.settings]]\nx = 1\n",
                "settings.toml: `languageServers.jdtls.settings` is an array of tables, not a table",
            ),
            (
                "[languageServers.jdtls.args]\nx = 1\n",
                "settings.toml: `languageServers.jdtls.args` is a table, not a value",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                merge_openidea_settings(Some(input), DATA_DIR).unwrap_err(),
                expected
            );
        }
    }

    const PATTERNS: &[&str] = &[
        "/.openidea/bazel/",
        "/.project",
        "/.classpath",
        "/.settings/",
    ];

    #[test]
    fn git_excludes_are_appended_once_under_a_comment() {
        assert_eq!(
            ensure_git_excludes(None, PATTERNS).unwrap(),
            "# openidea-bazel\n/.openidea/bazel/\n/.project\n/.classpath\n/.settings/\n"
        );
        let existing = "# git ls-files --others --exclude-from=.git/info/exclude\n*.log";
        let out = ensure_git_excludes(Some(existing), PATTERNS).unwrap();
        assert_eq!(
            out,
            format!(
                "{existing}\n# openidea-bazel\n/.openidea/bazel/\n/.project\n/.classpath\n/.settings/\n"
            )
        );
        assert_eq!(ensure_git_excludes(Some(&out), PATTERNS), None);
    }

    #[test]
    fn git_excludes_add_only_missing_patterns() {
        let existing = "# openidea-bazel\n/.project  \n/.classpath\n";
        assert_eq!(
            ensure_git_excludes(
                Some(existing),
                &["/.project", "/.settings/", "/.settings/", " ", ""]
            )
            .unwrap(),
            "# openidea-bazel\n/.project  \n/.classpath\n/.settings/\n"
        );
        assert_eq!(ensure_git_excludes(Some(existing), &["/.classpath"]), None);
        assert_eq!(ensure_git_excludes(Some(existing), &[]), None);
        assert_eq!(ensure_git_excludes(None, &[]), None);
    }

    #[test]
    fn git_excludes_keep_crlf_line_endings() {
        assert_eq!(
            ensure_git_excludes(Some("*.log\r\n*.tmp"), &["/.project"]).unwrap(),
            "*.log\r\n*.tmp\r\n# openidea-bazel\r\n/.project\r\n"
        );
    }
}

/// The plugin's language definitions (`languages/*.toml`), in OpenIDEA's `LanguageConfig` format.
#[cfg(test)]
mod language_definitions {
    use toml_edit::{DocumentMut, Item};

    const DEFINITIONS: [(&str, &str); 3] = [
        ("starlark", include_str!("../languages/starlark.toml")),
        (
            "bazelproject",
            include_str!("../languages/bazelproject.toml"),
        ),
        ("bazelrc", include_str!("../languages/bazelrc.toml")),
    ];

    /// CodeMirror modes the OpenIDEA UI resolves `ui-mode` against (a subset of its catalogue).
    const UI_MODES: &[&str] = &["Python", "YAML", "Shell", "Properties files"];

    fn strings(item: &Item) -> Vec<&str> {
        item.as_array()
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().expect("not a string"))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parsed() -> Vec<(&'static str, DocumentMut)> {
        DEFINITIONS
            .iter()
            .map(|(file, src)| {
                (
                    *file,
                    src.parse()
                        .unwrap_or_else(|e| panic!("languages/{file}.toml: {e}")),
                )
            })
            .collect()
    }

    #[test]
    fn definitions_have_the_required_keys() {
        for (file, doc) in parsed() {
            let id = doc["id"].as_str().expect("id");
            assert_eq!(id, file, "the file is named after the id");
            assert!(
                id.len() <= 64
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
            );
            assert!(!doc["name"].as_str().expect("name").trim().is_empty());
            assert_eq!(doc["line-comment"].as_str(), Some("#"), "{file}");
            assert!(
                UI_MODES.contains(&doc["ui-mode"].as_str().expect("ui-mode")),
                "{file}"
            );
            assert_eq!(
                doc["indent"]["insert-spaces"].as_bool(),
                Some(true),
                "{file}"
            );
            assert!(
                doc["indent"]["tab-size"]
                    .as_integer()
                    .is_some_and(|n| n > 0),
                "{file}"
            );
            let extensions = strings(&doc["extensions"]);
            assert!(!extensions.is_empty(), "{file}");
            for ext in extensions {
                assert!(
                    !ext.starts_with('.') && ext == ext.to_ascii_lowercase(),
                    "{file}: {ext}"
                );
            }
            assert!(!strings(&doc["filenames"]).is_empty(), "{file}");
            for key in doc.as_table().iter().map(|(k, _)| k) {
                let known = [
                    "id",
                    "name",
                    "extensions",
                    "filenames",
                    "line-comment",
                    "indent",
                    "ui-mode",
                    "tree-sitter",
                ];
                assert!(known.contains(&key), "{file}: unexpected key {key}");
            }
        }
        let starlark = &parsed()[0].1;
        assert_eq!(starlark["indent"]["tab-size"].as_integer(), Some(4));
        assert_eq!(starlark["tree-sitter"].as_str(), Some("python"));
    }

    /// OpenIDEA's detection (`LanguageRegistry::detect`): the exact file name, then every suffix
    /// after a dot, the longest first.
    fn detect(name: &str) -> Option<&'static str> {
        let docs = parsed();
        if let Some((file, _)) = docs
            .iter()
            .find(|(_, d)| strings(&d["filenames"]).contains(&name))
        {
            return Some(file);
        }
        let lower = name.to_ascii_lowercase();
        let mut rest = lower.as_str();
        while let Some(dot) = rest.find('.') {
            rest = &rest[dot + 1..];
            if let Some((file, _)) = docs
                .iter()
                .find(|(_, d)| strings(&d["extensions"]).contains(&rest))
            {
                return Some(file);
            }
        }
        None
    }

    #[test]
    fn bazel_files_are_detected() {
        let cases = [
            ("BUILD", Some("starlark")),
            ("BUILD.bazel", Some("starlark")),
            ("MODULE.bazel", Some("starlark")),
            ("WORKSPACE", Some("starlark")),
            ("WORKSPACE.bazel", Some("starlark")),
            ("WORKSPACE.bzlmod", Some("starlark")),
            ("defs.bzl", Some("starlark")),
            ("rules.star", Some("starlark")),
            ("BUCK", Some("starlark")),
            (".bazelproject", Some("bazelproject")),
            ("allocadia.bazelproject", Some("bazelproject")),
            (".bazelrc", Some("bazelrc")),
            ("user.bazelrc", Some("bazelrc")),
            (".bazelversion", None),
            ("MODULE.bazel.lock", None),
            ("BUILDING.md", None),
        ];
        for (name, expected) in cases {
            assert_eq!(detect(name), expected, "{name}");
        }
    }

    #[test]
    fn definitions_do_not_overlap() {
        let docs = parsed();
        let mut seen = std::collections::BTreeSet::new();
        for (_, doc) in &docs {
            for ext in strings(&doc["extensions"]) {
                assert!(seen.insert(format!("*.{ext}")), "{ext} twice");
            }
            for name in strings(&doc["filenames"]) {
                assert!(seen.insert(name.to_owned()), "{name} twice");
            }
        }
    }
}
