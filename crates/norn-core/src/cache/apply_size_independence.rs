//! Size-independence guard for apply-time cache refresh.
//!
//! A mutation apply must update the cache in work proportional to the paths it
//! CHANGED, never to the size of the vault. This guard drives the apply-increment
//! commit (`reserve` → `begin` → `chunk`) against a small (~200-doc) and a large
//! (~1500-doc) fixture and asserts the number of documents parsed from disk stays
//! bounded by the changed set on both — so the ~7.5x size difference produces no
//! growth in refresh scope. Document-parse count is the deterministic proxy for
//! refresh cost (a full walk parses one per vault document; a targeted refresh
//! parses one per changed path), chosen over wall-clock so the guard never flakes
//! in CI. Wall-clock is printed for visibility, never asserted.

use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};

use crate::cache::Cache;
use crate::graph::{docs_parsed_count, docs_parsed_reset};

/// Build a fixture vault of `expansion_docs` procedural documents, warm a cache
/// over it, then drive a single-file apply-increment and return
/// `(docs_parsed_by_the_increment, total_docs_in_vault, increment_wall_micros)`.
fn measure_single_file_apply(expansion_docs: usize) -> (usize, usize, u128) {
    let profile = norn_fixtures::Profile {
        name: "guard-size-independence",
        violations: false,
        expansion_docs,
        folder_depth: 3,
        folder_width: 5,
        max_links_per_doc: 3,
        broken_link_per_mille: 0,
        violation_per_mille: 0,
        text_edge: false,
        malformed_config: false,
        mutate_edge: false,
        section_edge: false,
        wikilink_edge: false,
    };

    let tmp = tempfile::TempDir::new().unwrap();
    let root = Utf8Path::from_path(tmp.path()).unwrap().join("vault");
    let manifest = norn_fixtures::generate(&profile, 0xA413, root.as_std_path()).unwrap();
    let total_docs = manifest.docs.len();

    // Warm the cache with a full build (parses the whole vault — the cold-start
    // path the guard is NOT measuring).
    let mut cache = Cache::open(&root).unwrap();
    cache.full_build(&root).unwrap();

    // A single new document lands on disk — the one path this apply touched.
    let changed = Utf8PathBuf::from("guard-probe.md");
    std::fs::write(
        root.join(&changed).as_std_path(),
        "---\ntitle: Guard Probe\n---\nprobe body\n",
    )
    .unwrap();

    let baseline = cache.load_graph_index().unwrap();
    let fingerprint: String = cache
        .conn()
        .query_row(
            "SELECT value FROM meta WHERE key = 'graph_fingerprint'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Measure ONLY the apply-increment refresh: reset the parse tally after the
    // warm-up, then drive the commit for the single changed path. The parse runs
    // synchronously on this thread, so the thread-local tally captures exactly the
    // increment's disk reads — the parallel runner's other parsers cannot pollute.
    docs_parsed_reset();
    let start = Instant::now();
    let reservation = cache.reserve_increment_commit(&fingerprint).unwrap();
    let mut commit = Cache::begin_increment_commit(
        &root,
        std::slice::from_ref(&changed),
        None,
        &[],
        &reservation,
        baseline,
    )
    .unwrap();
    let budget = std::time::Duration::from_millis(0);
    while cache.commit_increment_chunk(&mut commit, budget).unwrap() {}
    let elapsed = start.elapsed().as_micros();
    let parsed = docs_parsed_count();

    // The increment actually published the changed path (correctness floor).
    let loaded = cache.load_graph_index().unwrap();
    assert!(
        loaded.documents.iter().any(|d| d.path == changed),
        "the apply-increment must publish the changed document"
    );

    (parsed, total_docs, elapsed)
}

/// Apply-refresh document-parse count must track the CHANGED set, not the vault.
/// A single-file apply against a ~1500-doc vault must parse no more documents
/// than the same apply against a ~200-doc vault — proving apply cost is
/// vault-size-independent.
#[test]
fn apply_refresh_scope_is_vault_size_independent() {
    let (small_parsed, small_total, small_us) = measure_single_file_apply(200);
    let (large_parsed, large_total, large_us) = measure_single_file_apply(1500);

    eprintln!(
        "apply size-independence: small=(docs={small_total}, parsed={small_parsed}, {small_us}us) \
         large=(docs={large_total}, parsed={large_parsed}, {large_us}us)"
    );

    // The large vault is materially bigger — the guard is only meaningful if the
    // two fixtures actually differ in scale.
    assert!(
        large_total >= small_total * 4,
        "fixtures must differ in scale: small={small_total} large={large_total}"
    );

    // A single-file apply parses exactly the changed path on both sizes: refresh
    // scope tracks the changed set, not the vault. A small ceiling (not a hard
    // `== 1`) absorbs any incidental re-read without admitting a full walk.
    const CHANGED_PATHS: usize = 1;
    const SCOPE_CEILING: usize = CHANGED_PATHS + 2;
    assert!(
        small_parsed <= SCOPE_CEILING,
        "small-vault apply parsed {small_parsed} docs for {CHANGED_PATHS} changed path(s)"
    );
    assert!(
        large_parsed <= SCOPE_CEILING,
        "large-vault apply parsed {large_parsed} docs for {CHANGED_PATHS} changed path(s)"
    );
    // Floor: the tally must have counted AT LEAST the changed set. Without this,
    // a parse that moved off the thread the tally instruments (e.g. onto a
    // background worker) would read 0 and the ceiling checks above would pass
    // vacuously, silently certifying a broken measurement as scope-independence.
    assert!(
        small_parsed >= CHANGED_PATHS,
        "small-vault apply parsed {small_parsed} docs, below the {CHANGED_PATHS} changed-path \
         floor — the parse tally likely ran off the measuring thread"
    );
    assert!(
        large_parsed >= CHANGED_PATHS,
        "large-vault apply parsed {large_parsed} docs, below the {CHANGED_PATHS} changed-path \
         floor — the parse tally likely ran off the measuring thread"
    );

    // Bounded ratio: the ~7.5x larger vault must not inflate the parse count.
    assert!(
        large_parsed <= small_parsed.max(1) * 2,
        "apply parse count grew with vault size: small={small_parsed} large={large_parsed}"
    );

    // Regression witness: a reversion to the full-vault walk would parse ~every
    // document. Assert the count stays far below the vault size.
    assert!(
        large_parsed * 20 < large_total,
        "apply parsed {large_parsed} of {large_total} docs — refresh is walking the vault"
    );
}
