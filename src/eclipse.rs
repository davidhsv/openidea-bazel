//! Writes the model as an Eclipse project that jdtls imports (`.project`, `.classpath`, JDT prefs).
//!
//! The whole workspace is one Eclipse project, like IntelliJ's single `.workspace` module
//! (`BlazeJavaWorkspaceImporter` / `ContentEntryEditor`): every source root becomes a `src`
//! entry, every library jar a `lib` entry, and excluded directories become resource filters.

use crate::types::JavaModel;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Workspace-relative paths of the generated files.
pub const PROJECT_FILE: &str = ".project";
pub const CLASSPATH_FILE: &str = ".classpath";
pub const JDT_CORE_PREFS_FILE: &str = ".settings/org.eclipse.jdt.core.prefs";

/// Where jdtls writes the classes of main and test sources (workspace-relative).
pub const OUTPUT_MAIN: &str = ".openidea/bazel/eclipse-bin/main";
pub const OUTPUT_TEST: &str = ".openidea/bazel/eclipse-bin/test";

/// IntelliJ falls back to Java 11 when neither the project view nor the toolchain names a level
/// (`JavaLanguageLevelHelper.getDefaultLanguageLevel`).
const DEFAULT_LANGUAGE_LEVEL: u32 = 11;

/// Bazel files inside source folders: Eclipse copies non-Java files of every source folder to the
/// output folder, and two `BUILD.bazel` files landing on the same output path abort the jdtls build.
const BAZEL_FILE_EXCLUSIONS: &str = "**/BUILD|**/BUILD.bazel|**/*.bzl";

const REGEX_FILTER_MATCHER: &str = "org.eclipse.core.resources.regexFilterMatcher";

// `IResourceFilterDescription` type bits.
const EXCLUDE_ALL: u32 = 2;
const FOLDERS: u32 = 8;
const INHERITABLE: u32 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EclipseFiles {
    pub project: String,
    pub classpath: String,
    pub jdt_core_prefs: String,
    /// What the model holds that the Eclipse project cannot express, and was left out.
    pub warnings: Vec<String>,
}

impl EclipseFiles {
    /// Each file with its workspace-relative path.
    pub fn files(&self) -> [(&'static str, &str); 3] {
        [
            (PROJECT_FILE, &self.project),
            (CLASSPATH_FILE, &self.classpath),
            (JDT_CORE_PREFS_FILE, &self.jdt_core_prefs),
        ]
    }
}

pub fn emit(model: &JavaModel, project_name: &str) -> EclipseFiles {
    let mut warnings = Vec::new();
    let excludes = normalized_excludes(&model.excludes);
    let level = match model.language_level {
        0 => {
            warnings.push(format!(
                "no Java language level: using Java {DEFAULT_LANGUAGE_LEVEL}"
            ));
            DEFAULT_LANGUAGE_LEVEL
        }
        level => level,
    };
    let project = project_xml(project_name, &excludes);
    let classpath = classpath_xml(model, &excludes, level, &mut warnings);
    let jdt_core_prefs = jdt_core_prefs(level);
    EclipseFiles {
        project,
        classpath,
        jdt_core_prefs,
        warnings,
    }
}

/// The model's excluded directories, trimmed of slashes, without duplicates, in model order.
fn normalized_excludes(excludes: &[String]) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    excludes
        .iter()
        .map(|e| e.trim_matches('/'))
        .filter(|e| !e.is_empty() && seen.insert(*e))
        .collect()
}

struct Filter<'a> {
    /// Workspace-relative folder the filter is attached to; `""` is the project.
    container: &'a str,
    kind: u32,
    regex: String,
}

