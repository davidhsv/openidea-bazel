//! Model tests on target maps shaped like Allocadia's aspect output (`tests/data/info`).

use std::collections::{HashMap, HashSet};

use super::{SyncFiles, build_java_model};
use crate::types::{
    ArtifactLocation, BazelInfo, Dependency, DependencyType, ImportRoots, JavaIdeInfo, JavaModel,
    JavaToolchainIdeInfo, Library, LibraryArtifact, ProjectView, SourceRoot, TargetIdeInfo,
    TargetKey, TargetMap,
};

const BIN: &str = "bazel-out/darwin_arm64-fastbuild/bin";
const WS: &str = "/ws";
const EXECROOT: &str = "/ob/execroot/_main";
const MAVEN: &str = "external/maven";

fn src(rel: &str) -> ArtifactLocation {
    ArtifactLocation {
        relative_path: rel.into(),
        is_source: true,
        ..Default::default()
    }
}

fn out(rel: &str) -> ArtifactLocation {
    ArtifactLocation {
        root_execution_path_fragment: BIN.into(),
        relative_path: rel.into(),
        ..Default::default()
    }
}

fn ext_out(rel: &str) -> ArtifactLocation {
    ArtifactLocation {
        is_external: true,
        ..out(rel)
    }
}

fn abs(rel: &str) -> String {
    format!("{EXECROOT}/{BIN}/{rel}")
}

fn key(label: &str) -> TargetKey {
    TargetKey {
        label: label.into(),
        aspect_ids: vec![],
    }
}

fn jar(
    class: Option<ArtifactLocation>,
    interface: Option<ArtifactLocation>,
    source: Option<ArtifactLocation>,
) -> LibraryArtifact {
    LibraryArtifact {
        jar: class,
        interface_jar: interface,
        source_jars: source.into_iter().collect(),
    }
}

/// `lib<name>.jar`, `lib<name>-ijar.jar`, `lib<name>-src.jar` in `pkg`.
fn bazel_jars(pkg: &str, name: &str) -> LibraryArtifact {
    jar(
        Some(out(&format!("{pkg}/lib{name}.jar"))),
        Some(out(&format!("{pkg}/lib{name}-ijar.jar"))),
        Some(out(&format!("{pkg}/lib{name}-src.jar"))),
    )
}

/// rules_jvm_external's `header_`/`processed_`/`-sources` jars of a Maven artifact directory.
fn maven_jars(dir: &str, file: &str) -> LibraryArtifact {
    jar(
        Some(ext_out(&format!("{MAVEN}/{dir}/processed_{file}.jar"))),
        Some(ext_out(&format!("{MAVEN}/{dir}/header_{file}.jar"))),
        Some(ext_out(&format!("{MAVEN}/{dir}/{file}-sources.jar"))),
    )
}

fn java(label: &str, kind: &str) -> TargetIdeInfo {
    TargetIdeInfo {
        key: key(label),
        kind_string: kind.into(),
        java_ide_info: Some(JavaIdeInfo::default()),
        ..Default::default()
    }
}

fn library(label: &str, kind: &str, sources: &[&str], jars: Vec<LibraryArtifact>) -> TargetIdeInfo {
    let mut t = java(label, kind);
    let j = t.java_ide_info.as_mut().unwrap();
    j.sources = sources.iter().map(|s| src(s)).collect();
    j.jars = jars;
    t
}

fn with_jdeps(mut t: TargetIdeInfo, rel: &str) -> TargetIdeInfo {
    t.java_ide_info.as_mut().unwrap().jdeps = Some(out(rel));
    t
}

fn with_deps(mut t: TargetIdeInfo, compile: &[&str], runtime: &[&str]) -> TargetIdeInfo {
    let dep = |l: &&str, dependency_type| Dependency {
        key: key(l),
        dependency_type,
    };
    t.deps
        .extend(compile.iter().map(|l| dep(l, DependencyType::CompileTime)));
    t.deps
        .extend(runtime.iter().map(|l| dep(l, DependencyType::Runtime)));
    t
}

fn map(targets: Vec<TargetIdeInfo>) -> TargetMap {
    targets.into_iter().map(|t| (t.key.clone(), t)).collect()
}

fn info() -> BazelInfo {
    BazelInfo {
        execution_root: EXECROOT.into(),
        output_base: "/ob".into(),
        ..Default::default()
    }
}

fn whole_workspace() -> ImportRoots {
    ImportRoots {
        includes: vec!["".into()],
        excludes: vec!["bazel-bin".into()],
        ..Default::default()
    }
}

