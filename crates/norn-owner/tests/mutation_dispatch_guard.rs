//! Guard (NRN-411): every mutation-class `ClientFrame` variant's `dispatch`
//! match arm routes through `dispatch_mutation`, the one seam that acquires
//! `mutation_lock`. `dispatch_mutation`'s own doc comment concedes this is not
//! structurally impossible to bypass — a hand-rolled arm could still call
//! `slot.serve_read` directly (the `Probe` arm proves the shape compiles) — so
//! this source-scan is the cheap enforcement against a future arm that skips
//! the seam.
//!
//! A source scan, not a runtime test: routing is compile-time match-arm shape,
//! not an observable side effect a black-box test could assert against. It
//! reads `runtime.rs`'s own text, extracts each mutation variant's arm body by
//! brace-matching from its `=>`, and asserts the arm calls `dispatch_mutation(`
//! and never `spawn_blocking(` / `.serve_read(` directly — those two only
//! belong inside `dispatch_read` / `dispatch_mutation` themselves.
//!
//! Temporarily replacing one arm's body with a raw `slot.serve_read(...)` call
//! (bypassing `dispatch_mutation`) fails this test loudly; reverting restores
//! green. That was exercised by hand when this guard was authored — it is not
//! re-run on every CI pass, since a canary arm cannot be committed alongside
//! the guard it is meant to trip.

use std::fs;
use std::path::Path;

/// Every `ClientFrame` variant whose owner-side handling is a mutation
/// (applies when `confirm` is set, forecasts otherwise) and so must acquire
/// `mutation_lock` via `dispatch_mutation`. The companion `READ_VARIANTS` list
/// holds the read-class variants; together they must partition the live
/// `ClientFrame` enum (`every_client_frame_variant_is_classified` derives the
/// variant set from `norn-wire/src/control.rs` and asserts the cover is exact),
/// so a newly added variant forces a read-or-mutation decision here rather than
/// slipping through unclassified and unprotected.
const MUTATION_VARIANTS: &[&str] = &[
    "Set",
    "New",
    "Edit",
    "Move",
    "Delete",
    "RewriteWikilink",
    "Apply",
];

/// Every `ClientFrame` variant whose owner-side handling is a pure read: it
/// serves from the warm cache without acquiring `mutation_lock`. The complement
/// of `MUTATION_VARIANTS` over the live enum.
const READ_VARIANTS: &[&str] = &[
    "Ping", "Probe", "Find", "Count", "Get", "Describe", "Validate", "Repair",
];

/// The `norn-wire/src/control.rs` source that owns the `ClientFrame` enum — the
/// single authority both classification lists are checked against.
fn control_rs_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../norn-wire/src/control.rs")
}

