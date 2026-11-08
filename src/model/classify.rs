//! Source and library targets: port of `JavaSourceFilter`, `ProjectViewTargetImportFilter`,
//! `ImportRoots.importAsSource` and `TargetExpressionList.includesTarget`
//! (bazelbuild/intellij, ijwb branch).

use crate::fxhash::FxHashSet;

use super::paths::is_under;
use crate::types::{
    ArtifactLocation, ImportRoots, LibraryArtifact, ProjectView, TargetExpression, TargetIdeInfo,
    TargetKey, TargetMap,
};

const TAG_IMPORT_TARGET_OUTPUT: &str = "intellij-import-target-output";
const TAG_IMPORT_AS_LIBRARY_LEGACY: &str = "aswb-import-as-library";
const TAG_PROVIDED_BY_SDK: &str = "intellij-provided-by-sdk";
const TAG_EXCLUDE_TARGET: &str = "intellij-exclude-target";

/// `JavaLikeLanguage.Java.getNonSourceKinds`.
const NON_SOURCE_KINDS: [&str; 2] = ["java_wrap_cc", "java_import"];

/// `JavaBlazeRules.getJavaProtoLibraryKinds`.
pub(crate) const JAVA_PROTO_LIBRARY_KINDS: [&str; 3] = [
    "java_proto_library",
    "java_lite_proto_library",
    "java_mutable_proto_library",
];

/// The Java targets of a sync, split as `JavaSourceFilter` does.
pub(crate) struct Classified<'a> {
    /// Targets whose sources are edited in the project, in target-map order.
    pub source_targets: Vec<&'a TargetIdeInfo>,
    /// Targets whose jars are the project's libraries, in target-map order.
    pub library_targets: Vec<&'a TargetIdeInfo>,
    pub library_keys: FxHashSet<&'a TargetKey>,
    /// `jdepsPathsForExcludedJars`: exec paths of the jars of source and excluded targets.
    pub excluded_jars: FxHashSet<String>,
}

pub(crate) fn classify<'a>(
    targets: &'a TargetMap,
    view: &ProjectView,
    roots: &ImportRoots,
) -> Classified<'a> {
    let filter = ImportFilter::new(view, roots);
    let mut out = Classified {
        source_targets: Vec::new(),
        library_targets: Vec::new(),
        library_keys: FxHashSet::default(),
        excluded_jars: FxHashSet::default(),
    };
    for target in targets.values() {
        if filter.exclude_target(target) {
            add_jar_paths(&mut out.excluded_jars, target);
            continue;
        }
        if target.java_ide_info.is_none() {
            continue;
        }
        if filter.is_source_target(target)
            && (is_java_source_target(target) || is_java_proto_target(target))
        {
            add_jar_paths(&mut out.excluded_jars, target);
            out.source_targets.push(target);
        } else {
            out.library_keys.insert(&target.key);
            out.library_targets.push(target);
        }
    }
    out
}

/// The `.java` entries of a target's sources (`JavaLikeLanguage.getSourceFileMatcher`).
pub(crate) fn java_sources(target: &TargetIdeInfo) -> impl Iterator<Item = &ArtifactLocation> {
    target
        .java_ide_info
        .iter()
        .flat_map(|j| &j.sources)
        .filter(|s| s.relative_path.ends_with(".java"))
}

/// `JavaSourceFilter.isJavaProtoTarget`.
pub(crate) fn is_java_proto_target(target: &TargetIdeInfo) -> bool {
    target.java_ide_info.is_some()
        && (JAVA_PROTO_LIBRARY_KINDS.contains(&target.kind_string.as_str())
            || target.kind_string == "proto_library")
}

fn is_java_source_target(target: &TargetIdeInfo) -> bool {
    !NON_SOURCE_KINDS.contains(&target.kind_string.as_str())
        && java_sources(target).any(|s| s.is_source)
}

fn add_jar_paths(paths: &mut FxHashSet<String>, target: &TargetIdeInfo) {
    let Some(java) = &target.java_ide_info else {
        return;
    };
    for jar in &java.jars {
        paths.extend(artifact_paths(jar));
    }
}

