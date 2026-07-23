//! Typed-severity guard (NRN-407): severity and enum labels come from typed
//! fields, never from message text.
//!
//! Two static invariants, enforced across the live tree so they hold by
//! construction rather than by review:
//!
//! 1. **No message-text severity sniff.** A surface decides exit / `isError`
//!    from a typed [`Severity`](norn_wire::Severity) — the read report's
//!    [`Note`](norn_wire::Note) or the mutation report's outcome — never by
//!    matching a prose prefix like `error:` / `warning:`. So no `src/` may
//!    `starts_with` (or `contains`) one of the annotation prefixes: that is the
//!    text sniff this channel replaced.
//! 2. **No `Debug` label in a renderer.** A user-facing enum label is the value's
//!    `#[serde(rename_all = "kebab-case")]` name via `display::serde_label`, not
//!    `format!("{value:?}")` (whose `Debug` derives the variant identifier and
//!    only accidentally lowercases). So no file under `src/display/render/` may
//!    carry the positional `{:?}` placeholder.
//!
//! It lives in `tests/` (outside every scanned `src/` tree) so its own needle
//! literals are not scanned by invariant 1, and its `{:?}`-free source is not
//! scanned by invariant 2.

use std::fs;
use std::path::{Path, PathBuf};

/// Crates whose `src/` is exempt from the severity-sniff scan: the parity
/// harness legitimately classifies captured output text.
const SNIFF_EXEMPT_CRATES: &[&str] = &["norn-parity", "norn-fixtures"];

/// The forbidden message-text severity sniffs: matching an annotation prefix to
/// recover severity. Each is a `starts_with` / `contains` on a closed prefix.
const SNIFF_NEEDLES: &[&str] = &[
    "starts_with(\"error:",
    "starts_with(\"warning:",
    "starts_with(\"warn:",
    "starts_with(\"note:",
    "contains(\"error:\")",
    "contains(\"warning:\")",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above the crate manifest dir")
        .to_path_buf()
}

fn scan_rs<F: FnMut(&Path, &str)>(dir: &Path, visit: &mut F) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan_rs(&path, visit);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read source file");
        visit(&path, &src);
    }
}

#[test]
fn no_surface_sniffs_severity_from_message_text() {
    let crates = workspace_root().join("crates");
    let mut hits = Vec::new();
    for entry in fs::read_dir(&crates).expect("read_dir crates") {
        let crate_dir = entry.expect("dir entry").path();
        let name = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !crate_dir.is_dir() || SNIFF_EXEMPT_CRATES.contains(&name) {
            continue;
        }
        let src = crate_dir.join("src");
        if !src.is_dir() {
            continue;
        }
        scan_rs(&src, &mut |path, text| {
            for needle in SNIFF_NEEDLES {
                if text.contains(needle) {
                    hits.push(format!("{}: `{needle}`", path.display()));
                }
            }
        });
    }
    assert!(
        hits.is_empty(),
        "a surface must derive severity from the typed Note / outcome, never by \
         matching an annotation prefix in message text (NRN-407):\n{hits:#?}"
    );
}

#[test]
fn renderers_label_enums_via_serde_name_not_debug() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/display/render");
    let mut hits = Vec::new();
    scan_rs(&dir, &mut |path, text| {
        if text.contains("{:?}") {
            hits.push(path.display().to_string());
        }
    });
    assert!(
        hits.is_empty(),
        "a renderer must label enum values via display::serde_label (the serde \
         kebab name), never the positional `{{:?}}` Debug placeholder (NRN-407):\n{hits:#?}"
    );
}
