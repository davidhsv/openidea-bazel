//! Project-view globs (`test_sources`, `excluded_libraries`): port of `Glob.GlobSet` and
//! `SourceTestConfig` (bazelbuild/intellij, ijwb branch).

/// Whether `text` matches `pattern`, where `*` is any run of characters (`/` included) and `?`
/// is one character, as IntelliJ's wildcard file-name matcher does.
pub(crate) fn matches(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    // Where to resume after the last `*`: (pattern index after it, text index it has consumed up to).
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some(b'*') => {
                pi += 1;
                star = Some((pi, ti));
            }
            Some(b'?') => {
                pi += 1;
                ti += utf8_len(t[ti]);
            }
            Some(&c) if c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((resume, consumed)) => {
                    let consumed = consumed + utf8_len(t[consumed]);
                    star = Some((resume, consumed));
                    pi = resume;
                    ti = consumed;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

/// A set of globs; matches when any of them does.
#[derive(Debug, Default)]
pub(crate) struct GlobSet(Vec<String>);

impl GlobSet {
    pub(crate) fn new(patterns: &[String]) -> Self {
        Self(patterns.to_vec())
    }

    /// `SourceTestConfig`: `test_sources` entries `path`, `path/` and `path/*` all become `path*`.
    pub(crate) fn test_sources(patterns: &[String]) -> Self {
        Self(patterns.iter().map(|p| test_source_pattern(p)).collect())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn matches(&self, text: &str) -> bool {
        self.0.iter().any(|p| matches(p, text))
    }
}

/// `SourceTestConfig.modifyPattern`: drop one trailing `*`, then one trailing `/`, then append `*`.
fn test_source_pattern(pattern: &str) -> String {
    let pattern = pattern.trim();
    let pattern = pattern.strip_suffix('*').unwrap_or(pattern);
    let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
    format!("{pattern}*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_spans_directories() {
        assert!(matches("*/src/test/java*", "libs/a/src/test/java"));
        assert!(matches("*/src/test/java*", "libs/a/src/test/java/com/x"));
        assert!(!matches("*/src/test/java*", "libs/a/src/main/java"));
        assert!(!matches("*/src/test/java*", "src/test/java"));
    }

    #[test]
    fn question_mark_is_one_character() {
        assert!(matches("a?c", "abc"));
        assert!(matches("a?c", "aéc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
    }

    #[test]
    fn literal_and_edge_patterns() {
        assert!(matches("", ""));
        assert!(!matches("", "a"));
        assert!(matches("*", ""));
        assert!(matches("**", "x/y"));
        assert!(matches("*guava*", "external/maven/header_guava-33.jar"));
        assert!(!matches("abc", "abd"));
        assert!(matches("a*b*c", "aXbYbZc"));
        assert!(!matches("a*b*c", "aXbYbZ"));
        assert!(matches("é*", "éa"));
    }

    #[test]
    fn test_source_patterns_are_normalized() {
        assert_eq!(test_source_pattern("javatests/"), "javatests*");
        assert_eq!(test_source_pattern("javatests/*"), "javatests*");
        assert_eq!(test_source_pattern("javatests"), "javatests*");
        assert_eq!(test_source_pattern("*/src/test/java*"), "*/src/test/java*");
        let set = GlobSet::test_sources(&["javatests/".into()]);
        assert!(set.matches("javatests"));
        assert!(set.matches("javatests/com/x"));
        assert!(set.matches("javatests_foo"));
        assert!(!set.matches("java/com"));
        assert!(!GlobSet::test_sources(&[]).matches("javatests"));
    }
}
