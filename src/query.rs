//! Bazel command lines and their output: `bazel info`, target derivation (`bazel query`) and the sync build.
//!
//! Sources: `base/.../dependencies/BlazeQueryDirectoryToTargetProvider.java`, `base/.../query/BlazeQueryLabelKindParser.java`,
//! `base/.../sync/SyncProjectTargetsHelper.java`, `base/.../dependencies/SourceToTargetFilteringStrategy.java`,
//! `base/.../sync/projectview/TargetExpressionList.java`, `base/.../model/primitives/WildcardTargetPattern.java`,
//! `java/.../java/JavaBlazeRules.java`, `base/.../model/primitives/GenericBlazeRules.java`,
//! `base/.../sync/aspects/BlazeIdeInterfaceAspectsImpl.java`, `base/.../sync/aspects/strategy/AspectStrategy.java`
//! and `base/.../bazel/BazelVersion.java`.

use std::collections::HashSet;

use crate::import_roots::is_ancestor;
use crate::projectview::label_error;
use crate::types::{BazelInfo, ImportRoots, ProjectView};

/// Parses `bazel info` output (`key: value` lines).
///
/// Run `bazel info` with no key (or several): with a single key Bazel prints the bare value. Missing keys
/// stay empty; [`parse_bazel_info_checked`] rejects them.
pub fn parse_bazel_info(output: &str) -> BazelInfo {
    let mut info = BazelInfo::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let field = match key.trim() {
            "execution_root" => &mut info.execution_root,
            "output_base" => &mut info.output_base,
            "bazel-bin" => &mut info.bazel_bin,
            "release" => &mut info.release,
            _ => continue,
        };
        value.trim().clone_into(field);
    }
    info.release_major = release_major(&info.release);
    info
}

/// [`parse_bazel_info`], failing when a key the sync needs is missing.
pub fn parse_bazel_info_checked(output: &str) -> Result<BazelInfo, String> {
    let info = parse_bazel_info(output);
    let missing: Vec<&str> = [
        ("execution_root", &info.execution_root),
        ("output_base", &info.output_base),
        ("bazel-bin", &info.bazel_bin),
    ]
    .into_iter()
    .filter(|(_, value)| value.is_empty())
    .map(|(key, _)| key)
    .collect();
    if missing.is_empty() {
        Ok(info)
    } else {
        Err(format!("`bazel info` output lacks {}", missing.join(", ")))
    }
}

/// `BazelVersion.parseVersion`: the major of the first run of digits and dots (`release 7.7.1` → 7); 0, the
/// development version that IntelliJ treats as the newest, when there is none.
fn release_major(release: &str) -> u32 {
    let Some(start) = release.find(|c: char| c.is_ascii_digit() || c == '.') else {
        return 0;
    };
    let run = &release[start..];
    let run = &run[..run
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(run.len())];
    run.split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(0)
}

/// The query of `BlazeQueryDirectoryToTargetProvider` for the project's directories.
///
/// `attr("tags", "^((?!manual).)*$", //a/...:all + //b/...:all - //x/...:all)`; the root renders as
/// `//...:all` and the `attr` filter is omitted when manual targets are allowed. For Allocadia's project view
/// (root included, every exclude `.bazelignore`d) with data directory `.openidea/bazel` it is
/// `attr("tags", "^((?!manual).)*$", //...:all - //.openidea/bazel/...:all)`.
/// Empty when there are no includes: IntelliJ then derives no targets without querying.
/// Every `query_excludes` entry is subtracted; see [`derive_targets_query_with`].
pub fn derive_targets_query(roots: &ImportRoots, allow_manual_targets: bool) -> String {
    derive_targets_query_with(roots, allow_manual_targets, &mut |_| true)
}

