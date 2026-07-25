//! Scope guard for `new`'s post-create validate pass.
//!
//! A create must validate what it CREATED, in work proportional to the
//! configured rules, never to the size of the vault. This guard drives a
//! confirmed `new` against a small (~200-doc) and a large (~1500-doc) fixture
//! and asserts the number of documents the validate engine evaluated stays at
//! the created document on both — so the ~7.5x size difference produces no
//! growth in evaluation scope. Document-evaluation count is the deterministic
//! proxy for the pass's CPU (a whole-vault pass evaluates one per non-ignored
//! document; the scoped pass evaluates one), chosen over wall-clock so the guard
//! never flakes in CI. Wall-clock is printed for visibility, never asserted.

use std::time::Instant;

use camino::Utf8Path;

use crate::cache::Cache;
use crate::standards::engine::{docs_evaluated_count, docs_evaluated_reset};

/// Build a fixture vault of `expansion_docs` procedural documents, warm a cache
/// over it, then run one confirmed `new` and return
/// `(documents_evaluated_by_the_post_create_pass, total_docs_in_vault, wall_micros)`.
fn measure_single_create(expansion_docs: usize) -> (usize, usize, u128) {
    let profile = norn_fixtures::Profile {
        name: "guard-post-create-scope",
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
        validate_edge: false,
        alias_link: true,
    };

    let tmp = tempfile::TempDir::new().unwrap();
    let root = Utf8Path::from_path(tmp.path()).unwrap().join("vault");
    let manifest = norn_fixtures::generate(&profile, 0xA457, root.as_std_path()).unwrap();
    let total_docs = manifest.docs.len();

    // The fixture's own config — real rules, so the pass has something to
    // evaluate and the count reflects a configured vault, not an empty one.
    let config_path = root.join(".norn/config.yaml");
    let config_yaml = std::fs::read_to_string(config_path.as_std_path()).unwrap();
    let config = crate::standards::parse_config(&config_yaml, &config_path).unwrap();
    assert!(
        !config.validate.rules.is_empty(),
        "the guard is only meaningful against a vault with validate rules"
    );

    let mut cache = Cache::open(&root).unwrap();
    cache.full_build(&root).unwrap();

    let params = norn_wire::NewParams {
        path: Some("guard-probe.md".into()),
        confirm: true,
        ..Default::default()
    };
    let mut sink = crate::telemetry::EventSink::discard(
        crate::telemetry::IdGen::with_seed(0),
        crate::telemetry::Clock::fixed("2026-07-19T00:00:00.000Z"),
    );

    // Measure ONLY the create: reset the evaluation tally after the warm-up.
    // The pass runs synchronously on this thread, so the thread-local tally
    // captures exactly its evaluations.
    docs_evaluated_reset();
    let start = Instant::now();
    let execution = super::new::execute(&cache, Some(&config), &params, "2026-07-19", &mut sink)
        .expect("the guard's create must succeed");
    let elapsed = start.elapsed().as_micros();
    let evaluated = docs_evaluated_count();

    // The create actually landed, so the post-create pass really ran
    // (correctness floor for the measurement).
    assert!(
        execution.report.applied,
        "the guard's create must apply: {:?}",
        execution.report
    );
    assert!(
        root.join("guard-probe.md").as_std_path().exists(),
        "the guard's create must write the document"
    );

    (evaluated, total_docs, elapsed)
}

/// Post-create validate must evaluate the CREATED document, not the vault. A
/// single create against a ~1500-doc vault must evaluate no more documents than
/// the same create against a ~200-doc vault — proving the pass's rule-evaluation
/// cost is vault-size-independent.
#[test]
fn post_create_validate_scope_is_vault_size_independent() {
    let (small_evaluated, small_total, small_us) = measure_single_create(200);
    let (large_evaluated, large_total, large_us) = measure_single_create(1500);

    eprintln!(
        "post-create validate scope: small=(docs={small_total}, evaluated={small_evaluated}, \
         {small_us}us) large=(docs={large_total}, evaluated={large_evaluated}, {large_us}us)"
    );

    // The large vault is materially bigger — the guard is only meaningful if the
    // two fixtures actually differ in scale.
    assert!(
        large_total >= small_total * 4,
        "fixtures must differ in scale: small={small_total} large={large_total}"
    );

    // A single create evaluates exactly the created document on both sizes.
    const CREATED_DOCS: usize = 1;
    assert_eq!(
        small_evaluated, CREATED_DOCS,
        "small-vault create evaluated {small_evaluated} documents for {CREATED_DOCS} created path"
    );
    assert_eq!(
        large_evaluated, CREATED_DOCS,
        "large-vault create evaluated {large_evaluated} documents for {CREATED_DOCS} created path"
    );

    // Regression witness: a reversion to the whole-vault pass would evaluate
    // ~every document. Assert the count stays far below the vault size.
    assert!(
        large_evaluated * 20 < large_total,
        "the create evaluated {large_evaluated} of {large_total} documents — the post-create \
         pass is walking the vault"
    );
}
