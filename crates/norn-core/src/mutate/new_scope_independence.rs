//! Scope guard for `new`'s post-create validate pass.
//!
//! A create must validate what it CREATED, in work proportional to the
//! configured rules and to the change's blast radius, never to the size of the
//! vault. This guard drives a confirmed `new` against a small (~200-doc) and a
//! large (~1500-doc) fixture and asserts two counts stay flat across the ~7.5x
//! size difference:
//!
//! - **documents evaluated** by the validate engine — the created one, proving
//!   rule-evaluation cost tracks the rule count, not the document count;
//! - **links re-resolved** by the overlay — the created document's own links
//!   plus the blast radius the reverse link index reports, proving the
//!   maintenance path does not re-resolve the vault per touched path
//!   (architecture invariant 13).
//!
//! Both counts are deterministic proxies for the pass's CPU, chosen over
//! wall-clock so the guard never flakes in CI. Wall-clock is printed for
//! visibility, never asserted.

use std::time::Instant;

use camino::Utf8Path;

use crate::cache::Cache;
use crate::links::resolve::{links_resolved_count, links_resolved_reset};
use crate::standards::engine::{docs_evaluated_count, docs_evaluated_reset};

/// What one measured create cost, in vault-size-independent units.
struct CreateCost {
    docs_evaluated: usize,
    links_resolved: usize,
    total_docs: usize,
    total_links: usize,
    wall_micros: u128,
}

/// Build a fixture vault of `expansion_docs` procedural documents, warm a cache
/// over it, then run one confirmed `new` and report what the create cost.
fn measure_single_create(expansion_docs: usize) -> CreateCost {
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

    // A referrer whose wikilink DANGLES until the probe lands. It is what makes
    // the link-resolution measurement non-vacuous: the created path is outside
    // this document, so only a reverse lookup puts it in the affected set, and a
    // radius that missed it would leave a stale `unresolved` link behind.
    std::fs::write(
        root.join("guard-referrer.md").as_std_path(),
        "---\ntype: note\n---\nsee [[guard-probe]]\n",
    )
    .unwrap();

    let mut cache = Cache::open(&root).unwrap();
    cache.full_build(&root).unwrap();
    let total_links: usize = cache
        .load_graph_index()
        .unwrap()
        .documents
        .iter()
        .map(|document| document.links.len())
        .sum();

    // The probe lands where the fixture's rules bite (a `notes/**` rule forbids
    // `legacy`, and the `type: note` rule matches every path), so the measured
    // scope contains real rule evaluation rather than a document no rule selects.
    let params = norn_wire::NewParams {
        path: Some("notes/guard-probe.md".into()),
        fields: vec!["type=note".into(), "legacy=true".into()],
        // One outgoing link of its own, so the measured radius covers both
        // directions: links FROM the created document and links TO it.
        body: Some("links back to [[guard-referrer]]\n".into()),
        parents: true,
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
    links_resolved_reset();
    let start = Instant::now();
    let execution = super::new::execute(&cache, Some(&config), &params, "2026-07-19", &mut sink)
        .expect("the guard's create must succeed");
    let elapsed = start.elapsed().as_micros();
    let evaluated = docs_evaluated_count();
    let links_resolved = links_resolved_count();

    // The create actually landed, so the post-create pass really ran
    // (correctness floor for the measurement).
    assert!(
        execution.report.applied,
        "the guard's create must apply: {:?}",
        execution.report
    );
    assert!(
        root.join("notes/guard-probe.md").as_std_path().exists(),
        "the guard's create must write the document"
    );
    // A rule-scoped finding for the created path proves the counted evaluation
    // did rule work: without it, `evaluated == 1` could hold over a document no
    // rule selects, and the guard would certify a scope that measures nothing.
    assert!(
        execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.code == "frontmatter-forbidden-field"),
        "the post-create pass must report the created document's rule violation: {:?}",
        execution.report.warnings
    );

    CreateCost {
        docs_evaluated: evaluated,
        links_resolved,
        total_docs,
        total_links,
        wall_micros: elapsed,
    }
}

/// Post-create validate must evaluate the CREATED document, not the vault. A
/// single create against a ~1500-doc vault must evaluate no more documents than
/// the same create against a ~200-doc vault — proving the pass's rule-evaluation
/// cost is vault-size-independent.
#[test]
fn post_create_validate_scope_is_vault_size_independent() {
    let small = measure_single_create(200);
    let large = measure_single_create(1500);

    eprintln!(
        "post-create validate scope: small=(docs={}, links={}, evaluated={}, resolved={}, {}us) \
         large=(docs={}, links={}, evaluated={}, resolved={}, {}us)",
        small.total_docs,
        small.total_links,
        small.docs_evaluated,
        small.links_resolved,
        small.wall_micros,
        large.total_docs,
        large.total_links,
        large.docs_evaluated,
        large.links_resolved,
        large.wall_micros,
    );

    // The large vault is materially bigger — the guard is only meaningful if the
    // two fixtures actually differ in scale.
    assert!(
        large.total_docs >= small.total_docs * 4,
        "fixtures must differ in scale: small={} large={}",
        small.total_docs,
        large.total_docs
    );

    // A single create evaluates exactly the created document on both sizes.
    const CREATED_DOCS: usize = 1;
    assert_eq!(
        small.docs_evaluated, CREATED_DOCS,
        "small-vault create evaluated {} documents for {CREATED_DOCS} created path",
        small.docs_evaluated
    );
    assert_eq!(
        large.docs_evaluated, CREATED_DOCS,
        "large-vault create evaluated {} documents for {CREATED_DOCS} created path",
        large.docs_evaluated
    );

    // Regression witness: a reversion to the whole-vault pass would evaluate
    // ~every document. Assert the count stays far below the vault size.
    assert!(
        large.docs_evaluated * 20 < large.total_docs,
        "the create evaluated {} of {} documents — the post-create pass is walking the vault",
        large.docs_evaluated,
        large.total_docs
    );

    // The overlay's link re-resolution tracks the blast radius, not the vault
    // (NRN-506). The radius here is the created document's own link plus the
    // referrer's link that the create newly satisfies — a fixed count on both
    // sizes. A reversion to the whole-composite re-resolve would read ~every
    // link in the vault.
    assert!(
        large.total_links >= small.total_links * 4,
        "fixtures must differ in link scale: small={} large={}",
        small.total_links,
        large.total_links
    );
    // Floor: the tally must have counted the two-way radius. Without it a
    // measurement that ran off this thread — or a radius that silently dropped
    // the reverse-lookup hit — would read low and pass the ceilings vacuously.
    const RADIUS_LINKS: usize = 2;
    for cost in [&small, &large] {
        assert!(
            cost.links_resolved >= RADIUS_LINKS,
            "the create re-resolved {} links, below the {RADIUS_LINKS}-link floor (the created \
             document's own link plus the referrer the reverse index must find) — the radius or \
             the measurement is broken",
            cost.links_resolved
        );
    }
    assert!(
        large.links_resolved <= small.links_resolved * 2,
        "link re-resolution grew with vault size: small={} large={}",
        small.links_resolved,
        large.links_resolved
    );
    assert!(
        large.links_resolved * 20 < large.total_links,
        "the create re-resolved {} of {} links — the overlay is re-resolving the vault",
        large.links_resolved,
        large.total_links
    );
}
