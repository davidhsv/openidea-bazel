//! [`SyncHost`] over the OpenIDEA plugin API: Bazel runs as a kept-running process whose
//! progress feeds a cancellable task, so a sync can outlive the per-call time budget.

use std::collections::VecDeque;

use openidea_plugin::host::{self, Command, Project, Task, fs};
use openidea_plugin::{Error, FileKind};

use crate::query;
use crate::sync::{BazelRun, SyncError, SyncHost};

/// Stdout kept from one Bazel run (`bazel info`; the query and the build write elsewhere).
const MAX_STDOUT: usize = 1 << 20;
const STDERR_TAIL: usize = 80;

pub struct IdeHost<'a> {
    pub project: &'a Project,
    pub task: &'a Task,
    /// `bazelisk` or `bazel`, whichever the user granted.
    pub bazel: &'a str,
}

impl SyncHost for IdeHost<'_> {
    fn workspace_root(&mut self) -> Result<String, String> {
        self.project.root().map_err(|e| e.to_string())
    }

    fn read_workspace(&mut self, path: &str) -> Result<Option<String>, String> {
        optional(self.project.read_file(path))
    }

    fn write_workspace(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.project
            .write_file(path, text)
            .map_err(|e| e.to_string())
    }

    fn is_workspace_dir(&mut self, path: &str) -> bool {
        self.project
            .stat(path)
            .is_ok_and(|s| s.kind == FileKind::Directory)
    }

    fn read_text(&mut self, path: &str) -> Result<Option<String>, String> {
        optional(fs::read_file(path))
    }

    fn read_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        optional(fs::read_bytes(path))
    }

    fn exists(&mut self, path: &str) -> bool {
        fs::stat(path).is_ok()
    }

    fn private_dir(&mut self) -> Result<String, String> {
        fs::plugin_dir().map_err(|e| e.to_string())
    }

    fn write_private(&mut self, path: &str, text: &str) -> Result<(), String> {
        fs::write_file(path, text).map_err(|e| e.to_string())
    }

    fn run_bazel(&mut self, args: &[String], step: &str) -> Result<BazelRun, SyncError> {
        self.task.report(0, None, Some(step));
        let child =
            host::start(&Command::new(self.bazel).args(args), Some(self.project)).map_err(|e| {
                match e {
                    Error::Denied(c) => SyncError::Failed(format!(
                        "{c} is not granted (Settings › Plugins › Bazel)"
                    )),
                    e => SyncError::Failed(format!("could not start {}: {e}", self.bazel)),
                }
            })?;
        child
            .close_stdin()
            .map_err(|e| SyncError::Failed(e.to_string()))?;
        let mut run = BazelRun::default();
        let mut tail: VecDeque<String> = VecDeque::new();
        let mut partial = String::new();
        let mut last_line = String::new();
        let code = loop {
            if self.task.is_cancelled() {
                let _ = child.kill();
                return Err(SyncError::Cancelled);
            }
            let chunk = match child.wait(1000) {
                Ok(chunk) => chunk,
                Err(Error::Cancelled) => {
                    let _ = child.kill();
                    return Err(SyncError::Cancelled);
                }
                Err(e) => return Err(SyncError::Failed(format!("{step}: {e}"))),
            };
            if run.stdout.len() < MAX_STDOUT {
                run.stdout.push_str(&chunk.stdout);
            }
            partial.push_str(&chunk.stderr);
            // Bazel ends progress lines with \r when it rewrites them in place.
            while let Some(end) = partial.find(['\n', '\r']) {
                let line: String = partial.drain(..=end).collect();
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                last_line = line.chars().take(200).collect();
                if tail.len() == STDERR_TAIL {
                    tail.pop_front();
                }
                tail.push_back(line.to_string());
            }
            // Every report keeps the call alive for another minute.
            match last_line
                .split_once(']')
                .and_then(|_| query::parse_progress(&last_line))
            {
                Some((done, total)) => {
                    self.task
                        .report(done, Some(total), Some(&format!("{step}: {last_line}")))
                }
                None => self
                    .task
                    .report(0, None, Some(&format!("{step}: {last_line}"))),
            }
            if let Some(code) = chunk.status {
                break code;
            }
        };
        if !partial.trim().is_empty() {
            tail.push_back(partial.trim().to_string());
        }
        run.exit_code = code;
        run.stderr_tail = tail.into_iter().collect();
        Ok(run)
    }

    fn report(&mut self, message: &str) {
        self.task.report(0, None, Some(message));
    }

    fn cancelled(&mut self) -> bool {
        self.task.is_cancelled()
    }
}

/// `NotFound` is an expected answer (a file Bazel didn't write), not an error.
fn optional<T>(r: Result<T, Error>) -> Result<Option<T>, String> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
