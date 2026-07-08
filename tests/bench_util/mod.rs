//! Deterministic synthetic-vault generator for the integrity-check acceptance
//! benchmark (NRN-83) and any future scale regression harness.
//!
//! `generate_vault(dir, n, seed)` writes `n` Markdown documents whose frontmatter,
//! wikilinks, and headings are a pure function of `(index, seed)` — no
//! `Date::now`, no thread RNG, no filesystem entropy — so a run is byte-for-byte
//! reproducible and drives no flakiness into the assertions layered on top of it.
//! The docs are realistic enough that the graph/link/field index is genuinely
//! exercised: a few indexed frontmatter fields (`type`/`status`/`priority`/
//! `title`), real inter-doc `[[wikilinks]]`, and headings + body.
//!
//! Cargo compiles this into each test binary that declares `mod bench_util;`.
//! Only the benchmark uses it today; silence the per-binary dead-code lint.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::Path;

/// SplitMix64 — a tiny, dependency-free deterministic PRNG. Seeded from
/// `(seed, index)` so each document draws an independent-looking but fully
/// reproducible stream. We only need cheap, well-distributed integers here, not
/// cryptographic quality.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish integer in `[0, bound)`. `bound` must be non-zero.
    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

const TYPES: &[&str] = &["note", "task", "log", "reference"];
const STATUSES: &[&str] = &["active", "backlog", "done", "archived"];

/// Zero-padded stem for document `i` (`doc-00042`). Width 6 keeps stems sorted
/// and stable up to a million docs — comfortably past the 50k benchmark scale.
pub fn doc_stem(i: usize) -> String {
    format!("doc-{i:06}")
}

/// Write a deterministic vault of `n` documents into `dir`. Returns the number
/// of documents written (always `n`). Panics on any I/O error — this is test
/// infrastructure and a failed write is a hard stop.
pub fn generate_vault(dir: &Path, n: usize, seed: u64) -> usize {
    assert!(n > 0, "generate_vault needs at least one document");
    std::fs::create_dir_all(dir).expect("create vault dir");

    for i in 0..n {
        let mut rng = SplitMix64::new(seed ^ (i as u64).wrapping_mul(0x0100_0000_01B3));
        let doc_type = TYPES[(rng.below(TYPES.len() as u64)) as usize];
        let status = STATUSES[(rng.below(STATUSES.len() as u64)) as usize];
        let priority = 1 + rng.below(5); // 1..=5

        // Two deterministic wikilinks to other docs (never self), so the link
        // graph is dense enough to exercise the links index.
        let link_a = pick_other(&mut rng, i, n);
        let link_b = pick_other(&mut rng, i, n);

        let mut body = String::with_capacity(256);
        // Frontmatter — the indexed fields queries filter on.
        write!(
            body,
            "---\ntype: {doc_type}\nstatus: {status}\npriority: {priority}\ntitle: Document {i}\n---\n"
        )
        .unwrap();
        // Headings + prose so the headings index and body scans have real input.
        write!(
            body,
            "# Document {i}\n\n## Overview\n\nSynthetic document {i} of type {doc_type}. \
             Links to [[{a}]] and [[{b}]].\n\n## Details\n\nStatus is {status}; priority {priority}. \
             Lorem ipsum body text for index exercise.\n",
            a = doc_stem(link_a),
            b = doc_stem(link_b),
        )
        .unwrap();

        let path = dir.join(format!("{}.md", doc_stem(i)));
        std::fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    n
}

/// Pick a document index other than `i` in `[0, n)`. With `n == 1` there is no
/// other doc, so it links to itself (harmless for the benchmark's purposes).
fn pick_other(rng: &mut SplitMix64, i: usize, n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let mut j = rng.below(n as u64) as usize;
    if j == i {
        j = (j + 1) % n;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_for_same_seed() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        generate_vault(a.path(), 50, 7);
        generate_vault(b.path(), 50, 7);
        for i in 0..50 {
            let fa = std::fs::read_to_string(a.path().join(format!("{}.md", doc_stem(i)))).unwrap();
            let fb = std::fs::read_to_string(b.path().join(format!("{}.md", doc_stem(i)))).unwrap();
            assert_eq!(fa, fb, "doc {i} must be identical across same-seed runs");
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        generate_vault(a.path(), 50, 1);
        generate_vault(b.path(), 50, 2);
        let any_diff = (0..50).any(|i| {
            let fa = std::fs::read_to_string(a.path().join(format!("{}.md", doc_stem(i)))).unwrap();
            let fb = std::fs::read_to_string(b.path().join(format!("{}.md", doc_stem(i)))).unwrap();
            fa != fb
        });
        assert!(
            any_diff,
            "different seeds should produce at least one differing doc"
        );
    }
}
