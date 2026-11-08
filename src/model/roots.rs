//! Source folders from the packages of source directories: port of the directory-tree voting of
//! `SourceDirectoryCalculator` (bazelbuild/intellij, ijwb branch).

use crate::fxhash::FxHashMap;

use super::package::package_prefix_of;

/// A directory holding sources of the project, with the package its sources declare.
#[derive(Debug)]
pub(crate) struct DirPackage<'a> {
    /// Workspace-relative, at or below the content root.
    pub dir: &'a str,
    pub package: String,
}

/// The (directory, package prefix) source folders of one content root
/// (`SourceDirectoryCalculator.calculateSourceDirectoriesForContentRoot`), sorted by directory.
///
/// Every directory votes its package (and the parent packages its path agrees with) up the tree;
/// a depth-first walk then emits a folder wherever the winning prefix is not the one inherited
/// from the folder above. Without sources, the content root itself is the folder.
pub(crate) fn content_root_folders<'a>(
    root: &'a str,
    dirs: &'a [DirPackage<'a>],
) -> Vec<(&'a str, String)> {
    let mut tree = Tree::new(root);
    for d in dirs {
        tree.insert(root, d.dir);
    }
    for d in dirs {
        tree.vote(root, d);
    }
    let mut out = Vec::new();
    tree.visit(0, None, &mut out);
    if out.is_empty() {
        out.push((root, package_prefix_of(root)));
    }
    out.sort_unstable_by(|a, b| a.0.cmp(b.0));
    out
}

struct Node<'a> {
    path: &'a str,
    name: &'a str,
    children: Vec<usize>,
    /// The package of the node's own sources, which wins over any vote.
    forced: Option<&'a str>,
    votes: Vec<(&'a str, u32)>,
}

struct Tree<'a> {
    nodes: Vec<Node<'a>>,
    by_path: FxHashMap<&'a str, usize>,
}

impl<'a> Tree<'a> {
    fn new(root: &'a str) -> Self {
        let node = Node {
            path: root,
            name: "",
            children: Vec::new(),
            forced: None,
            votes: Vec::new(),
        };
        Self {
            nodes: vec![node],
            by_path: FxHashMap::from_iter([(root, 0)]),
        }
    }

    /// Adds the directories from the content root down to `dir`.
    fn insert(&mut self, root: &'a str, dir: &'a str) {
        let mut parent = 0;
        for (end, name) in components(root, dir) {
            let path = &dir[..end];
            let next = match self.by_path.get(path) {
                Some(&i) => i,
                None => {
                    let i = self.nodes.len();
                    self.nodes.push(Node {
                        path,
                        name,
                        children: Vec::new(),
                        forced: None,
                        votes: Vec::new(),
                    });
                    self.by_path.insert(path, i);
                    self.nodes[parent].children.push(i);
                    i
                }
            };
            parent = next;
        }
    }

    /// Walks up from `d.dir` while the trailing path and package components agree, forcing the
    /// package on the directory itself and voting the shortened package on each ancestor.
    fn vote(&mut self, root: &'a str, d: &'a DirPackage<'a>) {
        let path: Vec<(usize, &str)> = components(root, d.dir).collect();
        // Guava's `Splitter.on('.')` yields one empty component for the default package.
        let package: Vec<(usize, &str)> = d
            .package
            .split('.')
            .scan(0usize, |start, part| {
                let end = *start + part.len();
                *start = end + 1;
                Some((end, part))
            })
            .collect();
        let (mut pi, mut ki) = (path.len(), package.len());
        loop {
            let node_path = if pi == 0 {
                root
            } else {
                &d.dir[..path[pi - 1].0]
            };
            let prefix = if ki == 0 {
                ""
            } else {
                &d.package[..package[ki - 1].0]
            };
            let node = &mut self.nodes[self.by_path[node_path]];
            if node_path == d.dir {
                node.forced = Some(prefix);
            } else {
                match node.votes.iter_mut().find(|(p, _)| *p == prefix) {
                    Some((_, count)) => *count += 1,
                    None => node.votes.push((prefix, 1)),
                }
            }
            let path_part = if pi == 0 { "" } else { path[pi - 1].1 };
            let package_part = if ki == 0 { "" } else { package[ki - 1].1 };
            if path_part != package_part || pi == 0 || ki == 0 {
                break;
            }
            pi -= 1;
            ki -= 1;
        }
    }

    /// `SourceDirectoryCalculator.visitDirectoryNode`.
    fn visit(&self, i: usize, inherited: Option<&str>, out: &mut Vec<(&'a str, String)>) {
        let node = &self.nodes[i];
        let chosen = node
            .forced
            .or_else(|| most_frequent(&node.votes, node.path));
        let prefix = chosen.or(inherited);
        if let Some(p) = prefix
            && Some(p) != inherited
        {
            out.push((node.path, p.to_owned()));
        }
        for &c in &node.children {
            let child = &self.nodes[c];
            let compatible = prefix.map(|p| {
                if p.is_empty() {
                    child.name.to_owned()
                } else {
                    format!("{p}.{}", child.name)
                }
            });
            self.visit(c, compatible.as_deref(), out);
        }
    }
}

/// `pickMostFrequentlyOccurring`: the most voted prefix; a tie goes to the prefix the directory's
/// layout implies, then (where Java's hash order decides) to the smallest prefix.
fn most_frequent<'a>(votes: &[(&'a str, u32)], dir: &str) -> Option<&'a str> {
    let best = votes.iter().map(|&(_, n)| n).max()?;
    let mut tied = votes.iter().filter(|&&(_, n)| n == best).map(|&(p, _)| p);
    let first = tied.next()?;
    let rest: Vec<&str> = tied.collect();
    if rest.is_empty() {
        return Some(first);
    }
    let preferred = package_prefix_of(dir);
    std::iter::once(first)
        .chain(rest)
        .min_by_key(|&p| (p != preferred, p))
}

