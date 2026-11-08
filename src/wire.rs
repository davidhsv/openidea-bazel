//! Binary protobuf files the aspect points at: jdeps (`blaze_deps.Dependencies`) and package manifests.

use crate::types::ArtifactLocation;

/// Nesting of (deprecated) groups an unknown field may hold, as protobuf's default recursion limit.
const MAX_GROUP_DEPTH: usize = 100;

#[derive(Clone, Copy, Debug)]
enum WireValue<'a> {
    Varint(u64),
    /// A length-delimited payload and its offset in the file.
    Bytes(&'a [u8], usize),
    /// Fixed32, fixed64 and groups: no field read here has those types.
    Skipped,
}

/// One field of an encoded message; `offset` is where its tag starts in the whole file.
struct WireField<'a> {
    number: u32,
    offset: usize,
    value: WireValue<'a>,
}

/// Reads the fields of one message; `base` is the message's offset in the file, for errors.
struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    base: usize,
}

impl<'a> Decoder<'a> {
    fn new(buf: &'a [u8], base: usize) -> Self {
        Decoder { buf, pos: 0, base }
    }

    fn error(&self, at: usize, message: &str) -> String {
        format!("byte {}: {message}", self.base + at)
    }

    fn varint(&mut self) -> Result<u64, String> {
        let start = self.pos;
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&byte) = self.buf.get(self.pos) else {
                return Err(self.error(start, "truncated varint"));
            };
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(self.error(start, "varint longer than 10 bytes"))
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        let start = self.pos;
        let end = start
            .checked_add(len)
            .filter(|&end| end <= self.buf.len())
            .ok_or_else(|| self.error(start, &format!("truncated {what}")))?;
        self.pos = end;
        Ok(&self.buf[start..end])
    }

    /// `(field number, wire type)` of the next tag, or `None` at the end of the message.
    fn tag(&mut self) -> Result<Option<(u32, u8)>, String> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let start = self.pos;
        let tag = self.varint()?;
        let number = u32::try_from(tag >> 3)
            .ok()
            .filter(|&n| (1..=0x1fff_ffff).contains(&n))
            .ok_or_else(|| self.error(start, "invalid field number"))?;
        Ok(Some((number, (tag & 7) as u8)))
    }

    fn next(&mut self) -> Result<Option<WireField<'a>>, String> {
        let offset = self.base + self.pos;
        let Some((number, wire_type)) = self.tag()? else {
            return Ok(None);
        };
        let value = self.value(number, wire_type, offset, 0)?;
        Ok(Some(WireField {
            number,
            offset,
            value,
        }))
    }

    fn value(
        &mut self,
        number: u32,
        wire_type: u8,
        offset: usize,
        depth: usize,
    ) -> Result<WireValue<'a>, String> {
        Ok(match wire_type {
            0 => WireValue::Varint(self.varint()?),
            1 => {
                self.take(8, "fixed64")?;
                WireValue::Skipped
            }
            2 => {
                let start = self.pos;
                let len = usize::try_from(self.varint()?)
                    .map_err(|_| self.error(start, "length too large"))?;
                let offset = self.base + self.pos;
                WireValue::Bytes(self.take(len, "length-delimited field")?, offset)
            }
            3 => {
                if depth >= MAX_GROUP_DEPTH {
                    return Err(format!("byte {offset}: groups nested too deeply"));
                }
                loop {
                    let at = self.base + self.pos;
                    match self.tag()? {
                        None => return Err(format!("byte {offset}: group {number} is not closed")),
                        Some((end, 4)) if end == number => break,
                        Some((_, 4)) => return Err(format!("byte {at}: mismatched end of group")),
                        Some((inner, inner_type)) => {
                            self.value(inner, inner_type, at, depth + 1)?;
                        }
                    }
                }
                WireValue::Skipped
            }
            5 => {
                self.take(4, "fixed32")?;
                WireValue::Skipped
            }
            4 => {
                return Err(format!(
                    "byte {offset}: end of group {number} without a start"
                ));
            }
            _ => return Err(format!("byte {offset}: invalid wire type {wire_type}")),
        })
    }
}

/// `blaze_deps.Dependency.Kind`.
const EXPLICIT: u32 = 0;
const IMPLICIT: u32 = 1;
const INCOMPLETE: u32 = 3;