fn artifact_paths(jar: &LibraryArtifact) -> impl Iterator<Item = String> + '_ {
    jar.jar
        .iter()
        .chain(&jar.interface_jar)
        .chain(&jar.source_jars)
        .map(ArtifactLocation::exec_path)
}

/// `ProjectViewTargetImportFilter` with `ImportRoots.importAsSource`.
struct ImportFilter<'v> {
    roots: &'v ImportRoots,
    targets: Vec<Pattern<'v>>,
    import_target_output: FxHashSet<Label<'v>>,
    exclude_target: FxHashSet<Label<'v>>,
}

impl<'v> ImportFilter<'v> {
    fn new(view: &'v ProjectView, roots: &'v ImportRoots) -> Self {
        let labels = |list: &'v [String]| list.iter().filter_map(|l| Label::parse(l)).collect();
        Self {
            roots,
            targets: view.targets.iter().filter_map(Pattern::parse).collect(),
            import_target_output: labels(&view.import_target_output),
            exclude_target: labels(&view.exclude_target),
        }
    }

    fn exclude_target(&self, target: &TargetIdeInfo) -> bool {
        Label::parse(&target.key.label).is_some_and(|l| self.exclude_target.contains(&l))
            || has_tag(target, TAG_PROVIDED_BY_SDK)
            || has_tag(target, TAG_EXCLUDE_TARGET)
    }

    fn is_source_target(&self, target: &TargetIdeInfo) -> bool {
        let Some(label) = Label::parse(&target.key.label) else {
            return false;
        };
        let import_as_source = self.contains(label.package) || self.target_in_project(&label);
        import_as_source
            && !has_tag(target, TAG_IMPORT_TARGET_OUTPUT)
            && !has_tag(target, TAG_IMPORT_AS_LIBRARY_LEGACY)
            && !self.import_target_output.contains(&label)
    }

    /// `ProjectDirectoriesHelper.containsWorkspacePath`.
    fn contains(&self, package: &str) -> bool {
        self.roots.includes.iter().any(|i| is_under(i, package))
            && !self.roots.excludes.iter().any(|e| is_under(e, package))
    }

    /// `TargetExpressionList.includesTarget`: the last expression covering the label decides.
    /// Its fallback for derived targets is `contains`, already checked by the caller.
    fn target_in_project(&self, label: &Label) -> bool {
        self.targets
            .iter()
            .rev()
            .find(|p| p.covers(label))
            .is_some_and(|p| !p.excluded)
    }
}

fn has_tag(target: &TargetIdeInfo, tag: &str) -> bool {
    target.tags.iter().any(|t| t == tag)
}

/// A main-repository label. `@//` and `@@//` name the main repository too; any other `@repo`
/// is external and never imported as source (`Label.isExternal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Label<'s> {
    package: &'s str,
    name: &'s str,
}

impl<'s> Label<'s> {
    fn parse(label: &'s str) -> Option<Self> {
        let label = label.trim();
        let rest = ["@@//", "@//", "//"]
            .iter()
            .find_map(|p| label.strip_prefix(p))?;
        Some(match rest.split_once(':') {
            Some((package, name)) => Label { package, name },
            None => Label {
                package: rest,
                name: rest.rsplit('/').next().unwrap_or(rest),
            },
        })
    }
}

/// One `targets:` entry (`WildcardTargetPattern` or a single label).
struct Pattern<'v> {
    excluded: bool,
    kind: PatternKind<'v>,
}

