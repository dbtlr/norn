//! End-to-end smoke tests against the built `norn-parity` binary
//! (`env!("CARGO_BIN_EXE_norn-parity")`), matching the CI recipe
//! (`.github/workflows/ci.yml`'s "parity self-check + consistency" step).
//!
//! Every invocation sets an explicit `current_dir` and passes `--oracle`
//! / `--rewrite` explicitly rather than relying on the bin's own
//! PATH/relative-path defaults — the test binary's own cwd under `cargo
//! test` is not a documented contract, so nothing here leaves resolution
//! to chance.

mod common;

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn-parity")
}

#[test]
fn self_check_end_to_end_is_all_match_exit_0() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();

    // Deliberately points --rewrite at a nonexistent path: self-check runs
    // oracle-vs-oracle and must not require the rewrite artifact (its whole
    // purpose is vetting a case set before any rewrite binary exists).
    let output = Command::new(bin())
        .current_dir(&workspace)
        .arg("--self-check")
        .args(["--oracle", "norn"])
        .args(["--rewrite", "/nonexistent/rewrite-norn"])
        .output()
        .expect("failed to run norn-parity --self-check");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    // NRN-383 adds two `mcp` suite cases (initialize/tools/list handshake +
    // a tools/call), both `ported: false` — self-check ignores `ported` and
    // runs every case, so the total grew from 56 to 58. NRN-378 adds seven
    // `mutate` cases (set/new forecast + refusal), taking the total to 65.
    // NRN-388 adds eight more `mutate` cases (confirmed applies + report bodies:
    // two records applies, a push --format json apply, a warning-bearing json
    // forecast, two refusal-body cases, and the two NRN-371 null-/comment-block
    // promotions), taking the total to 73; all must Match (oracle vs. itself —
    // the confirmed-apply cases via the per-case trace-id normalization).
    // NRN-379 adds five `edit` cases (a confirmed apply, a dry-run forecast, and
    // three refusal/json-ops shapes), taking the total to 78; all Match.
    // NRN-380 adds six `mutate` cascade cases (move apply + dry-run forecast +
    // dry-run --format json, delete apply with --rewrite-to, a backlink-present
    // refusal, and a rewrite-wikilink apply), taking the total to 84; all Match.
    // NRN-382 adds four `repair` cases (bare summary on clean + zoo, `--plan
    // --format paths`, and `--plan --format report`), taking the total to 88; all
    // Match (a `--plan --format json` case is intentionally omitted — the plan's
    // wall-clock `generated_at`, the SHA-256 → BLAKE3 `change_id` swap, and its
    // raw-finding-order arrays make the raw JSON unpinnable; see `REPAIR_CASES`).
    // NRN-394 adds five `apply` cases exercising a HAND-AUTHORED `MigrationPlan`
    // (never one a mutation verb generated) — a move, an add_frontmatter dry-run
    // json forecast, an ADR 0015 owner-set precondition-mismatch refusal, a
    // schema_version refusal, and two `{{seq}}` creates sharing one template in
    // one plan — taking the total to 93; all Match.
    // NRN-384 adds five more `mcp` tools/call cases (get-missing, count, validate,
    // set-forecast, set-confirm-refusal) atop the two existing handshake/get cases,
    // taking the total to 98; self-check ignores `ported`, so all seven `mcp` cases
    // Match (oracle vs. itself).
    // NRN-405 adds four `apply` cases exercising malformed / mis-declared authored
    // plans (a kind/operation mismatch refusal, and unknown-kind + missing-field +
    // wrong-typed-member --format json refusals), taking the total to 102;
    // self-check ignores the divergence (oracle vs. itself always Matches), so all
    // 102 Match.
    // NRN-437 adds five `edit` section-edge cases (SETEXT replace/insert-after +
    // heading-at-EOF replace/append/insert-after), taking the total to 107;
    // self-check runs oracle vs. itself, so all 107 Match.
    // NRN-424 adds four `mutate` wikilink-edge cases (an embed move cascade, two
    // code-fence-shadow rewrites, and a caret-stem rewrite-wikilink), taking the
    // total to 111; self-check runs oracle vs. itself, so all 111 Match.
    // NRN-424 (review round) adds three more `mutate` wikilink-edge cases (a delete
    // --rewrite-to embed variant, and the two PD-119 interior-whitespace cases),
    // taking the total to 114; self-check runs oracle vs. itself, so all 114 Match.
    // NRN-424 (CodeRabbit round) adds two more (PD-120: a rewrite-wikilink refusal
    // and a move skip on an unrepresentable target), taking the total to 116;
    // self-check runs oracle vs. itself, so all 116 Match.
    // NRN-406 (ADR 0022 strict decode) adds three `apply` cases exercising a
    // wrong-typed op field (move `force`, delete `rewrite_to`, str_replace
    // `document_hash`), taking the total to 119; self-check ignores the divergence
    // (oracle vs. itself always Matches), so all 119 Match.
    // NRN-427/NRN-428 (ADR 0023) add two `find` cases (a non-ISO date value and a
    // malformed `--path` glob), taking the total to 121; self-check runs oracle
    // vs. itself, so both Match.
    // NRN-426 (ADR 0023 amendment) adds three `find` predicate-typing cases (a
    // numeric-looking `--eq` and `--not-eq` on a quoted stored value, and a
    // declared-date value-operator refusal), taking the total to 124; self-check
    // runs oracle vs. itself, so all three Match.
    // NRN-436 adds five `apply` cases exercising the bare-anyhow user-fault
    // families now given typed codes (create-destination-exists,
    // create-parent-missing, non-object frontmatter, empty-stem precondition,
    // duplicate op id), taking the total to 129.
    // NRN-406 (ADR 0024) adds one `apply` case exercising independent-files-proceed
    // partial apply, taking the total to 130; self-check runs oracle vs. itself,
    // so all 130 Match.
    // NRN-151 (ADR 0024) adds two `apply` cases (a hand-authored hash-less
    // `delete_document` refusal and a `move_document` stale-hash refusal), taking
    // the total to 132; self-check runs oracle vs. itself, so all 132 Match.
    // NRN-164 adds one `edit` case (a `replace_section` anchored on the ATX-
    // prefixed `## Section One` form) taking the total to 133; self-check runs
    // oracle vs. itself (both refuse the markdown form), so all 133 Match.
    // NRN-407 adds one `get` case (an unknown `--col` field warning) taking the
    // total to 134; self-check runs oracle vs. itself (both emit the `warn:`
    // annotation), so all 134 Match.
    // NRN-417 adds two `errors` cases (the two service-local `--vault` flag
    // collision orderings) taking the total to 136; self-check runs oracle vs.
    // itself, so both Match.
    // NRN-408 adds four mutation refusal `--format json` cases (`set` / `new` /
    // `move` refusals + an `edit` refusal) taking the total to 140; self-check
    // runs oracle vs. itself (both emit the oracle's own bare refusal shape), so
    // all 140 Match.
    assert!(
        stdout.contains("140 cases: 140 match, 0 diverged, 0 drift, 0 stale entries"),
        "expected the exact all-match summary, got:\n{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("  drift  ") || l.trim_end().ends_with("drift")),
        "expected no per-case drift rows, got:\n{stdout}"
    );
}

