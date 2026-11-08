//! The Java project model: port of `JavaSourceFilter`, `SourceDirectoryCalculator` and
//! `BlazeJavaWorkspaceImporter` (bazelbuild/intellij, ijwb branch).

mod classify;
mod glob;
mod jdeps;
mod libs;
mod package;
mod paths;
mod roots;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use crate::fxhash::FxHashMap;

use classify::{Classified, classify, java_sources};
use glob::GlobSet;
use libs::LibraryInputs;
use package::{declared_package, package_prefix_of};
use paths::{PathDecoder, is_under, parent};
use roots::{DirPackage, content_root_folders};

use crate::types::{
    BazelInfo, ImportRoots, JavaModel, ProjectView, SourceRoot, TargetIdeInfo, TargetMap,
};

/// `JavaLanguageLevelHelper.getDefaultLanguageLevel`.
pub const DEFAULT_LANGUAGE_LEVEL: u32 = 11;

/// Files the model reads while it is computed.
pub trait SyncFiles {
    /// A workspace file (a Java source, to read its `package`), workspace-relative.
    fn read_workspace_file(&mut self, path: &str) -> Option<String>;
    /// A file under the execution root (jdeps), exec-root-relative.
    fn read_exec_bytes(&mut self, exec_path: &str) -> Option<Vec<u8>>;
    /// Whether an absolute path exists (choosing a class jar over an interface jar).
    fn exists(&mut self, abs_path: &str) -> bool;
}

/// The project jdtls imports: source folders from the project's source targets, libraries from
/// the jars they use, and the language level.
///
/// Reads one `.java` file per source directory, every source target's jdeps file, and checks the
/// existence of each library's preferred jar.
pub fn build_java_model(
    targets: &TargetMap,
    view: &ProjectView,
    roots: &ImportRoots,
    info: &BazelInfo,
    workspace_root: &str,
    files: &mut dyn SyncFiles,
) -> JavaModel {
    let mut warnings = Vec::new();
    let decoder = PathDecoder::new(workspace_root, info);
    let classified = classify(targets, view, roots);
    let test_sources = GlobSet::test_sources(&view.test_sources);
    let layout = source_layout(&classified, roots, &test_sources, files, &mut warnings);
    let test_targets: Vec<bool> = classified
        .source_targets
        .iter()
        .map(|t| is_test_target(t, &layout))
        .collect();
    let excluded_libraries = GlobSet::new(&view.excluded_libraries);
    let libraries = libs::libraries(LibraryInputs {
        targets,
        classified: &classified,
        test_targets: &test_targets,
        excluded_libraries: &excluded_libraries,
        decoder: &decoder,
        files,
        warnings: &mut warnings,
    });
    let language_level = language_level(view, targets, &mut warnings);
    JavaModel {
        workspace_root: workspace_root.to_owned(),
        source_roots: layout.roots,
        libraries,
        excludes: roots.excludes.clone(),
        language_level,
        warnings,
    }
}

/// The source folders and which one holds each source directory.
struct SourceLayout<'a> {
    roots: Vec<SourceRoot>,
    folder_of_dir: FxHashMap<&'a str, usize>,
}