fn project_xml(project_name: &str, excludes: &[&str]) -> String {
    let mut filters = vec![
        Filter {
            container: "",
            kind: EXCLUDE_ALL | FOLDERS,
            regex: "bazel-.*".to_owned(),
        },
        Filter {
            container: "",
            kind: EXCLUDE_ALL | FOLDERS,
            regex: "\\.git".to_owned(),
        },
        // Inheritable only for names that are never Java packages: a package may be called `target`.
        Filter {
            container: "",
            kind: EXCLUDE_ALL | FOLDERS | INHERITABLE,
            regex: "node_modules".to_owned(),
        },
    ];
    for exclude in excludes {
        let (container, name) = exclude.rsplit_once('/').unwrap_or(("", exclude));
        if !covered_by_fixed_filters(container, name) {
            let mut regex = String::with_capacity(name.len() + 2);
            regex.push('^');
            push_regex_escaped(&mut regex, name);
            regex.push('$');
            filters.push(Filter {
                container,
                kind: EXCLUDE_ALL | FOLDERS,
                regex,
            });
        }
    }

    let mut out = String::with_capacity(512 + filters.len() * 256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<projectDescription>\n\t<name>");
    push_xml_escaped(&mut out, project_name);
    out.push_str(
        "</name>\n\
         \t<comment>Generated from Bazel by openidea-bazel</comment>\n\
         \t<projects>\n\
         \t</projects>\n\
         \t<buildSpec>\n\
         \t\t<buildCommand>\n\
         \t\t\t<name>org.eclipse.jdt.core.javabuilder</name>\n\
         \t\t\t<arguments>\n\
         \t\t\t</arguments>\n\
         \t\t</buildCommand>\n\
         \t</buildSpec>\n\
         \t<natures>\n\
         \t\t<nature>org.eclipse.jdt.core.javanature</nature>\n\
         \t</natures>\n\
         \t<filteredResources>\n",
    );
    for (i, filter) in filters.iter().enumerate() {
        out.push_str("\t\t<filter>\n\t\t\t<id>");
        out.push_str(&(i + 1).to_string());
        out.push_str("</id>\n\t\t\t<name>");
        push_xml_escaped(&mut out, filter.container);
        out.push_str("</name>\n\t\t\t<type>");
        out.push_str(&filter.kind.to_string());
        out.push_str("</type>\n\t\t\t<matcher>\n\t\t\t\t<id>");
        out.push_str(REGEX_FILTER_MATCHER);
        out.push_str("</id>\n\t\t\t\t<arguments>");
        push_xml_escaped(&mut out, &filter.regex);
        out.push_str("</arguments>\n\t\t\t</matcher>\n\t\t</filter>\n");
    }
    out.push_str("\t</filteredResources>\n</projectDescription>\n");
    out
}

/// Whether one of the fixed project-root filters already hides `container/name`.
fn covered_by_fixed_filters(container: &str, name: &str) -> bool {
    name == "node_modules"
        || (container.is_empty() && (name == ".git" || name.starts_with("bazel-")))
}

/// Whether a resource filter hides the workspace-relative directory `path`, or one above it.
fn hidden_by_filters(path: &str, excludes: &BTreeSet<&str>) -> bool {
    if path.is_empty() {
        return false;
    }
    let mut start = 0;
    loop {
        let end = path[start..].find('/').map_or(path.len(), |i| start + i);
        let container = if start == 0 { "" } else { &path[..start - 1] };
        if covered_by_fixed_filters(container, &path[start..end]) || excludes.contains(&path[..end])
        {
            return true;
        }
        if end == path.len() {
            return false;
        }
        start = end + 1;
    }
}

struct Root<'a> {
    path: &'a str,
    package_prefix: &'a str,
    is_test: bool,
}

/// `a` is a directory strictly above `b` (`""` is above every other path).
fn is_ancestor(a: &str, b: &str) -> bool {
    if a.is_empty() {
        return !b.is_empty();
    }
    b.len() > a.len() && b.starts_with(a) && b.as_bytes()[a.len()] == b'/'
}

/// For each root, the roots directly inside it. Eclipse forbids overlapping `src` entries, so an
/// outer root must exclude the roots nested in it.
fn nested_roots(roots: &[Root]) -> Vec<Vec<usize>> {
    // Component-wise order puts every root right before the roots inside it (a plain string order
    // would sort `a/b-c` between `a/b` and `a/b/c`).
    let mut order: Vec<usize> = (0..roots.len()).collect();
    order.sort_by(|&a, &b| roots[a].path.split('/').cmp(roots[b].path.split('/')));
    let mut nested = vec![Vec::new(); roots.len()];
    let mut enclosing: Vec<usize> = Vec::new();
    for i in order {
        while let Some(&outer) = enclosing.last() {
            if is_ancestor(roots[outer].path, roots[i].path) {
                break;
            }
            enclosing.pop();
        }
        if let Some(&outer) = enclosing.last() {
            nested[outer].push(i);
        }
        enclosing.push(i);
    }
    nested
}