/// [`derive_targets_query`] subtracting only the `query_excludes` for which `exists` (given the
/// workspace-relative directory) is true: Bazel fails on patterns under directories that don't exist.
pub fn derive_targets_query_with(
    roots: &ImportRoots,
    allow_manual_targets: bool,
    exists: &mut dyn FnMut(&str) -> bool,
) -> String {
    if roots.includes.is_empty() {
        return String::new();
    }
    let mut targets = String::new();
    for (i, include) in roots.includes.iter().enumerate() {
        if i > 0 {
            targets.push_str(" + ");
        }
        push_recursive_pattern(&mut targets, include);
    }
    for exclude in &roots.query_excludes {
        if exists(exclude) {
            targets.push_str(" - ");
            push_recursive_pattern(&mut targets, exclude);
        }
    }
    if allow_manual_targets {
        targets
    } else {
        format!("attr(\"tags\", \"^((?!manual).)*$\", {targets})")
    }
}

/// `TargetExpression.allFromPackageRecursive`, quoted when the path has characters the query lexer doesn't
/// accept in a bare word.
fn push_recursive_pattern(out: &mut String, directory: &str) {
    let pattern = if directory.is_empty() {
        "//...:all".to_owned()
    } else {
        format!("//{directory}/...:all")
    };
    let bare = pattern
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "*/@.-_:$~[]".contains(c));
    if bare {
        out.push_str(&pattern);
    } else {
        let quote = if pattern.contains('"') { '\'' } else { '"' };
        out.push(quote);
        out.push_str(&pattern);
        out.push(quote);
    }
}

/// Parses `bazel query --output=label_kind` lines (`<kind> rule <label>`) into (kind, label).
///
/// `BlazeQueryLabelKindParser`: other lines (`source file`, `generated file`, Bazel's stderr) and invalid
/// labels are skipped.
pub fn parse_label_kind(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (kind, label) = line.split_once(' ')?;
            let label = label.strip_prefix("rule ")?;
            let valid =
                !kind.is_empty() && !label.contains(is_regex_space) && label_error(label).is_none();
            valid.then(|| (kind.to_owned(), label.to_owned()))
        })
        .collect()
}

/// Java regex `\s`.
fn is_regex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r')
}

/// `Kind.RuleType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleType {
    Library,
    Binary,
    Test,
    Unknown,
}

/// The rule type of a kind the IDE knows for the Java and generic languages (`JavaBlazeRules`,
/// `GenericBlazeRules`), after `Kind.fromRuleName`'s `_transition_` prefix strip; `None` for any other kind.
pub fn known_rule_type(kind: &str) -> Option<RuleType> {
    use RuleType::*;
    let kind = kind.strip_prefix("_transition_").unwrap_or(kind);
    Some(match kind {
        "java_library"
        | "java_proto_library"
        | "java_lite_proto_library"
        | "java_mutable_proto_library" => Library,
        "java_test" | "java_junit5_test" | "java_test_suite" | "gwt_test" | "java_web_test" => Test,
        "java_binary" => Binary,
        "java_import" | "java_toolchain" | "java_plugin" | "java_wrap_cc" | "gwt_application"
        | "gwt_host" | "gwt_module" => Unknown,
        "proto_library" | "sh_library" => Library,
        "test_suite" | "web_test" | "sh_test" => Test,
        "sh_binary" => Binary,
        "intellij_plugin_debug_target" => Unknown,
        _ => return None,
    })
}

