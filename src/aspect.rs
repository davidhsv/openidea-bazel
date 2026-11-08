//! The IntelliJ aspect (bazelbuild/intellij, ijwb branch), shipped with the plugin and written into the workspace.
//!
//! `aspect/` holds the aspect as IntelliJ writes it for Bazel 7, patched so that it needs no tool
//! jars. Four files are templates that IntelliJ realizes per sync (`AspectTemplateWriter.kt`,
//! `CcAspectTemplateWriter.kt`); `aspect/bazel8` and `aspect/bazel9` hold their realizations for
//! newer Bazel versions with the options this port always uses: Java on, Python, Scala and C++ off,
//! `use_get_tool_for_action` on, no code-generator rules.

/// Workspace-relative package the aspect is written to.
pub const ASPECT_DIR: &str = ".openidea/bazel/aspects";

/// `--aspects` value.
pub fn aspect_flag() -> String {
    format!("//{ASPECT_DIR}:intellij_info_bundled.bzl%intellij_info_aspect")
}

/// Files that are the same for every Bazel version.
const COMMON_FILES: [(&str, &str); 8] = [
    ("BUILD.bazel", include_str!("../aspect/BUILD.bazel")),
    ("artifacts.bzl", include_str!("../aspect/artifacts.bzl")),
    ("cc_info.bzl", include_str!("../aspect/cc_info.bzl")),
    (
        "code_generator_info.bzl",
        include_str!("../aspect/code_generator_info.bzl"),
    ),
    (
        "intellij_info_impl_bundled.bzl",
        include_str!("../aspect/intellij_info_impl_bundled.bzl"),
    ),
    (
        "java_classpath.bzl",
        include_str!("../aspect/java_classpath.bzl"),
    ),
    (
        "make_variables.bzl",
        include_str!("../aspect/make_variables.bzl"),
    ),
    ("scala_info.bzl", include_str!("../aspect/scala_info.bzl")),
];

/// `java_info.bzl`: no load before Bazel 8 (`JavaInfo` and `java_common` are built in), rules_java's
/// providers on Bazel 8, rules_java's compatibility proxy from Bazel 9.
const JAVA_INFO_7: &str = include_str!("../aspect/java_info.bzl");
const JAVA_INFO_8: &str = include_str!("../aspect/bazel8/java_info.bzl");
const JAVA_INFO_9: &str = include_str!("../aspect/bazel9/java_info.bzl");

/// `python_info.bzl`: the native `PyInfo` is only looked at before Bazel 8.
const PYTHON_INFO_7: &str = include_str!("../aspect/python_info.bzl");
const PYTHON_INFO_8: &str = include_str!("../aspect/bazel8/python_info.bzl");

/// `intellij_info_bundled.bzl`: from Bazel 9 the aspect also propagates to the C++ toolchain
/// (`toolchains_aspects`).
const INTELLIJ_INFO_7: &str = include_str!("../aspect/intellij_info_bundled.bzl");
const INTELLIJ_INFO_9: &str = include_str!("../aspect/bazel9/intellij_info_bundled.bzl");