/// `SourceDirectoryCalculator.calculateContentEntries` with `SourceTestConfig` per folder.
///
/// IntelliJ reads the package of every source file and votes per directory. Reading them all is
/// most of a sync's file IO (Allocadia averages about eight sources per directory), so each
/// directory takes the package of its first `.java` file in path order; a directory whose files
/// disagree gets that file's package instead of the majority's.
fn source_layout<'a>(
    classified: &Classified<'a>,
    import: &ImportRoots,
    test_sources: &GlobSet,
    files: &mut dyn SyncFiles,
    warnings: &mut Vec<String>,
) -> SourceLayout<'a> {
    let mut first_file: BTreeMap<&'a str, &'a str> = BTreeMap::new();
    for target in &classified.source_targets {
        for source in java_sources(target).filter(|s| s.is_main_repo_source()) {
            let path = source.relative_path.as_str();
            first_file
                .entry(parent(path))
                .and_modify(|first| *first = (*first).min(path))
                .or_insert(path);
        }
    }

    // `sortArtifactLocationsByRootDirectory`: excluded directories are dropped, the rest go to
    // the first content root holding them.
    let mut by_content_root: Vec<Vec<(&'a str, &'a str)>> = vec![Vec::new(); import.includes.len()];
    for (&dir, &file) in &first_file {
        if import.excludes.iter().any(|e| is_under(e, dir)) {
            continue;
        }
        if let Some(i) = import.includes.iter().position(|root| is_under(root, dir)) {
            by_content_root[i].push((dir, file));
        }
    }

    let mut roots = Vec::new();
    for (content_root, dirs) in import.includes.iter().zip(&by_content_root) {
        let packages: Vec<DirPackage> = dirs
            .iter()
            .map(|&(dir, file)| DirPackage {
                dir,
                package: directory_package(dir, file, files, warnings),
            })
            .collect();
        for (path, package_prefix) in content_root_folders(content_root, &packages) {
            let is_test = test_sources.matches(path);
            roots.push(SourceRoot {
                path: path.to_owned(),
                package_prefix,
                is_test,
            });
        }
    }
    roots.sort_by(|a, b| a.path.cmp(&b.path));
    roots.dedup_by(|a, b| a.path == b.path);

    let index: FxHashMap<&str, usize> = roots
        .iter()
        .enumerate()
        .map(|(i, r)| (r.path.as_str(), i))
        .collect();
    let mut folder_of_dir = FxHashMap::default();
    let mut nested_test_dir: Vec<Option<&str>> = vec![None; roots.len()];
    for &(dir, _) in by_content_root.iter().flatten() {
        let Some(folder) = enclosing_folder(&index, dir) else {
            continue;
        };
        folder_of_dir.insert(dir, folder);
        if !roots[folder].is_test && nested_test_dir[folder].is_none() && test_sources.matches(dir)
        {
            nested_test_dir[folder] = Some(dir);
        }
    }
    for (root, dir) in roots.iter().zip(&nested_test_dir) {
        if let Some(dir) = dir {
            warnings.push(format!(
                "{dir} matches test_sources but its source folder {} does not; its sources are treated as main sources",
                root.path
            ));
        }
    }
    SourceLayout {
        roots,
        folder_of_dir,
    }
}

/// The deepest source folder at or above `dir`.
fn enclosing_folder(index: &FxHashMap<&str, usize>, dir: &str) -> Option<usize> {
    let mut d = dir;
    loop {
        if let Some(&i) = index.get(d) {
            return Some(i);
        }
        if d.is_empty() {
            return None;
        }
        d = parent(d);
    }
}

/// The package of a directory, from its first source (`JavaSourcePackageReader`), else from its
/// path (`FilePathJavaPackageReader`).
fn directory_package(
    dir: &str,
    file: &str,
    files: &mut dyn SyncFiles,
    warnings: &mut Vec<String>,
) -> String {
    match files.read_workspace_file(file) {
        Some(text) => {
            if let Some(package) = declared_package(&text) {
                return package.to_owned();
            }
            if !file.ends_with("module-info.java") {
                warnings.push(format!(
                    "no package declaration found in {file}; using the package its path implies"
                ));
            }
        }
        None => warnings.push(format!(
            "cannot read {file}; using the package its path implies"
        )),
    }
    package_prefix_of(dir)
}

/// Whether a source target builds only test code: all its sources in the project are in test
/// folders, or, with none there, it is a test rule. A library pulled in only by such targets is
/// test-only, which Eclipse hides from main source folders, so the folders decide first.
fn is_test_target(target: &TargetIdeInfo, layout: &SourceLayout) -> bool {
    let mut in_folders = false;
    for source in java_sources(target).filter(|s| s.is_main_repo_source()) {
        if let Some(&folder) = layout.folder_of_dir.get(parent(&source.relative_path)) {
            if !layout.roots[folder].is_test {
                return false;
            }
            in_folders = true;
        }
    }
    in_folders || target.kind_string.ends_with("_test") || target.kind_string == "test_suite"
}

/// `JavaLanguageLevelHelper.getJavaLanguageLevel`: the project view's level, else the highest
/// toolchain source version, else Java 11. IntelliJ takes the string maximum (`"8" > "17"`);
/// versions are compared as numbers here.
fn language_level(view: &ProjectView, targets: &TargetMap, warnings: &mut Vec<String>) -> u32 {
    if let Some(level) = view.java_language_level {
        return level;
    }
    let mut best = None;
    for target in targets.values() {
        let Some(toolchain) = &target.java_toolchain_ide_info else {
            continue;
        };
        let version = toolchain.source_version.trim();
        if version.is_empty() {
            continue;
        }
        match java_feature_version(version) {
            Some(v) => best = best.max(Some(v)),
            None => warnings.push(format!(
                "{}: unknown Java source version {version:?}",
                target.key.label
            )),
        }
    }
    best.unwrap_or(DEFAULT_LANGUAGE_LEVEL)
}

/// `"1.8"` → 8, `"17"` → 17.
fn java_feature_version(version: &str) -> Option<u32> {
    let version = version.strip_prefix("1.").unwrap_or(version);
    version.split('.').next()?.parse().ok()
}
