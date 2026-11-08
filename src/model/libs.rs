//! The project's libraries: port of `BlazeJavaWorkspaceImporter.buildLibraries`/`addTargetAsSource`,
//! `LibraryGlobFilter` and `LibraryArtifact` (bazelbuild/intellij, ijwb branch), with jar choices
//! made for jdtls.

use crate::fxhash::{FxHashMap, FxHashSet, fingerprint, key_fingerprint};

use super::SyncFiles;
use super::classify::{Classified, JAVA_PROTO_LIBRARY_KINDS, is_java_proto_target};
use super::glob::GlobSet;
use super::jdeps::parse_jdeps;
use super::paths::{PathDecoder, exec_relative_path, parent};
use crate::types::{DependencyType, Library, LibraryArtifact, TargetIdeInfo, TargetKey, TargetMap};

/// How many missing jars a warning names before it only counts them.
const MISSING_JARS_NAMED: usize = 10;

pub(crate) struct LibraryInputs<'a, 'f> {
    pub targets: &'a TargetMap,
    pub classified: &'a Classified<'a>,
    /// Parallel to `classified.source_targets`: whether the target only builds test code.
    pub test_targets: &'a [bool],
    pub excluded_libraries: &'a GlobSet,
    pub decoder: &'a PathDecoder<'a>,
    pub files: &'f mut dyn SyncFiles,
    pub warnings: &'f mut Vec<String>,
}

/// The libraries in classpath order.
///
/// Like `buildLibraries`, three passes over the source targets fill one access-ordered map: the
/// jars their jdeps name, the jars of their direct compile-time dependencies, then their generated
/// jars. Putting a library again moves it to the end, so a jar both in jdeps and a direct
/// dependency sits with the direct dependencies. Within a pass IntelliJ iterates hash sets; here the
/// order is the target map's, then each list's, which keeps the result deterministic.
pub(crate) fn libraries(mut input: LibraryInputs) -> Vec<Library> {
    let mut arena = Arena::default();
    // `jdepsPathToLibrary` and the library half of `targetKeyToLibrary`.
    // Keyed by fingerprint: these are looked up for every jdeps entry and dependency.
    let mut by_jar_path: FxHashMap<u64, usize> = FxHashMap::default();
    let mut by_target: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    let excluded_jars: FxHashSet<u64> = input
        .classified
        .excluded_jars
        .iter()
        .map(|p| fingerprint(p))
        .collect();
    for target in &input.classified.library_targets {
        for jar in jars(target) {
            let Some(lib) = arena.artifact(jar) else {
                input.warnings.push(format!(
                    "{}: a jar entry has neither a class nor an interface jar",
                    target.key.label
                ));
                continue;
            };
            by_target
                .entry(key_fingerprint(&target.key))
                .or_default()
                .push(lib);
            for loc in jar.interface_jar.iter().chain(&jar.jar) {
                by_jar_path.insert(fingerprint(&loc.exec_path()), lib);
            }
        }
    }

    let mut puts = Puts::default();
    let sources = &input.classified.source_targets;

    for (target, &test) in sources.iter().zip(input.test_targets) {
        for path in jdeps_paths(target, &mut input) {
            let fp = fingerprint(&path);
            if is_kotlin_jar(&path) || excluded_jars.contains(&fp) {
                continue;
            }
            let lib = match by_jar_path.get(&fp) {
                Some(&lib) => lib,
                None => arena.bare(path),
            };
            puts.put(&mut arena, lib, test, 1);
        }
    }

    // A java_proto_library's own jars are empty; its proto_library deps carry them. There are few,
    // so a small set answers for every dependency instead of a lookup in the whole target map.
    let java_proto_libraries: FxHashMap<&TargetKey, &TargetIdeInfo> = input
        .targets
        .iter()
        .filter(|(_, t)| JAVA_PROTO_LIBRARY_KINDS.contains(&t.kind_string.as_str()))
        .collect();
    for (target, &test) in sources.iter().zip(input.test_targets) {
        for dep in target
            .deps
            .iter()
            .filter(|d| d.dependency_type == DependencyType::CompileTime)
        {
            let forwarded = if java_proto_libraries.is_empty() {
                None
            } else {
                java_proto_libraries.get(&dep.key)
            };
            let mut put_jars_of = |key: &TargetKey| {
                for &lib in by_target.get(&key_fingerprint(key)).into_iter().flatten() {
                    puts.put(&mut arena, lib, test, 2);
                }
            };
            match forwarded {
                Some(t) => t.deps.iter().for_each(|d| put_jars_of(&d.key)),
                None => put_jars_of(&dep.key),
            }
        }
    }

    for (target, &test) in sources.iter().zip(input.test_targets) {
        let Some(java) = &target.java_ide_info else {
            continue;
        };
        let mut generated: Vec<&LibraryArtifact> = java
            .generated_jars
            .iter()
            .chain(&java.filtered_gen_jar)
            .collect();
        if is_java_proto_target(target) {
            generated.extend(&java.jars);
        } else if java.filtered_gen_jar.is_none() && has_generated_sources(target) {
            // Without the aspect's JarFilter there is no jar of just the generated classes; the
            // target's own jar has them, after its sources on the classpath so sources win.
            generated.extend(&java.jars);
        }
        for jar in generated {
            match arena.artifact(jar) {
                Some(lib) => puts.put(&mut arena, lib, test, 3),
                None => input.warnings.push(format!(
                    "{}: a generated jar entry has neither a class nor an interface jar",
                    target.key.label
                )),
            }
        }
    }

    resolve(&arena, puts.ordered(&arena), &mut input)
}

