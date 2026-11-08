//! jdeps files: the binary `blaze_deps.Dependencies` message of `deps.proto`, read as
//! `JdepsFileReader` does (bazelbuild/intellij, ijwb branch).

/// The jar paths (execution-root-relative) of the EXPLICIT and IMPLICIT dependencies, in file order.
/// Same shape as `crate::wire::parse_jdeps`, which may replace it.
pub(crate) fn parse_jdeps(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut r = Reader { buf: bytes, pos: 0 };
    while let Some((field, wire)) = r.tag()? {
        if field == 1 && wire == LEN {
            let dep = r.len_delimited()?;
            if let Some(path) = dependency(dep)? {
                out.push(path);
            }
        } else {
            r.skip(wire)?;
        }
    }
    Ok(out)
}

const VARINT: u8 = 0;
const I64: u8 = 1;
const LEN: u8 = 2;
const I32: u8 = 5;

/// `Dependency { required string path = 1; required Kind kind = 2; }`: the path when the kind is
/// EXPLICIT (0, also proto2's default) or IMPLICIT (1); UNUSED and INCOMPLETE are dropped.
fn dependency(bytes: &[u8]) -> Result<Option<String>, String> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let mut path = None;
    let mut kind = 0;
    while let Some((field, wire)) = r.tag()? {
        match (field, wire) {
            (1, LEN) => {
                let raw = r.len_delimited()?;
                let text = std::str::from_utf8(raw)
                    .map_err(|_| "dependency path is not UTF-8".to_owned())?;
                path = Some(text);
            }
            (2, VARINT) => kind = r.varint()?,
            _ => r.skip(wire)?,
        }
    }
    let path = path.ok_or_else(|| "dependency without a path".to_owned())?;
    Ok(matches!(kind, 0 | 1).then(|| path.to_owned()))
}

struct Reader<'b> {
    buf: &'b [u8],
    pos: usize,
}

impl<'b> Reader<'b> {
    fn tag(&mut self) -> Result<Option<(u64, u8)>, String> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = key >> 3;
        if field == 0 {
            return Err(format!("invalid field number 0 at byte {}", self.pos));
        }
        Ok(Some((field, (key & 7) as u8)))
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.buf.get(self.pos).ok_or("truncated varint")?;
            self.pos += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(format!("varint longer than 10 bytes at byte {}", self.pos))
    }

    fn len_delimited(&mut self) -> Result<&'b [u8], String> {
        let len = self.varint()?;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| self.pos.checked_add(len))
            .filter(|&end| end <= self.buf.len())
            .ok_or_else(|| {
                format!(
                    "field of {len} bytes runs past the end at byte {}",
                    self.pos
                )
            })?;
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn skip(&mut self, wire: u8) -> Result<(), String> {
        let fixed = match wire {
            VARINT => return self.varint().map(drop),
            LEN => return self.len_delimited().map(drop),
            I64 => 8,
            I32 => 4,
            other => {
                return Err(format!(
                    "unsupported wire type {other} at byte {}",
                    self.pos
                ));
            }
        };
        if self.buf.len() - self.pos < fixed {
            return Err("truncated fixed-width field".to_owned());
        }
        self.pos += fixed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/data/bin/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn len_field(field: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut v = vec![(field << 3) | LEN, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    fn dep(path: &str, kind: Option<u8>) -> Vec<u8> {
        let mut d = len_field(1, path.as_bytes());
        if let Some(k) = kind {
            d.extend_from_slice(&[2 << 3, k]);
        }
        len_field(1, &d)
    }

    #[test]
    fn decodes_allocadia_util_jdeps() {
        let paths = parse_jdeps(&fixture("liballocadia-util.jdeps")).unwrap();
        assert_eq!(
            paths,
            [
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/com/google/guava/guava/33.7.2-jre/header_guava-33.7.2-jre.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/apache/commons/commons-lang3/3.20.0/header_commons-lang3-3.20.0.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/springframework/spring-core/7.0.9/header_spring-core-7.0.9.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/springframework/spring-web/7.0.9/header_spring-web-7.0.9.jar",
            ]
        );
    }

    #[test]
    fn decodes_kpi_jdeps() {
        let paths = parse_jdeps(&fixture("libkpi.jdeps")).unwrap();
        assert_eq!(
            paths,
            [
                "bazel-out/darwin_arm64-fastbuild/bin/projects/uptempo/performance-management/src/main/java/io/uptempo/perf/domain/libdomain-ijar.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/projectlombok/lombok/1.18.48/header_lombok-1.18.48.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/springframework/spring-context/7.0.9/header_spring-context-7.0.9.jar",
                "bazel-out/darwin_arm64-fastbuild/bin/external/maven/org/springframework/spring-core/7.0.9/header_spring-core-7.0.9.jar",
            ]
        );
    }

    #[test]
    fn keeps_explicit_and_implicit_only() {
        let mut bytes = dep("a.jar", Some(0));
        bytes.extend(dep("b.jar", Some(1)));
        bytes.extend(dep("unused.jar", Some(2)));
        bytes.extend(dep("incomplete.jar", Some(3)));
        bytes.extend(dep("default.jar", None));
        bytes.extend(len_field(2, b"//a:b"));
        bytes.extend([3 << 3, 1]);
        assert_eq!(
            parse_jdeps(&bytes).unwrap(),
            ["a.jar", "b.jar", "default.jar"]
        );
        assert_eq!(parse_jdeps(&[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn rejects_malformed_input() {
        let full = dep("a.jar", Some(0));
        assert!(parse_jdeps(&full[..full.len() - 1]).is_err());
        assert!(parse_jdeps(&[0x0A]).is_err());
        assert!(parse_jdeps(&[0x0A, 0xFF]).is_err());
        assert!(parse_jdeps(&[(1 << 3) | 3]).is_err());
        assert!(parse_jdeps(&[0x00]).is_err());
        assert!(parse_jdeps(&[(5 << 3) | I64, 1, 2]).is_err());
        assert!(parse_jdeps(&len_field(1, &[2 << 3, 0])).is_err());
        assert!(parse_jdeps(&len_field(1, &len_field(1, &[0xC3, 0x28]))).is_err());
        assert!(
            parse_jdeps(&[
                0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01
            ])
            .is_err()
        );
    }
}
