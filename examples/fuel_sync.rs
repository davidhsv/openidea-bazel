//! CPU-fuel benchmark of a whole sync under wasmtime: the real [`sync::sync`] over WASI files,
//! with Bazel replaced by canned output (`bazel info` text and a query result file) and a build
//! that has already run. Workspace writes go to a scratch directory instead of the workspace.
//!
//! usage: wasmtime run -W fuel=N --dir … fuel_sync.wasm <workspace> <canned dir> <scratch dir>

use std::path::Path;

use openidea_bazel::sync::{self, BazelRun, SyncError, SyncHost};

struct BenchHost {
    root: String,
    canned: String,
    scratch: String,
    /// Files read through the host, preloaded so that their I/O (WASI's, not the plugin's) can be
    /// measured apart: the plugin receives file contents from the IDE without spending fuel.
    files: std::collections::HashMap<String, Option<Vec<u8>>>,
    stats: std::collections::HashMap<String, bool>,
    record: Option<Vec<String>>,
}

impl BenchHost {
    fn file(&mut self, path: &str) -> Option<Vec<u8>> {
        if let Some(r) = &mut self.record {
            r.push(path.to_string());
        }
        // The sync's own private files (the query result) are written during the run.
        if path.starts_with(&self.scratch) {
            return std::fs::read(path).ok();
        }
        match self.files.get(path) {
            Some(v) => v.clone(),
            None => std::fs::read(path).ok(),
        }
    }
}

fn optional<T>(r: std::io::Result<T>) -> Result<Option<T>, String> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

impl SyncHost for BenchHost {
    fn workspace_root(&mut self) -> Result<String, String> {
        Ok(self.root.clone())
    }
    fn read_workspace(&mut self, path: &str) -> Result<Option<String>, String> {
        let written = Path::new(&self.scratch).join(path);
        if written.exists() {
            return optional(std::fs::read(written))
                .map(|o| o.map(|b| String::from_utf8_lossy(&b).into_owned()));
        }
        let full = format!("{}/{path}", self.root);
        Ok(self
            .file(&full)
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }
    fn write_workspace(&mut self, path: &str, text: &str) -> Result<(), String> {
        let full = Path::new(&self.scratch).join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).map_err(|e| e.to_string())?;
        std::fs::write(full, text).map_err(|e| e.to_string())
    }
    fn is_workspace_dir(&mut self, path: &str) -> bool {
        Path::new(&self.root).join(path).is_dir()
    }
    fn read_text(&mut self, path: &str) -> Result<Option<String>, String> {
        Ok(self
            .file(path)
            .map(|b| unsafe { String::from_utf8_unchecked(b) }))
    }
    fn read_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.file(path))
    }
    fn exists(&mut self, path: &str) -> bool {
        if let Some(r) = &mut self.record {
            r.push(format!("?{path}"));
        }
        match self.stats.get(path) {
            Some(&e) => e,
            None => Path::new(path).exists(),
        }
    }
    fn private_dir(&mut self) -> Result<String, String> {
        Ok(format!("{}/private", self.scratch))
    }
    fn write_private(&mut self, path: &str, text: &str) -> Result<(), String> {
        std::fs::create_dir_all(Path::new(path).parent().expect("parent"))
            .map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
    fn run_bazel(&mut self, args: &[String], _step: &str) -> Result<BazelRun, SyncError> {
        let canned = |name: &str| {
            std::fs::read_to_string(format!("{}/{name}", self.canned)).expect("canned")
        };
        let stdout = match args[0].as_str() {
            "info" => canned("info.txt"),
            "query" => {
                let out = args
                    .iter()
                    .find_map(|a| a.strip_prefix("--output_file="))
                    .expect("--output_file");
                self.write_private(out, &canned("query.txt"))
                    .map_err(SyncError::Failed)?;
                String::new()
            }
            _ => String::new(),
        };
        Ok(BazelRun {
            exit_code: 0,
            stdout,
            stderr_tail: vec![],
        })
    }
    fn report(&mut self, _message: &str) {}
    fn cancelled(&mut self) -> bool {
        false
    }
}