/// `jar.contains("-kt-ijar.jar") || jar.contains("-kt-src.jar")` (`BlazeJavaWorkspaceImporter`),
/// in one pass over the path.
fn is_kotlin_jar(path: &str) -> bool {
    let b = path.as_bytes();
    (0..b.len().saturating_sub(3)).any(|i| {
        b[i] == b'-'
            && b[i + 1] == b'k'
            && b[i + 2] == b't'
            && b[i + 3] == b'-'
            && (path[i..].starts_with("-kt-ijar.jar") || path[i..].starts_with("-kt-src.jar"))
    })
}

fn jars(target: &TargetIdeInfo) -> impl Iterator<Item = &LibraryArtifact> {
    target.java_ide_info.iter().flat_map(|j| &j.jars)
}

/// A srcjar (generated or checked in) or a generated `.java` among the sources: classes jdtls cannot see as sources.
fn has_generated_sources(target: &TargetIdeInfo) -> bool {
    target
        .java_ide_info
        .iter()
        .flat_map(|j| &j.sources)
        .any(|s| {
            s.relative_path.ends_with(".srcjar")
                || (!s.is_source && s.relative_path.ends_with(".java"))
        })
}

/// The dependency jars a source target's jdeps file lists; unreadable files are warnings.
fn jdeps_paths(target: &TargetIdeInfo, input: &mut LibraryInputs) -> Vec<String> {
    let Some(jdeps) = target.java_ide_info.as_ref().and_then(|j| j.jdeps.as_ref()) else {
        return Vec::new();
    };
    let path = jdeps.exec_path();
    let parsed = match input.files.read_exec_bytes(&path) {
        Some(bytes) => parse_jdeps(&bytes),
        None => Err("not found".to_owned()),
    };
    parsed.unwrap_or_else(|e| {
        input.warnings.push(format!(
            "{}: cannot read jdeps {path} ({e}); its indirect dependencies are left off the classpath",
            target.key.label
        ));
        Vec::new()
    })
}

enum Jar<'a> {
    Artifact(&'a LibraryArtifact),
    /// A jdeps path no target of the map provides (`ExecutionPathHelper.parse`).
    Bare(String),
}

struct Entry<'a> {
    jar: Jar<'a>,
    /// Some source target that is not test-only pulled it in.
    for_main: bool,
    /// The pass that last put it, so a pass puts each library once.
    pass: u8,
    /// Its last position in `Puts::order`.
    last: usize,
}

#[derive(Default)]
struct Arena<'a> {
    entries: Vec<Entry<'a>>,
    /// By the fingerprint of the jar's exec path.
    by_key: FxHashMap<u64, usize>,
}

impl<'a> Arena<'a> {
    /// The library of a jar entry, keyed by its class jar (else interface jar) exec path.
    fn artifact(&mut self, jar: &'a LibraryArtifact) -> Option<usize> {
        let key = jar.jar.as_ref().or(jar.interface_jar.as_ref())?.exec_path();
        Some(self.intern(key, || Jar::Artifact(jar)))
    }

    fn bare(&mut self, exec_path: String) -> usize {
        let key = exec_path.clone();
        self.intern(key, || Jar::Bare(exec_path))
    }

    fn intern(&mut self, key: String, jar: impl FnOnce() -> Jar<'a>) -> usize {
        let next = self.entries.len();
        let i = *self.by_key.entry(fingerprint(&key)).or_insert(next);
        if i == next {
            self.entries.push(Entry {
                jar: jar(),
                for_main: false,
                pass: 0,
                last: 0,
            });
        }
        i
    }
}

#[derive(Default)]
struct Puts {
    order: Vec<usize>,
}

impl Puts {
    fn put(&mut self, arena: &mut Arena, lib: usize, test: bool, pass: u8) {
        let entry = &mut arena.entries[lib];
        entry.for_main |= !test;
        if entry.pass != pass {
            entry.pass = pass;
            entry.last = self.order.len();
            self.order.push(lib);
        }
    }

    /// Each library once, at its last position (`LinkedHashMap` with access order).
    fn ordered(&self, arena: &Arena) -> Vec<usize> {
        self.order
            .iter()
            .enumerate()
            .filter(|&(pos, &lib)| arena.entries[lib].last == pos)
            .map(|(_, &lib)| lib)
            .collect()
    }
}