#[test]
fn default_mode_gates_help_cases_exit_0() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();
    let rewrite = common::rewrite_debug_binary();

    let output = Command::new(bin())
        .current_dir(&workspace)
        .args(["--oracle", "norn"])
        .arg("--rewrite")
        .arg(&rewrite)
        .output()
        .expect("failed to run norn-parity (default mode)");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0 (find/count match, help cases diverge-with-entry, zero drift), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    // NRN-346 ports find + count; NRN-347 adds three deep-facet find cases, nine
    // get cases (incl. --section + alias addressing), a records-format deep-facet
    // case, and six describe cases; NRN-381 adds seven validate cases (summary
    // json/records, paths, code/severity-filtered json/jsonl) — all 42
    // find/count/get/describe/validate cases must Match the oracle (pure
    // byte-parity, no ledger entry). Fourteen cases diverge
    // with ledger entries: the three help cases (help-bare by the `vault`
    // namespace + GLOBAL OPTIONS PD-101, help-find / help-validate by the GLOBAL
    // OPTIONS change PD-102), the two text-layer edge cases (NRN-350 code-opacity
    // PD-103, NRN-349 BOM PD-104), the two CLI-semantics slate cases (zero-indexed
    // `--starts-at` PD-105, last-wins `--limit`/`--no-limit` PD-106), the two
    // URL-semantics slate cases (Markdown-link split-then-decode + block-ref
    // PD-107, external-vs-local scheme classification PD-108), and the five
    // presentation/errors slate cases (the soft-landing diagnostic surface — now
    // four shapes under PD-109, including the NRN-367 owner-side dynamic-field
    // gate's unknown-field rejection — and grammar-wide last-wins PD-110) —
    // covered divergences, not drift.
    // NRN-378 adds seven ported `mutate` cases (set/new forecast + refusal),
    // every one a byte-exact match against the oracle (no ledger entry): the
    // gated total grows from 56 to 63 and the match count from 42 to 49.
    // NRN-388 adds eight more ported `mutate` cases — five MATCH (two records
    // applies, a push --format json apply, a value-not-allowed and a
    // field-conflict refusal body) and three DIVERGE with new ledger entries
    // (the unified --format json warning envelope PD-111, and the two NRN-371
    // null-/comment-only frontmatter mapping-promotions PD-112) — so the gated
    // total grows to 71, the match count to 54, and the diverged count to 17.
    // NRN-379 adds five ported `edit` cases, every one a byte-exact match (no
    // ledger entry): the gated total grows to 76 and the match count to 59; the
    // diverged count stays 17.
    // NRN-380 adds six ported `mutate` cascade cases (move apply + dry-run
    // forecast + dry-run --format json, delete apply with --rewrite-to, a
    // backlink-present refusal, and a rewrite-wikilink apply), all byte-exact
    // matches — the gated total grows to 82 and the match count to 65 (diverged
    // stays 17).
    // NRN-382 adds four ported `repair` cases (bare summary on clean + zoo,
    // `--plan --format paths`, and `--plan --format report`), all byte-exact
    // matches (no ledger entry): the gated total grows to 86 and the match count
    // to 69 (diverged stays 17).
    // NRN-394 adds five ported `apply` cases exercising a hand-authored
    // `MigrationPlan` (a move, an add_frontmatter dry-run json forecast, an ADR
    // 0015 owner-set precondition-mismatch refusal, a schema_version refusal, and
    // two `{{seq}}` creates sharing one template), all byte-exact matches (no
    // ledger entry): the gated total grows to 91 and the match count to 74
    // (diverged stays 17).
    // NRN-384 ports the MCP catalog over the owner session and gates six `mcp`
    // tools/call cases (get-alpha flipped ported + get-missing→isError,
    // count-by-type, validate-code, set-forecast, set-confirm-refusal), all
    // byte-exact matches against the oracle (no ledger entry): the gated total
    // grows to 97 and the match count to 80 (diverged stays 17). The lone
    // tools/list case stays `ported: false` (audit-tool absence + intentional
    // schema divergences make the full catalog un-byte-matchable — see
    // `MCP_CASES`), so the gated `ported` filter skips it.
    // NRN-405 adds four ported `apply` cases, all DIVERGING with new ledger
    // entries: a change-op kind/operation mismatch refusal (PD-113) and the
    // unknown-kind + missing-field + wrong-typed-member malformed-plan refusal
    // codes (PD-114, three cases). The gated total grows to 101 and the diverged
    // count from 17 to 21; the match count stays 80.
    // NRN-437 adds five ported `edit` section-edge cases, all DIVERGING under one
    // ledger entry (PD-115): SETEXT replace_section / insert_after_heading and
    // heading-at-EOF replace_section / append_to_section / insert_after_heading —
    // the oracle corrupts the SETEXT underline / welds onto the EOF marker, the
    // rewrite does not. The gated total grows to 106 and the diverged count from
    // 21 to 26; the match count stays 80.
    // NRN-424 adds four ported `mutate` wikilink-edge cases, all DIVERGING under
    // three ledger entries grouped by mechanism: PD-116 (an embed move cascade
    // dropping the `!`/alias), PD-117 (a code-fence-shadowed backlink, both the
    // move cascade and the rewrite-wikilink verb), and PD-118 (a caret-stem
    // rewrite-wikilink). The gated total grows to 110 and the diverged count from
    // 26 to 30; the match count stays 80.
    // NRN-424 (review round) adds three more ported `mutate` cases: a `delete
    // --rewrite-to` embed variant added to PD-116, and PD-119 (decided-better —
    // interior-whitespace canonicalization on rewrite, a spaced-pipe cascade move
    // and a padded-target rewrite-wikilink). The gated total grows to 113 and the
    // diverged count from 30 to 33; the match count stays 80.
    // NRN-424 (CodeRabbit round) adds two more ported `mutate` cases under PD-120
    // (decided-better — refuse/skip a rename to an unrepresentable wikilink target:
    // a rewrite-wikilink refusal and a move cascade skip). The gated total grows to
    // 115 and the diverged count from 33 to 35; the match count stays 80.
    // NRN-406 (ADR 0022 strict decode) adds three ported `apply` cases, all
    // DIVERGING under one ledger entry (PD-121): a wrong-typed `force` bool, a
    // wrong-typed `rewrite_to`, and a wrong-typed `document_hash` — the oracle
    // silently coerces each (in the delete case, applying a destructive delete),
    // the rewrite refuses `malformed-plan`. The gated total grows to 118 and the
    // diverged count from 35 to 38; the match count stays 80.
    // NRN-406 (ADR 0022 flat finding contract) RE-ANCHORS three already-gated
    // cases from MATCH to DIVERGE under one ledger entry (PD-122): the two CLI
    // finding shapes (`validate-code-filter-zoo` json, `validate-jsonl-code-zoo`
    // jsonl) and the MCP `mcp-tools-call-validate-code-zoo` structuredContent that
    // follows them — the oracle leaks the internal link/diagnostic model and
    // per-variant fields; the rewrite emits the flat closed contract. The gated
    // total stays 118; the match count drops from 80 to 77 and the diverged count
    // grows from 38 to 41.
    // NRN-427/NRN-428 (ADR 0023) add two `find` cases that DIVERGE under one
    // ledger entry (PD-123): the oracle accepts a non-ISO date value and a
    // malformed `--path` glob silently at exit 0 (a wrong result / an empty set),
    // the rewrite refuses at exit 2. The gated total grows to 120 and the diverged
    // count from 41 to 43; the match count stays 77.
    // NRN-426 (ADR 0023 amendment) adds three `find` cases that DIVERGE under one
    // ledger entry (PD-124): the oracle eager-coerces a numeric-looking value so
    // `--eq zip:07030` misses the quoted "07030" (empty) and `--not-eq zip:07030`
    // returns it (the corrupting direction), and a value operator on the declared
    // `due` date accepts a non-ISO value silently; the rewrite dual-types (match /
    // exclude) and refuses the declared-date value at exit 2. The gated total grows
    // to 123 and the diverged count from 43 to 46; the match count stays 77.
    // NRN-436 adds five ported `apply` cases, all DIVERGING under one ledger entry
    // (PD-125): the oracle flattens each bare-anyhow user fault to `internal-error`,
    // the rewrite carries a typed code (create-destination-exists /
    // create-parent-missing / malformed-plan / invalid-precondition). The gated
    // total grows to 128 and the diverged count from 46 to 51; the match count
    // stays 77.
    // NRN-406 (ADR 0024) flips the `apply-authored-sequenced-seq-creates-zoo` case
    // from match to DIVERGED (PD-126: true per-op tracking fixes the NRN-425 report
    // under-count) and adds one DIVERGING case (PD-127:
    // apply-authored-independent-files-proceed-zoo). The gated total grows to 129,
    // the match count drops to 76, and the diverged count grows from 51 to 53.
    // NRN-151 (ADR 0024) adds two DIVERGING `apply` cases: PD-128 (a hand-authored
    // hash-less `delete_document` refuses `delete-hash-required` where the oracle
    // forecasts) and PD-129 (a `move_document` stale-hash refuses
    // `stale-document-hash` — the new optional move CAS the donor lacked). The
    // gated total grows to 131, the diverged count from 53 to 55; match stays 76.
    // NRN-164 adds one DIVERGING `edit` case under ledger entry PD-130
    // (decided-better): a `replace_section` anchored on the natural markdown form
    // `## Section One` — the oracle's resolver requires the bare heading text and
    // refuses `heading not found` (exit 2, write-free), the rewrite strips the ATX
    // prefix and applies (exit 0); stdout, exit, and post-state all diverge. The
    // gated total grows to 132, the diverged count from 55 to 56; match stays 76.
    // NRN-407 (ADR 0022, typed severity channel) RE-ANCHORS three already-gated
    // cases from MATCH to DIVERGE and adds one DIVERGING `get` case, under three
    // ledger entries: PD-131 (get's ambiguity `note:` and unknown-`--col` `warn:`
    // annotations both converge onto the closed `warning:` prefix — the
    // re-anchored `read-get-ambiguous-json-zoo` plus the new
    // `read-get-unknown-col-warning-zoo`), PD-132 (a records not-run label prints
    // the serde-kebab `[not-run]`, not the Debug-lowered `[notrun]` —
    // `apply-authored-precondition-mismatch-refusal-zoo`), and PD-133 (MCP
    // `vault.get` notes cross as typed `{severity, code, message}` objects —
    // `mcp-tools-call-get-missing-zoo`). The gated total grows to 133, the match
    // count drops from 76 to 73 and the diverged count grows from 56 to 60.
    // NRN-417 adds two ported `errors` cases, both DIVERGING under one ledger
    // entry (PD-134): the service-local `--vault <PATH>` flag collided with the
    // global `--vault <NAME>` selector and panicked the rewrite; deleting it
    // means both collision orderings now reach the uniform not-yet-ported
    // outcome instead of the oracle's real local-flag behavior. The gated total
    // grows to 135 and the diverged count from 60 to 62; the match count stays
    // 73.
    // NRN-408 (ADR 0016, one mutation-report JSON policy) RE-ANCHORS three
    // already-gated cases from MATCH to DIVERGE and adds four DIVERGING
    // refusal-json cases, under two ledger entries: PD-135 (the serializer policy
    // — every mutation verb's `--format json` emits the full report envelope,
    // pretty in struct order with one trailing newline, on every outcome path:
    // the re-anchored `mutate-set-push-apply-json-zoo` plus the new
    // `mutate-set-refusal-json-zoo` / `mutate-new-refusal-json-zoo` /
    // `edit-refusal-json-zoo` / `mutate-move-refusal-json-zoo`) and PD-136
    // (`MutationOutcome::Forecast` — a `set`/`new`/`edit` dry-run reports
    // `outcome: forecast`, re-anchoring `edit-json-ops-forecast-zoo` and the MCP
    // `mcp-tools-call-set-forecast-zoo`). The gated total grows to 139, the match
    // count settles at 70 and the diverged count at 69 (the three former MATCH
    // cases become diverged, plus the four new refusal-json cases).
    assert!(
        stdout.contains("139 cases: 70 match, 69 diverged, 0 drift, 0 stale entries"),
        "expected the exact gated summary, got:\n{stdout}"
    );
    for needle in [
        "help-bare",
        "diverged",
        "PD-101",
        "PD-102",
        "PD-103",
        "PD-104",
        "PD-105",
        "PD-106",
        "PD-107",
        "PD-108",
        "PD-109",
        "PD-110",
        "PD-111",
        "PD-112",
        "PD-113",
        "PD-114",
        "PD-115",
        "PD-116",
        "PD-117",
        "PD-118",
        "PD-119",
        "PD-120",
        "PD-121",
        "PD-122",
        "PD-123",
        "PD-124",
        "PD-125",
        "PD-131",
        "PD-132",
        "PD-133",
        "PD-134",
        "err-service-status-local-vault-flag-deleted-zoo",
        "err-service-vault-global-flag-before-subcommand-zoo",
        "PD-135",
        "PD-136",
        "mutate-set-refusal-json-zoo",
        "mutate-new-refusal-json-zoo",
        "edit-refusal-json-zoo",
        "mutate-move-refusal-json-zoo",
        "mutate-set-push-apply-json-zoo",
        "edit-json-ops-forecast-zoo",
        "mcp-tools-call-set-forecast-zoo",
        "read-get-ambiguous-json-zoo",
        "read-get-unknown-col-warning-zoo",
        "mcp-tools-call-get-missing-zoo",
        "read-find-eq-numeric-quoted-value-zoo",
        "read-find-not-eq-numeric-quoted-value-zoo",
        "read-find-declared-date-eq-refuses-zoo",
        "apply-authored-create-destination-exists-refusal-json-zoo",
        "apply-authored-create-parent-missing-refusal-json-zoo",
        "apply-authored-create-nonobject-frontmatter-refusal-json-zoo",
        "apply-authored-empty-stem-precondition-refusal-json-zoo",
        "apply-authored-duplicate-op-id-refusal-json-zoo",
        "validate-code-filter-zoo",
        "validate-jsonl-code-zoo",
        "mcp-tools-call-validate-code-zoo",
        "apply-authored-wrong-typed-bool-refusal-zoo",
        "apply-authored-wrong-typed-rewrite-to-refusal-zoo",
        "apply-authored-wrong-typed-document-hash-refusal-zoo",
        "edit-setext-replace-section-diverge",
        "edit-setext-insert-after-heading-diverge",
        "edit-eof-heading-replace-section-diverge",
        "edit-eof-heading-append-to-section-diverge",
        "edit-eof-heading-insert-after-heading-diverge",
        "wl-move-embed-backlink-diverge",
        "wl-move-code-fence-shadow-diverge",
        "wl-rewrite-wikilink-code-fence-shadow-diverge",
        "wl-rewrite-wikilink-caret-stem-diverge",
        "wl-delete-embed-backlink-diverge",
        "wl-move-spaced-alias-diverge",
        "wl-rewrite-wikilink-padded-target-diverge",
        "wl-rewrite-wikilink-unrepresentable-refusal",
        "wl-move-unrepresentable-skip-diverge",
        "apply-authored-kind-operation-mismatch-refusal-zoo",
        "apply-authored-unknown-kind-refusal-json-zoo",
        "apply-authored-missing-field-refusal-json-zoo",
        "apply-authored-wrong-typed-field-refusal-json-zoo",
        "mutate-set-apply-records-zoo",
        "mutate-new-unknown-field-warning-json-zoo",
        "mutate-set-null-block-promote",
        "mutate-set-comment-block-promote",
        "text-edge-bom-doc-all-cols",
        "text-edge-code-fenced-block-id-link",
        "url-edge-decode-split-blockref",
        "url-edge-scheme-classification",
        "read-find-starts-at-zero-indexed-zoo",
        "read-find-limit-nolimit-last-wins-zoo",
        "err-malformed-config",
        "err-repeated-limit-last-wins-zoo",
        "help-find",
        "help-validate",
        "read-find-json-zoo",
        "read-count-clean",
        "validate-summary-zoo",
        "repair-summary-zoo",
        "repair-plan-report-zoo",
        "apply-authored-move-plan-zoo",
        "apply-authored-precondition-mismatch-refusal-zoo",
        "match",
    ] {
        assert!(
            stdout.contains(needle),
            "expected the gated report to mention `{needle}`, got:\n{stdout}"
        );
    }
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains("  drift  ") || l.trim_end().ends_with("drift")),
        "expected no per-case drift rows, got:\n{stdout}"
    );
}

#[test]
fn consistency_mode_exits_0_with_no_disagreements() {
    if common::oracle_missing("bin_smoke") {
        return;
    }
    let workspace = common::workspace_root();

    let output = Command::new(bin())
        .current_dir(&workspace)
        .args(["--consistency", "--oracle", "norn"])
        .output()
        .expect("failed to run norn-parity --consistency");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
}