fn allocadia_view() -> ProjectView {
    ProjectView {
        test_sources: vec!["*/src/test/java*".into()],
        ..Default::default()
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/bin/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A jdeps file listing `paths` as EXPLICIT dependencies.
fn jdeps_bytes(paths: &[&str]) -> Vec<u8> {
    fn len_prefixed(field: u8, payload: &[u8], out: &mut Vec<u8>) {
        out.push((field << 3) | 2);
        let mut n = payload.len();
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend_from_slice(payload);
    }
    let mut out = Vec::new();
    for p in paths {
        let mut dep = Vec::new();
        len_prefixed(1, p.as_bytes(), &mut dep);
        dep.extend([2 << 3, 0]);
        len_prefixed(1, &dep, &mut out);
    }
    out
}

#[derive(Default)]
struct FakeFiles {
    workspace: HashMap<String, String>,
    exec: HashMap<String, Vec<u8>>,
    existing: HashSet<String>,
    workspace_reads: Vec<String>,
}

impl FakeFiles {
    fn java(&mut self, path: &str, text: &str) -> &mut Self {
        self.workspace.insert(path.into(), text.into());
        self
    }

    /// Source files declaring the package their `src/*/java` layout implies.
    fn sources(&mut self, paths: &[&str]) -> &mut Self {
        for p in paths {
            let dir = &p[..p.rfind('/').unwrap()];
            let package = dir
                .split_once("/java/")
                .map_or("", |(_, pkg)| pkg)
                .replace('/', ".");
            self.java(p, &format!("package {package};\n\nclass X {{}}\n"));
        }
        self
    }

    fn jdeps(&mut self, rel: &str, bytes: Vec<u8>) -> &mut Self {
        self.exec.insert(format!("{BIN}/{rel}"), bytes);
        self
    }

    fn exist(&mut self, paths: &[String]) -> &mut Self {
        self.existing.extend(paths.iter().cloned());
        self
    }
}

impl SyncFiles for FakeFiles {
    fn read_workspace_file(&mut self, path: &str) -> Option<String> {
        self.workspace_reads.push(path.into());
        self.workspace.get(path).cloned()
    }

    fn read_exec_bytes(&mut self, exec_path: &str) -> Option<Vec<u8>> {
        self.exec.get(exec_path).cloned()
    }

    fn exists(&mut self, abs_path: &str) -> bool {
        self.existing.contains(abs_path)
    }
}

fn build(
    targets: &TargetMap,
    view: &ProjectView,
    roots: &ImportRoots,
    files: &mut FakeFiles,
) -> JavaModel {
    build_java_model(targets, view, roots, &info(), WS, files)
}

fn root(path: &str, prefix: &str, is_test: bool) -> SourceRoot {
    SourceRoot {
        path: path.into(),
        package_prefix: prefix.into(),
        is_test,
    }
}

fn lib(jar: String, source_jar: Option<String>, test_only: bool) -> Library {
    Library {
        jar,
        source_jar,
        test_only,
    }
}

fn jars(model: &JavaModel) -> Vec<&str> {
    model.libraries.iter().map(|l| l.jar.as_str()).collect()
}

const UTIL: &str = "libs/commons/allocadia-util";
const GUAVA: (&str, &str) = ("com/google/guava/guava/33.7.2-jre", "guava-33.7.2-jre");
const LANG3: (&str, &str) = (
    "org/apache/commons/commons-lang3/3.20.0",
    "commons-lang3-3.20.0",
);
const SPRING_CORE: (&str, &str) = ("org/springframework/spring-core/7.0.9", "spring-core-7.0.9");
const SPRING_WEB: (&str, &str) = ("org/springframework/spring-web/7.0.9", "spring-web-7.0.9");
const JUNIT: (&str, &str) = ("junit/junit/4.13.2", "junit-4.13.2");
const HAMCREST: (&str, &str) = ("org/hamcrest/hamcrest/2.2", "hamcrest-2.2");

fn maven_path(m: (&str, &str), prefix: &str, suffix: &str) -> String {
    abs(&format!("{MAVEN}/{}/{prefix}{}{suffix}.jar", m.0, m.1))
}

fn maven_exec(m: (&str, &str), prefix: &str) -> String {
    format!("{BIN}/{MAVEN}/{}/{prefix}{}.jar", m.0, m.1)
}

/// allocadia-util (`allocadia-util--1755630977.intellij-info.txt`), its test library and the
/// `AsyncTest` java_test that only has runtime deps, with their Maven jars.
fn allocadia_util() -> TargetMap {
    let util_sources = [
        "libs/commons/allocadia-util/src/main/java/com/allocadia/util/Async.java",
        "libs/commons/allocadia-util/src/main/java/com/allocadia/util/ThreadClock.java",
        "libs/commons/allocadia-util/src/main/java/com/allocadia/util/impl/ThreadClockImpl.java",
    ];
    let util = library(
        "//libs/commons/allocadia-util:allocadia-util",
        "java_library",
        &util_sources,
        vec![bazel_jars(UTIL, "allocadia-util")],
    );
    let util = with_deps(
        with_jdeps(util, &format!("{UTIL}/liballocadia-util.jdeps")),
        &[
            "@@rules_java~//toolchains:current_java_toolchain",
            "//libs/commons/allocadia-logging:allocadia-logging",
            "@@maven//:com_google_guava_guava",
            "@@maven//:org_apache_commons_commons_lang3",
        ],
        &["@@maven//:software_amazon_awssdk_sso"],
    );
    let tests = library(
        "//libs/commons/allocadia-util:tests",
        "java_library",
        &["libs/commons/allocadia-util/src/test/java/com/allocadia/util/AsyncTest.java"],
        vec![bazel_jars(UTIL, "tests")],
    );
    let tests = with_deps(
        with_jdeps(tests, &format!("{UTIL}/libtests.jdeps")),
        &[
            "//libs/commons/allocadia-util:allocadia-util",
            "@@maven//:junit_junit",
            "@@maven//:com_google_guava_guava",
        ],
        &[],
    );
    let async_test = with_deps(
        library(
            "//libs/commons/allocadia-util:com.allocadia.util.AsyncTest",
            "java_test",
            &[],
            vec![jar(
                Some(out(&format!("{UTIL}/com.allocadia.util.AsyncTest.jar"))),
                None,
                None,
            )],
        ),
        &["@@rules_java~//toolchains:current_java_toolchain"],
        &["//libs/commons/allocadia-util:tests"],
    );
    let mut toolchain = TargetIdeInfo {
        key: key("@@rules_java~//toolchains:current_java_toolchain"),
        kind_string: "java_toolchain_alias".into(),
        ..Default::default()
    };
    toolchain.java_toolchain_ide_info = Some(JavaToolchainIdeInfo {
        source_version: "17".into(),
        target_version: "17".into(),
    });
    map(vec![
        util,
        tests,
        async_test,
        toolchain,
        library(
            "@@maven//:com_google_guava_guava",
            "jvm_import",
            &[],
            vec![maven_jars(GUAVA.0, GUAVA.1)],
        ),
        library(
            "@@maven//:org_apache_commons_commons_lang3",
            "jvm_import",
            &[],
            vec![maven_jars(LANG3.0, LANG3.1)],
        ),
        library(
            "@@maven//:junit_junit",
            "jvm_import",
            &[],
            vec![maven_jars(JUNIT.0, JUNIT.1)],
        ),
        library(
            "@@maven//:software_amazon_awssdk_sso",
            "jvm_import",
            &[],
            vec![maven_jars(
                "software/amazon/awssdk/sso/2.22.12",
                "sso-2.22.12",
            )],
        ),
    ])
}

fn allocadia_util_files() -> FakeFiles {
    let mut files = FakeFiles::default();
    files
        .sources(&[
            "libs/commons/allocadia-util/src/main/java/com/allocadia/util/Async.java",
            "libs/commons/allocadia-util/src/main/java/com/allocadia/util/ThreadClock.java",
            "libs/commons/allocadia-util/src/main/java/com/allocadia/util/impl/ThreadClockImpl.java",
            "libs/commons/allocadia-util/src/test/java/com/allocadia/util/AsyncTest.java",
        ])
        .jdeps(&format!("{UTIL}/liballocadia-util.jdeps"), fixture("liballocadia-util.jdeps"))
        .jdeps(
            &format!("{UTIL}/libtests.jdeps"),
            jdeps_bytes(&[
                &format!("{BIN}/{UTIL}/liballocadia-util-ijar.jar"),
                &maven_exec(JUNIT, "header_"),
                &maven_exec(HAMCREST, "header_"),
                &maven_exec(GUAVA, "header_"),
            ]),
        )
        .exist(&[
            maven_path(GUAVA, "processed_", ""),
            maven_path(LANG3, "header_", ""),
            maven_path(SPRING_CORE, "processed_", ""),
            maven_path(SPRING_CORE, "", "-sources"),
            maven_path(SPRING_WEB, "header_", ""),
            maven_path(JUNIT, "processed_", ""),
            maven_path(HAMCREST, "processed_", ""),
        ]);
    files
}

#[test]
fn allocadia_util_source_folders_and_test_flags() {
    let mut files = allocadia_util_files();
    let model = build(
        &allocadia_util(),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(
        model.source_roots,
        [
            root("libs/commons/allocadia-util/src/main/java", "", false),
            root("libs/commons/allocadia-util/src/test/java", "", true),
        ]
    );
    // One read per source directory: the first file of util/, util/impl/ and the test directory.
    assert_eq!(
        files.workspace_reads,
        [
            "libs/commons/allocadia-util/src/main/java/com/allocadia/util/Async.java",
            "libs/commons/allocadia-util/src/main/java/com/allocadia/util/impl/ThreadClockImpl.java",
            "libs/commons/allocadia-util/src/test/java/com/allocadia/util/AsyncTest.java",
        ]
    );
    assert_eq!(model.language_level, 17);
    assert_eq!(model.workspace_root, WS);
    assert_eq!(model.excludes, ["bazel-bin"]);
    assert!(model.warnings.is_empty(), "{:?}", model.warnings);
}

#[test]
fn allocadia_util_libraries_order_jars_and_test_only() {
    let mut files = allocadia_util_files();
    let model = build(
        &allocadia_util(),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(
        model.libraries,
        [
            // jdeps-only jars (no target in the map): processed_ sibling and sources when they exist.
            lib(
                maven_path(SPRING_CORE, "processed_", ""),
                Some(maven_path(SPRING_CORE, "", "-sources")),
                false
            ),
            lib(maven_path(SPRING_WEB, "header_", ""), None, false),
            lib(maven_path(HAMCREST, "processed_", ""), None, true),
            // Also direct deps, so they move to the direct-deps pass.
            lib(
                maven_path(GUAVA, "processed_", ""),
                Some(maven_path(GUAVA, "", "-sources")),
                false
            ),
            lib(
                maven_path(LANG3, "header_", ""),
                Some(maven_path(LANG3, "", "-sources")),
                false
            ),
            lib(
                maven_path(JUNIT, "processed_", ""),
                Some(maven_path(JUNIT, "", "-sources")),
                true
            ),
        ]
    );
    let all = jars(&model).join("\n");
    // Source targets' own jars, runtime-only deps and the sourceless java_test stay off the classpath.
    assert!(!all.contains("allocadia-util/lib"), "{all}");
    assert!(!all.contains("sso"), "{all}");
    assert!(!all.contains("AsyncTest"), "{all}");
}

#[test]
fn excluded_libraries_globs_drop_jars() {
    let mut files = allocadia_util_files();
    let view = ProjectView {
        excluded_libraries: vec!["*guava*".into(), "*/spring-web/*".into()],
        ..allocadia_view()
    };
    let model = build(&allocadia_util(), &view, &whole_workspace(), &mut files);
    let all = jars(&model).join("\n");
    assert!(
        !all.contains("guava") && !all.contains("spring-web"),
        "{all}"
    );
    assert_eq!(model.libraries.len(), 4);
}

#[test]
fn unreadable_and_malformed_jdeps_are_warnings() {
    let mut files = allocadia_util_files();
    files
        .exec
        .remove(&format!("{BIN}/{UTIL}/liballocadia-util.jdeps"));
    files.jdeps(&format!("{UTIL}/libtests.jdeps"), vec![0x0A, 0x7F]);
    let model = build(
        &allocadia_util(),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(model.warnings.len(), 2, "{:?}", model.warnings);
    assert!(
        model.warnings[0].contains("liballocadia-util.jdeps")
            && model.warnings[0].contains("not found")
    );
    assert!(model.warnings[1].contains("libtests.jdeps"));
    // Direct deps still make it.
    assert_eq!(
        jars(&model),
        [
            maven_path(GUAVA, "processed_", ""),
            maven_path(LANG3, "header_", ""),
            maven_path(JUNIT, "processed_", "")
        ]
    );
}

#[test]
fn missing_jars_are_kept_and_reported_once() {
    let mut files = allocadia_util_files();
    files.existing.clear();
    let model = build(
        &allocadia_util(),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    // Without any jar on disk the interface jars IntelliJ would index are used.
    assert_eq!(model.libraries[3].jar, maven_path(GUAVA, "header_", ""));
    assert_eq!(model.libraries.len(), 6);
    assert_eq!(model.warnings.len(), 1, "{:?}", model.warnings);
    assert!(
        model.warnings[0].starts_with("6 library jars do not exist"),
        "{}",
        model.warnings[0]
    );
}

const CORE: &str = "projects/uptempo/core-monolith/core";

/// `core-woven-918125081.intellij-info.txt`: an `_aspectj_weave` jar with the classes of `core`.
fn woven() -> TargetMap {
    let core = library(
        "//projects/uptempo/core-monolith/core:core",
        "java_library",
        &["projects/uptempo/core-monolith/core/src/main/java/com/allocadia/Core.java"],
        vec![bazel_jars(CORE, "core")],
    );
    let woven_jar = out(&format!("{CORE}/core-woven.jar"));
    let woven = with_deps(
        library(
            "//projects/uptempo/core-monolith/core:core-woven",
            "_aspectj_weave",
            &[],
            vec![jar(Some(woven_jar.clone()), Some(woven_jar), None)],
        ),
        &[],
        &[
            "//projects/uptempo/core-monolith/core:core",
            "@@maven//:org_aspectj_aspectjrt",
        ],
    );
    let app = with_deps(
        with_jdeps(
            library(
                "//projects/uptempo/core-monolith/app:app",
                "java_library",
                &["projects/uptempo/core-monolith/app/src/main/java/com/allocadia/app/App.java"],
                vec![bazel_jars("projects/uptempo/core-monolith/app", "app")],
            ),
            "projects/uptempo/core-monolith/app/libapp.jdeps",
        ),
        &[
            "//projects/uptempo/core-monolith/core:core",
            "//projects/uptempo/core-monolith/core:core-woven",
        ],
        &[],
    );
    map(vec![core, woven, app])
}

#[test]
fn woven_jar_is_one_library_and_source_jars_are_not() {
    let mut files = FakeFiles::default();
    files
        .sources(&[
            "projects/uptempo/core-monolith/core/src/main/java/com/allocadia/Core.java",
            "projects/uptempo/core-monolith/app/src/main/java/com/allocadia/app/App.java",
        ])
        .jdeps(
            "projects/uptempo/core-monolith/app/libapp.jdeps",
            jdeps_bytes(&[
                &format!("{BIN}/{CORE}/libcore-ijar.jar"),
                &format!("{BIN}/{CORE}/core-woven.jar"),
            ]),
        )
        .exist(&[abs(&format!("{CORE}/core-woven.jar"))]);
    let model = build(&woven(), &allocadia_view(), &whole_workspace(), &mut files);
    assert_eq!(
        model.libraries,
        [lib(abs(&format!("{CORE}/core-woven.jar")), None, false)]
    );
    assert_eq!(
        model.source_roots,
        [
            root(
                "projects/uptempo/core-monolith/app/src/main/java",
                "",
                false
            ),
            root(
                "projects/uptempo/core-monolith/core/src/main/java",
                "",
                false
            ),
        ]
    );
}

const CORE_MODEL: &str = "projects/uptempo/core-monolith/core-model";

/// `dto-99807.intellij-info.txt`: a java_library whose only source is a generated srcjar.
fn dto() -> TargetMap {
    let mut dto = library(
        "//projects/uptempo/core-monolith/core-model:dto",
        "java_library",
        &[],
        vec![bazel_jars(CORE_MODEL, "dto")],
    );
    dto.java_ide_info.as_mut().unwrap().sources = vec![out(&format!(
        "{CORE_MODEL}/dto.generated-java_generated.srcjar"
    ))];
    let model = with_deps(
        library(
            "//projects/uptempo/core-monolith/core-model:core-model",
            "java_library",
            &[
                "projects/uptempo/core-monolith/core-model/src/main/java/com/allocadia/model/Plan.java",
            ],
            vec![bazel_jars(CORE_MODEL, "core-model")],
        ),
        &["//projects/uptempo/core-monolith/core-model:dto"],
        &[],
    );
    map(vec![dto, model])
}

#[test]
fn srcjar_only_target_is_a_library() {
    let mut files = FakeFiles::default();
    files.sources(&[
        "projects/uptempo/core-monolith/core-model/src/main/java/com/allocadia/model/Plan.java",
    ]);
    files.exist(&[abs(&format!("{CORE_MODEL}/libdto.jar"))]);
    let model = build(&dto(), &allocadia_view(), &whole_workspace(), &mut files);
    assert_eq!(
        model.libraries,
        [lib(
            abs(&format!("{CORE_MODEL}/libdto.jar")),
            Some(abs(&format!("{CORE_MODEL}/libdto-src.jar"))),
            false
        )]
    );
    // Without the class jar on disk, the interface jar.
    files.existing.clear();
    files.exist(&[abs(&format!("{CORE_MODEL}/libdto-ijar.jar"))]);
    let model = build(&dto(), &allocadia_view(), &whole_workspace(), &mut files);
    assert_eq!(
        jars(&model),
        [abs(&format!("{CORE_MODEL}/libdto-ijar.jar"))]
    );
    assert!(model.warnings.is_empty(), "{:?}", model.warnings);
}

const KPI: &str =
    "projects/uptempo/performance-management/src/main/java/io/uptempo/perf/domain/kpi";
const DOMAIN: &str = "projects/uptempo/performance-management/src/main/java/io/uptempo/perf/domain";

/// `kpi-106404.intellij-info.txt`: Lombok makes a `-gen.jar`; its jdeps name the `domain` source target.
fn kpi() -> TargetMap {
    let mut kpi = library(
        &format!("//{KPI}:kpi"),
        "java_library",
        &[
            &format!("{KPI}/Kpi.java"),
            &format!("{KPI}/KpiService.java"),
        ],
        vec![bazel_jars(KPI, "kpi")],
    );
    let j = kpi.java_ide_info.as_mut().unwrap();
    j.generated_jars = vec![jar(
        Some(out(&format!("{KPI}/libkpi-gen.jar"))),
        None,
        Some(out(&format!("{KPI}/libkpi-gensrc.jar"))),
    )];
    j.plugin_processor_jars = vec![jar(
        Some(ext_out(&format!(
            "{MAVEN}/org/projectlombok/lombok/1.18.48/processed_lombok-1.18.48.jar"
        ))),
        None,
        None,
    )];
    let kpi = with_deps(
        with_jdeps(kpi, &format!("{KPI}/libkpi.jdeps")),
        &[
            &format!("//{DOMAIN}:domain"),
            "//tools/java/lombok:lombok",
            "@@maven//:org_springframework_spring_context",
        ],
        &[],
    );
    let domain = library(
        &format!("//{DOMAIN}:domain"),
        "java_library",
        &[&format!("{DOMAIN}/Entity.java")],
        vec![bazel_jars(DOMAIN, "domain")],
    );
    map(vec![kpi, domain])
}

#[test]
fn generated_jars_follow_dependency_jars() {
    let lombok = ("org/projectlombok/lombok/1.18.48", "lombok-1.18.48");
    let context = (
        "org/springframework/spring-context/7.0.9",
        "spring-context-7.0.9",
    );
    let mut files = FakeFiles::default();
    files
        .sources(&[&format!("{KPI}/Kpi.java"), &format!("{DOMAIN}/Entity.java")])
        .jdeps(&format!("{KPI}/libkpi.jdeps"), fixture("libkpi.jdeps"))
        .exist(&[
            maven_path(lombok, "processed_", ""),
            abs(&format!("{KPI}/libkpi-gen.jar")),
        ]);
    let model = build(&kpi(), &allocadia_view(), &whole_workspace(), &mut files);
    assert_eq!(
        model.libraries,
        [
            lib(maven_path(lombok, "processed_", ""), None, false),
            lib(maven_path(context, "header_", ""), None, false),
            lib(maven_path(SPRING_CORE, "header_", ""), None, false),
            lib(
                abs(&format!("{KPI}/libkpi-gen.jar")),
                Some(abs(&format!("{KPI}/libkpi-gensrc.jar"))),
                false
            ),
        ]
    );
    assert_eq!(
        model.source_roots,
        [root(
            "projects/uptempo/performance-management/src/main/java",
            "",
            false
        )]
    );
    assert_eq!(model.warnings.len(), 1, "{:?}", model.warnings);
    assert!(model.warnings[0].starts_with("2 library jars do not exist"));
}

const AUDIT: &str = "projects/uptempo/audit-api/src/main/proto";
const PROTO_ASPECT: &str =
    "@@_builtins//:common/java/proto/java_proto_library.bzl%bazel_java_proto_aspect";

fn proto_key(label: &str) -> TargetKey {
    TargetKey {
        label: label.into(),
        aspect_ids: vec![PROTO_ASPECT.into()],
    }
}

/// `audit_java_proto` / `audit_proto`: the java_proto_library forwards to the aspect-carrying
/// proto_library, which holds the jars.
fn protos() -> TargetMap {
    let mut java_proto = java(&format!("//{AUDIT}:audit_java_proto"), "java_proto_library");
    java_proto.deps = vec![Dependency {
        key: proto_key(&format!("//{AUDIT}:audit_proto")),
        dependency_type: DependencyType::CompileTime,
    }];
    let mut proto = library(
        &format!("//{AUDIT}:audit_proto"),
        "proto_library",
        &[&format!("{AUDIT}/audit_service.proto")],
        vec![jar(
            Some(out(&format!("{AUDIT}/libaudit_proto-speed.jar"))),
            Some(out(&format!("{AUDIT}/libaudit_proto-speed-ijar.jar"))),
            Some(out(&format!("{AUDIT}/audit_proto-speed-src.jar"))),
        )],
    );
    proto.key = proto_key(&format!("//{AUDIT}:audit_proto"));
    // The same shape from a repository outside the project's directories.
    let mut ext_java_proto = java("//third/proto:x_java_proto", "java_proto_library");
    ext_java_proto.deps = vec![Dependency {
        key: proto_key("//third/proto:x_proto"),
        dependency_type: DependencyType::CompileTime,
    }];
    let mut ext_proto = library(
        "//third/proto:x_proto",
        "proto_library",
        &["third/proto/x.proto"],
        vec![bazel_jars("third/proto", "x_proto-speed")],
    );
    ext_proto.key = proto_key("//third/proto:x_proto");
    let api = with_deps(
        library(
            "//projects/uptempo/audit-api:api",
            "java_library",
            &["projects/uptempo/audit-api/src/main/java/com/allocadia/audit/Api.java"],
            vec![bazel_jars("projects/uptempo/audit-api", "api")],
        ),
        &[
            &format!("//{AUDIT}:audit_java_proto"),
            "//third/proto:x_java_proto",
        ],
        &[],
    );
    map(vec![java_proto, proto, ext_java_proto, ext_proto, api])
}

#[test]
fn proto_libraries() {
    let mut files = FakeFiles::default();
    files.sources(&["projects/uptempo/audit-api/src/main/java/com/allocadia/audit/Api.java"]);
    files.exist(&[
        abs("third/proto/libx_proto-speed.jar"),
        abs(&format!("{AUDIT}/libaudit_proto-speed.jar")),
    ]);
    let roots = ImportRoots {
        includes: vec!["projects".into()],
        ..Default::default()
    };
    let model = build(&protos(), &allocadia_view(), &roots, &mut files);
    assert_eq!(
        model.libraries,
        [
            lib(
                abs("third/proto/libx_proto-speed.jar"),
                Some(abs("third/proto/libx_proto-speed-src.jar")),
                false
            ),
            lib(
                abs(&format!("{AUDIT}/libaudit_proto-speed.jar")),
                Some(abs(&format!("{AUDIT}/audit_proto-speed-src.jar"))),
                false
            ),
        ]
    );
    assert_eq!(
        model.source_roots,
        [root("projects/uptempo/audit-api/src/main/java", "", false)]
    );
}

#[test]
fn own_jar_covers_generated_sources_without_jar_filter() {
    let mut mixed = library(
        "//m:m",
        "java_library",
        &["m/src/main/java/com/m/A.java"],
        vec![bazel_jars("m", "m")],
    );
    mixed
        .java_ide_info
        .as_mut()
        .unwrap()
        .sources
        .push(out("m/gen.srcjar"));
    let mut filtered = library(
        "//n:n",
        "java_library",
        &["n/src/main/java/com/n/A.java"],
        vec![bazel_jars("n", "n")],
    );
    let j = filtered.java_ide_info.as_mut().unwrap();
    j.sources.push(out("n/src/main/java/com/n/Gen.java"));
    j.filtered_gen_jar = Some(jar(Some(out("n/libn-filtered-gen.jar")), None, None));
    let mut files = FakeFiles::default();
    files.sources(&[
        "m/src/main/java/com/m/A.java",
        "n/src/main/java/com/n/A.java",
    ]);
    files.exist(&[abs("m/libm.jar"), abs("n/libn-filtered-gen.jar")]);
    let model = build(
        &map(vec![mixed, filtered]),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(
        model.libraries,
        [
            lib(abs("m/libm.jar"), Some(abs("m/libm-src.jar")), false),
            lib(abs("n/libn-filtered-gen.jar"), None, false)
        ]
    );
    // Generated sources are not source folders.
    assert_eq!(
        model.source_roots,
        [
            root("m/src/main/java", "", false),
            root("n/src/main/java", "", false)
        ]
    );
}

#[test]
fn excluded_and_output_imported_targets() {
    let mut targets = woven();
    let core = targets
        .get_mut(&key("//projects/uptempo/core-monolith/core:core"))
        .unwrap();
    core.tags.push("intellij-import-target-output".into());
    let woven = targets
        .get_mut(&key("//projects/uptempo/core-monolith/core:core-woven"))
        .unwrap();
    woven.tags.push("intellij-exclude-target".into());
    let mut files = FakeFiles::default();
    files
        .sources(&["projects/uptempo/core-monolith/app/src/main/java/com/allocadia/app/App.java"])
        .jdeps(
            "projects/uptempo/core-monolith/app/libapp.jdeps",
            jdeps_bytes(&[
                &format!("{BIN}/{CORE}/libcore-ijar.jar"),
                &format!("{BIN}/{CORE}/core-woven.jar"),
            ]),
        )
        .exist(&[abs(&format!("{CORE}/libcore.jar"))]);
    let model = build(&targets, &allocadia_view(), &whole_workspace(), &mut files);
    // core is imported as a library (its jar, moved to the direct-deps pass); the excluded woven
    // jar is neither a jdeps library nor a direct-dep one.
    assert_eq!(
        model.libraries,
        [lib(
            abs(&format!("{CORE}/libcore.jar")),
            Some(abs(&format!("{CORE}/libcore-src.jar"))),
            false
        )]
    );
    assert_eq!(
        model.source_roots,
        [root(
            "projects/uptempo/core-monolith/app/src/main/java",
            "",
            false
        )]
    );

    let view = ProjectView {
        exclude_target: vec!["//projects/uptempo/core-monolith/core:core".into()],
        ..allocadia_view()
    };
    let model = build(&targets, &view, &whole_workspace(), &mut files);
    assert!(model.libraries.is_empty(), "{:?}", model.libraries);
}

#[test]
fn test_only_follows_the_folders_of_the_pulling_targets() {
    let junit = library(
        "@@maven//:junit_junit",
        "jvm_import",
        &[],
        vec![maven_jars(JUNIT.0, JUNIT.1)],
    );
    // A java_test whose sources sit in a main folder: its deps must stay visible to that folder.
    let misplaced = with_deps(
        library(
            "//tools/rules/war:test",
            "java_test",
            &["tools/rules/war/TomcatBootstrapTest.java"],
            vec![],
        ),
        &["@@maven//:junit_junit"],
        &[],
    );
    let mut files = FakeFiles::default();
    files.java(
        "tools/rules/war/TomcatBootstrapTest.java",
        "package com.allocadia.tools.war;",
    );
    let targets = map(vec![junit.clone(), misplaced]);
    let model = build(&targets, &allocadia_view(), &whole_workspace(), &mut files);
    assert_eq!(
        model.source_roots,
        [root("tools/rules", "com.allocadia.tools", false)]
    );
    assert!(!model.libraries[0].test_only);

    // A test rule with no sources in the project's folders is judged by its kind.
    let hidden = with_deps(
        library("//y:t", "java_test", &["y/hidden/T.java"], vec![]),
        &["@@maven//:junit_junit"],
        &[],
    );
    let roots = ImportRoots {
        includes: vec!["".into()],
        excludes: vec!["y/hidden".into()],
        ..Default::default()
    };
    let model = build(
        &map(vec![junit, hidden]),
        &allocadia_view(),
        &roots,
        &mut files,
    );
    assert!(model.libraries[0].test_only);
    assert_eq!(model.source_roots, [root("", "", false)]);
}

#[test]
fn nested_test_directory_is_a_warning() {
    let t = library(
        "//tools/rules/war:war",
        "java_library",
        &["tools/rules/war/TomcatBootstrap.java"],
        vec![],
    );
    let mut files = FakeFiles::default();
    files.java(
        "tools/rules/war/TomcatBootstrap.java",
        "package com.allocadia.tools.war;",
    );
    let view = ProjectView {
        test_sources: vec!["tools/rules/war/".into()],
        ..Default::default()
    };
    let model = build(&map(vec![t]), &view, &whole_workspace(), &mut files);
    assert_eq!(
        model.source_roots,
        [root("tools/rules", "com.allocadia.tools", false)]
    );
    assert_eq!(model.warnings.len(), 1, "{:?}", model.warnings);
    assert!(model.warnings[0].starts_with("tools/rules/war matches test_sources"));
}

#[test]
fn first_file_decides_a_directory_with_conflicting_packages() {
    let t = library(
        "//a:a",
        "java_library",
        &[
            "a/src/main/java/com/x/B.java",
            "a/src/main/java/com/x/A.java",
            "a/src/main/java/com/x/C.java",
        ],
        vec![],
    );
    let mut files = FakeFiles::default();
    files
        .java(
            "a/src/main/java/com/x/A.java",
            "/* header */\npackage org.other.x;",
        )
        .java("a/src/main/java/com/x/B.java", "package com.x;")
        .java("a/src/main/java/com/x/C.java", "package com.x;");
    let model = build(
        &map(vec![t]),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(files.workspace_reads, ["a/src/main/java/com/x/A.java"]);
    assert_eq!(
        model.source_roots,
        [root("a/src/main/java/com", "org.other", false)]
    );
}

#[test]
fn file_without_package_uses_its_path() {
    let t = library(
        "//a:a",
        "java_library",
        &[
            "a/src/main/java/com/x/Empty.java",
            "a/src/main/java/com/x/Real.java",
            "a/src/main/java/module-info.java",
        ],
        vec![],
    );
    let mut files = FakeFiles::default();
    files
        .java("a/src/main/java/com/x/Empty.java", "")
        .java("a/src/main/java/com/x/Real.java", "package com.x;")
        .java("a/src/main/java/module-info.java", "module a {}");
    let model = build(
        &map(vec![t]),
        &allocadia_view(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(model.source_roots, [root("a/src/main/java", "", false)]);
    assert_eq!(
        model.warnings,
        [
            "no package declaration found in a/src/main/java/com/x/Empty.java; using the package its path implies"
        ]
    );

    let t = library(
        "//a:a",
        "java_library",
        &["a/src/main/java/com/x/Gone.java"],
        vec![],
    );
    let model = build(
        &map(vec![t]),
        &allocadia_view(),
        &whole_workspace(),
        &mut FakeFiles::default(),
    );
    assert_eq!(model.source_roots, [root("a/src/main/java", "", false)]);
    assert_eq!(
        model.warnings,
        ["cannot read a/src/main/java/com/x/Gone.java; using the package its path implies"]
    );
}

#[test]
fn no_sources_make_each_content_root_a_folder() {
    let roots = ImportRoots {
        includes: vec!["docs".into(), "tools/java".into()],
        ..Default::default()
    };
    let model = build(
        &TargetMap::new(),
        &allocadia_view(),
        &roots,
        &mut FakeFiles::default(),
    );
    assert_eq!(
        model.source_roots,
        [root("docs", "docs", false), root("tools/java", "", false)]
    );
    assert!(model.libraries.is_empty());
    assert_eq!(model.language_level, 11);

    let model = build(
        &TargetMap::new(),
        &allocadia_view(),
        &ImportRoots::default(),
        &mut FakeFiles::default(),
    );
    assert!(model.source_roots.is_empty());
}

#[test]
fn sources_go_to_the_first_content_root_holding_them() {
    let t = library(
        "//p:p",
        "java_library",
        &["p/java/com/p/A.java", "q/gen/B.java"],
        vec![],
    );
    let mut files = FakeFiles::default();
    files
        .java("p/java/com/p/A.java", "package com.p;")
        .java("q/gen/B.java", "package q.gen;");
    let roots = ImportRoots {
        includes: vec!["p".into(), "q".into()],
        excludes: vec!["q/gen".into()],
        ..Default::default()
    };
    let model = build(&map(vec![t]), &allocadia_view(), &roots, &mut files);
    // q has no (non-excluded) sources, so the content root itself is the folder.
    assert_eq!(
        model.source_roots,
        [root("p/java", "", false), root("q", "q", false)]
    );
    assert_eq!(files.workspace_reads, ["p/java/com/p/A.java"]);
}

#[test]
fn language_level_sources() {
    let toolchain = |label: &str, version: &str| TargetIdeInfo {
        key: key(label),
        java_toolchain_ide_info: Some(JavaToolchainIdeInfo {
            source_version: version.into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let targets = map(vec![
        toolchain("//t:a", "8"),
        toolchain("//t:b", "17"),
        toolchain("//t:c", "1.8"),
        toolchain("//t:d", ""),
    ]);
    let model = build(
        &targets,
        &ProjectView::default(),
        &whole_workspace(),
        &mut FakeFiles::default(),
    );
    assert_eq!(model.language_level, 17);
    let view = ProjectView {
        java_language_level: Some(21),
        ..Default::default()
    };
    assert_eq!(
        build(
            &targets,
            &view,
            &whole_workspace(),
            &mut FakeFiles::default()
        )
        .language_level,
        21
    );
    let bad = map(vec![toolchain("//t:x", "JDK_9000")]);
    let model = build(
        &bad,
        &ProjectView::default(),
        &whole_workspace(),
        &mut FakeFiles::default(),
    );
    assert_eq!(model.language_level, 11);
    assert_eq!(
        model.warnings,
        ["//t:x: unknown Java source version \"JDK_9000\""]
    );
}

#[test]
fn bare_bazel_jars_and_external_sources() {
    let t = with_jdeps(
        library("//a:a", "java_library", &["a/A.java"], vec![]),
        "a/liba.jdeps",
    );
    let mut files = FakeFiles::default();
    files
        .java("a/A.java", "package a;")
        .jdeps(
            "a/liba.jdeps",
            jdeps_bytes(&[
                &format!("{BIN}/b/libb-hjar.jar"),
                &format!("{BIN}/c/libc-ijar.jar"),
                "external/javax_json/jar/downloaded.jar",
                "third_party/checked_in.jar",
                &format!("{BIN}/k/libk-kt-ijar.jar"),
            ]),
        )
        .exist(&[
            abs("b/libb.jar"),
            abs("b/libb-src.jar"),
            abs("c/libc-ijar.jar"),
            "/ob/external/javax_json/jar/downloaded.jar".into(),
            format!("{WS}/third_party/checked_in.jar"),
        ]);
    let model = build(
        &map(vec![t]),
        &ProjectView::default(),
        &whole_workspace(),
        &mut files,
    );
    assert_eq!(
        model.libraries,
        [
            lib(abs("b/libb.jar"), Some(abs("b/libb-src.jar")), false),
            lib(abs("c/libc-ijar.jar"), None, false),
            lib(
                "/ob/external/javax_json/jar/downloaded.jar".into(),
                None,
                false
            ),
            lib(format!("{WS}/third_party/checked_in.jar"), None, false),
        ]
    );
    assert!(model.warnings.is_empty(), "{:?}", model.warnings);
}

/// Checks the folders against IntelliJ's `.workspace.iml` for the Allocadia checkout: every
/// main-repository `.java` source the aspect reported (`bazel-bin/**/*.intellij-info.txt`) feeds
/// a synthetic source target, packages are read from the real files.
#[test]
#[ignore = "reads the Allocadia checkout, its bazel-bin and .ijwb"]
fn allocadia_source_folders_match_intellij() {
    use std::path::Path;
    let ws = "/Users/davidvieira/code/Allocadia";
    if !Path::new(ws)
        .join(".ijwb/.blaze/modules/.workspace.iml")
        .exists()
    {
        return;
    }

    struct RealFiles(&'static str, usize);
    impl SyncFiles for RealFiles {
        fn read_workspace_file(&mut self, path: &str) -> Option<String> {
            self.1 += 1;
            std::fs::read(Path::new(self.0).join(path))
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        }
        fn read_exec_bytes(&mut self, _: &str) -> Option<Vec<u8>> {
            None
        }
        fn exists(&mut self, abs_path: &str) -> bool {
            Path::new(abs_path).exists()
        }
    }

    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.to_string_lossy().ends_with(".intellij-info.txt") {
                out.push(path);
            }
        }
    }

    let mut infos = Vec::new();
    walk(
        &std::fs::canonicalize(Path::new(ws).join("bazel-bin")).unwrap(),
        &mut infos,
    );
    let mut targets = TargetMap::new();
    for path in &infos {
        let text = std::fs::read_to_string(path).unwrap();
        if !text.contains("\njava_ide_info {") {
            continue;
        }
        let mut label = None;
        let mut kind = String::new();
        let mut sources = Vec::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if line == "key {" {
                for l in lines.by_ref().take_while(|l| *l != "}") {
                    if let Some(v) = l.strip_prefix("  label: ") {
                        label = Some(v.trim_matches('"').to_owned());
                    }
                }
            } else if let Some(k) = line.strip_prefix("kind_string: ") {
                kind = k.trim_matches('"').to_owned();
            } else if line == "  sources {" {
                let block: Vec<&str> = lines.by_ref().take_while(|l| *l != "  }").collect();
                let rel = block
                    .iter()
                    .find_map(|l| l.strip_prefix("    relative_path: "))
                    .map(|v| v.trim_matches('"'));
                if block.contains(&"    is_source: true")
                    && block.contains(&"    is_external: false")
                {
                    sources.extend(rel.map(src));
                }
            }
        }
        let Some(label) = label else { continue };
        // bazel-bin keeps info files of targets that no longer exist.
        sources.retain(|s| Path::new(ws).join(&s.relative_path).exists());
        if label.starts_with('@') || sources.is_empty() {
            continue;
        }
        let mut t = library(&label, &kind, &[], vec![]);
        t.java_ide_info.as_mut().unwrap().sources = sources;
        targets.insert(t.key.clone(), t);
    }

    let data = format!("{}/tests/data", env!("CARGO_MANIFEST_DIR"));
    let view_text = std::fs::read_to_string(format!("{data}/allocadia.bazelproject")).unwrap();
    let ignore_text = std::fs::read_to_string(format!("{data}/allocadia.bazelignore")).unwrap();
    let mut excludes: Vec<String> = view_text
        .lines()
        .filter_map(|l| l.trim().strip_prefix('-'))
        .map(str::to_owned)
        .collect();
    excludes.extend(
        ignore_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.trim_end_matches('/').to_owned()),
    );
    excludes.extend(
        [
            "bazel-bin",
            "bazel-out",
            "bazel-testlogs",
            "bazel-Allocadia",
        ]
        .map(String::from),
    );
    let roots = ImportRoots {
        includes: vec!["".into()],
        excludes,
        ..Default::default()
    };
    let mut files = RealFiles(ws, 0);
    let model = build_java_model(&targets, &allocadia_view(), &roots, &info(), ws, &mut files);

    let iml =
        std::fs::read_to_string(Path::new(ws).join(".ijwb/.blaze/modules/.workspace.iml")).unwrap();
    let mut expected: Vec<SourceRoot> = iml
        .lines()
        .filter_map(|l| {
            let rest = l
                .split("<sourceFolder url=\"file://$MODULE_DIR$/../../../")
                .nth(1)?;
            let path = rest.split('"').next()?.to_owned();
            let is_test = l.contains("isTestSource=\"true\"");
            let package_prefix = l
                .split("packagePrefix=\"")
                .nth(1)
                .and_then(|p| p.split('"').next())
                .unwrap_or("")
                .to_owned();
            Some(SourceRoot {
                path,
                package_prefix,
                is_test,
            })
        })
        .collect();
    expected.sort();
    let mut actual = model.source_roots.clone();
    actual.sort();
    let missing: Vec<_> = expected.iter().filter(|r| !actual.contains(r)).collect();
    let extra: Vec<_> = actual.iter().filter(|r| !expected.contains(r)).collect();
    let source_files: HashSet<&str> = targets
        .values()
        .flat_map(|t| &t.java_ide_info.as_ref().unwrap().sources)
        .map(|s| s.relative_path.as_str())
        .collect();
    eprintln!(
        "{} targets, {} source files, {} package reads, {} folders (IntelliJ {}), warnings: {:?}",
        targets.len(),
        source_files.len(),
        files.1,
        actual.len(),
        expected.len(),
        model.warnings
    );
    // IntelliJ's module is a snapshot of its last sync: folders of modules added since then (e.g.
    // `campaign-api`, 2026-10-30, after the 2026-10-13 sync) are extra here, so only missing ones fail.
    eprintln!("extra (newer than IntelliJ's last sync?): {extra:#?}");
    assert!(missing.is_empty(), "missing {missing:#?}");
}