fn resolve(arena: &Arena, ordered: Vec<usize>, input: &mut LibraryInputs) -> Vec<Library> {
    let mut out: Vec<Library> = Vec::with_capacity(ordered.len());
    let mut by_jar: FxHashMap<String, usize> = FxHashMap::default();
    let mut missing: Vec<String> = Vec::new();
    for lib in ordered {
        let entry = &arena.entries[lib];
        if excluded(&entry.jar, input.excluded_libraries) {
            continue;
        }
        let (jar, source_jar) = match &entry.jar {
            Jar::Artifact(a) => choose_artifact(a, input, &mut missing),
            Jar::Bare(path) => choose_bare(path, input, &mut missing),
        };
        let test_only = !entry.for_main;
        match by_jar.get(&jar) {
            Some(&i) => {
                let kept = &mut out[i];
                kept.test_only &= test_only;
                if kept.source_jar.is_none() {
                    kept.source_jar = source_jar;
                }
            }
            None => {
                by_jar.insert(jar.clone(), out.len());
                out.push(Library {
                    jar,
                    source_jar,
                    test_only,
                });
            }
        }
    }
    if !missing.is_empty() {
        let named: Vec<&str> = missing
            .iter()
            .take(MISSING_JARS_NAMED)
            .map(String::as_str)
            .collect();
        let more = missing.len().saturating_sub(MISSING_JARS_NAMED);
        let tail = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        input.warnings.push(format!(
            "{} library jars do not exist (has the sync build run?): {}{tail}",
            missing.len(),
            named.join(", ")
        ));
    }
    out
}

/// `LibraryGlobFilter`: `excluded_libraries` globs match a jar's relative path.
fn excluded(jar: &Jar, globs: &GlobSet) -> bool {
    if globs.is_empty() {
        return false;
    }
    match jar {
        Jar::Artifact(a) => a
            .interface_jar
            .iter()
            .chain(&a.jar)
            .any(|l| globs.matches(&l.relative_path)),
        Jar::Bare(path) => globs.matches(exec_relative_path(path)),
    }
}

/// The class jar when it exists, else the interface jar. IntelliJ indexes interface jars, but
/// jdtls runs and tests from this classpath and interface jars have no method bodies.
fn choose_artifact(
    a: &LibraryArtifact,
    input: &mut LibraryInputs,
    missing: &mut Vec<String>,
) -> (String, Option<String>) {
    let class = a.jar.as_ref().map(|l| input.decoder.artifact(l));
    let interface = a.interface_jar.as_ref().map(|l| input.decoder.artifact(l));
    let jar = match (class, interface) {
        (Some(c), _) if input.files.exists(&c) => c,
        (c, Some(i)) => {
            if !input.files.exists(&i) {
                missing.push(c.unwrap_or_else(|| i.clone()));
            }
            i
        }
        (Some(c), None) => {
            missing.push(c.clone());
            c
        }
        (None, None) => unreachable!("libraries are made only from entries with a jar"),
    };
    let source_jar = a.source_jars.first().map(|l| input.decoder.artifact(l));
    (jar, source_jar)
}

/// A jar only jdeps names: rules_jvm_external's `header_X.jar` has `processed_X.jar` and
/// `X-sources.jar` beside it, Bazel's `X-hjar.jar`/`X-ijar.jar` have `X.jar` and `X-src.jar`
/// (`guessSrcJarRelativePath`). Siblings are used only when they exist.
fn choose_bare(
    exec_path: &str,
    input: &mut LibraryInputs,
    missing: &mut Vec<String>,
) -> (String, Option<String>) {
    let dir = parent(exec_path);
    let name = &exec_path[if dir.is_empty() { 0 } else { dir.len() + 1 }..];
    let sibling = |file: String| {
        if dir.is_empty() {
            file
        } else {
            format!("{dir}/{file}")
        }
    };
    let (class, source) = if let Some(stem) = name
        .strip_prefix("header_")
        .and_then(|n| n.strip_suffix(".jar"))
    {
        (
            Some(sibling(format!("processed_{stem}.jar"))),
            Some(sibling(format!("{stem}-sources.jar"))),
        )
    } else if let Some(stem) = name
        .strip_suffix("-hjar.jar")
        .or_else(|| name.strip_suffix("-ijar.jar"))
    {
        (
            Some(sibling(format!("{stem}.jar"))),
            Some(sibling(format!("{stem}-src.jar"))),
        )
    } else {
        (None, None)
    };
    let jar = match class.map(|c| input.decoder.exec_path(&c)) {
        Some(c) if input.files.exists(&c) => c,
        _ => {
            let original = input.decoder.exec_path(exec_path);
            if !input.files.exists(&original) {
                missing.push(original.clone());
            }
            original
        }
    };
    let source_jar = source
        .map(|s| input.decoder.exec_path(&s))
        .filter(|s| input.files.exists(s));
    (jar, source_jar)
}