/// `(end offset in dir, name)` of each component of `dir` below `root`.
fn components<'a>(root: &str, dir: &'a str) -> impl Iterator<Item = (usize, &'a str)> {
    let start = if root.is_empty() {
        0
    } else {
        (root.len() + 1).min(dir.len())
    };
    let rel = &dir[start..];
    rel.split('/')
        .scan(start, |offset, part| {
            let end = *offset + part.len();
            *offset = end + 1;
            Some((end, part))
        })
        .filter(|(_, part)| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dp(dir: &'static str, package: &str) -> DirPackage<'static> {
        DirPackage {
            dir,
            package: package.into(),
        }
    }

    fn folders(
        root: &'static str,
        dirs: &'static [DirPackage<'static>],
    ) -> Vec<(&'static str, String)> {
        content_root_folders(root, dirs)
    }

    fn owned(v: Vec<(&str, String)>) -> Vec<(String, String)> {
        v.into_iter().map(|(a, b)| (a.to_owned(), b)).collect()
    }

    fn expect(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|&(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    }

    #[test]
    fn maven_layout_under_workspace_root() {
        let dirs = Box::leak(Box::new([
            dp(
                "libs/u/src/main/java/com/allocadia/util",
                "com.allocadia.util",
            ),
            dp(
                "libs/u/src/main/java/com/allocadia/util/impl",
                "com.allocadia.util.impl",
            ),
            dp(
                "libs/u/src/test/java/com/allocadia/util",
                "com.allocadia.util",
            ),
        ]));
        assert_eq!(
            owned(folders("", dirs)),
            expect(&[("libs/u/src/main/java", ""), ("libs/u/src/test/java", "")])
        );
    }

    #[test]
    fn package_not_matching_layout_gets_a_prefix() {
        let dirs = Box::leak(Box::new([dp("tools/rules/war", "com.allocadia.tools.war")]));
        assert_eq!(
            owned(folders("", dirs)),
            expect(&[("tools/rules", "com.allocadia.tools")])
        );
    }

    #[test]
    fn bazel_layout_and_nested_content_root() {
        let dirs = Box::leak(Box::new([
            dp("java/com/google/a", "com.google.a"),
            dp("java/com/google/b", "com.google.b"),
        ]));
        assert_eq!(owned(folders("", dirs)), expect(&[("java", "")]));
        let dirs = Box::leak(Box::new([dp("proj/java/com/x", "com.x")]));
        assert_eq!(owned(folders("proj", dirs)), expect(&[("proj/java", "")]));
    }

    #[test]
    fn default_package_directory_is_its_own_folder() {
        let dirs = Box::leak(Box::new([
            dp("scripts", ""),
            dp("app/src/main/java/com/a", "com.a"),
        ]));
        assert_eq!(
            owned(folders("", dirs)),
            expect(&[("app/src/main/java", ""), ("scripts", "")])
        );
    }

    #[test]
    fn sources_in_the_content_root_itself() {
        let dirs = Box::leak(Box::new([dp("proj", "com.p")]));
        assert_eq!(owned(folders("proj", dirs)), expect(&[("proj", "com.p")]));
    }

    #[test]
    fn mismatched_subdirectory_becomes_nested_folder() {
        let dirs = Box::leak(Box::new([
            dp("a/src/main/java/com/x", "com.x"),
            dp("a/src/main/java/com/x/odd", "org.other"),
        ]));
        assert_eq!(
            owned(folders("", dirs)),
            expect(&[
                ("a/src/main/java", ""),
                ("a/src/main/java/com/x/odd", "org.other")
            ])
        );
    }

    #[test]
    fn majority_vote_decides_ancestors() {
        // Two directories put `src` at the root of `com`, one says it is `org.foo.com`'s parent.
        let dirs = Box::leak(Box::new([
            dp("m/src/com/a", "com.a"),
            dp("m/src/com/b", "com.b"),
            dp("m/src/com/c", "org.foo.com.c"),
        ]));
        assert_eq!(
            owned(folders("", dirs)),
            expect(&[("m/src", ""), ("m/src/com/c", "org.foo.com.c")])
        );
    }

    #[test]
    fn ties_prefer_the_layout_package() {
        assert_eq!(
            most_frequent(&[("b", 1), ("com.x", 1)], "p/src/main/java/com/x"),
            Some("com.x")
        );
        assert_eq!(most_frequent(&[("b", 1), ("a", 1)], "p/q"), Some("a"));
        assert_eq!(most_frequent(&[("b", 2), ("a", 1)], "p/q"), Some("b"));
        assert_eq!(most_frequent(&[], "p/q"), None);
    }

    #[test]
    fn no_sources_falls_back_to_the_content_root() {
        assert_eq!(
            owned(folders("docs/java", &[])),
            expect(&[("docs/java", "")])
        );
        assert_eq!(owned(folders("", &[])), expect(&[("", "")]));
        assert_eq!(owned(folders("tools", &[])), expect(&[("tools", "tools")]));
    }
}
