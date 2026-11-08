//! Java packages: the declaration scanner of `JavaSourcePackageReader` and the path heuristic of
//! `PackagePrefixCalculator` (bazelbuild/intellij, ijwb branch).

/// Directory layouts whose remainder is the package (`PackagePrefixCalculator`).
const SOURCE_ROOT_MARKERS: [&str; 8] = [
    "java/",
    "javatests/",
    "src/main/java/",
    "src/test/java/",
    "src/main/scala/",
    "src/test/scala/",
    "src/main/kotlin/",
    "src/test/kotlin/",
];

/// The package a directory's layout implies: the part after the first source-root marker
/// (`src/main/java/`, `java/`, …), `/` → `.`; the whole path when there is no marker.
///
/// `PackagePrefixCalculator` only looks for the markers at the start of the workspace path.
/// In a Maven-style monorepo (`libs/x/src/main/java/com/…`) that yields `libs.x.src.main.java.com…`,
/// and a source file whose package falls back to it then votes the content root itself in as a
/// source root that nests every other one, which Eclipse rejects. Markers are therefore matched at
/// any component boundary, and `dir` is treated as a directory (`x/src/main/java` → `""`).
pub(crate) fn package_prefix_of(dir: &str) -> String {
    let skip = marker_end(dir).unwrap_or(0);
    dir.get(skip..).unwrap_or_default().replace('/', ".")
}

/// The byte offset just past the leftmost component-aligned source-root marker of `dir`.
fn marker_end(dir: &str) -> Option<usize> {
    let starts = std::iter::once(0).chain(dir.match_indices('/').map(|(i, _)| i + 1));
    for start in starts {
        let rest = &dir[start..];
        for marker in SOURCE_ROOT_MARKERS {
            let name = &marker[..marker.len() - 1];
            if rest.starts_with(marker) {
                return Some(start + marker.len());
            }
            if rest == name {
                return Some(dir.len());
            }
        }
    }
    None
}

/// The declared package of a Java source: `^\s*package\s+([\w\.]+)` (`JavaSourcePackageReader`),
/// scanning only the head of the file. Whitespace, a byte-order mark, comments and annotations
/// (`package-info.java`) may precede the declaration; any other token means the file has none.
pub fn declared_package(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let mut i = if b.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    loop {
        i = skip_trivia(b, i)?;
        if b.get(i) != Some(&b'@') {
            break;
        }
        i = skip_trivia(b, i + 1)?;
        let name_end = qualified_name_end(b, i);
        if name_end == i || &b[i..name_end] == b"interface" {
            return None;
        }
        i = skip_trivia(b, name_end)?;
        if b.get(i) == Some(&b'(') {
            i = skip_parenthesized(b, i)?;
        }
    }
    let rest = b.get(i..)?;
    if !rest.starts_with(b"package") || rest.get(7).is_some_and(|&c| is_ident_byte(c)) {
        return None;
    }
    let start = skip_trivia(b, i + 7)?;
    if start == i + 7 {
        return None;
    }
    let end = (start..b.len())
        .find(|&j| !(is_ident_byte(b[j]) || b[j] == b'.'))
        .unwrap_or(b.len());
    (end > start).then(|| &text[start..end])
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// The end of `ident(.ident)*` starting at `i` (whitespace and comments allowed around dots).
fn qualified_name_end(b: &[u8], mut i: usize) -> usize {
    let mut end = i;
    loop {
        let ident_end = (i..b.len())
            .find(|&j| !is_ident_byte(b[j]))
            .unwrap_or(b.len());
        if ident_end == i {
            return end;
        }
        end = ident_end;
        match skip_trivia(b, ident_end) {
            Some(dot) if b.get(dot) == Some(&b'.') => match skip_trivia(b, dot + 1) {
                Some(next) => i = next,
                None => return end,
            },
            _ => return end,
        }
    }
}

/// Skips whitespace and comments; `None` when a block comment never ends.
fn skip_trivia(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        match (b.get(i), b.get(i + 1)) {
            (Some(c), _) if c.is_ascii_whitespace() => i += 1,
            (Some(b'/'), Some(b'/')) => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |n| i + n + 1);
            }
            (Some(b'/'), Some(b'*')) => {
                let close = b[i + 2..].windows(2).position(|w| w == b"*/")?;
                i += 2 + close + 2;
            }
            _ => return Some(i),
        }
    }
}

