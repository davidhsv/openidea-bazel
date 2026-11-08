//! Parses every `*.intellij-info.txt` under a directory, as a sync reads the aspect's output:
//! `parse_corpus <dir>`. Symlinks below `<dir>` are not followed (`bazel-bin` holds runfiles trees).

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use openidea_bazel::ideinfo::parse_target_ide_info;

const SHOWN_ERRORS: usize = 10;

fn main() -> ExitCode {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: parse_corpus <dir>");
        return ExitCode::from(2);
    };
    let start = Instant::now();
    let (mut files, mut bytes, mut errors, mut io_errors) = (0usize, 0usize, 0usize, 0usize);
    let mut parse_time = Duration::ZERO;
    let mut buf = Vec::new();
    let mut pending = vec![PathBuf::from(root)];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("{}: {e}", dir.display());
                io_errors += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry.and_then(|entry| entry.file_type().map(|t| (entry, t))) {
                Ok(entry) => entry,
                Err(e) => {
                    eprintln!("{}: {e}", dir.display());
                    io_errors += 1;
                    continue;
                }
            };
            let (entry, file_type) = entry;
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".intellij-info.txt")
            {
                continue;
            }
            buf.clear();
            if let Err(e) = fs::File::open(&path).and_then(|mut f| f.read_to_end(&mut buf)) {
                eprintln!("{}: {e}", path.display());
                io_errors += 1;
                continue;
            }
            files += 1;
            bytes += buf.len();
            let parse_start = Instant::now();
            let result = parse_target_ide_info(&String::from_utf8_lossy(&buf));
            parse_time += parse_start.elapsed();
            if let Err(e) = result {
                errors += 1;
                if errors <= SHOWN_ERRORS {
                    eprintln!("{}: {e}", path.display());
                }
            }
        }
    }
    println!(
        "files: {files}\nbytes: {bytes}\nerrors: {errors}\nio_errors: {io_errors}\nparse_ms: {}\ntotal_ms: {}",
        parse_time.as_millis(),
        start.elapsed().as_millis()
    );
    if errors == 0 && io_errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