enum PatternKind<'v> {
    /// `//pkg/...`, `//pkg/...:all`, `//pkg/...:*`, `//pkg/...:all-targets`.
    Recursive(&'v str),
    /// `//pkg:all`, `//pkg:*`, `//pkg:all-targets`.
    Package(&'v str),
    Label(Label<'v>),
}

impl<'v> Pattern<'v> {
    fn parse(expr: &'v TargetExpression) -> Option<Self> {
        let raw = expr.pattern.trim();
        let raw = raw.strip_prefix('-').unwrap_or(raw);
        let body = ["@@//", "@//", "//"]
            .iter()
            .find_map(|p| raw.strip_prefix(p))
            .unwrap_or(raw);
        if body.starts_with('@') {
            return None;
        }
        let (package, target) = body.rsplit_once(':').unwrap_or((body, ""));
        let recursive = if package == "..." {
            Some("")
        } else {
            package.strip_suffix("/...")
        };
        let kind = match recursive {
            Some(base) if matches!(target, "" | "all" | "*" | "all-targets") => {
                PatternKind::Recursive(base)
            }
            Some(_) => return None,
            None if matches!(target, "all" | "*" | "all-targets") => PatternKind::Package(package),
            None => {
                let name = if target.is_empty() {
                    package.rsplit('/').next().unwrap_or(package)
                } else {
                    target
                };
                PatternKind::Label(Label { package, name })
            }
        };
        Some(Pattern {
            excluded: expr.excluded || expr.pattern.trim_start().starts_with('-'),
            kind,
        })
    }

    fn covers(&self, label: &Label) -> bool {
        match &self.kind {
            PatternKind::Recursive(base) => is_under(base, label.package),
            PatternKind::Package(package) => *package == label.package,
            PatternKind::Label(l) => l == label,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{JavaIdeInfo, TargetExpression};

    fn expr(pattern: &str, excluded: bool) -> TargetExpression {
        TargetExpression {
            pattern: pattern.into(),
            excluded,
        }
    }

    fn label(s: &str) -> Label<'_> {
        Label::parse(s).unwrap()
    }

    #[test]
    fn parses_labels() {
        assert_eq!(
            label("//a/b:c"),
            Label {
                package: "a/b",
                name: "c"
            }
        );
        assert_eq!(
            label("//a/b"),
            Label {
                package: "a/b",
                name: "b"
            }
        );
        assert_eq!(
            label("@@//a:c"),
            Label {
                package: "a",
                name: "c"
            }
        );
        assert_eq!(
            label("//:root"),
            Label {
                package: "",
                name: "root"
            }
        );
        assert_eq!(Label::parse("@@maven//:guava"), None);
        assert_eq!(Label::parse("@rules_java//x:y"), None);
    }

    #[test]
    fn target_patterns() {
        let covers =
            |p: &str, l: &str| Pattern::parse(&expr(p, false)).is_some_and(|p| p.covers(&label(l)));
        assert!(covers("//a/...", "//a:x"));
        assert!(covers("//a/...", "//a/b/c:x"));
        assert!(!covers("//a/...", "//ab:x"));
        assert!(covers("//...", "//z:x"));
        assert!(covers("//a/...:all", "//a/b:x"));
        assert!(covers("//a:all", "//a:x"));
        assert!(covers("//a:*", "//a:x"));
        assert!(!covers("//a:all", "//a/b:x"));
        assert!(covers("//a:x", "//a:x"));
        assert!(!covers("//a:x", "//a:y"));
        assert!(covers("//a/x", "//a/x:x"));
        assert!(!covers("//a/...:x", "//a:x"));
        assert!(!covers("@maven//:x", "//:x"));
    }

    fn java_target(label: &str, kind: &str, sources: &[&str]) -> TargetIdeInfo {
        TargetIdeInfo {
            key: TargetKey {
                label: label.into(),
                aspect_ids: vec![],
            },
            kind_string: kind.into(),
            java_ide_info: Some(JavaIdeInfo {
                sources: sources
                    .iter()
                    .map(|s| ArtifactLocation {
                        relative_path: (*s).into(),
                        is_source: true,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_targets_outside_directories_are_sources() {
        let roots = ImportRoots {
            includes: vec!["a".into()],
            excludes: vec!["a/gen".into()],
            ..Default::default()
        };
        let view = ProjectView {
            targets: vec![
                expr("//b/...", false),
                expr("//b/no:no", true),
                expr("//a/gen:keep", false),
            ],
            ..Default::default()
        };
        let mut map = TargetMap::new();
        for t in [
            java_target("//a:in", "java_library", &["a/A.java"]),
            java_target("//a/gen:out", "java_library", &["a/gen/A.java"]),
            java_target("//a/gen:keep", "java_library", &["a/gen/K.java"]),
            java_target("//b/yes:yes", "java_library", &["b/yes/A.java"]),
            java_target("//b/no:no", "java_library", &["b/no/A.java"]),
            java_target("//c:c", "java_library", &["c/A.java"]),
            java_target("@@ext//a:a", "java_library", &["a/A.java"]),
        ] {
            map.insert(t.key.clone(), t);
        }
        let c = classify(&map, &view, &roots);
        let sources: Vec<&str> = c
            .source_targets
            .iter()
            .map(|t| t.key.label.as_str())
            .collect();
        assert_eq!(sources, ["//a/gen:keep", "//a:in", "//b/yes:yes"]);
        assert_eq!(c.library_targets.len(), 4);
    }

    #[test]
    fn tags_and_project_view_lists() {
        let roots = ImportRoots {
            includes: vec!["".into()],
            ..Default::default()
        };
        let view = ProjectView {
            exclude_target: vec!["//x:excluded".into()],
            import_target_output: vec!["//x:output".into()],
            ..Default::default()
        };
        let mut tagged = java_target("//x:tagged", "java_library", &["x/T.java"]);
        tagged.tags = vec![TAG_EXCLUDE_TARGET.into()];
        let mut sdk = java_target("//x:sdk", "java_library", &["x/S.java"]);
        sdk.tags = vec![TAG_PROVIDED_BY_SDK.into()];
        let mut as_lib = java_target("//x:as_lib", "java_library", &["x/L.java"]);
        as_lib.tags = vec![TAG_IMPORT_TARGET_OUTPUT.into()];
        let mut legacy = java_target("//x:legacy", "java_library", &["x/G.java"]);
        legacy.tags = vec![TAG_IMPORT_AS_LIBRARY_LEGACY.into()];
        let mut excluded = java_target("//x:excluded", "java_library", &["x/E.java"]);
        excluded.java_ide_info.as_mut().unwrap().jars = vec![LibraryArtifact {
            jar: Some(ArtifactLocation {
                root_execution_path_fragment: "bazel-out/k8/bin".into(),
                relative_path: "x/libexcluded.jar".into(),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let import = java_target("//x:import", "java_import", &["x/I.java"]);
        let generated_only = TargetIdeInfo {
            java_ide_info: Some(JavaIdeInfo {
                sources: vec![ArtifactLocation {
                    relative_path: "x/G.java".into(),
                    is_source: false,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..java_target("//x:generated", "java_library", &[])
        };
        let no_java = TargetIdeInfo {
            java_ide_info: None,
            ..java_target("//x:cc", "cc_library", &[])
        };
        let mut map = TargetMap::new();
        for t in [
            tagged,
            sdk,
            as_lib,
            legacy,
            excluded,
            import,
            generated_only,
            no_java,
            java_target("//x:output", "java_library", &["x/O.java"]),
        ] {
            map.insert(t.key.clone(), t);
        }
        let c = classify(&map, &view, &roots);
        assert!(c.source_targets.is_empty());
        let libs: Vec<&str> = c
            .library_targets
            .iter()
            .map(|t| t.key.label.as_str())
            .collect();
        assert_eq!(
            libs,
            [
                "//x:as_lib",
                "//x:generated",
                "//x:import",
                "//x:legacy",
                "//x:output"
            ]
        );
        assert!(
            c.excluded_jars
                .contains("bazel-out/k8/bin/x/libexcluded.jar")
        );
    }

    #[test]
    fn proto_targets_are_sources_without_java() {
        let roots = ImportRoots {
            includes: vec!["".into()],
            ..Default::default()
        };
        let mut map = TargetMap::new();
        for t in [
            java_target("//p:java_proto", "java_proto_library", &[]),
            java_target("//p:proto", "proto_library", &["p/a.proto"]),
            java_target("//p:lite", "java_lite_proto_library", &[]),
        ] {
            map.insert(t.key.clone(), t);
        }
        let c = classify(&map, &ProjectView::default(), &roots);
        assert_eq!(c.source_targets.len(), 3);
    }
}
