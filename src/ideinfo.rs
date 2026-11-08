//! `.intellij-info.txt` files: parsing into [`TargetIdeInfo`] and finding them under `bazel-bin`.

use std::fmt::Write as _;

use crate::textproto::{EnumValue, Error, Field, Reader};
use crate::types::{
    ArtifactLocation, Dependency, DependencyType, JavaIdeInfo, JavaToolchainIdeInfo,
    LibraryArtifact, TargetIdeInfo, TargetKey,
};

/// Parses one info file (protobuf text format of `blaze.TargetIdeInfo`), ignoring unknown fields.
///
/// Ports `AspectStrategy.readAspectFile` (a `TextFormat` parser allowing unknown fields) and the
/// `fromProto` methods of `TargetIdeInfo`, `TargetKey`, `Dependency`, `ArtifactLocation`,
/// `LibraryArtifact` and `JavaIdeInfo` (base/src/com/google/idea/blaze/base/ideinfo). Repeated
/// singular fields behave as in `TextFormat`: the last scalar wins and messages merge. A file
/// without a `key` (or with an empty label) is an error, where IntelliJ drops the target.
pub fn parse_target_ide_info(text: &str) -> Result<TargetIdeInfo, String> {
    parse(text, true).map_err(|e| e.describe(text))
}

/// `fast`: scan `deps` blocks with [`fast_dependency`] (the reader alone is used to check it).
fn parse(text: &str, fast: bool) -> Result<TargetIdeInfo, Error> {
    let mut r = Reader::new(text);
    let mut info = TargetIdeInfo::default();
    let mut has_key = false;
    loop {
        if fast {
            r.skip_insignificant();
            let at = r.offset();
            if text.as_bytes()[at..].starts_with(b"deps {")
                && let Some((dep, end)) = fast_dependency(text, at + "deps {".len())
            {
                r.jump_to(end);
                info.deps.push(dep);
                continue;
            }
        }
        let Some(f) = r.next_field()? else { break };
        match f.name {
            "key" => {
                has_key = true;
                target_key(&mut r, &f, &mut info.key)?;
            }
            "kind_string" => info.kind_string = f.string()?.into_owned(),
            "build_file_artifact_location" => artifact_location(
                &mut r,
                &f,
                info.build_file_artifact_location
                    .get_or_insert_with(Default::default),
            )?,
            "deps" => info.deps.push(dependency(&mut r, &f)?),
            "tags" => info.tags.push(f.string()?.into_owned()),
            "java_ide_info" => {
                java_ide_info(
                    &mut r,
                    &f,
                    info.java_ide_info.get_or_insert_with(Default::default),
                )?;
            }
            "java_toolchain_ide_info" => java_toolchain_ide_info(
                &mut r,
                &f,
                info.java_toolchain_ide_info
                    .get_or_insert_with(Default::default),
            )?,
            _ => r.skip(&f)?,
        }
    }
    if !has_key {
        return Err(Error::new(0, "no `key` field"));
    }
    if info.key.label.is_empty() {
        return Err(Error::new(0, "`key` has no `label`"));
    }
    Ok(info)
}

fn target_key<'a>(r: &mut Reader<'a>, f: &Field<'a>, key: &mut TargetKey) -> Result<(), Error> {
    f.expect_message()?;
    while let Some(f) = r.next_field()? {
        match f.name {
            "label" => key.label = f.string()?.into_owned(),
            "aspect_ids" => key.aspect_ids.push(f.string()?.into_owned()),
            _ => r.skip(&f)?,
        }
    }
    Ok(())
}

/// An unrecognized `dependency_type` number counts as runtime: IntelliJ only ever compares it with
/// `COMPILE_TIME` (`DependencyFinder`, `BlazeJavaWorkspaceImporter`).
fn dependency<'a>(r: &mut Reader<'a>, f: &Field<'a>) -> Result<Dependency, Error> {
    f.expect_message()?;
    let mut key = TargetKey::default();
    let mut dependency_type = DependencyType::CompileTime;
    while let Some(f) = r.next_field()? {
        match f.name {
            "dependency_type" => {
                dependency_type = match f.enum_value()? {
                    EnumValue::Number(0) | EnumValue::Name("COMPILE_TIME") => {
                        DependencyType::CompileTime
                    }
                    EnumValue::Number(_) | EnumValue::Name("RUNTIME") => DependencyType::Runtime,
                    EnumValue::Name(other) => {
                        return Err(Error::new(
                            f.offset,
                            format!("unknown dependency_type `{other}`"),
                        ));
                    }
                }
            }
            "target" => target_key(r, &f, &mut key)?,
            _ => r.skip(&f)?,
        }
    }
    Ok(Dependency {
        key,
        dependency_type,
    })
}