/// Skips an annotation's balanced `( … )` starting at `b[i] == '('`, through string, character
/// and text-block literals and comments; `None` when it never closes.
fn skip_parenthesized(b: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                let close = b[i + 3..].windows(3).position(|w| w == b"\"\"\"")?;
                i += 3 + close + 3;
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                loop {
                    match b.get(i)? {
                        b'\\' => i += 2,
                        &c if c == quote => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'/' if matches!(b.get(i + 1), Some(b'/' | b'*')) => i = skip_trivia(b, i)?,
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_prefix_from_layout() {
        assert_eq!(package_prefix_of("libs/a/src/main/java/com/x"), "com.x");
        assert_eq!(package_prefix_of("libs/a/src/test/java/com/x"), "com.x");
        assert_eq!(package_prefix_of("libs/a/src/main/java"), "");
        assert_eq!(package_prefix_of("java/com/google"), "com.google");
        assert_eq!(package_prefix_of("javatests/com/google"), "com.google");
        assert_eq!(package_prefix_of("tools/rules/war"), "tools.rules.war");
        assert_eq!(package_prefix_of(""), "");
        // Only whole components count; the leftmost marker wins.
        assert_eq!(package_prefix_of("myjava/com"), "myjava.com");
        assert_eq!(
            package_prefix_of("a/java/b/src/main/java/c"),
            "b.src.main.java.c"
        );
    }

    #[test]
    fn plain_declaration() {
        assert_eq!(
            declared_package("package com.allocadia.util;\n\nimport x;"),
            Some("com.allocadia.util")
        );
        assert_eq!(declared_package("  package\tcom.a ;"), Some("com.a"));
        assert_eq!(declared_package("package com.a\r\n;"), Some("com.a"));
        assert_eq!(declared_package("package a.b$c.d_1;"), Some("a.b$c.d_1"));
    }

    #[test]
    fn skips_bom_comments_and_annotations() {
        let text = "\u{FEFF}/*\n * Copyright\n * package not.this;\n */\n// package nor.this;\n\
                    @ParametersAreNonnullByDefault\n@javax.annotation.Generated(value = \"pa)ckage x\", \
                    comments = {'(', \"\\\")\"}) /* ) */\npackage com.a.b;";
        assert_eq!(declared_package(text), Some("com.a.b"));
        assert_eq!(
            declared_package("@Deprecated(\"\"\"\n  )\n  \"\"\")\npackage com.t;"),
            Some("com.t")
        );
        assert_eq!(
            declared_package("@ com . foo . Ann\npackage com.q;"),
            Some("com.q")
        );
    }

    #[test]
    fn no_declaration() {
        assert_eq!(declared_package(""), None);
        assert_eq!(declared_package("\n\n"), None);
        assert_eq!(declared_package("import java.util.List;\nclass A {}"), None);
        assert_eq!(declared_package("public class A {}\n// package a.b;"), None);
        assert_eq!(declared_package("@interface Ann {}"), None);
        assert_eq!(declared_package("packages a;"), None);
        assert_eq!(declared_package("package;"), None);
        assert_eq!(declared_package("package"), None);
        assert_eq!(declared_package("/* unterminated package a;"), None);
        assert_eq!(declared_package("@Ann(\"unterminated)\npackage a;"), None);
        assert_eq!(declared_package("@Ann(x\npackage a;"), None);
        assert_eq!(declared_package("module com.a { }"), None);
    }
}
