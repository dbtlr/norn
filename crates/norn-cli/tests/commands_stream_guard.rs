//! I1 guard (NRN-370): the command modules never touch the process streams.
//!
//! A verb returns an `Output` for the single `display::emit` seam to render, so
//! stdout stays payload-only and stderr stays the one diagnostic/conversation
//! path — structurally, not by reviewer vigilance. This test rejects any
//! `print!` / `println!` / `eprint!` / `eprintln!` macro under `src/commands/`.
//! It lives in `tests/` (not under `src/commands/`) so its own needle literals
//! are not scanned.

use std::fs;
use std::path::Path;

fn scan(dir: &Path, hits: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read_dir src/commands") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read source file");
        for needle in ["println!", "print!", "eprintln!", "eprint!"] {
            if src.contains(needle) {
                hits.push(format!("{}: {needle}", path.display()));
            }
        }
    }
}

#[test]
fn command_modules_never_write_the_process_streams() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands");
    let mut hits = Vec::new();
    scan(&dir, &mut hits);
    assert!(
        hits.is_empty(),
        "command modules must return an Output for `display::emit`, never print directly (NRN-370): {hits:?}"
    );
}
