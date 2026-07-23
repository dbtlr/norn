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
/// `mutation_lock` via `dispatch_mutation`. Kept in sync with `ClientFrame` in
/// `norn-wire/src/control.rs` — the read-class variants (`Ping`, `Probe`,
/// `Find`, `Count`, `Get`, `Describe`, `Validate`, `Repair`) are excluded.
const MUTATION_VARIANTS: &[&str] = &[
    "Set",
    "New",
    "Edit",
    "Move",
    "Delete",
    "RewriteWikilink",
    "Apply",
];

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