fn classpath_xml(
    model: &JavaModel,
    excludes: &[&str],
    level: u32,
    warnings: &mut Vec<String>,
) -> String {
    // Eclipse rejects a classpath with the same `src` path twice: a directory listed as both main
    // and test is kept once, as main, so that main sources still see it.
    let mut roots: Vec<Root> = Vec::with_capacity(model.source_roots.len());
    let mut root_index: BTreeMap<&str, usize> = BTreeMap::new();
    for root in &model.source_roots {
        let path = root.path.trim_matches('/');
        match root_index.get(path) {
            Some(&i) => {
                let kept = &mut roots[i];
                kept.is_test &= root.is_test;
                warnings.push(format!(
                    "source root `{path}` is listed more than once: kept once, as {}",
                    if kept.is_test { "test" } else { "main" }
                ));
            }
            None => {
                root_index.insert(path, roots.len());
                roots.push(Root {
                    path,
                    package_prefix: &root.package_prefix,
                    is_test: root.is_test,
                });
            }
        }
    }
    let nested = nested_roots(&roots);
    let exclude_set: BTreeSet<&str> = excludes.iter().copied().collect();

    let mut out = String::with_capacity(256 + roots.len() * 256 + model.libraries.len() * 320);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<classpath>\n");
    for (i, root) in roots.iter().enumerate() {
        if hidden_by_filters(root.path, &exclude_set) {
            warnings.push(format!(
                "source root `{}` is inside an excluded directory: left out",
                root.path
            ));
            continue;
        }
        if !root.package_prefix.is_empty() {
            warnings.push(format!(
                "source root `{}` has package prefix `{}`, which Eclipse cannot express: left out",
                root.path, root.package_prefix
            ));
            continue;
        }
        out.push_str("\t<classpathentry kind=\"src\" path=\"");
        push_xml_escaped(&mut out, root.path);
        out.push_str("\" excluding=\"");
        out.push_str(BAZEL_FILE_EXCLUSIONS);
        if root.path.is_empty() {
            // The project folder holds the output folder, which must not be a source.
            out.push_str("|.openidea/");
        }
        for &inner in &nested[i] {
            let inner = roots[inner].path;
            out.push('|');
            push_xml_escaped(
                &mut out,
                if root.path.is_empty() {
                    inner
                } else {
                    &inner[root.path.len() + 1..]
                },
            );
            out.push('/');
        }
        out.push_str("\" output=\"");
        out.push_str(if root.is_test {
            OUTPUT_TEST
        } else {
            OUTPUT_MAIN
        });
        if root.is_test {
            out.push_str("\">\n");
            push_test_attribute(&mut out);
        } else {
            out.push_str("\"/>\n");
        }
    }

    push_libraries(&mut out, model, warnings);

    out.push_str(
        "\t<classpathentry kind=\"con\" path=\"org.eclipse.jdt.launching.JRE_CONTAINER/\
         org.eclipse.jdt.internal.debug.ui.launcher.StandardVMType/",
    );
    out.push_str(&execution_environment(level));
    out.push_str("\"/>\n\t<classpathentry kind=\"output\" path=\"");
    out.push_str(OUTPUT_MAIN);
    out.push_str("\"/>\n</classpath>\n");
    out
}

struct Lib<'a> {
    jar: &'a str,
    source_jar: Option<&'a str>,
    test_only: bool,
}