/// `deps` blocks are nearly all of an info file's bytes (a `java_test` lists hundreds of
/// dependencies) and `proto.encode_text` always writes them the same way: `dependency_type: N`,
/// then `target { aspect_ids: "…"* label: "…" }`, fields in name order. This scans that shape
/// directly from `start` (just past the block's `{`) and returns the dependency and the offset
/// just past its closing `}`; anything else (escapes, named enums, other fields) returns `None` so
/// the reader parses the block instead. It runs within the plugin's CPU budget only because it
/// stays this tight: one pass, the index in a local, words compared eight bytes at a time.
fn fast_dependency(text: &str, start: usize) -> Option<(Dependency, usize)> {
    let b = text.as_bytes();
    let mut i = skip_ws(b, start);
    let mut dependency_type = DependencyType::CompileTime;
    if has_literal(b, i, b"dependency_type: ") {
        i += "dependency_type: ".len();
        let from = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        dependency_type = match &b[from..i] {
            b"0" => DependencyType::CompileTime,
            [] => return None,
            _ => DependencyType::Runtime,
        };
        i = skip_ws(b, i);
    }
    if !has_literal(b, i, b"target {") {
        return None;
    }
    i += "target {".len();
    let mut key = TargetKey::default();
    loop {
        i = skip_ws(b, i);
        if has_literal(b, i, b"label: \"") {
            let from = i + "label: \"".len();
            let to = closing_quote(b, from)?;
            key.label = text.get(from..to)?.to_owned();
            i = to + 1;
        } else if has_literal(b, i, b"aspect_ids: \"") {
            let from = i + "aspect_ids: \"".len();
            let to = closing_quote(b, from)?;
            key.aspect_ids.push(text.get(from..to)?.to_owned());
            i = to + 1;
        } else if b.get(i) == Some(&b'}') {
            i += 1;
            break;
        } else {
            return None;
        }
    }
    i = skip_ws(b, i);
    (b.get(i) == Some(&b'}')).then_some((
        Dependency {
            key,
            dependency_type,
        },
        i + 1,
    ))
}

#[inline(always)]
fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\n' | b'\t' | b'\r') {
        i += 1;
    }
    i
}

