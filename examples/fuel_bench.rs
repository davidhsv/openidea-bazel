//! CPU-fuel benchmark for the sync's hot path under wasmtime: parses the listed info files the way
//! the plugin does (whole file as text, then `parse_target_ide_info`).
//!
//! usage: wasmtime run -W fuel=N --dir <root>::/r fuel_bench.wasm <list file> [<prefix to strip>]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let list = std::fs::read_to_string(&args[0]).expect("list");
    let (mut files, mut bytes, mut errors) = (0usize, 0usize, 0usize);
    for path in list.lines().filter(|l| !l.is_empty()) {
        let data = std::fs::read(path).expect("read");
        bytes += data.len();
        // The host hands the plugin strings it already validated.
        let text = unsafe { String::from_utf8_unchecked(data) };
        match std::env::var("MODE").as_deref() {
            Ok("read") => {}
            Ok("lines") => {
                if text.bytes().filter(|&c| c == b'\n').count() == usize::MAX {
                    errors += 1;
                }
            }
            Ok("lines-only-deps") => {
                // Fast path cost alone: scan each deps block, nothing else.
                let b = text.as_bytes();
                let mut i = 0;
                while let Some(off) = text[i..].find("deps {") {
                    i += off + 6;
                    match openidea_bazel::ideinfo::__bench_fast_dependency(&text, i) {
                        Some(end) => i = end,
                        None => break,
                    }
                }
                let _ = b;
            }
            Ok("tokens") => {
                let mut r = openidea_bazel::textproto::Reader::new(&text);
                let mut depth = 0usize;
                loop {
                    match r.next_field() {
                        Ok(Some(f)) => {
                            if f.is_message() {
                                depth += 1;
                            }
                        }
                        Ok(None) if depth == 0 => break,
                        Ok(None) => depth -= 1,
                        Err(_) => {
                            errors += 1;
                            break;
                        }
                    }
                }
            }
            _ => {
                if openidea_bazel::ideinfo::parse_target_ide_info(&text).is_err() {
                    errors += 1;
                }
            }
        }
        files += 1;
    }
    println!("{files} files, {bytes} bytes, {errors} errors");
}