/// MODE=record writes the paths the sync reads to <scratch>/paths.txt; MODE=preload reads the
/// paths of an earlier record run and stops; MODE=sync preloads them, then syncs.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mode = std::env::var("MODE").unwrap_or_default();
    let mut host = BenchHost {
        root: a[0].clone(),
        canned: a[1].clone(),
        scratch: a[2].clone(),
        files: Default::default(),
        stats: Default::default(),
        record: (mode == "record").then(Vec::new),
    };
    if mode != "record" {
        let list =
            std::fs::read_to_string(format!("{}/../paths.txt", host.scratch)).expect("paths.txt");
        for p in list.lines() {
            match p.strip_prefix('?') {
                Some(p) => {
                    host.stats.insert(p.to_string(), Path::new(p).exists());
                }
                None => {
                    let data = std::fs::read(p).ok();
                    host.files.insert(p.to_string(), data);
                }
            }
        }
        if mode == "preload" {
            println!("ok preloaded {}", host.files.len());
            return;
        }
    }
    if mode == "bfs" || mode == "model" {
        phases(&mut host, mode == "model");
        return;
    }
    let result = sync::sync(&mut host);
    if let Some(paths) = &host.record {
        std::fs::write(format!("{}/../paths.txt", host.scratch), paths.join("\n"))
            .expect("paths.txt");
    }
    match result {
        Ok(s) => println!(
            "ok {} targets, {} info files, {} roots, {} libraries",
            s.targets, s.info_files, s.source_roots, s.libraries
        ),
        Err(e) => println!("failed: {e}"),
    }
}

/// The BFS over info files (as `sync::read_target_map` does it), then optionally the model, so
/// each phase's fuel can be measured apart.
fn phases(host: &mut BenchHost, with_model: bool) {
    use openidea_bazel::{ideinfo, import_roots, model, projectview, query, types::*};
    let info = query::parse_bazel_info(
        &std::fs::read_to_string(format!("{}/info.txt", host.canned)).unwrap(),
    );
    let targets: Vec<String> =
        std::fs::read_to_string(format!("{}/../paths-targets.txt", host.scratch))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
    let mut map = TargetMap::new();
    let mut queue: std::collections::VecDeque<(TargetKey, u32)> = targets
        .iter()
        .map(|l| {
            (
                TargetKey {
                    label: l.clone(),
                    aspect_ids: vec![],
                },
                0,
            )
        })
        .collect();
    use openidea_bazel::fxhash::key_fingerprint;
    let mut seen: openidea_bazel::fxhash::FxHashSet<u64> =
        queue.iter().map(|(k, _)| key_fingerprint(k)).collect();
    while let Some((key, depth)) = queue.pop_front() {
        let path = ideinfo::info_file_path(&info.bazel_bin, &key);
        let Some(text) = host.file(&path) else {
            continue;
        };
        let t = ideinfo::parse_target_ide_info(unsafe { std::str::from_utf8_unchecked(&text) })
            .unwrap();
        if depth < 2 {
            for d in &t.deps {
                if seen.insert(key_fingerprint(&d.key)) {
                    queue.push_back((d.key.clone(), depth + 1));
                }
            }
        }
        map.insert(t.key.clone(), t);
    }
    if !with_model {
        println!("ok bfs {}", map.len());
        return;
    }
    let root = host.root.clone();
    let view = projectview::parse(
        &std::fs::read_to_string(format!("{root}/.bazelproject")).unwrap(),
        ".bazelproject",
        &mut |_| None,
    );
    let roots = import_roots::import_roots(
        &view,
        std::fs::read_to_string(format!("{root}/.bazelignore"))
            .ok()
            .as_deref(),
        "Allocadia",
        ".openidea/bazel",
    );
    struct Files<'a>(&'a mut BenchHost, String);
    impl model::SyncFiles for Files<'_> {
        fn read_workspace_file(&mut self, path: &str) -> Option<String> {
            let p = format!("{}/{path}", self.0.root);
            self.0
                .file(&p)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        }
        fn read_exec_bytes(&mut self, exec_path: &str) -> Option<Vec<u8>> {
            let p = format!("{}/{exec_path}", self.1);
            self.0.file(&p)
        }
        fn exists(&mut self, abs_path: &str) -> bool {
            SyncHost::exists(self.0, abs_path)
        }
    }
    let exec = info.execution_root.clone();
    let m = model::build_java_model(&map, &view, &roots, &info, &root, &mut Files(host, exec));
    println!(
        "ok model {} roots {} libs",
        m.source_roots.len(),
        m.libraries.len()
    );
}