fn push_libraries(out: &mut String, model: &JavaModel, warnings: &mut Vec<String>) {
    // A jar on the classpath twice is a build path error in Eclipse: merge duplicates in place
    // (test-only only when every occurrence is).
    let mut libs: Vec<Lib> = Vec::with_capacity(model.libraries.len());
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut duplicates, mut without_jar) = (0usize, 0usize);
    for lib in &model.libraries {
        if lib.jar.is_empty() {
            without_jar += 1;
            continue;
        }
        let source_jar = lib.source_jar.as_deref().filter(|s| !s.is_empty());
        match index.get(lib.jar.as_str()) {
            Some(&i) => {
                duplicates += 1;
                let kept = &mut libs[i];
                kept.test_only &= lib.test_only;
                kept.source_jar = kept.source_jar.or(source_jar);
            }
            None => {
                index.insert(&lib.jar, libs.len());
                libs.push(Lib {
                    jar: &lib.jar,
                    source_jar,
                    test_only: lib.test_only,
                });
            }
        }
    }
    if duplicates > 0 {
        warnings.push(format!("{duplicates} duplicate library jars merged"));
    }
    if without_jar > 0 {
        warnings.push(format!(
            "{without_jar} libraries without a jar path left out"
        ));
    }
    for lib in &libs {
        out.push_str("\t<classpathentry kind=\"lib\" path=\"");
        push_xml_escaped(out, lib.jar);
        if let Some(source_jar) = lib.source_jar {
            out.push_str("\" sourcepath=\"");
            push_xml_escaped(out, source_jar);
        }
        if lib.test_only {
            out.push_str("\">\n");
            push_test_attribute(out);
        } else {
            out.push_str("\"/>\n");
        }
    }
}

/// The rest of an entry opened with `>`: Eclipse's `test` classpath attribute (test sources and
/// test-only jars are invisible to main sources).
fn push_test_attribute(out: &mut String) {
    out.push_str(
        "\t\t<attributes>\n\
         \t\t\t<attribute name=\"test\" value=\"true\"/>\n\
         \t\t</attributes>\n\
         \t</classpathentry>\n",
    );
}

/// The compliance level as JDT writes it: `1.8` up to Java 8, the feature version after.
fn compliance(level: u32) -> String {
    if level <= 8 {
        format!("1.{level}")
    } else {
        level.to_string()
    }
}

/// The execution environment id of the JRE container (`JavaSE-17`, `JavaSE-1.8`, `J2SE-1.5`).
fn execution_environment(level: u32) -> String {
    if level <= 5 {
        format!("J2SE-1.{level}")
    } else {
        format!("JavaSE-{}", compliance(level))
    }
}

fn jdt_core_prefs(level: u32) -> String {
    let level = compliance(level);
    format!(
        "eclipse.preferences.version=1\n\
         org.eclipse.jdt.core.compiler.codegen.targetPlatform={level}\n\
         org.eclipse.jdt.core.compiler.compliance={level}\n\
         org.eclipse.jdt.core.compiler.source={level}\n\
         org.eclipse.jdt.core.compiler.release=enabled\n\
         org.eclipse.jdt.core.builder.duplicateResourceTask=warning\n\
         org.eclipse.jdt.core.builder.resourceCopyExclusionFilter=*.launch,BUILD,BUILD.bazel,*.bzl\n"
    )
}

fn push_xml_escaped(out: &mut String, s: &str) {
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let escaped = match c {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&apos;",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(escaped);
        start = i + 1;
    }
    out.push_str(&s[start..]);
}