/// The labels to build: derived targets of the Java and generic kinds the IDE knows (filegroups dropped),
/// then the explicit `targets:`, minus `-` targets and `exclude_target`.
///
/// Derived targets are ordered libraries, then tests and binaries, then the rest (`PrioritizeKnownRules`).
/// As in `TargetExpressionList`, an entry covered by a later `-` pattern (the same label, or a package its
/// wildcard covers) is dropped; the `-` patterns themselves stay, `-`prefixed, so Bazel also subtracts what
/// no single entry shows (`//a/...` then `-//a/b:c`), unless no positive pattern precedes them.
/// Labels in `exclude_target` are dropped (IntelliJ only hides them at import; the aspect still reaches
/// them through deps). Duplicates keep their last position, which leaves Bazel's result unchanged.
/// The result is the content of the target pattern file, one per line; empty when there is nothing to build.
pub fn sync_targets(derived: &[(String, String)], view: &ProjectView) -> Vec<String> {
    let mut retained: Vec<(u8, &str)> = derived
        .iter()
        .filter_map(|(kind, label)| {
            known_rule_type(kind).map(|rule_type| (weight(rule_type), label.as_str()))
        })
        .collect();
    retained.sort_by_key(|&(weight, _)| weight);

    let mut entries: Vec<Entry> = retained
        .into_iter()
        .map(|(_, label)| Entry::new(label, false))
        .collect();
    entries.extend(
        view.targets
            .iter()
            .map(|t| Entry::new(&t.pattern, t.excluded)),
    );

    // Reverse scan: the excludes seen so far are the ones later in the list.
    let mut excluded_patterns: HashSet<&str> = HashSet::new();
    let mut excluded_wildcards: Vec<Wildcard> = Vec::new();
    let mut keep = vec![false; entries.len()];
    for (i, entry) in entries.iter().enumerate().rev() {
        if entry.excluded {
            excluded_patterns.insert(entry.pattern);
            excluded_wildcards.extend(entry.wildcard);
            keep[i] = true;
            continue;
        }
        let covered = match entry.wildcard {
            Some(wildcard) => excluded_wildcards
                .iter()
                .any(|e| e.covers_wildcard(wildcard)),
            None => {
                entry.is_label
                    && (excluded_patterns.contains(entry.pattern)
                        || entry.package.is_some_and(|p| {
                            excluded_wildcards.iter().any(|e| e.covers_package(p))
                        }))
            }
        };
        keep[i] = !covered;
    }

    let exclude_target: HashSet<&str> = view.exclude_target.iter().map(String::as_str).collect();
    let kept: Vec<&Entry> = entries
        .iter()
        .zip(keep)
        .filter(|(entry, keep)| {
            *keep && (entry.excluded || !entry.is_label || !exclude_target.contains(entry.pattern))
        })
        .map(|(entry, _)| entry)
        .collect();

    let mut seen: HashSet<(bool, &str)> = HashSet::new();
    let mut deduped: Vec<&Entry> = kept
        .into_iter()
        .rev()
        .filter(|e| seen.insert((e.excluded, e.pattern)))
        .collect();
    deduped.reverse();

    let first_positive = deduped
        .iter()
        .position(|e| !e.excluded)
        .unwrap_or(deduped.len());
    deduped[first_positive..]
        .iter()
        .map(|e| {
            if e.excluded {
                format!("-{}", e.pattern)
            } else {
                e.pattern.to_owned()
            }
        })
        .collect()
}

/// `PrioritizeKnownRules.weight`.
fn weight(rule_type: RuleType) -> u8 {
    match rule_type {
        RuleType::Library => 0,
        RuleType::Binary | RuleType::Test => 1,
        RuleType::Unknown => 2,
    }
}

/// `TargetExpressionList.TargetData`: a target pattern without its `-`.
struct Entry<'a> {
    pattern: &'a str,
    excluded: bool,
    is_label: bool,
    /// The package of a main-repository label (`//pkg:name`).
    package: Option<&'a str>,
    wildcard: Option<Wildcard<'a>>,
}

impl<'a> Entry<'a> {
    fn new(pattern: &'a str, excluded: bool) -> Self {
        let is_label = label_error(pattern).is_none();
        let package = match pattern.strip_prefix("//") {
            Some(rest) if is_label => rest.split_once(':').map(|(package, _)| package),
            _ => None,
        };
        Entry {
            pattern,
            excluded,
            is_label,
            package,
            wildcard: Wildcard::parse(pattern),
        }
    }
}

/// `WildcardTargetPattern` of the main repository: `//pkg:all`, `//pkg:*`, `//pkg:all-targets`, and
/// `//pkg/...` with any of those or no target. External (`@repo//`) patterns never match here.
#[derive(Clone, Copy)]
struct Wildcard<'a> {
    package: &'a str,
    recursive: bool,
}