/// Whether `b[i..]` starts with `lit`, compared a word at a time.
#[inline(always)]
fn has_literal(b: &[u8], i: usize, lit: &[u8]) -> bool {
    let Some(s) = b.get(i..i + lit.len()) else {
        return false;
    };
    let mut k = 0;
    while k + 8 <= lit.len() {
        if word(s, k) != word(lit, k) {
            return false;
        }
        k += 8;
    }
    while k < lit.len() {
        if s[k] != lit[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// The offset of the `"` closing a string that starts at `from`, or `None` when the string has an
/// escape or a line break first (the reader handles those). Skips eight bytes at a time while none
/// of them is `"`, `\\` or a line feed.
fn closing_quote(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 8 <= b.len() {
        let w = word(b, i);
        if has_byte(w, b'"') | has_byte(w, b'\\') | has_byte(w, b'\n') != 0 {
            break;
        }
        i += 8;
    }
    while i < b.len() {
        match b[i] {
            b'"' => return Some(i),
            b'\\' | b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

#[inline(always)]
fn word(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().expect("eight bytes"))
}

/// Non-zero when some byte of `w` is `c` (the classic has-zero-byte test on `w ^ c…c`).
#[inline(always)]
fn has_byte(w: u64, c: u8) -> u64 {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let x = w ^ (LO * u64::from(c));
    x.wrapping_sub(LO) & !x & HI
}

#[doc(hidden)]
pub fn __bench_fast_dependency(text: &str, start: usize) -> Option<usize> {
    fast_dependency(text, start).map(|(_, end)| end)
}

fn artifact_location<'a>(
    r: &mut Reader<'a>,
    f: &Field<'a>,
    loc: &mut ArtifactLocation,
) -> Result<(), Error> {
    f.expect_message()?;
    while let Some(f) = r.next_field()? {
        match f.name {
            "relative_path" => loc.relative_path = f.string()?.into_owned(),
            "is_source" => loc.is_source = f.bool()?,
            "is_external" => loc.is_external = f.bool()?,
            "root_execution_path_fragment" => {
                loc.root_execution_path_fragment = f.string()?.into_owned()
            }
            _ => r.skip(&f)?,
        }
    }
    Ok(())
}

fn new_artifact_location<'a>(r: &mut Reader<'a>, f: &Field<'a>) -> Result<ArtifactLocation, Error> {
    let mut loc = ArtifactLocation::default();
    artifact_location(r, f, &mut loc)?;
    Ok(loc)
}

/// `LibraryArtifact.fromProto`: `source_jars`, or the deprecated `source_jar` when there are none.
fn library_artifact<'a>(r: &mut Reader<'a>, f: &Field<'a>) -> Result<LibraryArtifact, Error> {
    f.expect_message()?;
    let mut lib = LibraryArtifact::default();
    let mut source_jar = None;
    while let Some(f) = r.next_field()? {
        match f.name {
            "jar" => artifact_location(r, &f, lib.jar.get_or_insert_with(Default::default))?,
            "interface_jar" => artifact_location(
                r,
                &f,
                lib.interface_jar.get_or_insert_with(Default::default),
            )?,
            "source_jar" => {
                artifact_location(r, &f, source_jar.get_or_insert_with(Default::default))?
            }
            "source_jars" => lib.source_jars.push(new_artifact_location(r, &f)?),
            _ => r.skip(&f)?,
        }
    }
    if lib.source_jars.is_empty() {
        lib.source_jars.extend(source_jar);
    }
    Ok(lib)
}

fn java_ide_info<'a>(
    r: &mut Reader<'a>,
    f: &Field<'a>,
    java: &mut JavaIdeInfo,
) -> Result<(), Error> {
    f.expect_message()?;
    while let Some(f) = r.next_field()? {
        match f.name {
            "jars" => java.jars.push(library_artifact(r, &f)?),
            "generated_jars" => java.generated_jars.push(library_artifact(r, &f)?),
            "sources" => java.sources.push(new_artifact_location(r, &f)?),
            "package_manifest" => {
                artifact_location(
                    r,
                    &f,
                    java.package_manifest.get_or_insert_with(Default::default),
                )?;
            }
            "jdeps" => artifact_location(r, &f, java.jdeps.get_or_insert_with(Default::default))?,
            "filtered_gen_jar" => java.filtered_gen_jar = Some(library_artifact(r, &f)?),
            "main_class" => java.main_class = f.string()?.into_owned(),
            "test_class" => java.test_class = f.string()?.into_owned(),
            "plugin_processor_jars" => java.plugin_processor_jars.push(library_artifact(r, &f)?),
            _ => r.skip(&f)?,
        }
    }
    Ok(())
}

fn java_toolchain_ide_info<'a>(
    r: &mut Reader<'a>,
    f: &Field<'a>,
    toolchain: &mut JavaToolchainIdeInfo,
) -> Result<(), Error> {
    f.expect_message()?;
    while let Some(f) = r.next_field()? {
        match f.name {
            "source_version" => toolchain.source_version = f.string()?.into_owned(),
            "target_version" => toolchain.target_version = f.string()?.into_owned(),
            _ => r.skip(&f)?,
        }
    }
    Ok(())
}

/// Java's `String.hashCode` over UTF-16 code units, which the Starlark spec gives for `hash()`.
/// Bazel's own evaluation differs for non-ASCII strings: see [`bazel_string_hash`].
pub fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

/// Starlark's `hash()` as Bazel evaluates it. Bazel keeps Starlark strings as one Java `char` per
/// UTF-8 byte (which is why `len("é") == 2` there), so `String.hashCode` runs over the UTF-8 bytes.
/// Equal to [`java_string_hash`] for ASCII.
pub fn bazel_string_hash(s: &str) -> i32 {
    s.bytes().fold(0i32, hash_step)
}

fn hash_step(h: i32, b: u8) -> i32 {
    h.wrapping_mul(31).wrapping_add(i32::from(b))
}