/// Extract the variant identifiers declared in `pub enum ClientFrame { .. }` by
/// brace-matching the enum body and taking the leading identifier of each
/// declaration line (skipping doc comments, attributes, and blank lines). A
/// source scan, matching this file's `dispatch`-arm scan: the enum is the
/// single source of truth for the variant universe, so deriving from its text
/// means a new variant can never be silently omitted from the classification
/// cover below.
fn client_frame_variants(src: &str) -> Vec<String> {
    let enum_start = src
        .find("pub enum ClientFrame")
        .expect("no `pub enum ClientFrame` in control.rs");
    let brace_start = src[enum_start..]
        .find('{')
        .map(|i| enum_start + i)
        .expect("no `{` opening the ClientFrame enum body");
    let mut depth = 0i32;
    let mut body_end = brace_start;
    for (i, c) in src[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = brace_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &src[brace_start + 1..body_end];

    let mut variants = Vec::new();
    for raw in body.lines() {
        let line = raw.trim_start();
        // Skip doc comments, attributes, and blank lines — only variant
        // declarations start with an uppercase identifier at brace depth 1.
        // Nested `{ .. }` payload lines are indented past their variant's own
        // line, which starts with the uppercase name, so a first-token scan of
        // each line that begins with an uppercase ASCII letter is sufficient.
        let first = line.chars().next();
        if !matches!(first, Some(c) if c.is_ascii_uppercase()) {
            continue;
        }
        let ident: String = line
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() {
            variants.push(ident);
        }
    }
    variants
}

/// Extract the `dispatch` match arm body for `ClientFrame::{variant}`: the
/// brace-delimited block following that pattern's `=>`. Panics with a
/// diagnostic if the variant's arm cannot be found, so a rename that drops a
/// variant from `runtime.rs` fails this test rather than silently narrowing
/// what it checks.
fn extract_arm_body<'a>(src: &'a str, variant: &str) -> &'a str {
    let needle = format!("ClientFrame::{variant} ");
    let pat_start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("no `ClientFrame::{variant}` arm found in runtime.rs"));
    let arrow = src[pat_start..]
        .find("=>")
        .map(|i| pat_start + i)
        .unwrap_or_else(|| panic!("no `=>` after `ClientFrame::{variant}` pattern"));
    let brace_start = src[arrow..]
        .find('{')
        .map(|i| arrow + i)
        .unwrap_or_else(|| panic!("no `{{` opening the `ClientFrame::{variant}` arm body"));
    let mut depth = 0i32;
    for (i, c) in src[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[brace_start..=brace_start + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces scanning the `ClientFrame::{variant}` arm body");
}

#[test]
fn every_mutation_variant_routes_through_dispatch_mutation() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime.rs");
    let src = fs::read_to_string(&path).expect("read runtime.rs");

    let mut failures = Vec::new();
    for variant in MUTATION_VARIANTS {
        let body = extract_arm_body(&src, variant);
        if !body.contains("dispatch_mutation(") {
            failures.push(format!(
                "ClientFrame::{variant}'s arm does not call `dispatch_mutation(` — it bypasses \
                 the single mutation_lock acquisition site"
            ));
        }
        if body.contains("spawn_blocking(") || body.contains(".serve_read(") {
            failures.push(format!(
                "ClientFrame::{variant}'s arm calls `spawn_blocking`/`serve_read` directly — \
                 those belong only inside `dispatch_read`/`dispatch_mutation`, not a per-verb arm"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "every mutation-class ClientFrame variant must route through dispatch_mutation \
         (NRN-411):\n{failures:#?}"
    );
}

/// The classification cover must be exact: every live `ClientFrame` variant is
/// named in exactly one of `MUTATION_VARIANTS` / `READ_VARIANTS`, and neither
/// list names a variant the enum no longer has. Deriving the variant universe
/// from `control.rs` (rather than a third hand-kept list) means a newly added
/// variant fails this test until it is deliberately classified read-or-mutation
/// — the mutation guard above only protects variants it knows are mutations, so
/// an unclassified variant would otherwise be silently unprotected.
#[test]
fn every_client_frame_variant_is_classified() {
    let src = fs::read_to_string(control_rs_path()).expect("read control.rs");
    let variants = client_frame_variants(&src);
    assert!(
        !variants.is_empty(),
        "parsed no variants from ClientFrame — the enum scan is broken"
    );

    let mut unclassified = Vec::new();
    let mut double_classified = Vec::new();
    for v in &variants {
        let is_mut = MUTATION_VARIANTS.contains(&v.as_str());
        let is_read = READ_VARIANTS.contains(&v.as_str());
        match (is_mut, is_read) {
            (false, false) => unclassified.push(v.clone()),
            (true, true) => double_classified.push(v.clone()),
            _ => {}
        }
    }
    assert!(
        unclassified.is_empty(),
        "ClientFrame variant(s) not classified read-or-mutation — a new variant must be added to \
         MUTATION_VARIANTS or READ_VARIANTS (an unclassified mutation variant would bypass the \
         dispatch_mutation guard unprotected): {unclassified:?}"
    );
    assert!(
        double_classified.is_empty(),
        "ClientFrame variant(s) in BOTH classification lists — a variant is read xor mutation: \
         {double_classified:?}"
    );

    // Neither list may name a variant the enum no longer declares (a stale
    // classification is as misleading as a missing one).
    for listed in MUTATION_VARIANTS.iter().chain(READ_VARIANTS.iter()) {
        assert!(
            variants.iter().any(|v| v == listed),
            "classification list names `{listed}`, absent from the live ClientFrame enum — \
             remove the stale entry"
        );
    }
}
