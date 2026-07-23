//! Comment-truth guard (NRN-450): live-tree comments speak in present-tense
//! code facts. Provenance — who or what once validated a shape — belongs to git
//! history and ADRs, never to a source comment.
//!
//! This test scans every live crate's `src/`, `tests/`, and `benches/` trees
//! and rejects the authority needles a migration leaves behind: "donor"
//! attributions, "oracle" / "parity"-harness citations, byte-identity framing,
//! retired-tree citations, and PD-ledger ids used as rationale. A shape's
//! justification must be the constraint itself (what the contract IS and why
//! it holds), stated in the present tense.
//!
//! The scan matches on whole file text — code and string literals alike, not
//! just `//` / `///` comments — so a needle inside a test identifier or an
//! assertion message trips it exactly like a needle inside a comment. That is
//! why the sweep this guard enforces renamed a handful of test helpers and
//! functions rather than only editing comments.
//!
//! ADR references (`ADR 0018`) and pending-work task ids (`NRN-123`) are FINE —
//! they are durable decision records and live TODO markers, not point-in-time
//! validation authorities. The needle list below is deliberately narrow so it
//! catches the disease without flagging legitimate uses of the word "parity"
//! (e.g. "dry-run/apply parity", "CLI ⇄ plan parity", "EAV/scan parity") or of
//! the word "retired" in ordinary prose (e.g. "the split was retired (ADR
//! 0022)") — only the `retired/` path form is a needle.
//!
//! It lives in `tests/` under `norn-cli`, whose own `tests/` directory IS
//! scanned by this guard — so this file's own needle literals must stay out of
//! its source text (this doc comment paraphrases rather than quotes them).

use std::fs;
use std::path::{Path, PathBuf};

/// Crates under `crates/` whose `src/` is NOT scanned: migration scaffolding
/// that legitimately names its comparator.
const UNSCANNED_CRATES: &[&str] = &["norn-parity", "norn-fixtures"];

/// Authority needles (matched case-insensitively). Each entry is the needle and
/// the reason it must not appear in a live-tree source comment.
const NEEDLES: &[(&str, &str)] = &[
    (
        "donor",
        "state the present-tense contract, not who/what the shape was ported from",
    ),
    (
        "donor-faithful",
        "\"verified preserved\" is not a justification; state why the shape holds",
    ),
    (
        "oracle",
        "the parity oracle is not an authority a comment may cite; state the fact",
    ),
    (
        "byte-identical",
        "byte-identity framing is banned; say what is unchanged and why it holds",
    ),
    (
        "byte-for-byte",
        "byte-identity framing is banned; say what is unchanged and why it holds",
    ),
    (
        "parity harness",
        "the parity harness is not a comment authority; state the code contract",
    ),
    (
        "parity oracle",
        "the parity oracle is not a comment authority; state the code contract",
    ),
    (
        "parity case",
        "a parity case is not what pins a contract; state the contract itself",
    ),
    (
        "parity-pinned",
        "a contract is not \"parity-pinned\"; state what makes it a contract",
    ),
    (
        "retired/",
        "a retired/-tree citation is not a rationale; state the present-tense contract",
    ),
    (
        "ported from",
        "provenance belongs to git history, not a comment; state the contract itself",
    ),
    (
        "pre-rewrite",
        "the pre-rewrite tree is not a comment authority; state the present-tense contract",
    ),
];

/// An explicit allowlist for genuinely operational references that would
/// otherwise trip a needle. Every entry needs a stated reason. It is EMPTY by
/// design — the comment-truth sweep (NRN-450) drove the live tree to zero, and
/// a new entry means a comment is citing an authority it should not. Prefer
/// rewriting the comment to stating the present-tense fact.
///
/// Format: `(crate-relative path, needle, reason)`.
const ALLOWLIST: &[(&str, &str, &str)] = &[];

/// This guard's own test file, excluded from the scan despite living in a
/// scanned `tests/` tree: it must define the needle literals verbatim to
/// match against, so it cannot itself be needle-free text.
const SELF_PATH: &str = "crates/norn-cli/tests/comment_truth_guard.rs";

/// A PD-ledger id (`PD-1`, `PD-42`, …) cited as rationale is the same disease:
/// a point-in-time ledger reference standing in for the present-tense contract.
fn contains_pd_ledger_id(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let needle = b"pd-";
    let mut i = 0;
    while let Some(rel) = lower[i..].find("pd-") {
        let start = i + rel;
        // Leading boundary: not part of a larger identifier (e.g. `upd-`).
        let leading_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after = start + needle.len();
        let trailing_digit = bytes.get(after).is_some_and(u8::is_ascii_digit);
        if leading_ok && trailing_digit {
            return true;
        }
        i = start + needle.len();
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if `needle` appears in `lower` with a leading identifier boundary, so a
/// needle embedded inside a larger word or identifier does not false-positive.
fn contains_needle(lower: &str, needle: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(rel) = lower[i..].find(needle) {
        let start = i + rel;
        let leading_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        if leading_ok {
            return true;
        }
        i = start + needle.len();
    }
    false
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/crates/norn-cli`; the workspace root is two
    // levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above the crate manifest dir")
        .to_path_buf()
}

fn allowlisted(rel_path: &str, needle: &str) -> bool {
    ALLOWLIST
        .iter()
        .any(|(p, n, _)| *p == rel_path && *n == needle)
}

fn scan_file(path: &Path, rel_path: &str, hits: &mut Vec<String>) {
    let src = fs::read_to_string(path).expect("read source file");
    let lower = src.to_lowercase();
    for (needle, why) in NEEDLES {
        if contains_needle(&lower, needle) && !allowlisted(rel_path, needle) {
            hits.push(format!("{rel_path}: `{needle}` — {why}"));
        }
    }
    if contains_pd_ledger_id(&lower) && !allowlisted(rel_path, "PD-<n>") {
        hits.push(format!(
            "{rel_path}: `PD-<n>` — a PD-ledger id is not a rationale; state the present-tense contract"
        ));
    }
}

fn scan_dir(dir: &Path, root: &Path, hits: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read_dir src tree") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan_dir(&path, root, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("scanned path is under the workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == SELF_PATH {
            continue;
        }
        scan_file(&path, &rel, hits);
    }
}

#[test]
fn live_tree_comments_carry_no_authority_needles() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut hits = Vec::new();
    let mut scanned_any = false;

    for entry in fs::read_dir(&crates_dir).expect("read_dir crates") {
        let crate_dir = entry.expect("dir entry").path();
        let name = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !crate_dir.is_dir() || UNSCANNED_CRATES.contains(&name) {
            continue;
        }
        for sub in ["src", "tests", "benches"] {
            let dir = crate_dir.join(sub);
            if dir.is_dir() {
                scanned_any = true;
                scan_dir(&dir, &root, &mut hits);
            }
        }
    }

    assert!(
        scanned_any,
        "guard scanned no crate sources — path resolution is wrong (root: {})",
        root.display()
    );
    assert!(
        hits.is_empty(),
        "live-tree comments must state present-tense code facts, not migration \
         authorities (NRN-450). Rewrite each to the constraint it enforces, or — \
         if genuinely operational — allowlist it with a stated reason:\n{hits:#?}"
    );
}