impl<'a> Wildcard<'a> {
    fn parse(pattern: &'a str) -> Option<Self> {
        let rest = pattern.strip_prefix("//")?;
        let (package, target) = rest.rsplit_once(':').unwrap_or((rest, ""));
        let all = matches!(target, "all" | "*" | "all-targets");
        let recursive_base = if package == "..." {
            Some("")
        } else {
            package.strip_suffix("/...")
        };
        let wildcard = match recursive_base {
            Some(base) if all || target.is_empty() => Wildcard {
                package: base,
                recursive: true,
            },
            Some(_) => return None,
            None if all => Wildcard {
                package,
                recursive: false,
            },
            None => return None,
        };
        let valid = wildcard.package.is_empty()
            || (!wildcard.package.starts_with('/')
                && !wildcard.package.ends_with('/')
                && !wildcard.package.starts_with("../"));
        valid.then_some(wildcard)
    }

    /// `WildcardTargetPattern.coversPackage`.
    fn covers_package(&self, package: &str) -> bool {
        if self.recursive {
            is_ancestor(self.package, package, false)
        } else {
            self.package == package
        }
    }

    /// `TargetData.coversTargetData` for a wildcard entry.
    fn covers_wildcard(&self, other: Wildcard) -> bool {
        (self.recursive || !other.recursive) && self.covers_package(other.package)
    }
}

/// Arguments after `bazel` for the sync build (`build … --aspects=… --output_groups=…`), reading the
/// labels from `target_pattern_file`.
///
/// `BlazeIdeInterfaceAspectsImpl.runBuildForTargets` with `BlazeFlags`; the plugin's own flags come first
/// so `build_flags` and then `sync_flags` can override them. `--experimental_convenience_symlinks=ignore`
/// leaves the user's `bazel-*` symlinks as their own builds set them. The output groups are
/// `AspectStrategy`'s for the RESOLVE and INFO groups of Java and generic, trimmed to direct deps
/// (`-direct-deps`) when `derive_targets_from_directories` is on.
pub fn sync_build_args(
    view: &ProjectView,
    info: &BazelInfo,
    aspect: &str,
    target_pattern_file: &str,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "build",
        "--tool_tag=openidea:sync",
        "--keep_going",
        "--norun_validations",
        "--color=no",
        "--curses=no",
        "--progress_in_terminal_title=no",
        "--experimental_convenience_symlinks=ignore",
    ]
    .map(String::from)
    .into();
    // Bazel 7+ (0 is a development build, treated as the newest).
    if info.release_major >= 7 || info.release_major == 0 {
        args.push("--skip_incompatible_explicit_targets".to_owned());
    }
    args.extend(view.build_flags.iter().cloned());
    args.extend(view.sync_flags.iter().cloned());
    args.push(format!("--aspects={aspect}"));
    let suffix = if view.derive_targets_from_directories {
        "-direct-deps"
    } else {
        ""
    };
    args.push(format!(
        "--output_groups=intellij-resolve-java{suffix},intellij-info-generic,intellij-info-java{suffix}"
    ));
    args.push(format!("--target_pattern_file={target_pattern_file}"));
    args
}

/// Parses Bazel's `[done / total]` progress from a stderr line.
///
/// Counts may use `,` thousands separators (`[1,234 / 5,678] ...`); leading whitespace and ANSI escape
/// sequences (a user's `--color=yes`) are skipped.
pub fn parse_progress(line: &str) -> Option<(u32, u32)> {
    let rest = skip_terminal_noise(line).strip_prefix('[')?;
    let (done, rest) = rest.split_once(" / ")?;
    let (total, _) = rest.split_once(']')?;
    Some((progress_count(done)?, progress_count(total)?))
}

