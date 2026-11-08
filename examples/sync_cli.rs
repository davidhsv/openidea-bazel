//! Runs the plugin's sync natively, outside OpenIDEA: the same pipeline over std::fs and a real
//! Bazel, for development and for checking a workspace before opening it in the IDE.
//!
//! usage: cargo run --example sync_cli -- <workspace> [<private dir>] [--bazel <program>]

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use openidea_bazel::sync::{self, BazelRun, SyncError, SyncHost};

struct NativeHost {
    root: String,
    private: String,
    bazel: String,
}

fn optional<T>(r: std::io::Result<T>) -> Result<Option<T>, String> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

impl SyncHost for NativeHost {
    fn workspace_root(&mut self) -> Result<String, String> {
        Ok(self.root.clone())
    }

    fn read_workspace(&mut self, path: &str) -> Result<Option<String>, String> {
        optional(std::fs::read_to_string(Path::new(&self.root).join(path)))
    }

    fn write_workspace(&mut self, path: &str, text: &str) -> Result<(), String> {
        let full = Path::new(&self.root).join(path);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(full, text).map_err(|e| e.to_string())
    }

    fn is_workspace_dir(&mut self, path: &str) -> bool {
        Path::new(&self.root).join(path).is_dir()
    }

    fn read_text(&mut self, path: &str) -> Result<Option<String>, String> {
        optional(std::fs::read(path)).map(|o| o.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    fn read_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        optional(std::fs::read(path))
    }

    fn exists(&mut self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn private_dir(&mut self) -> Result<String, String> {
        std::fs::create_dir_all(&self.private).map_err(|e| e.to_string())?;
        Ok(self.private.clone())
    }

    fn write_private(&mut self, path: &str, text: &str) -> Result<(), String> {
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    fn run_bazel(&mut self, args: &[String], step: &str) -> Result<BazelRun, SyncError> {
        eprintln!("== {step}: {} {}", self.bazel, args.join(" "));
        let mut child = Command::new(&self.bazel)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SyncError::Failed(format!("{}: {e}", self.bazel)))?;
        let mut out = child.stdout.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            out.read_to_string(&mut s).map(|_| s)
        });
        let (tx, rx) = mpsc::channel();
        let err = child.stderr.take().expect("piped");
        let errs = std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("   {line}");
                let _ = tx.send(line);
            }
        });
        let status = child.wait().map_err(|e| SyncError::Failed(e.to_string()))?;
        errs.join().expect("stderr reader");
        let stdout = reader
            .join()
            .expect("stdout reader")
            .map_err(|e| SyncError::Failed(e.to_string()))?;
        let lines: Vec<String> = rx.try_iter().collect();
        let tail = lines[lines.len().saturating_sub(80)..].to_vec();
        Ok(BazelRun {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr_tail: tail,
        })
    }

    fn report(&mut self, message: &str) {
        eprintln!("-- {message}");
    }

    fn cancelled(&mut self) -> bool {
        false
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut bazel = "bazelisk".to_string();
    if let Some(i) = args.iter().position(|a| a == "--bazel") {
        bazel = args.remove(i + 1);
        args.remove(i);
    }
    let root = std::fs::canonicalize(
        args.first()
            .expect("usage: sync_cli <workspace> [<private dir>]"),
    )
    .expect("workspace")
    .to_string_lossy()
    .into_owned();
    let private = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| format!("{root}/.openidea/bazel/cli-private"));
    let mut host = NativeHost {
        root,
        private,
        bazel,
    };
    match sync::sync(&mut host) {
        Ok(s) => {
            println!("{s:#?}");
        }
        Err(e) => {
            eprintln!("sync failed: {e}");
            std::process::exit(1);
        }
    }
}