/// The exec-root-relative jar paths a jdeps file lists as EXPLICIT or IMPLICIT, in file order.
///
/// Ports `JdepsFileReader` (java/src/com/google/idea/blaze/java/sync/jdeps): `Deps.Dependencies`
/// parsed with protobuf-java's proto2 rules, then filtered by `relevantDep`. As there, a
/// dependency missing its required `path` or `kind` fails the whole file, and an unknown `kind`
/// number counts as missing (proto2 enums are closed).
pub fn parse_jdeps(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    let mut d = Decoder::new(bytes, 0);
    while let Some(f) = d.next()? {
        if let (1, WireValue::Bytes(dependency, offset)) = (f.number, f.value)
            && let Some(path) = jdeps_dependency(dependency, offset)?
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn jdeps_dependency(bytes: &[u8], base: usize) -> Result<Option<String>, String> {
    let mut path = None;
    let mut kind = None;
    let mut d = Decoder::new(bytes, base);
    while let Some(f) = d.next()? {
        match (f.number, f.value) {
            (1, WireValue::Bytes(p, _)) => path = Some(p),
            // protobuf-java reads an enum as the low 32 bits of the varint.
            (2, WireValue::Varint(v)) if (v as u32) <= INCOMPLETE => kind = Some(v as u32),
            _ => {}
        }
    }
    let path =
        path.ok_or_else(|| format!("byte {base}: dependency without its required `path`"))?;
    let kind =
        kind.ok_or_else(|| format!("byte {base}: dependency without its required `kind`"))?;
    // proto2 strings decode leniently: invalid UTF-8 becomes U+FFFD.
    Ok((kind == EXPLICIT || kind == IMPLICIT).then(|| String::from_utf8_lossy(path).into_owned()))
}

/// (package, source file) pairs of a `PackageManifest`.
///
/// Ports `PackageManifestReader.parseManifestFile` (java/src/com/google/idea/blaze/java/sync/source):
/// `PackageManifest{repeated JavaSourcePackage sources = 1}`,
/// `JavaSourcePackage{string package_string = 2; ArtifactLocation artifact_location = 3}`, proto3
/// (strings must be valid UTF-8). Pairs keep file order; IntelliJ collects them into a map keyed by
/// the location, so a later duplicate wins.
pub fn parse_package_manifest(bytes: &[u8]) -> Result<Vec<(String, ArtifactLocation)>, String> {
    let mut sources = Vec::new();
    let mut d = Decoder::new(bytes, 0);
    while let Some(f) = d.next()? {
        if let (1, WireValue::Bytes(source, offset)) = (f.number, f.value) {
            sources.push(java_source_package(source, offset)?);
        }
    }
    Ok(sources)
}

fn java_source_package(bytes: &[u8], base: usize) -> Result<(String, ArtifactLocation), String> {
    let mut package = String::new();
    let mut location = ArtifactLocation::default();
    let mut d = Decoder::new(bytes, base);
    while let Some(f) = d.next()? {
        match (f.number, f.value) {
            (2, WireValue::Bytes(s, _)) => package = utf8(s, f.offset)?.to_owned(),
            (3, WireValue::Bytes(m, offset)) => artifact_location(m, offset, &mut location)?,
            _ => {}
        }
    }
    Ok((package, location))
}

/// `blaze.ArtifactLocation` (common.proto); a repeated occurrence merges into `location`.
fn artifact_location(
    bytes: &[u8],
    base: usize,
    location: &mut ArtifactLocation,
) -> Result<(), String> {
    let mut d = Decoder::new(bytes, base);
    while let Some(f) = d.next()? {
        match (f.number, f.value) {
            (2, WireValue::Bytes(s, _)) => location.relative_path = utf8(s, f.offset)?.to_owned(),
            (3, WireValue::Varint(v)) => location.is_source = v != 0,
            (4, WireValue::Bytes(s, _)) => {
                location.root_execution_path_fragment = utf8(s, f.offset)?.to_owned()
            }
            (5, WireValue::Varint(v)) => location.is_external = v != 0,
            _ => {}
        }
    }
    Ok(())
}

fn utf8(bytes: &[u8], offset: usize) -> Result<&str, String> {
    std::str::from_utf8(bytes)
        .map_err(|_| format!("byte {offset}: string field is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/bin")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn bytes_field(number: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3 | 2, &mut out);
        varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    fn varint_field(number: u64, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(number << 3, &mut out);
        varint(value, &mut out);
        out
    }

    fn dependency(path: &str, kind: u64) -> Vec<u8> {
        bytes_field(
            1,
            &[bytes_field(1, path.as_bytes()), varint_field(2, kind)].concat(),
        )
    }

    const BIN: &str = "bazel-out/darwin_arm64-fastbuild/bin";

    #[test]
    fn reads_jdeps_fixtures() {
        let maven = format!("{BIN}/external/maven");
        assert_eq!(
            parse_jdeps(&fixture("liballocadia-util.jdeps")).unwrap(),
            [
                format!("{maven}/com/google/guava/guava/33.7.2-jre/header_guava-33.7.2-jre.jar"),
                format!(
                    "{maven}/org/apache/commons/commons-lang3/3.20.0/header_commons-lang3-3.20.0.jar"
                ),
                format!(
                    "{maven}/org/springframework/spring-core/7.0.9/header_spring-core-7.0.9.jar"
                ),
                format!("{maven}/org/springframework/spring-web/7.0.9/header_spring-web-7.0.9.jar"),
            ]
        );
        assert_eq!(
            parse_jdeps(&fixture("libkpi.jdeps")).unwrap(),
            [
                format!(
                    "{BIN}/projects/uptempo/performance-management/src/main/java/io/uptempo/perf/domain/libdomain-ijar.jar"
                ),
                format!("{maven}/org/projectlombok/lombok/1.18.48/header_lombok-1.18.48.jar"),
                format!(
                    "{maven}/org/springframework/spring-context/7.0.9/header_spring-context-7.0.9.jar"
                ),
                format!(
                    "{maven}/org/springframework/spring-core/7.0.9/header_spring-core-7.0.9.jar"
                ),
            ]
        );
    }

    #[test]
    fn keeps_only_explicit_and_implicit_deps() {
        let bytes = [
            dependency("explicit.jar", 0),
            dependency("implicit.jar", 1),
            dependency("unused.jar", 2),
            dependency("incomplete.jar", 3),
            bytes_field(2, b"//a:a"),
            varint_field(3, 1),
            bytes_field(4, b"com.a"),
        ]
        .concat();
        assert_eq!(
            parse_jdeps(&bytes).unwrap(),
            ["explicit.jar", "implicit.jar"]
        );
        // The low 32 bits of 2^32 + 1 are IMPLICIT.
        assert_eq!(
            parse_jdeps(&dependency("wide.jar", (1 << 32) + 1)).unwrap(),
            ["wide.jar"]
        );
        assert_eq!(parse_jdeps(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn jdeps_skips_unknown_and_mismatched_fields() {
        let location = bytes_field(3, &[bytes_field(1, b"A.java"), varint_field(2, 4)].concat());
        let mut fixed: Vec<u8> = vec![(9 << 3) | 1];
        fixed.extend_from_slice(&[0; 8]);
        fixed.push((10 << 3) | 5);
        fixed.extend_from_slice(&[0; 4]);
        // Group 11 holding a varint field and a nested group 12.
        let group: Vec<u8> = vec![
            (11 << 3) | 3,
            1 << 3,
            5,
            (12 << 3) | 3,
            (12 << 3) | 4,
            (11 << 3) | 4,
        ];
        let dep = bytes_field(
            1,
            &[
                bytes_field(1, b"a.jar"),
                varint_field(2, 0),
                location,
                fixed,
                group.clone(),
                varint_field(1, 7),
            ]
            .concat(),
        );
        // `varint_field(1, 7)` has the wrong wire type for `path`: unknown, so `a.jar` stays.
        assert_eq!(parse_jdeps(&[group, dep].concat()).unwrap(), ["a.jar"]);
    }

    #[test]
    fn jdeps_required_fields() {
        let no_kind = bytes_field(1, &bytes_field(1, b"a.jar"));
        assert!(
            parse_jdeps(&no_kind)
                .unwrap_err()
                .contains("required `kind`")
        );
        let no_path = bytes_field(1, &varint_field(2, 0));
        assert!(
            parse_jdeps(&no_path)
                .unwrap_err()
                .contains("required `path`")
        );
        let unknown_kind = dependency("a.jar", 7);
        assert!(
            parse_jdeps(&unknown_kind)
                .unwrap_err()
                .contains("required `kind`")
        );
    }

    #[test]
    fn jdeps_paths_decode_lossily() {
        let bytes = bytes_field(
            1,
            &[bytes_field(1, b"a\xffb.jar"), varint_field(2, 0)].concat(),
        );
        assert_eq!(parse_jdeps(&bytes).unwrap(), ["a\u{FFFD}b.jar"]);
    }

    #[test]
    fn reads_package_manifest_fixture() {
        let sources = parse_package_manifest(&fixture("allocadia-util.java-manifest")).unwrap();
        assert_eq!(sources.len(), 14);
        let src = "libs/commons/allocadia-util/src/main/java/com/allocadia/util";
        assert_eq!(
            sources[0],
            (
                "com.allocadia.util".to_owned(),
                ArtifactLocation {
                    relative_path: format!("{src}/FixedValueFuture.java"),
                    is_source: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(sources[1].0, "com.allocadia.util.impl");
        assert_eq!(
            sources[1].1.relative_path,
            format!("{src}/impl/ThreadClockImpl.java")
        );
        assert_eq!(
            sources[13].1.relative_path,
            format!("{src}/StringSanitizer.java")
        );
        assert!(
            sources
                .iter()
                .all(|(_, l)| l.is_main_repo_source() && l.root_execution_path_fragment.is_empty())
        );
    }

    #[test]
    fn package_manifest_merges_and_defaults() {
        let first = bytes_field(
            3,
            &[bytes_field(2, b"a/A.java"), varint_field(5, 1)].concat(),
        );
        let second = bytes_field(
            3,
            &[bytes_field(4, b"bazel-out/bin"), varint_field(3, 1)].concat(),
        );
        let source = bytes_field(1, &[bytes_field(2, b"a"), first, second].concat());
        let empty = bytes_field(1, &[]);
        assert_eq!(
            parse_package_manifest(&[source, empty].concat()).unwrap(),
            [
                (
                    "a".to_owned(),
                    ArtifactLocation {
                        root_execution_path_fragment: "bazel-out/bin".into(),
                        relative_path: "a/A.java".into(),
                        is_source: true,
                        is_external: true,
                    }
                ),
                (String::new(), ArtifactLocation::default()),
            ]
        );
    }

    #[test]
    fn package_manifest_rejects_invalid_utf8() {
        let bytes = bytes_field(1, &bytes_field(2, b"com.\xff"));
        assert_eq!(
            parse_package_manifest(&bytes).unwrap_err(),
            "byte 2: string field is not valid UTF-8"
        );
        let nested = bytes_field(1, &bytes_field(3, &bytes_field(2, b"\xc3")));
        assert_eq!(
            parse_package_manifest(&nested).unwrap_err(),
            "byte 4: string field is not valid UTF-8"
        );
    }

    #[test]
    fn malformed_wire_data_is_an_error() {
        for (bytes, expected) in [
            (vec![0x0a], "byte 1: truncated varint"),
            (
                vec![0x0a, 0x05, 1, 2],
                "byte 2: truncated length-delimited field",
            ),
            (vec![0x80; 11], "byte 0: varint longer than 10 bytes"),
            (vec![0x00], "byte 0: invalid field number"),
            (vec![0x0e], "byte 0: invalid wire type 6"),
            (vec![0x0f], "byte 0: invalid wire type 7"),
            (vec![0x0c], "byte 0: end of group 1 without a start"),
            (vec![0x0b, 0x08, 0x01], "byte 0: group 1 is not closed"),
            (vec![0x0b, 0x14], "byte 1: mismatched end of group"),
            (vec![0x09, 0, 0], "byte 1: truncated fixed64"),
            (vec![0x0d, 0], "byte 1: truncated fixed32"),
            (
                [vec![0x0a, 0x02], vec![0x0a, 0x05]].concat(),
                "byte 4: truncated length-delimited field",
            ),
            (
                vec![0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f],
                "byte 6: truncated length-delimited field",
            ),
        ] {
            assert_eq!(parse_jdeps(&bytes).unwrap_err(), expected, "{bytes:x?}");
            assert_eq!(
                parse_package_manifest(&bytes).unwrap_err(),
                expected,
                "{bytes:x?}"
            );
        }
        let deep: Vec<u8> = std::iter::repeat_n(0x0b, MAX_GROUP_DEPTH + 1).collect();
        assert!(
            parse_jdeps(&deep)
                .unwrap_err()
                .contains("groups nested too deeply")
        );
    }

    #[test]
    fn truncated_fixtures_fail_without_panicking() {
        type Parse = fn(&[u8]) -> Result<usize, String>;
        let cases: [(&str, Parse); 3] = [
            ("liballocadia-util.jdeps", |b| {
                parse_jdeps(b).map(|v| v.len())
            }),
            ("libkpi.jdeps", |b| parse_jdeps(b).map(|v| v.len())),
            ("allocadia-util.java-manifest", |b| {
                parse_package_manifest(b).map(|v| v.len())
            }),
        ];
        for (name, parse) in cases {
            let bytes = fixture(name);
            let full = parse(&bytes).unwrap();
            let mut errors = 0;
            for len in 0..bytes.len() {
                match parse(&bytes[..len]) {
                    Ok(n) => assert!(n <= full, "{name}[..{len}]"),
                    Err(_) => errors += 1,
                }
            }
            // Only cuts between top-level fields parse.
            assert!(
                errors > bytes.len() * 9 / 10,
                "{name}: {errors} of {}",
                bytes.len()
            );
        }
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for round in 0..2000 {
            let len = round % 97;
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            let _ = parse_jdeps(&bytes);
            let _ = parse_package_manifest(&bytes);
        }
    }

    #[test]
    #[ignore = "reads a real bazel-bin"]
    fn real_jdeps_and_manifests_parse() {
        let jdeps = crate::ideinfo::test_support::real_files(".jdeps");
        let manifests = crate::ideinfo::test_support::real_files(".java-manifest");
        assert!(!jdeps.is_empty() && !manifests.is_empty());
        for path in &jdeps {
            parse_jdeps(&std::fs::read(path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        for path in &manifests {
            parse_package_manifest(&std::fs::read(path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        println!(
            "{} jdeps and {} package manifests parsed",
            jdeps.len(),
            manifests.len()
        );
    }
}