/// Escapes `java.util.regex` metacharacters, so the name matches literally.
fn push_regex_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Library, SourceRoot};

    fn root(path: &str, is_test: bool) -> SourceRoot {
        SourceRoot {
            path: path.to_owned(),
            package_prefix: String::new(),
            is_test,
        }
    }

    fn lib(jar: &str, source_jar: Option<&str>, test_only: bool) -> Library {
        Library {
            jar: jar.to_owned(),
            source_jar: source_jar.map(str::to_owned),
            test_only,
        }
    }

    fn model(
        source_roots: Vec<SourceRoot>,
        libraries: Vec<Library>,
        excludes: &[&str],
        level: u32,
    ) -> JavaModel {
        JavaModel {
            workspace_root: "/ws".to_owned(),
            source_roots,
            libraries,
            excludes: excludes.iter().map(|e| (*e).to_owned()).collect(),
            language_level: level,
            warnings: Vec::new(),
        }
    }

    /// The `src` entries' `path` and `excluding` attributes.
    fn src_entries(classpath: &str) -> Vec<(String, String)> {
        classpath
            .lines()
            .filter(|l| l.contains("kind=\"src\""))
            .map(|l| (attr(l, "path").to_owned(), attr(l, "excluding").to_owned()))
            .collect()
    }

    fn attr<'a>(line: &'a str, name: &str) -> &'a str {
        let start = line.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
        let end = start + line[start..].find('"').unwrap();
        &line[start..end]
    }

    /// `(container, type, regex)` of every filter in a `.project`.
    fn filters(project: &str) -> Vec<(String, String, String)> {
        let tag = |block: &str, name: &str| {
            let open = format!("<{name}>");
            let start = block.find(&open).unwrap() + open.len();
            block[start..start + block[start..].find("</").unwrap()].to_owned()
        };
        project
            .split("<filter>")
            .skip(1)
            .map(|f| {
                let matcher = &f[f.find("<matcher>").unwrap()..];
                (tag(f, "name"), tag(f, "type"), tag(matcher, "arguments"))
            })
            .collect()
    }

    #[test]
    fn small_model_golden() {
        let m = model(
            vec![
                root("lib/src/main/java", false),
                root("lib/src/test/java", true),
            ],
            vec![
                lib(
                    "/out/bin/lib/liba.jar",
                    Some("/out/bin/lib/liba-src.jar"),
                    false,
                ),
                lib("/ext/junit.jar", None, true),
            ],
            &["lib/target"],
            17,
        );
        let files = emit(&m, "my-project");
        assert!(files.warnings.is_empty(), "{:?}", files.warnings);
        assert_eq!(
            files.project,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<projectDescription>
\t<name>my-project</name>
\t<comment>Generated from Bazel by openidea-bazel</comment>
\t<projects>
\t</projects>
\t<buildSpec>
\t\t<buildCommand>
\t\t\t<name>org.eclipse.jdt.core.javabuilder</name>
\t\t\t<arguments>
\t\t\t</arguments>
\t\t</buildCommand>
\t</buildSpec>
\t<natures>
\t\t<nature>org.eclipse.jdt.core.javanature</nature>
\t</natures>
\t<filteredResources>
\t\t<filter>
\t\t\t<id>1</id>
\t\t\t<name></name>
\t\t\t<type>10</type>
\t\t\t<matcher>
\t\t\t\t<id>org.eclipse.core.resources.regexFilterMatcher</id>
\t\t\t\t<arguments>bazel-.*</arguments>
\t\t\t</matcher>
\t\t</filter>
\t\t<filter>
\t\t\t<id>2</id>
\t\t\t<name></name>
\t\t\t<type>10</type>
\t\t\t<matcher>
\t\t\t\t<id>org.eclipse.core.resources.regexFilterMatcher</id>
\t\t\t\t<arguments>\\.git</arguments>
\t\t\t</matcher>
\t\t</filter>
\t\t<filter>
\t\t\t<id>3</id>
\t\t\t<name></name>
\t\t\t<type>26</type>
\t\t\t<matcher>
\t\t\t\t<id>org.eclipse.core.resources.regexFilterMatcher</id>
\t\t\t\t<arguments>node_modules</arguments>
\t\t\t</matcher>
\t\t</filter>
\t\t<filter>
\t\t\t<id>4</id>
\t\t\t<name>lib</name>
\t\t\t<type>10</type>
\t\t\t<matcher>
\t\t\t\t<id>org.eclipse.core.resources.regexFilterMatcher</id>
\t\t\t\t<arguments>^target$</arguments>
\t\t\t</matcher>
\t\t</filter>
\t</filteredResources>
</projectDescription>
"
        );
        assert_eq!(
            files.classpath,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<classpath>
\t<classpathentry kind=\"src\" path=\"lib/src/main/java\" excluding=\"**/BUILD|**/BUILD.bazel|**/*.bzl\" output=\".openidea/bazel/eclipse-bin/main\"/>
\t<classpathentry kind=\"src\" path=\"lib/src/test/java\" excluding=\"**/BUILD|**/BUILD.bazel|**/*.bzl\" output=\".openidea/bazel/eclipse-bin/test\">
\t\t<attributes>
\t\t\t<attribute name=\"test\" value=\"true\"/>
\t\t</attributes>
\t</classpathentry>
\t<classpathentry kind=\"lib\" path=\"/out/bin/lib/liba.jar\" sourcepath=\"/out/bin/lib/liba-src.jar\"/>
\t<classpathentry kind=\"lib\" path=\"/ext/junit.jar\">
\t\t<attributes>
\t\t\t<attribute name=\"test\" value=\"true\"/>
\t\t</attributes>
\t</classpathentry>
\t<classpathentry kind=\"con\" path=\"org.eclipse.jdt.launching.JRE_CONTAINER/org.eclipse.jdt.internal.debug.ui.launcher.StandardVMType/JavaSE-17\"/>
\t<classpathentry kind=\"output\" path=\".openidea/bazel/eclipse-bin/main\"/>
</classpath>
"
        );
        assert_eq!(
            files.jdt_core_prefs,
            "eclipse.preferences.version=1
org.eclipse.jdt.core.compiler.codegen.targetPlatform=17
org.eclipse.jdt.core.compiler.compliance=17
org.eclipse.jdt.core.compiler.source=17
org.eclipse.jdt.core.compiler.release=enabled
org.eclipse.jdt.core.builder.duplicateResourceTask=warning
org.eclipse.jdt.core.builder.resourceCopyExclusionFilter=*.launch,BUILD,BUILD.bazel,*.bzl
"
        );
        let paths: Vec<&str> = files.files().iter().map(|(p, _)| *p).collect();
        assert_eq!(
            paths,
            [
                ".project",
                ".classpath",
                ".settings/org.eclipse.jdt.core.prefs"
            ]
        );
    }

    #[test]
    fn java_8_is_written_as_1_8() {
        let files = emit(&model(vec![], vec![], &[], 8), "p");
        assert!(files.jdt_core_prefs.contains("compiler.compliance=1.8\n"));
        assert!(files.jdt_core_prefs.contains("compiler.source=1.8\n"));
        assert!(
            files
                .jdt_core_prefs
                .contains("codegen.targetPlatform=1.8\n")
        );
        assert!(files.classpath.contains("StandardVMType/JavaSE-1.8\"/>"));
        assert_eq!(execution_environment(21), "JavaSE-21");
        assert_eq!(execution_environment(7), "JavaSE-1.7");
        assert_eq!(execution_environment(5), "J2SE-1.5");
    }

    #[test]
    fn missing_language_level_falls_back_to_java_11_with_a_warning() {
        let files = emit(&model(vec![], vec![], &[], 0), "p");
        assert!(files.jdt_core_prefs.contains("compiler.compliance=11\n"));
        assert!(files.classpath.contains("JavaSE-11\"/>"));
        assert_eq!(files.warnings, ["no Java language level: using Java 11"]);
    }

    #[test]
    fn nested_roots_are_excluded_from_the_enclosing_root() {
        let m = model(
            vec![
                root("a", false),
                root("a/b-c", false),
                root("a/b", false),
                root("a/b/c", false),
                root("a/b/c/d", true),
                root("a/b/e", false),
                root("ab", false),
                root("x/y", true),
            ],
            vec![],
            &[],
            17,
        );
        let files = emit(&m, "p");
        assert!(files.warnings.is_empty(), "{:?}", files.warnings);
        let base = BAZEL_FILE_EXCLUSIONS;
        let expected: Vec<(String, String)> = [
            ("a", format!("{base}|b/|b-c/")),
            ("a/b-c", base.to_owned()),
            ("a/b", format!("{base}|c/|e/")),
            ("a/b/c", format!("{base}|d/")),
            ("a/b/c/d", base.to_owned()),
            ("a/b/e", base.to_owned()),
            ("ab", base.to_owned()),
            ("x/y", base.to_owned()),
        ]
        .into_iter()
        .map(|(p, e)| (p.to_owned(), e))
        .collect();
        assert_eq!(src_entries(&files.classpath), expected);
    }

    #[test]
    fn workspace_root_as_source_root_excludes_every_other_root_and_the_output() {
        let m = model(
            vec![
                root("", false),
                root("java/src", false),
                root("java/src/gen", false),
                root("t", true),
            ],
            vec![],
            &[],
            17,
        );
        let files = emit(&m, "p");
        let entries = src_entries(&files.classpath);
        assert_eq!(
            entries[0],
            (
                "".to_owned(),
                format!("{BAZEL_FILE_EXCLUSIONS}|.openidea/|java/src/|t/")
            )
        );
        assert_eq!(
            entries[1],
            (
                "java/src".to_owned(),
                format!("{BAZEL_FILE_EXCLUSIONS}|gen/")
            )
        );
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn package_prefix_roots_are_skipped_with_a_warning_but_still_excluded_from_outer_roots() {
        // `tools/rules` is a root with `packagePrefix="com.allocadia.tools"` in Allocadia's `.workspace.iml`.
        let m = model(
            vec![
                root("tools", false),
                SourceRoot {
                    path: "tools/rules".to_owned(),
                    package_prefix: "com.allocadia.tools".to_owned(),
                    is_test: false,
                },
                root("tools/sanity/tests/java", false),
            ],
            vec![],
            &[],
            17,
        );
        let files = emit(&m, "p");
        assert_eq!(
            src_entries(&files.classpath),
            [
                (
                    "tools".to_owned(),
                    format!("{BAZEL_FILE_EXCLUSIONS}|rules/|sanity/tests/java/")
                ),
                (
                    "tools/sanity/tests/java".to_owned(),
                    BAZEL_FILE_EXCLUSIONS.to_owned()
                ),
            ]
        );
        assert_eq!(
            files.warnings,
            [
                "source root `tools/rules` has package prefix `com.allocadia.tools`, which Eclipse cannot express: left out"
            ]
        );
    }

    #[test]
    fn duplicate_roots_and_jars_are_emitted_once() {
        let m = model(
            vec![
                root("a/src", true),
                root("a/src/", false),
                root("b/test", true),
                root("b/test", true),
            ],
            vec![
                lib("/j/a.jar", None, true),
                lib("/j/b.jar", None, false),
                lib("/j/a.jar", Some("/j/a-src.jar"), false),
                lib("", None, false),
            ],
            &[],
            17,
        );
        let files = emit(&m, "p");
        assert_eq!(files.classpath.matches("path=\"a/src\"").count(), 1);
        assert!(files.classpath.contains("path=\"a/src\" excluding=\"**/BUILD|**/BUILD.bazel|**/*.bzl\" output=\".openidea/bazel/eclipse-bin/main\"/>"));
        assert!(files.classpath.contains("path=\"b/test\" excluding=\"**/BUILD|**/BUILD.bazel|**/*.bzl\" output=\".openidea/bazel/eclipse-bin/test\">"));
        let libs: Vec<&str> = files
            .classpath
            .lines()
            .filter(|l| l.contains("kind=\"lib\""))
            .collect();
        // First position kept; main use wins over test-only; the source jar of a later duplicate is kept.
        assert_eq!(
            libs,
            [
                "\t<classpathentry kind=\"lib\" path=\"/j/a.jar\" sourcepath=\"/j/a-src.jar\"/>",
                "\t<classpathentry kind=\"lib\" path=\"/j/b.jar\"/>",
            ]
        );
        assert_eq!(
            files.warnings,
            [
                "source root `a/src` is listed more than once: kept once, as main",
                "source root `b/test` is listed more than once: kept once, as test",
                "1 duplicate library jars merged",
                "1 libraries without a jar path left out",
            ]
        );
    }

    #[test]
    fn excludes_become_non_inheritable_filters_on_their_parent() {
        let m = model(
            vec![],
            vec![],
            &[
                "projects/web/target",
                "/libs/v1.2+beta/",
                ".claude/worktrees",
                ".husky",
                "bazel-bin",
                ".git",
                "web/node_modules",
                "projects/web/target",
                "a/bazel-x",
                "",
            ],
            17,
        );
        let files = emit(&m, "p");
        let f = |c: &str, t: &str, r: &str| (c.to_owned(), t.to_owned(), r.to_owned());
        assert_eq!(
            filters(&files.project),
            [
                f("", "10", "bazel-.*"),
                f("", "10", "\\.git"),
                f("", "26", "node_modules"),
                f("projects/web", "10", "^target$"),
                f("libs", "10", "^v1\\.2\\+beta$"),
                f(".claude", "10", "^worktrees$"),
                f("", "10", "^\\.husky$"),
                f("a", "10", "^bazel-x$"),
            ]
        );
        let ids: Vec<&str> = files
            .project
            .lines()
            .filter_map(|l| l.trim().strip_prefix("<id>")?.strip_suffix("</id>"))
            .filter(|id| *id != REGEX_FILTER_MATCHER)
            .collect();
        assert_eq!(ids, ["1", "2", "3", "4", "5", "6", "7", "8"]);
    }

    #[test]
    fn roots_hidden_by_filters_are_left_out() {
        let m = model(
            vec![
                root("app/target/generated-sources", false),
                root("web/node_modules/pkg/src", false),
                root("bazel-out/gen", false),
                root("app/targets/src", false),
            ],
            vec![],
            &["app/target"],
            17,
        );
        let files = emit(&m, "p");
        assert_eq!(
            src_entries(&files.classpath),
            [(
                "app/targets/src".to_owned(),
                BAZEL_FILE_EXCLUSIONS.to_owned()
            )]
        );
        assert_eq!(files.warnings.len(), 3);
        assert!(files.warnings[0].starts_with(
            "source root `app/target/generated-sources` is inside an excluded directory"
        ));
    }

    #[test]
    fn names_and_paths_are_xml_escaped() {
        let m = model(
            vec![root("a&b/src", false), root("a&b/src/x<y>", false)],
            vec![lib("/j/\"q\"'s.jar", Some("/j/a&b-src.jar"), false)],
            &["dir&<x>"],
            17,
        );
        let files = emit(&m, "Tom & \"Jerry\" <x>");
        assert!(
            files
                .project
                .contains("<name>Tom &amp; &quot;Jerry&quot; &lt;x&gt;</name>")
        );
        assert!(
            files
                .project
                .contains("<arguments>^dir&amp;&lt;x&gt;$</arguments>")
        );
        assert!(files.classpath.contains(
            "path=\"a&amp;b/src\" excluding=\"**/BUILD|**/BUILD.bazel|**/*.bzl|x&lt;y&gt;/\""
        ));
        assert!(
            files.classpath.contains(
                "path=\"/j/&quot;q&quot;&apos;s.jar\" sourcepath=\"/j/a&amp;b-src.jar\"/>"
            )
        );
        for xml in [&files.project, &files.classpath] {
            let body = xml
                .replace("&amp;", "")
                .replace("&lt;", "")
                .replace("&gt;", "")
                .replace("&quot;", "")
                .replace("&apos;", "");
            assert!(!body.contains('&'));
        }
    }

    #[test]
    fn output_is_deterministic() {
        let m = model(
            vec![root("b", false), root("a", true), root("a/x", false)],
            vec![
                lib("/z.jar", None, false),
                lib("/a.jar", Some("/a-src.jar"), true),
            ],
            &["c/d", "a/x/out"],
            21,
        );
        let first = emit(&m, "p");
        let second = emit(&m.clone(), "p");
        assert_eq!(first, second);
        // Model order is classpath order: libraries are not sorted.
        assert!(first.classpath.find("/z.jar").unwrap() < first.classpath.find("/a.jar").unwrap());
        assert!(
            first.classpath.find("path=\"b\"").unwrap()
                < first.classpath.find("path=\"a\"").unwrap()
        );
    }

    #[test]
    fn empty_model_still_produces_a_valid_project() {
        let files = emit(
            &JavaModel {
                language_level: 17,
                ..JavaModel::default()
            },
            "",
        );
        assert!(files.project.contains("<name></name>"));
        assert_eq!(
            files.classpath,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<classpath>\n\
             \t<classpathentry kind=\"con\" path=\"org.eclipse.jdt.launching.JRE_CONTAINER/org.eclipse.jdt.internal.debug.ui.launcher.StandardVMType/JavaSE-17\"/>\n\
             \t<classpathentry kind=\"output\" path=\".openidea/bazel/eclipse-bin/main\"/>\n</classpath>\n"
        );
        assert!(files.warnings.is_empty());
    }
}
