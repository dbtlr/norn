//! Render-vocabulary guard (NRN-409): the per-verb renderers compose ONLY
//! through the [`Sink`] / [`Conversation`] vocabulary — never a hand-rolled
//! output primitive.
//!
//! Sibling to the I1 `commands_stream_guard` (which forbids print macros under
//! `src/commands/`). Emit resolves the palette and format once and hands every
//! renderer a ready `Sink`; a renderer that reaches back to
//! `output::primitives` directly, prints to a process stream, or hand-writes a
//! `trace:` footer has re-introduced exactly the drift ADR 0021 removed. This
//! test rejects those patterns statically under `src/display/render/`, so the
//! contract holds by construction rather than by review. It lives in `tests/`
//! (not under the scanned tree) so its own needle literals are not scanned.

use std::fs;
use std::path::Path;

/// Every forbidden needle, paired with why it must not appear in a renderer.
const FORBIDDEN: &[(&str, &str)] = &[
    // The styled record/report primitives are exposed as `Sink` methods; a
    // renderer must call `sink.status_headline(..)` etc., never the free
    // `output::primitives::*` functions (which take a raw writer + palette and
    // so can emit an unstyled block off the resolved sink).
    (
        "output::primitives",
        "use the Sink method (e.g. sink.status_headline / sink.change_line), not output::primitives::*",
    ),
    // stdout/stderr belong to the emit seam; a renderer writes through the Sink
    // or the Conversation, never a process-stream print macro.
    ("println!", "write through the Sink, not println!"),
    ("print!", "write through the Sink, not print!"),
    ("eprintln!", "write through the Conversation, not eprintln!"),
    ("eprint!", "write through the Conversation, not eprint!"),
    // The applied-mutation footer is a single shared shape: `sink.trace_footer`.
    // A hand-written `trace:` writeln is the 7×-duplicated drift it replaced.
    (
        "\"trace:",
        "emit the footer with sink.trace_footer(&trace_id), not a raw \"trace:\" writeln",
    ),
];

fn scan(dir: &Path, hits: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read_dir src/display/render") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read source file");
        for (needle, why) in FORBIDDEN {
            if src.contains(needle) {
                hits.push(format!("{}: `{needle}` — {why}", path.display()));
            }
        }
    }
}

#[test]
fn renderers_compose_only_through_the_sink_vocabulary() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/display/render");
    let mut hits = Vec::new();
    scan(&dir, &mut hits);
    assert!(
        hits.is_empty(),
        "renderers must compose through the Sink / Conversation vocabulary (NRN-409, ADR 0021): {hits:#?}"
    );
}