/// The (repository, package, name) of a label in the form the aspect writes it (`//pkg:name`,
/// `@@repo//pkg:name`, `@repo//pkg:name`); the repository is empty for the main one. As in Bazel,
/// `//pkg` names the target `pkg`'s last segment and `@repo` is `@repo//:repo`.
pub fn split_label(label: &str) -> (&str, &str, &str) {
    let (repo, rest) = match label.strip_prefix('@') {
        Some(at) => {
            let at = at.strip_prefix('@').unwrap_or(at);
            match at.find("//") {
                Some(i) => (&at[..i], &at[i + 2..]),
                None => (at, ""),
            }
        }
        None => ("", label.strip_prefix("//").unwrap_or(label)),
    };
    let (package, name) = match rest.find(':') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None if rest.is_empty() => (rest, repo),
        None => (rest, rest.rsplit('/').next().unwrap_or(rest)),
    };
    (repo, package, name)
}

/// Where the aspect writes the info file of `key` (see `intellij_info_impl_bundled.bzl`):
/// `<bazel_bin>/[external/<repo>/]<package>/<name>-<hash(name)>[-<hash(".".join(aspect_ids))>].intellij-info.txt`.
///
/// `ctx.actions.declare_file` puts the file in the target's package directory of its repository's
/// output tree; a name with `/` lands in a subdirectory and is hashed whole.
pub fn info_file_path(bazel_bin: &str, key: &TargetKey) -> String {
    let (repo, package, name) = split_label(&key.label);
    let base = if bazel_bin.len() > 1 {
        bazel_bin.trim_end_matches('/')
    } else {
        bazel_bin
    };
    let mut path =
        String::with_capacity(base.len() + repo.len() + package.len() + 2 * name.len() + 48);
    path.push_str(base);
    fn push_segment(path: &mut String, segment: &str) {
        if !path.is_empty() && !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(segment);
    }
    if !repo.is_empty() {
        push_segment(&mut path, "external");
        push_segment(&mut path, repo);
    }
    if !package.is_empty() {
        push_segment(&mut path, package);
    }
    push_segment(&mut path, name);
    // Writing to a String cannot fail.
    let _ = write!(path, "-{}", bazel_string_hash(name));
    if !key.aspect_ids.is_empty() {
        let mut h = 0i32;
        for (i, id) in key.aspect_ids.iter().enumerate() {
            if i > 0 {
                h = hash_step(h, b'.');
            }
            h = id.bytes().fold(h, hash_step);
        }
        let _ = write!(path, "-{h}");
    }
    path.push_str(".intellij-info.txt");
    path
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    fn walk(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                walk(&entry.path(), suffix, out);
            } else if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
                out.push(entry.path());
            }
        }
    }

    pub fn real_bazel_bin() -> String {
        std::env::var("OPENIDEA_BAZEL_BIN")
            .unwrap_or_else(|_| "/Users/davidvieira/code/Allocadia/bazel-bin".to_owned())
    }

    pub fn real_files(suffix: &str) -> Vec<PathBuf> {
        let mut files = Vec::new();
        walk(Path::new(&real_bazel_bin()), suffix, &mut files);
        files
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{real_bazel_bin, real_files};
    use super::*;
    use std::path::Path;

    const BIN: &str = "bazel-out/darwin_arm64-fastbuild/bin";

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/info")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn loc(
        root: &str,
        relative_path: &str,
        is_source: bool,
        is_external: bool,
    ) -> ArtifactLocation {
        ArtifactLocation {
            root_execution_path_fragment: root.to_owned(),
            relative_path: relative_path.to_owned(),
            is_source,
            is_external,
        }
    }

    fn key(label: &str, aspect_ids: &[&str]) -> TargetKey {
        TargetKey {
            label: label.to_owned(),
            aspect_ids: aspect_ids.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn hashes_match_starlark() {
        assert_eq!(java_string_hash("allocadia-util"), -1755630977);
        assert_eq!(
            java_string_hash("com_google_guava_failureaccess"),
            1987092101
        );
        assert_eq!(java_string_hash("current_java_toolchain"), -1286701614);
        assert_eq!(java_string_hash("kpi"), 106404);
        assert_eq!(java_string_hash("dto"), 99807);
        assert_eq!(java_string_hash("core-woven"), 918125081);
        assert_eq!(
            java_string_hash("com.allocadia.util.AsyncTest"),
            -1075662227
        );
        assert_eq!(java_string_hash("audit_proto"), 924727428);
        assert_eq!(
            java_string_hash(
                "@@_builtins//:common/java/proto/java_proto_library.bzl%bazel_java_proto_aspect"
            ),
            1618710034
        );
        assert_eq!(java_string_hash(""), 0);
    }

    #[test]
    fn hashes_of_non_ascii() {
        // UTF-16 code units: 'é' is one, '😀' is a surrogate pair.
        assert_eq!(java_string_hash("é"), 0xe9);
        assert_eq!(java_string_hash("😀"), 0xd83d * 31 + 0xde00);
        // Bazel's byte-per-char strings: the UTF-8 bytes C3 A9.
        assert_eq!(bazel_string_hash("é"), 0xc3 * 31 + 0xa9);
        assert_eq!(
            bazel_string_hash("allocadia-util"),
            java_string_hash("allocadia-util")
        );
    }

    #[test]
    fn parses_java_library() {
        let info = parse_target_ide_info(&fixture("allocadia-util--1755630977.intellij-info.txt"))
            .unwrap();
        assert_eq!(
            info.key,
            key("//libs/commons/allocadia-util:allocadia-util", &[])
        );
        assert_eq!(info.kind_string, "java_library");
        assert_eq!(
            info.build_file_artifact_location,
            Some(loc("", "libs/commons/allocadia-util/BUILD", true, false))
        );
        assert_eq!(info.deps.len(), 10);
        assert_eq!(
            info.deps[0],
            Dependency {
                key: key("@@rules_java~//toolchains:current_java_toolchain", &[]),
                dependency_type: DependencyType::CompileTime
            }
        );
        assert_eq!(
            info.deps[9],
            Dependency {
                key: key("@@maven//:software_amazon_awssdk_sso", &[]),
                dependency_type: DependencyType::Runtime
            }
        );
        assert!(info.tags.is_empty());
        assert!(info.java_toolchain_ide_info.is_none());
        let java = info.java_ide_info.unwrap();
        assert_eq!(
            java.jars,
            [LibraryArtifact {
                jar: Some(loc(
                    BIN,
                    "libs/commons/allocadia-util/liballocadia-util.jar",
                    false,
                    false
                )),
                interface_jar: Some(loc(
                    BIN,
                    "libs/commons/allocadia-util/liballocadia-util-ijar.jar",
                    false,
                    false
                )),
                source_jars: vec![loc(
                    BIN,
                    "libs/commons/allocadia-util/liballocadia-util-src.jar",
                    false,
                    false
                )],
            }]
        );
        assert_eq!(
            java.jdeps,
            Some(loc(
                BIN,
                "libs/commons/allocadia-util/liballocadia-util.jdeps",
                false,
                false
            ))
        );
        assert_eq!(
            java.package_manifest,
            Some(loc(
                BIN,
                "libs/commons/allocadia-util/allocadia-util.java-manifest",
                false,
                false
            ))
        );
        assert_eq!(java.sources.len(), 14);
        assert_eq!(
            java.sources[0],
            loc(
                "",
                "libs/commons/allocadia-util/src/main/java/com/allocadia/util/Async.java",
                true,
                false
            )
        );
        assert!(java.generated_jars.is_empty() && java.plugin_processor_jars.is_empty());
        assert!(java.filtered_gen_jar.is_none());
        assert_eq!(
            (java.main_class.as_str(), java.test_class.as_str()),
            ("", "")
        );
    }

    #[test]
    fn parses_generated_and_plugin_jars() {
        let info = parse_target_ide_info(&fixture("kpi-106404.intellij-info.txt")).unwrap();
        let java = info.java_ide_info.unwrap();
        assert_eq!(java.generated_jars.len(), 1);
        assert_eq!(java.plugin_processor_jars.len(), 1);
        assert!(java.generated_jars[0].jar.is_some());
        assert_eq!(java.sources.len(), 16);
    }

    #[test]
    fn parses_test_target_and_skips_test_info() {
        let info = parse_target_ide_info(&fixture(
            "com.allocadia.util.AsyncTest--1075662227.intellij-info.txt",
        ))
        .unwrap();
        assert_eq!(info.kind_string, "java_test");
        let java = info.java_ide_info.unwrap();
        assert_eq!(java.test_class, "com.allocadia.util.AsyncTest");
        assert_eq!(java.main_class, "");
        assert_eq!(info.deps.len(), 3);
    }

    #[test]
    fn parses_external_import_with_tags() {
        let info = parse_target_ide_info(&fixture(
            "com_google_guava_failureaccess-1987092101.intellij-info.txt",
        ))
        .unwrap();
        assert_eq!(
            info.key,
            key("@@maven//:com_google_guava_failureaccess", &[])
        );
        assert_eq!(
            info.build_file_artifact_location,
            Some(loc("", "/BUILD", true, true))
        );
        assert_eq!(info.tags.len(), 4);
        assert_eq!(
            info.tags[0],
            "maven_coordinates=com.google.guava:failureaccess:1.0.3"
        );
        let jar = &info.java_ide_info.unwrap().jars[0];
        assert!(jar.jar.as_ref().unwrap().is_external);
        assert_eq!(jar.source_jars.len(), 1);
    }

    #[test]
    fn parses_toolchain() {
        let info = parse_target_ide_info(&fixture(
            "current_java_toolchain--1286701614.intellij-info.txt",
        ))
        .unwrap();
        assert_eq!(
            info.java_toolchain_ide_info,
            Some(JavaToolchainIdeInfo {
                source_version: "17".into(),
                target_version: "17".into()
            })
        );
        assert!(info.java_ide_info.is_none());
        assert!(info.deps.is_empty());
    }

    #[test]
    fn keeps_aspect_ids_and_empty_java_ide_info() {
        let aspect =
            "@@_builtins//:common/java/proto/java_proto_library.bzl%bazel_java_proto_aspect";
        let proto = parse_target_ide_info(&fixture(
            "audit_proto-924727428-1618710034.intellij-info.txt",
        ))
        .unwrap();
        assert_eq!(
            proto.key,
            key(
                "//projects/uptempo/audit-api/src/main/proto:audit_proto",
                &[aspect]
            )
        );
        let java_proto =
            parse_target_ide_info(&fixture("audit_java_proto-1059358831.intellij-info.txt"))
                .unwrap();
        assert_eq!(java_proto.deps[0].key, proto.key);
        assert_eq!(java_proto.java_ide_info, Some(JavaIdeInfo::default()));
    }

    #[test]
    fn source_jars_fall_back_to_deprecated_source_jar() {
        let text = r#"
key { label: "//a:a" }
java_ide_info {
  jars { jar { relative_path: "a.jar" } source_jar { relative_path: "old-src.jar" } }
  jars {
    source_jar { relative_path: "old-src.jar" }
    source_jars { relative_path: "a-src.jar" }
    source_jars { relative_path: "b-src.jar" }
  }
  jars { }
}"#;
        let jars = parse_target_ide_info(text)
            .unwrap()
            .java_ide_info
            .unwrap()
            .jars;
        let paths = |lib: &LibraryArtifact| {
            lib.source_jars
                .iter()
                .map(|l| l.relative_path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&jars[0]), ["old-src.jar"]);
        assert_eq!(paths(&jars[1]), ["a-src.jar", "b-src.jar"]);
        assert!(jars[2].source_jars.is_empty() && jars[2].jar.is_none());
    }

    #[test]
    fn skips_unknown_fields_and_messages() {
        let text = r#"
c_ide_info { source { relative_path: "x.cc" } rule_context { copts: "-O2" } }
features: "f"
key { label: "//a:a" future: 3 }
py_ide_info { sources { relative_path: "y.py" } }
sync_time_millis: 123
android_ide_info { manifest { relative_path: "AndroidManifest.xml" } java_package: "p" }
java_ide_info {
  transitive_compile_time_jars { relative_path: "t.jar" }
  main_class: "com.Main"
  filtered_gen_jar { jar { relative_path: "f.jar" } }
}
kind_string: "java_binary"
"#;
        let info = parse_target_ide_info(text).unwrap();
        assert_eq!(info.key, key("//a:a", &[]));
        assert_eq!(info.kind_string, "java_binary");
        let java = info.java_ide_info.unwrap();
        assert_eq!(java.main_class, "com.Main");
        assert_eq!(
            java.filtered_gen_jar.unwrap().jar.unwrap().relative_path,
            "f.jar"
        );
        assert!(java.jars.is_empty() && java.sources.is_empty());
    }

    #[test]
    fn repeated_singular_fields_merge_or_overwrite() {
        let text = r#"
key { label: "//a:old" }
key { label: "//a:a" aspect_ids: "x" }
kind_string: "one"
kind_string: "two"
build_file_artifact_location { relative_path: "a/BUILD" }
build_file_artifact_location { is_source: true }
"#;
        let info = parse_target_ide_info(text).unwrap();
        assert_eq!(info.key, key("//a:a", &["x"]));
        assert_eq!(info.kind_string, "two");
        assert_eq!(
            info.build_file_artifact_location,
            Some(loc("", "a/BUILD", true, false))
        );
    }

    #[test]
    fn dependency_types() {
        let text = r#"
key { label: "//a:a" }
deps { target { label: "//d:default" } }
deps { dependency_type: COMPILE_TIME target { label: "//d:c" } }
deps { dependency_type: RUNTIME target { label: "//d:r" } }
deps { dependency_type: 7 target { label: "//d:unknown" } }
deps { dependency_type: 1 }
"#;
        let deps = parse_target_ide_info(text).unwrap().deps;
        let types: Vec<_> = deps.iter().map(|d| d.dependency_type).collect();
        use DependencyType::*;
        assert_eq!(types, [CompileTime, CompileTime, Runtime, Runtime, Runtime]);
        assert_eq!(deps[4].key, TargetKey::default());

        let err =
            parse_target_ide_info("key { label: \"//a:a\" }\ndeps { dependency_type: LATER }")
                .unwrap_err();
        assert!(err.contains("unknown dependency_type `LATER`"), "{err}");
    }

    #[test]
    fn decodes_escaped_strings() {
        let text = "key { label: \"//a:caf\\303\\251\" }\ntags: \"a\\\"b\"";
        let info = parse_target_ide_info(text).unwrap();
        assert_eq!(info.key.label, "//a:café");
        assert_eq!(info.tags, ["a\"b"]);
    }

    #[test]
    fn errors_name_the_position() {
        for (text, expected) in [
            ("kind_string: \"x\"", "no `key` field"),
            ("key { aspect_ids: \"x\" }", "`key` has no `label`"),
            (
                "key { label: \"//a:a\" }\njava_ide_info {\n  jars {\n",
                "line 3, column 8 (byte 46): message `jars` is not closed",
            ),
            (
                "key { label: //a:a }",
                "line 1, column 14 (byte 13): expected a value for field `label`",
            ),
            (
                "key: \"//a:a\"",
                "field `key`: expected a message, found a string",
            ),
            (
                "key { label: \"//a:a\" }\nkind_string { }",
                "field `kind_string`: expected a string, found a message",
            ),
            (
                "key { label: \"//a:a\" }\nbuild_file_artifact_location { is_source: maybe }",
                "expected a bool, found `maybe`",
            ),
            ("key { label: \"//a:a\" }\n}", "unexpected `}`"),
            ("", "no `key` field"),
        ] {
            let err = parse_target_ide_info(text).unwrap_err();
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn splits_labels() {
        assert_eq!(split_label("//libs/a:b"), ("", "libs/a", "b"));
        assert_eq!(split_label("//:root"), ("", "", "root"));
        assert_eq!(split_label("@@maven//:guava"), ("maven", "", "guava"));
        assert_eq!(split_label("@repo//p/q:n/m"), ("repo", "p/q", "n/m"));
        assert_eq!(split_label("@@//p:n"), ("", "p", "n"));
        assert_eq!(split_label("@//p:n"), ("", "p", "n"));
        assert_eq!(split_label("//p/q"), ("", "p/q", "q"));
        assert_eq!(split_label("@repo"), ("repo", "", "repo"));
        assert_eq!(
            split_label("@@rules_java~//toolchains:current_java_toolchain"),
            ("rules_java~", "toolchains", "current_java_toolchain")
        );
    }

    #[test]
    fn info_file_paths() {
        let bin = "/out/bin";
        for (label, aspect_ids, expected) in [
            (
                "//libs/commons/allocadia-util:allocadia-util",
                &[][..],
                "/out/bin/libs/commons/allocadia-util/allocadia-util--1755630977.intellij-info.txt",
            ),
            (
                "@@maven//:com_google_guava_failureaccess",
                &[],
                "/out/bin/external/maven/com_google_guava_failureaccess-1987092101.intellij-info.txt",
            ),
            (
                "@maven//:com_google_guava_failureaccess",
                &[],
                "/out/bin/external/maven/com_google_guava_failureaccess-1987092101.intellij-info.txt",
            ),
            (
                "@@rules_java~//toolchains:current_java_toolchain",
                &[],
                "/out/bin/external/rules_java~/toolchains/current_java_toolchain--1286701614.intellij-info.txt",
            ),
            (
                "//projects/uptempo/audit-api/src/main/proto:audit_proto",
                &["@@_builtins//:common/java/proto/java_proto_library.bzl%bazel_java_proto_aspect"],
                "/out/bin/projects/uptempo/audit-api/src/main/proto/audit_proto-924727428-1618710034.intellij-info.txt",
            ),
            ("//:kpi", &[], "/out/bin/kpi-106404.intellij-info.txt"),
            (
                "//p:sub/kpi",
                &[],
                &format!(
                    "/out/bin/p/sub/kpi-{}.intellij-info.txt",
                    java_string_hash("sub/kpi")
                ),
            ),
        ] {
            assert_eq!(info_file_path(bin, &key(label, aspect_ids)), expected);
        }
        let ids = key("//p:dto", &["a", "b"]);
        assert_eq!(
            info_file_path("/out/bin/", &ids),
            format!(
                "/out/bin/p/dto-99807-{}.intellij-info.txt",
                java_string_hash("a.b")
            )
        );
        assert_eq!(
            info_file_path("/", &key("//p:dto", &[])),
            "/p/dto-99807.intellij-info.txt"
        );
        assert_eq!(
            info_file_path("", &key("//p:dto", &[])),
            "p/dto-99807.intellij-info.txt"
        );
    }

    #[test]
    fn fixture_names_match_info_file_path() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/info");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let file_name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let info = parse_target_ide_info(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let computed = info_file_path("bin", &info.key);
            assert!(
                computed.ends_with(&format!("/{file_name}")),
                "{computed} vs {file_name}"
            );
            checked += 1;
        }
        assert_eq!(checked, 9);
    }

    #[test]
    fn truncated_and_corrupted_files_fail_without_panicking() {
        let text = fixture("kpi-106404.intellij-info.txt");
        let mut parsed = 0;
        for len in (0..text.len()).filter(|&len| text.is_char_boundary(len)) {
            if parse_target_ide_info(&text[..len]).is_ok() {
                parsed += 1;
            }
        }
        // The file ends `key { … }\nkind_string: "java_library"\n`: only cuts right after the key's
        // `}`, after its newline, and after the complete `kind_string` value parse.
        assert_eq!(parsed, 3);

        let mut bytes = fixture("dto-99807.intellij-info.txt").into_bytes();
        for at in 0..bytes.len() {
            for c in *b"{}<>[]:\"'\\#\n;,-x" {
                let original = std::mem::replace(&mut bytes[at], c);
                let _ = parse_target_ide_info(&String::from_utf8_lossy(&bytes));
                bytes[at] = original;
            }
        }
    }

    /// Reads every info file of a real `bazel-bin` (`OPENIDEA_BAZEL_BIN`, default the Allocadia
    /// checkout): each must parse and live where `info_file_path` says.
    /// The `deps` fast path gives exactly what the reader gives, on every real info file.
    #[test]
    #[ignore = "reads a real bazel-bin"]
    fn fast_dependencies_match_the_reader_on_real_files() {
        let files = real_files(".intellij-info.txt");
        assert!(!files.is_empty());
        for path in &files {
            let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
            assert_eq!(
                parse(&text, true),
                parse(&text, false),
                "{}",
                path.display()
            );
        }
        eprintln!("{} files: fast path matches the reader", files.len());
    }

    #[test]
    fn fast_dependency_falls_back_on_unusual_blocks() {
        let usual = "deps {\n  dependency_type: 1\n  target {\n    aspect_ids: \"a\"\n    label: \"//x:y\"\n  }\n}\nkey {\n  label: \"//k:k\"\n}\n";
        let escaped = usual.replace("//x:y", "//x:\\\"y");
        let named = usual.replace("dependency_type: 1", "dependency_type: RUNTIME");
        let extra = usual.replace("    label:", "    extra: 1\n    label:");
        for text in [usual.to_string(), escaped, named, extra] {
            assert_eq!(parse(&text, true), parse(&text, false), "{text}");
        }
        assert!(fast_dependency(usual, usual.find('{').unwrap() + 1).is_some());
    }

    #[test]
    #[ignore = "reads a real bazel-bin"]
    fn real_info_files_parse_and_match_their_path() {
        let bazel_bin = real_bazel_bin();
        let files = real_files(".intellij-info.txt");
        assert!(!files.is_empty(), "no info files under {bazel_bin}");
        let mut mismatches = Vec::new();
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let info = parse_target_ide_info(&String::from_utf8_lossy(&bytes))
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let computed = info_file_path(&bazel_bin, &info.key);
            if computed != path.to_str().unwrap() {
                mismatches.push(format!("{} -> {computed}", path.display()));
            }
        }
        println!(
            "{} of {} info files are at info_file_path(key)",
            files.len() - mismatches.len(),
            files.len()
        );
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }
}