/// The aspect's files (name, text) realized for a Bazel major version.
///
/// `release_major` 0 (a development build) is taken as the newest Bazel, like IntelliJ's
/// `BazelVersion.parseVersion`, which reads a release without a version number as `DEVELOPMENT`.
pub fn aspect_files(release_major: u32) -> Vec<(&'static str, String)> {
    let at_least = |major: u32| release_major == 0 || release_major >= major;
    let java_info = if at_least(9) {
        JAVA_INFO_9
    } else if at_least(8) {
        JAVA_INFO_8
    } else {
        JAVA_INFO_7
    };
    let python_info = if at_least(8) {
        PYTHON_INFO_8
    } else {
        PYTHON_INFO_7
    };
    let intellij_info = if at_least(9) {
        INTELLIJ_INFO_9
    } else {
        INTELLIJ_INFO_7
    };

    let mut files: Vec<(&'static str, String)> = COMMON_FILES
        .iter()
        .map(|&(name, text)| (name, text.to_owned()))
        .collect();
    files.push(("intellij_info_bundled.bzl", intellij_info.to_owned()));
    files.push(("java_info.bzl", java_info.to_owned()));
    files.push(("python_info.bzl", python_info.to_owned()));
    files.sort_by_key(|&(name, _)| (name != "BUILD.bazel", name));
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSIONS: [u32; 6] = [6, 7, 8, 9, 10, 0];

    fn file(files: &[(&str, String)], name: &str) -> String {
        files
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} is not shipped"))
            .1
            .clone()
    }

    /// The first argument of every `load(...)`, also when the call spans several lines.
    fn load_labels(text: &str) -> Vec<&str> {
        let mut labels = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("load(") {
            let at_line_start = i == 0 || rest.as_bytes()[i - 1] == b'\n';
            rest = &rest[i + "load(".len()..];
            if !at_line_start {
                continue;
            }
            let arg = rest.trim_start();
            let quote = arg.chars().next().expect("load( without arguments");
            assert!(
                quote == '"' || quote == '\'',
                "load( must start with a string: {}",
                &arg[..arg.len().min(40)]
            );
            let end = arg[1..].find(quote).expect("unterminated load label") + 1;
            labels.push(&arg[1..end]);
        }
        labels
    }

    #[test]
    fn every_local_load_resolves_to_a_shipped_file() {
        for major in VERSIONS {
            let files = aspect_files(major);
            let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
            assert_eq!(names.len(), 11, "{names:?}");
            assert_eq!(names[0], "BUILD.bazel");
            let mut local_loads = 0;
            for (name, text) in &files {
                for label in load_labels(text) {
                    if let Some(local) = label.strip_prefix(':') {
                        assert!(
                            names.contains(&local),
                            "Bazel {major}: {name} loads {label}, which is not shipped"
                        );
                        local_loads += 1;
                    } else {
                        let allowed = ["@bazel_tools//", "@rules_java//", "@compatibility_proxy//"];
                        assert!(
                            allowed.iter().any(|p| label.starts_with(p)),
                            "Bazel {major}: {name} loads {label}"
                        );
                    }
                }
            }
            // intellij_info_bundled → impl; impl → 7 helpers; java_classpath → java_info.
            assert_eq!(local_loads, 9, "Bazel {major}");
        }
    }

    #[test]
    fn the_aspect_entry_point_is_shipped() {
        let (package, entry) = aspect_flag()
            .split_once(':')
            .map(|(p, e)| (p.to_owned(), e.to_owned()))
            .unwrap();
        assert_eq!(package, "//.openidea/bazel/aspects");
        let (file_name, symbol) = entry.split_once('%').unwrap();
        for major in VERSIONS {
            let text = file(&aspect_files(major), file_name);
            assert!(
                text.contains(&format!("\n{symbol} = make_intellij_info_aspect(")),
                "Bazel {major}"
            );
        }
    }

    #[test]
    fn bazel_7_and_older_load_nothing_from_rules_java() {
        for major in [6, 7] {
            let files = aspect_files(major);
            assert_eq!(file(&files, "java_info.bzl"), JAVA_INFO_7);
            assert!(load_labels(&file(&files, "java_info.bzl")).is_empty());
            assert!(file(&files, "python_info.bzl").contains("if PyInfo in target:"));
            assert!(!file(&files, "intellij_info_bundled.bzl").contains("toolchains_aspects"));
        }
    }

    #[test]
    fn bazel_8_loads_java_providers_from_rules_java() {
        let files = aspect_files(8);
        assert_eq!(
            load_labels(&file(&files, "java_info.bzl")),
            [
                "@rules_java//java/common:java_info.bzl",
                "@rules_java//java/common:java_common.bzl"
            ]
        );
        assert!(file(&files, "java_info.bzl").starts_with(
            "# TEMPLATE-INCLUDE-BEGIN\nload(\"@rules_java//java/common:java_info.bzl\", \"JavaInfo\")\n\
             load(\"@rules_java//java/common:java_common.bzl\", \"java_common\")\n# TEMPLATE-INCLUDE-END\n"
        ));
        assert!(!file(&files, "python_info.bzl").contains("PyInfo"));
        assert!(!file(&files, "intellij_info_bundled.bzl").contains("toolchains_aspects"));
    }

    #[test]
    fn bazel_9_and_dev_builds_load_java_providers_from_the_compatibility_proxy() {
        for major in [9, 10, 0] {
            let files = aspect_files(major);
            let java_info = file(&files, "java_info.bzl");
            assert_eq!(
                load_labels(&java_info),
                ["@compatibility_proxy//:proxy.bzl"],
                "Bazel {major}"
            );
            assert!(java_info.contains(
                "load(\"@compatibility_proxy//:proxy.bzl\", \"JavaInfo\", \"java_common\")\n"
            ));
            assert!(!file(&files, "python_info.bzl").contains("PyInfo"));
            assert!(file(&files, "intellij_info_bundled.bzl").contains(
                "intellij_info_aspect = make_intellij_info_aspect(\n    _aspect_impl,\n    semantics,\n    \
                 toolchains_aspects = TOOLCHAIN_TYPE_DEPS,\n)\n"
            ));
        }
    }

    /// `text` with the body of its `index`-th `TEMPLATE-INCLUDE` block replaced.
    fn replace_block(text: &str, index: usize, body: &str) -> String {
        const BEGIN: &str = "# TEMPLATE-INCLUDE-BEGIN\n";
        let mut start = 0;
        for _ in 0..=index {
            start += text[start..]
                .find(BEGIN)
                .expect("missing TEMPLATE-INCLUDE block")
                + BEGIN.len();
        }
        let end = start
            + text[start..]
                .find("# TEMPLATE-INCLUDE-END")
                .expect("unterminated TEMPLATE-INCLUDE block");
        format!("{}{body}{}", &text[..start], &text[end..])
    }

    /// The version variants are the Bazel 7 files with only the templated blocks realized
    /// differently (from the `#if( $bazel8OrAbove ...)` / `$bazel9OrAbove` branches of the templates
    /// `aspect/java_info.bzl`, `aspect/python_info.bzl` and `aspect/intellij_info.bzl` upstream), so
    /// the openidea-bazel patches of the Bazel 7 files are in them too.
    #[test]
    fn variants_differ_from_bazel_7_only_in_their_template_blocks() {
        assert_eq!(
            JAVA_INFO_8,
            replace_block(
                JAVA_INFO_7,
                0,
                "load(\"@rules_java//java/common:java_info.bzl\", \"JavaInfo\")\n\
                 load(\"@rules_java//java/common:java_common.bzl\", \"java_common\")\n"
            )
        );
        assert_eq!(
            JAVA_INFO_9,
            replace_block(
                JAVA_INFO_7,
                0,
                "load(\"@compatibility_proxy//:proxy.bzl\", \"JavaInfo\", \"java_common\")\n"
            )
        );
        assert_eq!(
            PYTHON_INFO_8,
            replace_block(
                &replace_block(PYTHON_INFO_7, 1, "  return False\n"),
                2,
                "  return None\n"
            )
        );
        assert_eq!(
            INTELLIJ_INFO_9,
            replace_block(
                INTELLIJ_INFO_7,
                0,
                "intellij_info_aspect = make_intellij_info_aspect(\n    _aspect_impl,\n    semantics,\n    \
                 toolchains_aspects = TOOLCHAIN_TYPE_DEPS,\n)\n"
            )
        );
        assert!(INTELLIJ_INFO_9.contains("openidea-bazel ships no tool jars"));
    }

    #[test]
    fn file_names_are_unique_and_output_is_stable() {
        for major in VERSIONS {
            let files = aspect_files(major);
            let mut names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
            names.dedup();
            assert_eq!(names.len(), files.len());
            assert_eq!(aspect_files(major), files);
        }
    }
}