fn skip_terminal_noise(mut line: &str) -> &str {
    loop {
        line = line.trim_start();
        let Some(rest) = line.strip_prefix("\u{1b}[") else {
            return line;
        };
        // CSI: parameter and intermediate bytes, then one final byte in @..~.
        let Some(end) = rest.find(|c: char| ('@'..='~').contains(&c)) else {
            return line;
        };
        line = &rest[end + 1..];
    }
}

fn progress_count(text: &str) -> Option<u32> {
    let mut groups = text.split(',');
    let first = groups.next()?;
    if first.is_empty() || (first.len() > 3 && text.contains(',')) {
        return None;
    }
    let mut value: u32 = parse_digits(first)?;
    for group in groups {
        if group.len() != 3 {
            return None;
        }
        value = value.checked_mul(1000)?.checked_add(parse_digits(group)?)?;
    }
    Some(value)
}

fn parse_digits(digits: &str) -> Option<u32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TargetExpression;

    const OUTPUT_BASE: &str =
        "/private/var/tmp/_bazel_davidvieira/52af286067236843f81b50b2c41f19c6";

    fn bazel_info_output(release: &str) -> String {
        format!(
            "Starting local Bazel server and connecting to it...\n\
             bazel-bin: {OUTPUT_BASE}/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin\n\
             bazel-genfiles: {OUTPUT_BASE}/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin\n\
             execution_root: {OUTPUT_BASE}/execroot/_main\n\
             java-home: /opt/homebrew/Cellar/openjdk/21/libexec/openjdk.jdk/Contents/Home\n\
             output_base: {OUTPUT_BASE}\r\n\
             release: {release}\n\
             starlark-semantics: StarlarkSemantics{{experimental_x=a: b}}\n"
        )
    }

    #[test]
    fn bazel_info() {
        let info = parse_bazel_info(&bazel_info_output("release 7.7.1"));
        assert_eq!(info.execution_root, format!("{OUTPUT_BASE}/execroot/_main"));
        assert_eq!(info.output_base, OUTPUT_BASE);
        assert_eq!(
            info.bazel_bin,
            format!("{OUTPUT_BASE}/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin")
        );
        assert_eq!(info.release, "release 7.7.1");
        assert_eq!(info.release_major, 7);
        assert_eq!(
            parse_bazel_info_checked(&bazel_info_output("release 8.4.2rc1"))
                .unwrap()
                .release_major,
            8
        );
        assert_eq!(
            parse_bazel_info(&bazel_info_output("development version")).release_major,
            0
        );
        assert_eq!(
            parse_bazel_info(&bazel_info_output("release 7.0.0-pre.20231011.2")).release_major,
            7
        );
        assert_eq!(
            parse_bazel_info("execution_root: /a: b\n").execution_root,
            "/a: b"
        );
    }

    #[test]
    fn bazel_info_missing_keys() {
        let info = parse_bazel_info("INFO: Invocation ID: 1234\nexecution_root: /x\n");
        assert_eq!(info.execution_root, "/x");
        assert_eq!(info.release_major, 0);
        assert_eq!(
            parse_bazel_info_checked("execution_root: /x\n").unwrap_err(),
            "`bazel info` output lacks output_base, bazel-bin"
        );
        assert!(parse_bazel_info_checked("").is_err());
    }

    fn roots(includes: &[&str], query_excludes: &[&str]) -> ImportRoots {
        ImportRoots {
            includes: includes.iter().map(|s| (*s).to_owned()).collect(),
            excludes: query_excludes.iter().map(|s| (*s).to_owned()).collect(),
            query_excludes: query_excludes.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn allocadia_query() {
        let view = crate::projectview::parse(
            include_str!("../tests/data/allocadia.bazelproject"),
            ".bazelproject",
            &mut |_| None,
        );
        let import_roots = crate::import_roots::import_roots(
            &view,
            Some(include_str!("../tests/data/allocadia.bazelignore")),
            "Allocadia",
            ".openidea/bazel",
        );
        assert_eq!(
            derive_targets_query(&import_roots, view.allow_manual_targets_sync),
            r#"attr("tags", "^((?!manual).)*$", //...:all - //.openidea/bazel/...:all)"#
        );
    }

    /// `BlazeQueryDirectoryToTargetProviderTest`.
    #[test]
    fn derived_query_strings() {
        let r = roots(&["foo", "bar/baz"], &["foo/gen", "bar/baz/out"]);
        assert_eq!(
            derive_targets_query(&r, false),
            r#"attr("tags", "^((?!manual).)*$", //foo/...:all + //bar/baz/...:all - //foo/gen/...:all - //bar/baz/out/...:all)"#
        );
        assert_eq!(
            derive_targets_query(&r, true),
            "//foo/...:all + //bar/baz/...:all - //foo/gen/...:all - //bar/baz/out/...:all"
        );
        let mut asked = Vec::new();
        let filtered = derive_targets_query_with(&r, true, &mut |path| {
            asked.push(path.to_owned());
            path != "foo/gen"
        });
        assert_eq!(
            filtered,
            "//foo/...:all + //bar/baz/...:all - //bar/baz/out/...:all"
        );
        assert_eq!(asked, ["foo/gen", "bar/baz/out"]);
        assert_eq!(derive_targets_query(&roots(&[""], &[]), true), "//...:all");
        assert_eq!(
            derive_targets_query(&roots(&["my dir"], &["a(b)"]), true),
            r#""//my dir/...:all" - "//a(b)/...:all""#
        );
        assert_eq!(derive_targets_query(&roots(&[], &["x"]), false), "");
    }

    const LABEL_KIND: &str = "Loading: 0 packages loaded\n\
        WARNING: /ws/projects/analytics/usage-metrics/BUILD.bazel:22:8: target 'cacerts' is both a rule and a file; please choose another name for the rule\n\
        _copy_to_bin rule //:README\n\
        filegroup rule //devops/organization-depro:src_ts_typecheck\n\
        java_war rule //projects/uptempo/core-monolith/app:allocadia\n\
        java_test rule //libs/commons/allocadia-cache:allocadia-cache-test\r\n\
        java_binary rule //libs/rosetta:rosetta-generate\n\
        java_import rule //third_party/jacoco:bazel-internal-jacoco-agent\n\
        java_library rule //libs/commons/allocadia-cache:allocadia-cache\n\
        jdo_enhanced_library rule //projects/uptempo/core-monolith/core:core-enhanced\n\
        proto_library rule //projects/uptempo/audit-api/src/main/proto:audit_proto\n\
        sh_binary rule //projects/eng-docsite:check_then_push_dev\n\
        test_suite rule //.aspect/bazelrc:update_aspect_bazelrc_presets_tests\n\
        java_proto_library rule //projects/uptempo/audit-api/src/main/proto:audit_java_proto\n\
        _transition_java_library rule //libs/x:transitioned\n\
        source file //libs/commons/allocadia-cache:BUILD.bazel\n\
        generated file //libs/x:libx.jar\n\
        java_library rule //bad label:x\n\
        java_library rule //no/target\n\
        java_library  rule //double:space\n\
        java_library rule @@rules_jvm_external~~maven~maven//:guava\n\
        INFO: Elapsed time: 1.2s\n";

    #[test]
    fn label_kind_lines() {
        let parsed = parse_label_kind(LABEL_KIND);
        let labels: Vec<&str> = parsed.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(parsed.len(), 14, "{labels:?}");
        assert_eq!(
            parsed[0],
            ("_copy_to_bin".to_owned(), "//:README".to_owned())
        );
        assert_eq!(
            parsed[3],
            (
                "java_test".to_owned(),
                "//libs/commons/allocadia-cache:allocadia-cache-test".to_owned()
            )
        );
        assert_eq!(labels[12], "//libs/x:transitioned");
        assert_eq!(labels[13], "@@rules_jvm_external~~maven~maven//:guava");
        assert!(parse_label_kind("").is_empty());
    }

    fn targets(entries: &[&str]) -> Vec<TargetExpression> {
        entries
            .iter()
            .map(|t| match t.strip_prefix('-') {
                Some(pattern) => TargetExpression {
                    pattern: pattern.to_owned(),
                    excluded: true,
                },
                None => TargetExpression {
                    pattern: (*t).to_owned(),
                    excluded: false,
                },
            })
            .collect()
    }

    #[test]
    fn sync_targets_filters_kinds_and_orders_by_rule_type() {
        let derived = parse_label_kind(LABEL_KIND);
        let synced = sync_targets(&derived, &ProjectView::default());
        assert_eq!(
            synced,
            [
                "//libs/commons/allocadia-cache:allocadia-cache",
                "//projects/uptempo/audit-api/src/main/proto:audit_proto",
                "//projects/uptempo/audit-api/src/main/proto:audit_java_proto",
                "//libs/x:transitioned",
                "@@rules_jvm_external~~maven~maven//:guava",
                "//libs/commons/allocadia-cache:allocadia-cache-test",
                "//libs/rosetta:rosetta-generate",
                "//projects/eng-docsite:check_then_push_dev",
                "//.aspect/bazelrc:update_aspect_bazelrc_presets_tests",
                "//third_party/jacoco:bazel-internal-jacoco-agent",
            ]
        );
    }

    fn derived(labels: &[&str]) -> Vec<(String, String)> {
        labels
            .iter()
            .map(|l| ("java_library".to_owned(), (*l).to_owned()))
            .collect()
    }

    #[test]
    fn explicit_targets_and_exclusions() {
        let view = ProjectView {
            targets: targets(&[
                "//extra:x",
                "-//a:one",
                "-//b/...",
                "//b/keep:k",
                "//c/...",
                "-//c/d:e",
                "//a:one",
            ]),
            exclude_target: vec!["//skip:me".to_owned(), "//c/...".to_owned()],
            ..ProjectView::default()
        };
        let synced = sync_targets(
            &derived(&[
                "//a:one",
                "//a:two",
                "//b:x",
                "//b/c:y",
                "//bb:z",
                "//skip:me",
                "//extra:x",
            ]),
            &view,
        );
        // `//a:one`, `//b:x` and `//b/c:y` are covered by later excludes (`//bb:z` isn't under `b`); `//skip:me`
        // is an exclude_target (which only drops labels); the derived `//extra:x` moves to its explicit position.
        assert_eq!(
            synced,
            [
                "//a:two",
                "//bb:z",
                "//extra:x",
                "-//a:one",
                "-//b/...",
                "//b/keep:k",
                "//c/...",
                "-//c/d:e",
                "//a:one"
            ]
        );
    }

    #[test]
    fn wildcard_exclusions_cover_wildcards() {
        let view = ProjectView {
            targets: targets(&[
                "//x/...",
                "//x/y:all",
                "//z:all",
                "-//x/...:*",
                "//x/y/w:all",
                "-//z/...",
            ]),
            ..ProjectView::default()
        };
        // `//x/...` and `//x/y:all` are covered by `-//x/...:*`; `//z:all` by the later recursive `-//z/...`;
        // `//x/y/w:all` follows the exclude that would cover it and stays.
        assert_eq!(sync_targets(&[], &view), ["//x/y/w:all", "-//z/..."]);

        let root = ProjectView {
            targets: targets(&["//...:all", "-//...", "//a:b", "-//c:d"]),
            ..ProjectView::default()
        };
        assert_eq!(
            sync_targets(&derived(&["//q:r"]), &root),
            ["//a:b", "-//c:d"]
        );

        // A non-recursive exclude doesn't cover a recursive pattern.
        let shallow = ProjectView {
            targets: targets(&["//p/...", "-//p:all"]),
            ..ProjectView::default()
        };
        assert_eq!(sync_targets(&[], &shallow), ["//p/...", "-//p:all"]);
    }

    #[test]
    fn only_excludes_build_nothing() {
        let view = ProjectView {
            targets: targets(&["-//a:b", "-//c/..."]),
            ..ProjectView::default()
        };
        assert!(sync_targets(&[], &view).is_empty());
        assert!(sync_targets(&derived(&["//a:b"]), &view).is_empty());
        assert!(sync_targets(&[], &ProjectView::default()).is_empty());
    }

    #[test]
    fn duplicates_keep_their_last_position() {
        let view = ProjectView {
            targets: targets(&["//a/...", "-//a/b:c", "//a/..."]),
            ..ProjectView::default()
        };
        assert_eq!(
            sync_targets(&derived(&["//d:e", "//d:e"]), &view),
            ["//d:e", "-//a/b:c", "//a/..."]
        );
    }

    #[test]
    fn build_args() {
        let mut view = ProjectView {
            derive_targets_from_directories: true,
            build_flags: vec!["--config=ide".to_owned()],
            sync_flags: vec!["--define=sync=1".to_owned()],
            ..ProjectView::default()
        };
        let info = BazelInfo {
            release_major: 7,
            ..BazelInfo::default()
        };
        let aspect = "//.openidea/bazel/aspects:intellij_info_bundled.bzl%intellij_info_aspect";
        assert_eq!(
            sync_build_args(&view, &info, aspect, "/ws/.openidea/bazel/targets.txt"),
            [
                "build",
                "--tool_tag=openidea:sync",
                "--keep_going",
                "--norun_validations",
                "--color=no",
                "--curses=no",
                "--progress_in_terminal_title=no",
                "--experimental_convenience_symlinks=ignore",
                "--skip_incompatible_explicit_targets",
                "--config=ide",
                "--define=sync=1",
                "--aspects=//.openidea/bazel/aspects:intellij_info_bundled.bzl%intellij_info_aspect",
                "--output_groups=intellij-resolve-java-direct-deps,intellij-info-generic,intellij-info-java-direct-deps",
                "--target_pattern_file=/ws/.openidea/bazel/targets.txt",
            ]
        );

        view.derive_targets_from_directories = false;
        let old = BazelInfo {
            release_major: 6,
            ..BazelInfo::default()
        };
        let args = sync_build_args(&view, &old, "a%b", "t");
        assert!(
            !args
                .iter()
                .any(|a| a == "--skip_incompatible_explicit_targets")
        );
        assert!(args.iter().any(|a| a
            == "--output_groups=intellij-resolve-java,intellij-info-generic,intellij-info-java"));

        let dev = BazelInfo::default();
        assert!(
            sync_build_args(&view, &dev, "a%b", "t")
                .iter()
                .any(|a| a == "--skip_incompatible_explicit_targets")
        );
    }

    #[test]
    fn progress() {
        assert_eq!(
            parse_progress("[1,234 / 5,678] Compiling Java headers //a:b; 3s darwin-sandbox"),
            Some((1234, 5678))
        );
        assert_eq!(
            parse_progress("[0 / 1] [Prepa] BazelWorkspaceStatusAction stable-status.txt"),
            Some((0, 1))
        );
        assert_eq!(
            parse_progress("  [12 / 12] checking cached actions"),
            Some((12, 12))
        );
        assert_eq!(
            parse_progress("\u{1b}[32m[1,000,000 / 2,000,001]\u{1b}[0m 8 actions running"),
            Some((1_000_000, 2_000_001))
        );
        assert_eq!(
            parse_progress("\r\u{1b}[1A\u{1b}[K[7 / 9] Linking"),
            Some((7, 9))
        );
        for line in [
            "INFO: Analyzed 1767 targets (0 packages loaded, 0 targets configured).",
            "[1234/5678] no spaces",
            "[a / b]",
            "[1 / ]",
            "[ / 2]",
            "[1,23 / 4]",
            "[12345,678 / 1]",
            "[1, / 2]",
            "[99,999,999,999 / 1]",
            "[4294967296 / 1]",
            "text [1 / 2]",
            "[1 / 2",
            "",
        ] {
            assert_eq!(parse_progress(line), None, "{line:?}");
        }
    }
}
