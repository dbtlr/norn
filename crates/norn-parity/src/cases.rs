//! The parity case/suite catalog (ADR 0018).
//!
//! Phase 1 (NRN-329) ported the full CLI grammar + custom help renderer, so
//! the three `help` cases (`help-bare`, `help-validate`, `help-find`) are
//! `ported: true` and gated against the oracle; they diverge on the GLOBAL
//! OPTIONS reshape (PD-101 / PD-102). NRN-346 ports `find` + `count` for real:
//! every find/count case is now `ported: true` and must MATCH the oracle (pure
//! byte-parity — no ledger entry). The still-unported surfaces (`get`,
//! `describe`, `validate`) stay `ported: false` — proven sound via
//! `--self-check` (oracle vs. itself) and flipped as later phases port them.
//! `ported` lives on the [`Case`], not the [`Suite`]: phase 1 ports commands
//! individually, so case-level granularity avoids a reshuffle at each port.
//! [`Suite`] stays a reporting/grouping label.
//!
//! Every case here was run against the installed oracle (v0.48.1) and
//! confirmed rerun-stable (identical stdout/stderr/exit code across repeated
//! invocations) before being added — see the crate's implementation notes.
//! Two starter cases required adjustment from a naive reading of the ADR
//! 0018 spec's example argv:
//!
//! - `find` with no predicate and no `--all`/`--in`/`--eq`/etc. prints its
//!   help page and exits 2 by design ("a full-vault dump is almost always a
//!   mistake; require opt-in") — both read-suite `find` cases pass `--all`.
//! - `get notes/alpha --format json` does not resolve (`get` wants a stem or
//!   a full vault-relative path with extension, not a directory+stem
//!   without `.md`) — the case uses the resolvable stem `alpha`.
//!
//! A third adjustment is a dropped case, not a rewritten one: the starter
//! set specified bare `validate --format json` on the zoo profile as the
//! `read`-adjacent raw-findings case. Empirically, the oracle's raw finding
//! order is **not** rerun-stable when a single document carries more than
//! one finding (confirmed: 5 consecutive runs produced 5 distinct SHA-256
//! hashes) — a real oracle non-determinism, not a fixture or harness issue.
//! `--summary` output (grouped counts) *is* stable across the same runs, so
//! it is unaffected. The case is kept but narrowed to `--code
//! frontmatter-required-field-missing`, which matches at most one finding
//! per document in the zoo fixture and was confirmed stable across 5 runs.
//!
//! A fourth finding shaped `HELP_FIXTURE` rather than any one case's argv:
//! see its doc comment below for the empirically-discovered `--help`
//! cache-warming behavior that made self-check unstable until the `help`
//! suite was moved off the fixture the `validate`/`read`/`describe` suites
//! share.

use crate::normalize::Normalization;

/// One fixture vault a [`Case`] runs against: a named `norn-fixtures`
/// [`Profile`](norn_fixtures::Profile) plus the seed that makes generation
/// deterministic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fixture {
    pub profile_name: &'static str,
    pub seed: u64,
}

/// One parity case: an argv to run against both binaries over a fixture
/// vault, with the vault directory as the process cwd (no `-C` flag — see
/// `crate::exec`).
pub struct Case {
    /// Unique across all suites (enforced at runtime in `crate::run` —
    /// [`duplicate_case_id`] — and exercised by the ledger's unknown-case-id
    /// check).
    pub id: &'static str,
    pub argv: &'static [&'static str],
    pub fixture: Fixture,
    /// Ordered JSON-RPC request frames for an MCP case (`argv == ["mcp"]`):
    /// each element is one single-line JSON-RPC message, fed to `norn mcp`'s
    /// stdin newline-delimited in this declaration order. A frame with no
    /// `id` (e.g. `notifications/initialized`) is a fire-and-forget
    /// notification — JSON-RPC promises it no response, so
    /// `crate::mcp::run_case` excludes it from response pairing. `None` for
    /// every non-MCP case (the field this module long stubbed for "future
    /// MCP frame driving" — now activated; see `crate::mcp` for the driver
    /// and `crate::run::run_suites`, which branches on
    /// `case.stdin.is_some()` to route an MCP case through it instead of
    /// the ordinary argv/stdout/stderr comparison).
    pub stdin: Option<&'static [&'static str]>,
    /// Whether this argv WRITES to the vault (`set`/`new`/`edit`/`move`/
    /// `delete`/`rewrite-wikilink`/`migrate` — arriving in later phases). A
    /// mutating case runs each side against its OWN freshly generated vault
    /// (never the cached per-side copy a read case shares) so one case's
    /// writes never contaminate another and both sides start from identical
    /// pre-state; after both sides run, the two resulting vault TREES are
    /// compared and a difference feeds the same three-verdict machinery as a
    /// stdout/stderr/exit difference (match / diverged-with-entry / drift —
    /// no fourth state). `false` everywhere today: every current case is a
    /// pure read.
    pub mutating: bool,
    /// Gates whether the default (gated) bin run includes this case. Phase 0:
    /// `false` everywhere; flips to `true` per-command as phases 1-3 port
    /// surfaces. A ledger entry may only cite `ported` cases (see
    /// `crate::ledger`) — divergence can only be observed on a ported surface.
    pub ported: bool,
    /// The exit code the oracle is expected to produce for this argv. Any
    /// oracle exit differing from this — in self-check AND comparison modes —
    /// is a runner error (exit 2) naming the case, not a quiet match: it
    /// catches silent case rot (e.g. a `get` target that stops existing
    /// yields identical error/error output and would Match forever).
    pub expect_oracle_exit: i32,
    /// Vault-relative doc path this argv depends on (e.g. the `get` target),
    /// checked against the generated fixture [`Manifest`](norn_fixtures::Manifest)
    /// before the case runs. Unmet -> runner error naming case + requirement.
    pub requires_doc: Option<&'static str>,
    /// Validation finding code this argv depends on (e.g. the `--code`
    /// filter), checked against the manifest's expected finding codes. Unmet
    /// -> runner error naming case + requirement.
    pub requires_code: Option<&'static str>,
    /// Per-case normalization steps, appended to the universal
    /// [`DEFAULT`](crate::normalize::DEFAULT). Empty everywhere today —
    /// later-phase ported surfaces that emit e.g. timestamps add steps here
    /// deliberately.
    pub normalize: &'static [Normalization],
    /// Authored-plan capability (NRN-394): for an `apply`-verb case, the raw
    /// `MigrationPlan` source text (JSON or YAML) with every
    /// [`PLAN_VAULT_ROOT_TOKEN`] token standing in for the vault root — the
    /// static fixture Case model has no way to know a per-side temp vault's
    /// absolute path at declaration time, and the pinned oracle REJECTS a plan
    /// whose `vault_root` does not canonicalize to the invoked cwd (see
    /// `norn-core::apply::executor::apply_migration_plan`'s `vault-root-mismatch`
    /// barrier), so an authored plan cannot simply hardcode one.
    /// `crate::run::run_suites` materializes this template into a real file once
    /// PER SIDE (never inside the vault directory itself, so it is invisible to
    /// `crate::poststate`'s tree snapshot) — substituting the token for that
    /// side's own materialized vault path — before running the case, and expects
    /// `argv` to carry exactly one [`PLAN_ARGV_PLACEHOLDER`] token naming where
    /// the materialized file's path is substituted into the argv actually
    /// executed. `None` for every non-apply case; the (`plan.is_some()` ==
    /// argv contains the placeholder) pairing is enforced at runtime (not just a
    /// debug assertion) by [`plan_argv_mismatch`], the same discipline
    /// [`duplicate_case_id`] applies to case ids.
    pub plan: Option<&'static str>,
}

/// The token an authored [`Case::plan`] template uses in place of the
/// materializing side's own absolute vault root. Never collides with real
/// plan content: no vault-relative path or field value plausibly contains
/// literal double-brace `VAULT_ROOT`.
pub const PLAN_VAULT_ROOT_TOKEN: &str = "{{VAULT_ROOT}}";

/// The token an authored-plan [`Case::argv`] uses in place of the materialized
/// plan file's path, substituted by `crate::run::run_suites` before executing.
/// Never collides with a real argv token — no flag or value in the CLI
/// grammar is spelled `{PLAN}`.
pub const PLAN_ARGV_PLACEHOLDER: &str = "{PLAN}";

/// A named group of [`Case`]s — purely a reporting/grouping label. Whether a
/// case is gated lives on the [`Case`]'s own `ported` flag, not here.
pub struct Suite {
    pub name: &'static str,
    pub cases: &'static [Case],
}

/// Shared with `crate::consistency`, whose oracle-only cross-command checks
/// run against the same two fixtures the starter case set uses.
pub(crate) const ZOO_1: Fixture = Fixture {
    profile_name: "zoo",
    seed: 1,
};

pub(crate) const CLEAN_1: Fixture = Fixture {
    profile_name: "clean",
    seed: 1,
};

/// A dedicated fixture for the `help` suite, deliberately NOT shared with
/// any suite that performs a real vault read. Empirically discovered
/// against the oracle: even `--help` (which never reads document content)
/// opens the on-disk cache and, on a vault whose config sets a non-default
/// `links.alias_field`, builds it once with default settings before the
/// real config is consulted — so the *next* command against that same
/// vault (the first real read) always prints a one-time `cache was built
/// with ...; rebuilding` notice on stderr. That notice is real,
/// deterministic oracle behavior, not runner flakiness — but it makes
/// oracle-vs-oracle self-check unstable if a `--help` case and a
/// vault-reading case share one fixture: self-check runs the SAME case
/// twice in a row, and whichever run is first to touch a shared,
/// help-warmed vault pays the rebuild notice while the second does not.
/// Isolating `help` onto its own fixture means no case ever follows a
/// prior case's touch of the *same* vault under a *different* implicit
/// config state, so every fixture's first-ever touch (by whichever case
/// reaches it first in declaration order) is clean and its self-check pair
/// is consistent.
const HELP_FIXTURE: Fixture = Fixture {
    profile_name: "zoo",
    seed: 2,
};

/// Every current case exits 0 on the oracle, reads no case-specific doc, and
/// needs no default-appended normalization — the fields that vary are spelled
/// out per case below.
const NO_NORM: &[Normalization] = &[];

const HELP_CASES: &[Case] = &[
    Case {
        id: "help-bare",
        argv: &["--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "help-validate",
        argv: &["validate", "--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "help-find",
        argv: &["find", "--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// validate ports for real (NRN-381): the read-side standards engine + verb.
/// Every case is `ported: true` and must Match the oracle (pure byte-parity — no
/// ledger entry). The matrix covers the summary shape (json + records), the raw
/// findings shape (json/jsonl/paths), and the triage filters (`--code` /
/// `--severity`). Bare `validate --format json` is deliberately NOT a case: the
/// oracle's raw finding ORDER is not rerun-stable when a document carries more
/// than one finding (see module docs), so every raw-shape case here is either
/// `--summary` (grouped counts, order-free), `--format paths` (sorted + deduped,
/// order-free), or `--code frontmatter-required-field-missing` (at most one
/// finding per document in the zoo).
const VALIDATE_CASES: &[Case] = &[
    Case {
        id: "validate-summary-clean",
        argv: &["validate", "--summary", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "validate-summary-zoo",
        argv: &["validate", "--summary", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The human `--summary` records view: status headline + severity tally +
        // by-code group. Piped (the harness), so palette off — the tally/leader
        // primitives render plain. Grouped counts are order-free → stable.
        id: "validate-summary-records-zoo",
        argv: &["validate", "--summary", "--format", "records"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--format paths`: every affected document path, sorted + deduplicated —
        // order-free, so stable even though the raw finding order is not.
        id: "validate-paths-zoo",
        argv: &["validate", "--format", "paths"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // Narrowed from bare `validate --format json` — see module docs: the
    // oracle's raw finding order is not rerun-stable when a document
    // carries more than one finding. `frontmatter-required-field-missing`
    // matches at most once per document in the zoo fixture, so this stays
    // deterministic while still exercising the raw (non-summary) findings
    // shape. DIVERGES under PD-122 (ADR 0022): the oracle flattens an untagged
    // per-variant body onto the finding; the rewrite emits the flat closed
    // contract (`path`-first, absent optionals omitted, no leaked models).
    Case {
        id: "validate-code-filter-zoo",
        argv: &[
            "validate",
            "--format",
            "json",
            "--code",
            "frontmatter-required-field-missing",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        // The `--code` filter is only meaningful if the zoo fixture actually
        // emits this code; tie the argv to the manifest that generates it.
        requires_code: Some("frontmatter-required-field-missing"),
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--format jsonl` over the same single-per-document code: one finding
        // per line, in document (path) order — stable. DIVERGES under PD-122
        // (ADR 0022): the per-line bytes are now the flat closed contract's field
        // order (`path`-first, `rule` omitted when absent rather than `null`),
        // not the oracle's untagged-body order.
        id: "validate-jsonl-code-zoo",
        argv: &[
            "validate",
            "--format",
            "jsonl",
            "--code",
            "frontmatter-required-field-missing",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: Some("frontmatter-required-field-missing"),
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The `--severity` triage filter, over the grouped summary so the output
        // stays order-free. Exercises the filter plumbing end to end.
        id: "validate-severity-summary-zoo",
        argv: &[
            "validate",
            "--summary",
            "--format",
            "json",
            "--severity",
            "warning",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// `repair` ports as a READ verb (NRN-382): findings → `MigrationPlan`, never
/// applied (the donor's `norn repair` is read-only; `apply` executes the plan).
/// Bare `norn repair` prints the findings summary (order-free: sorted by-code
/// counts + operation/skip tallies); `--plan` emits the plan.
///
/// A `--plan --format json` case is NOT included — the raw serialized
/// `MigrationPlan` is unpinnable for THREE independent reasons, any one fatal:
///   (a) it carries a wall-clock `generated_at`, so its bytes are not
///       rerun-stable — the oracle self-check DRIFTS on it (the same class of
///       limitation that keeps bare `validate --format json` out of the suite);
///   (b) `change_id` is a deliberate SHA-256 → BLAKE3 swap (a pure-Rust
///       dependency choice — see `standards::repair::derive_change_id`), so the
///       per-op ids differ oracle-vs-rewrite by design; and
///   (c) the `operations` / `skipped` arrays carry the raw finding order — the
///       same multiset on both binaries, but a genuinely different sequence, so
///       they diverge oracle-vs-rewrite even setting (a) and (b) aside.
/// Every `--plan` case here is therefore projection-stable under all three:
/// `--format paths` (sorted + deduped) and `--format report` (BTreeMap tallies,
/// sorted top-files, no id/timestamp bytes). The plan's raw JSON SHAPE is pinned
/// by `norn-core`'s `read::repair` + `plan_from_findings` unit tests instead.
/// The zoo carries no error-severity diagnostic, so `has_diagnostic_errors` is
/// false → exit 0.
const REPAIR_CASES: &[Case] = &[
    Case {
        // Bare `norn repair` on a clean vault — the no-findings summary: "0
        // findings across N documents" + "0 repairable as operations, 0
        // skipped", no apply guidance. Order-free and write-free.
        id: "repair-summary-clean",
        argv: &["repair"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Bare `norn repair` on the zoo — the findings summary: total findings,
        // per-code tally (sorted), and the repairable/skipped counts. All
        // order-free (counts, not ordered lists), so rerun-stable.
        id: "repair-summary-zoo",
        argv: &["repair"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--plan --format paths`: every affected document path, sorted +
        // deduplicated — order-free even though the raw plan-op order is not.
        // Exercises the findings → plan generation end to end.
        id: "repair-plan-paths-zoo",
        argv: &["repair", "--plan", "--format", "paths"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--plan --format report`: the human decision-support view — the richest
        // repair surface. Every section is order-free / rerun-stable: the
        // operations-by-kind and skipped-by-reason tallies are BTreeMap-ordered,
        // top-affected-files is sorted (count desc, then path asc), the footnotes
        // and apply-guidance lines are content the raw op order does not perturb,
        // and the header's absolute `vault_root` is folded by the default
        // `VaultRoot` normalization. Piped (the harness), so the palette is off
        // and the tally/headline primitives render plain.
        id: "repair-plan-report-zoo",
        argv: &["repair", "--plan", "--format", "report"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // (A `--plan --format json` case is deliberately omitted — three independent
    // reasons, spelled out in the suite doc comment above: (a) wall-clock
    // `generated_at` self-drifts the oracle, (b) the SHA-256 → BLAKE3 `change_id`
    // swap differs oracle-vs-rewrite by design, and (c) the raw-finding-order
    // `operations`/`skipped` arrays diverge oracle-vs-rewrite. The report/paths
    // cases above are projection-stable under all three; the raw JSON shape is
    // pinned by `norn-core`'s `read::repair` + `plan_from_findings` unit tests.)
];

/// find + count now port for real (NRN-346): the read-surface parity anchor.
/// Every find/count case is `ported: true` and must Match the oracle (pure
/// byte-parity — no ledger entry). The argv matrix covers the filter surface
/// (eq / in / has / missing / dates / text), sort + limit + paging, `--col`
/// projection with the flat facets, and every output format the non-tty harness
/// can drive (paths / records / json / jsonl for find; text / json for count).
/// The dynamic-predicate desugar (`--type note` → `--eq type:note`) and the
/// alias pack (`--group-by` → `--by`) are exercised too, since they lower to the
/// same canonical predicates before parse. `get` stays unported (next task).
const READ_CASES: &[Case] = &[
    // ── count ────────────────────────────────────────────────────────────
    Case {
        id: "read-count-clean",
        argv: &["count"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-count-by-status-clean",
        argv: &["count", "--by", "status"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-count-by-multi-json-clean",
        argv: &["count", "--by", "type,status", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-count-eq-zoo",
        argv: &["count", "--eq", "type:note"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The alias pack: `--group-by` lowers to `--by` before parse.
        id: "read-count-group-by-alias-clean",
        argv: &["count", "--group-by", "status"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── find ─────────────────────────────────────────────────────────────
    Case {
        id: "read-find-json-zoo",
        // `--all`: bare `find` with no predicate prints help and exits 2 by
        // design (see module docs).
        argv: &["find", "--format", "json", "--all"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-col-title-clean",
        argv: &["find", "--col", "title", "--format", "json", "--all"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-eq-json-zoo",
        argv: &["find", "--eq", "type:note", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-in-json-zoo",
        argv: &["find", "--in", "status:active,backlog", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-has-missing-json-zoo",
        argv: &[
            "find",
            "--has",
            "title",
            "--missing",
            "status",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-date-before-json-clean",
        argv: &["find", "--before", "created:2026-01-01", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-427 (ADR 0023): a non-ISO date value on a date operator. The
        // pinned oracle substitutes only `today` and passes `yesterday` verbatim
        // into a TEXT lexical compare — `created < 'yesterday'` matches every
        // stored ISO date (exit 0, a match). The rewrite refuses (exit 2). The
        // divergence is ledgered decided-better (PD-123).
        id: "read-find-bad-date-value-refuses-clean",
        argv: &["find", "--before", "created:yesterday", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-428 (ADR 0023): a malformed `--path` glob. The pinned oracle
        // `.ok()`-discards the parse error and filters out every doc — an empty
        // result at exit 0, indistinguishable from a real no-match. The rewrite
        // refuses (exit 2). Ledgered decided-better (PD-123).
        id: "read-find-malformed-path-glob-refuses-clean",
        argv: &["find", "--path", "{unclosed", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-426 (ADR 0023 amendment): a numeric-looking `--eq` value against a
        // QUOTED stored value. The pinned oracle eagerly coerces `07030` to the
        // integer 7030 before SQL, which never equals the stored string "07030" —
        // zero results at exit 0, a silent miss. The rewrite dual-types the
        // undeclared field, matching either representation (here the quoted
        // "07030"). Ledgered decided-better (PD-124).
        id: "read-find-eq-numeric-quoted-value-zoo",
        argv: &["find", "--eq", "zip:07030", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-426: the corrupting direction — `--not-eq` on the same value. The
        // oracle compares the stored string "07030" against the integer 7030,
        // finds them unequal, and RETURNS the doc the user meant to exclude. The
        // rewrite excludes either representation (De Morgan over the dual). The
        // oracle wrongly returns alpha; the rewrite drops alpha and returns the
        // rest (field-missing docs stay included). PD-124.
        id: "read-find-not-eq-numeric-quoted-value-zoo",
        argv: &["find", "--not-eq", "zip:07030", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-426: a value-comparison predicate on a schema-DECLARED date field
        // (`due: date`) with a non-ISO value. The oracle coerces `someday` to a
        // string and lexically compares — zero results at exit 0. The rewrite
        // refuses (exit 2) naming the field, the declared type, and the value,
        // extending the ADR 0023 strictness class to value operators. PD-124.
        id: "read-find-declared-date-eq-refuses-zoo",
        argv: &["find", "--eq", "due:someday", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-sort-limit-json-zoo",
        argv: &[
            "find",
            "--eq",
            "type:note",
            "--sort",
            "title",
            "--limit",
            "5",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Default (piped) format is paths; the 10-limit truncation note lands on
        // stderr — this pins the paths format AND the truncation signal.
        id: "read-find-paths-zoo",
        argv: &["find", "--format", "paths", "--all"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Records format under the non-tty harness: term_width 80, separators
        // capped at 60, the `·` count line — the whole records primitive path.
        id: "read-find-records-zoo",
        argv: &["find", "--eq", "type:note", "--format", "records"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "read-find-jsonl-zoo",
        argv: &["find", "--format", "jsonl", "--all"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The dynamic-predicate desugar: `--type note` lowers to `--eq
        // type:note` before parse (ADR 0010), so the routed query is identical.
        id: "read-find-dynamic-desugar-json-zoo",
        argv: &["find", "--type", "note", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Deep-facet load (NRN-347): `--col .headings` loads each match's
        // headings and emits them verbatim — the previously-gated facet, now
        // pinned to the oracle forever.
        id: "read-find-col-headings-json-zoo",
        argv: &[
            "find",
            "--eq",
            "type:note",
            "--col",
            ".headings",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--all-cols` — the full structured dump: frontmatter + headings + the
        // three link sets + body, all loaded via the deep-fetch (NRN-347).
        id: "read-find-all-cols-json-zoo",
        argv: &[
            "find",
            "--eq",
            "type:note",
            "--all-cols",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Incoming links exercise the back-link query (a distinct cache path
        // from headings/outgoing) — pin it too.
        id: "read-find-col-incoming-json-zoo",
        argv: &[
            "find",
            "--col",
            ".incoming_links",
            "--format",
            "json",
            "--all",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── get (NRN-347) ──────────────────────────────────────────────────────
    // The anchor: get ports for real. Stem addressing, each format, the
    // ambiguity/not-found note+exit contract, and the markdown exact-source read.
    Case {
        id: "read-get-alpha-zoo",
        // Stem form, not `notes/alpha` — see module docs. The resolved target
        // is the vault-relative doc below; tie the argv to the manifest.
        argv: &["get", "alpha", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Default records: the full field dump (frontmatter + headings + links),
        // no count line, no color.
        id: "read-get-alpha-records-zoo",
        argv: &["get", "alpha"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--all-cols` — the full structured dump incl. body.
        id: "read-get-alpha-all-cols-json-zoo",
        argv: &["get", "alpha", "--all-cols", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--format markdown` — the exact source file the owner read from disk
        // (ADR 0014). Byte-faithful, no trailing-newline fixup.
        id: "read-get-alpha-markdown-zoo",
        argv: &["get", "alpha", "--format", "markdown"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Ambiguous stem: one record per candidate + a `note:` on stderr, exit 0.
        // `duplicate` resolves to archive2/duplicate.md and notes/duplicate.md.
        id: "read-get-ambiguous-json-zoo",
        argv: &["get", "duplicate", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/duplicate.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Not-found: `[]` on stdout, an `error:` note on stderr, exit 1 — the
        // note-driven failure signal. `zzz-no-such-doc` is not a fixture stem.
        id: "read-get-not-found-json-zoo",
        argv: &["get", "zzz-no-such-doc", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 1,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--section`: resolve a named heading's exact span. `notes/alpha.md` has
        // nested headings (`# Alpha`, `## Section One/Two/Three`); the resolved
        // span is a keyed `sections` object. Pins the section-read primitive.
        id: "read-get-section-json-zoo",
        argv: &[
            "get",
            "alpha",
            "--section",
            "Section One",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The same section read in records — the verbatim span rendered as a
        // labeled block (request order preserved), byte-identical to the json span.
        id: "read-get-section-records-zoo",
        argv: &["get", "alpha", "--section", "Section One"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Alias addressing: the zoo config sets `links.alias_field: aliases` and
        // `notes/beta.md` declares `aliases: [bee]`, so `get bee` resolves via the
        // alias fallback (stem `bee` does not exist). Pins alias resolution.
        id: "read-get-alias-json-zoo",
        argv: &["get", "bee", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/beta.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Deep facets in the RECORDS format (json-only elsewhere in the matrix):
        // `.headings` folds to `# text` display lines via record_block reflow.
        id: "read-find-col-headings-records-zoo",
        argv: &[
            "find",
            "--eq",
            "type:note",
            "--col",
            ".headings",
            "--format",
            "records",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── NRN-331 / NRN-332: decided CLI-semantics divergences ─────────────────
    // Each DIFFERS from the oracle and is gated by a ledger entry (PD-105 /
    // PD-106). Both are ported: true so the gated run compares them.
    Case {
        // NRN-332: `--starts-at` is now a ZERO-indexed offset (default 0). With
        // `--sort path --limit 1 --starts-at 1` the rewrite returns the SECOND
        // document; the oracle's 1-indexed reading (offset floored to 0) returns
        // the FIRST. The stderr "showing 1 of N" note is identical on both sides,
        // so the divergence is the single stdout path line (PD-105).
        id: "read-find-starts-at-zero-indexed-zoo",
        argv: &[
            "find",
            "--all",
            "--sort",
            "path",
            "--limit",
            "1",
            "--starts-at",
            "1",
            "--format",
            "paths",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-331: `--limit N --no-limit` competes over the effective cap. The
        // oracle REJECTS the combo (`conflicts_with`, exit 2); the rewrite
        // accepts it last-wins, so `--no-limit` (last) wins → unlimited, exit 0.
        // The exit + stdout + stderr all diverge; gated by PD-106.
        id: "read-find-limit-nolimit-last-wins-zoo",
        argv: &[
            "find",
            "--eq",
            "type:note",
            "--sort",
            "path",
            "--limit",
            "2",
            "--no-limit",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        // The oracle errors on the conflicting pair — pin that exit so silent
        // case rot (e.g. the oracle later accepting it) is caught.
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// describe ports for real (NRN-347): the structure view (folders + declared
/// rules + inbox + the full schema under `--format json`) and the contents
/// summary (`--data`/`--stats`/`--by`). All `ported: true`, must Match the
/// oracle. The `describe-json-zoo` case pins the schema serialization (the full
/// validate config) as the end-user contract; any intended change moves through
/// the divergence ledger.
const DESCRIBE_CASES: &[Case] = &[
    Case {
        id: "describe-zoo",
        argv: &["describe"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        id: "describe-data-zoo",
        argv: &["describe", "--data"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--stats` is a pure alias for `--data` — same output.
        id: "describe-stats-zoo",
        argv: &["describe", "--stats"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // The structure view in full, incl. the serialized schema (validate
        // config) — this case pins the schema shape as the end-user contract;
        // any intended change moves through the divergence ledger.
        id: "describe-json-zoo",
        argv: &["describe", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // `--by` implies data and bypasses the identity-skip; nested fields.
        id: "describe-by-clean",
        argv: &["describe", "--by", "type,status"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // Data summary on the clean fixture — a different doc mix + date bounds.
        id: "describe-data-clean",
        argv: &["describe", "--data"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// The text-layer edge fixture (NRN-349 / NRN-350): the valid zoo plus three
/// isolated divergence probes (`edge/bom-doc.md`, `edge/code-anchor.md`,
/// `edge/code-linker.md`). Dedicated to the two cases below so the BOM /
/// code-opacity divergences never touch a shared zoo/clean case.
const TEXT_EDGE_1: Fixture = Fixture {
    profile_name: "text-edge",
    seed: 1,
};

/// The decided text-layer / URL-semantics divergences from the pinned oracle,
/// each pinned by a ledger entry (PD-104 / PD-103 for BOM + code-opacity;
/// PD-107 / PD-108 for the URL split-decode + scheme classification). Every case
/// is `ported: true` and DIVERGES — the oracle and rewrite differ, and the entry
/// documents why.
const TEXT_EDGE_CASES: &[Case] = &[
    Case {
        // NRN-349: the oracle reads a BOM-prefixed doc as frontmatter-less; the
        // rewrite skips the BOM and indexes the block. `--all-cols` surfaces the
        // frontmatter facet (present vs. empty) and the body (post-fence vs. the
        // whole file). Pinned by PD-104.
        id: "text-edge-bom-doc-all-cols",
        argv: &["get", "bom-doc", "--all-cols", "--format", "json"],
        fixture: TEXT_EDGE_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("edge/bom-doc.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-350 / ADR 0019: `code-linker` references a block-id defined only
        // inside a fenced code block. The oracle registers the code-fenced id and
        // resolves the link (outgoing); the rewrite treats code as opaque, so the
        // link is unresolved. Pinned by PD-103.
        id: "text-edge-code-fenced-block-id-link",
        argv: &[
            "get",
            "code-linker",
            "--col",
            ".outgoing_links,.unresolved_links",
            "--format",
            "json",
        ],
        fixture: TEXT_EDGE_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("edge/code-linker.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-356: split-then-decode + block-ref reclassification of Markdown
        // link destinations. `note%23draft.md` stays one path segment (oracle:
        // `note` + anchor `draft.md`); `url-block-target.md#^blk1` is a block ref
        // resolving against the sibling's `^blk1` (oracle: heading anchor
        // `^blk1`, anchor-missing). Pinned by PD-107.
        id: "url-edge-decode-split-blockref",
        argv: &[
            "get",
            "url-decode-linker",
            "--col",
            ".outgoing_links,.unresolved_links",
            "--format",
            "json",
        ],
        fixture: TEXT_EDGE_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("edge/url-decode-linker.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-357: generic external-vs-local classification by URL rules. Every
        // destination here is external (URI scheme incl. mixed-case, `//`, or a
        // drive letter), so the rewrite drops them all; the oracle's lowercase
        // prefix list models each as an unresolved local link. Pinned by PD-108.
        id: "url-edge-scheme-classification",
        argv: &[
            "get",
            "url-scheme-linker",
            "--col",
            ".outgoing_links,.unresolved_links",
            "--format",
            "json",
        ],
        fixture: TEXT_EDGE_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("edge/url-scheme-linker.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// A dedicated valid-zoo fixture for the error-surface suite (NRN-361 / NRN-365),
/// seed 3 — kept off the read suite's `ZOO_1` (seed 1) and the `help` suite
/// (seed 2) so an error probe never shares warm-cache state with a matched read
/// case (the text-edge isolation discipline).
const ERR_ZOO: Fixture = Fixture {
    profile_name: "zoo",
    seed: 3,
};

/// The malformed-config fixture: the valid zoo doc tree under a deliberately
/// invalid `.norn/config.yaml` (the `bad-config` profile). Every read against it
/// warms into a config rejection — its own profile, shared with no other case.
const BAD_CONFIG_1: Fixture = Fixture {
    profile_name: "bad-config",
    seed: 1,
};

/// The decided error-surface divergences from the pinned oracle (NRN-361 /
/// NRN-365). Every case is `ported: true` and DIVERGES — the rewrite's soft-
/// landing diagnostic surface (prefix + wording + did-you-mean hints) and its
/// grammar-wide last-wins differ from the oracle, each pinned by a ledger entry
/// (PD-109 / PD-110). Isolated onto dedicated fixtures so no divergence perturbs
/// a matched read case.
const ERROR_CASES: &[Case] = &[
    Case {
        // NRN-361: an unresolvable `--links-to` target is rejected. The oracle
        // prints a bare `no document matched: <t>` line; the rewrite prints the
        // prefixed, reworded `norn: no document matched path or stem: <t>` — the
        // soft-landing diagnostic surface. stdout empty and exit 1 on both;
        // stderr diverges. Pinned by PD-109.
        id: "err-links-to-unresolvable-zoo",
        argv: &[
            "find",
            "--links-to",
            "zzz-nonexistent-target",
            "--format",
            "json",
        ],
        fixture: ERR_ZOO,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 1,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-361: a near-miss `--col` facet typo. Both sides warn and exit 0
        // with identical stdout; the rewrite leads the warning with a
        // did-you-mean (`did you mean '.headings'?`) via the shared `closest`
        // heuristic, so stderr diverges. Pinned by PD-109.
        id: "err-col-facet-did-you-mean-zoo",
        argv: &["find", "--all", "--col", ".headngs", "--format", "json"],
        fixture: ERR_ZOO,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-361: a malformed `.norn/config.yaml` (one unknown top-level key).
        // The vault warms into a config rejection on both sides — exit 1, empty
        // stdout — but the rewrite prefixes the diagnostic (`norn: invalid
        // config …`) where the oracle prints the bare line. The previously
        // deferred malformed-config case, now addable. Pinned by PD-109.
        id: "err-malformed-config",
        argv: &["find", "--all", "--format", "json"],
        fixture: BAD_CONFIG_1,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 1,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-367: an unknown dynamic field. `--titel foo` desugars to a dynamic
        // `titel` predicate; the owner-side field-universe gate rejects it with a
        // did-you-mean. Both sides exit 1 with empty stdout; the oracle prints
        // one bare inline line (`unknown field `titel` — did you mean `title`?
        // (…)`), the rewrite prints the soft-landing split — a `norn:`-prefixed
        // headline naming the field plus a `hint:` did-you-mean — so stderr
        // diverges. Same soft-landing surface as the other error cases, pinned by
        // PD-109. `title` is a common field in the zoo, so the near-miss resolves.
        id: "err-unknown-dynamic-field-did-you-mean-zoo",
        argv: &["find", "--titel", "foo", "--format", "json"],
        fixture: ERR_ZOO,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 1,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // NRN-365: a repeated scalar flag. `--limit 5 --limit 1` is a hard
        // `ArgumentConflict` on the oracle (exit 2, nothing on stdout); the
        // rewrite's grammar-wide `args_override_self` resolves it last-wins →
        // limit 1, exit 0, one path on stdout. Exit + stdout + stderr all
        // diverge. Pinned by PD-110.
        id: "err-repeated-limit-last-wins-zoo",
        argv: &[
            "find", "--all", "--sort", "path", "--limit", "5", "--limit", "1", "--format", "paths",
        ],
        fixture: ERR_ZOO,
        stdin: None,
        mutating: false,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// A dedicated zoo fixture for the MCP suite (seed 4 — the next free seed
/// after `ERR_ZOO`'s 3), never shared with any CLI suite. `norn mcp`'s
/// `initialize`/`tools/list` never touch the vault at all, and `tools/call`
/// against it was empirically confirmed clean (no cache-rebuild stderr
/// noise, rerun-stable stdout across repeated invocations on both a
/// never-touched and an already-touched copy of the real generated `zoo`
/// vault) — but isolation is cheap insurance matching the same discipline
/// `HELP_FIXTURE`'s doc comment explains: a suite with any risk of
/// first-touch-order sensitivity gets its own fixture rather than
/// potentially interacting with another suite's warm cache state.
const MCP_FIXTURE: Fixture = Fixture {
    profile_name: "zoo",
    seed: 4,
};

/// MCP frame driving (NRN-383/NRN-384, ADR 0018 phase 3): drive `norn mcp`'s
/// stdio JSON-RPC surface with the ordered frames in `stdin` and compare
/// responses frame-by-frame (see `crate::mcp`), instead of the ordinary
/// argv/stdout/stderr comparison every other case uses.
///
/// NRN-384 ported the MCP catalog over the owner session (see `norn-mcp`), so the
/// `tools/call` cases are now `ported: true` and gated against the oracle. The
/// lone `tools/list` case (`mcp-initialize-tools-list-zoo`) stays `ported: false`
/// DELIBERATELY: the full catalog cannot byte-match the pinned oracle, for two
/// structural reasons, so gating it would force ledger entries this task does not
/// own. First, `vault.audit` — the oracle serves 14 tools, but the rewrite's audit
/// VERB is not yet ported (no `session.audit`, no durable telemetry store), so the
/// catalog omits it (13 tools); advertising a dead tool would violate the
/// thin-adapter contract. Second, the published `inputSchema` reflects the
/// rewrite's DELIBERATE param redesigns — most visibly zero-indexed paging
/// (NRN-332), which changes `get`'s `starts_at` default. Both are flagged for
/// adjudication (audit-verb port; a tools/list ledger entry or a normalization
/// rule) rather than silently ledgered here. `--self-check` (oracle vs. itself)
/// still runs and Matches this case, keeping the frame-driving harness exercised
/// over the full catalog.
const MCP_CASES: &[Case] = &[
    Case {
        // The MCP session lifecycle anchor: `initialize` (id 1) then
        // `tools/list` (id 2), with the standard `notifications/initialized`
        // in between (a notification — no `id`, no response expected, so
        // `crate::mcp::run_case` excludes it from pairing). Stays `ported:
        // false` — see the module note above (audit-tool absence + intentional
        // schema divergences make the full catalog un-byte-matchable).
        id: "mcp-initialize-tools-list-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ]),
        mutating: false,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A `tools/call` of a read tool (`vault.get`) against an existing fixture
        // doc — the mechanics end to end past the handshake: a real tool
        // invocation, structured JSON in `structuredContent`, compared
        // frame-by-frame. Now `ported: true` (NRN-384) and gated against the
        // oracle: the full-facet record projection is byte-identical.
        id: "mcp-tools-call-get-alpha-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.get","arguments":{"targets":["alpha"]}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A `vault.get` for a target that does not resolve: the report's
        // `error:`-prefixed missing-target note maps to `isError: true` while the
        // envelope still carries the (empty) records + notes — the NRN-214 signal,
        // proven byte-identical on the MCP surface.
        id: "mcp-tools-call-get-missing-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.get","arguments":{"targets":["nonexistent-doc-xyz"]}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A read `tools/call` with args: `vault.count` grouped by a frontmatter
        // field. The untagged wire `CountReport` projects into the flat
        // `{ total, by, groups }` envelope byte-identically to the oracle.
        id: "mcp-tools-call-count-by-type-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.count","arguments":{"by":"type"}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A `vault.validate` tools/call, narrowed by `code` to a single finding
        // class. The narrow is deliberate: a bare full-vault validate on the zoo
        // trips a finding-ORDER divergence between the oracle and the rewrite
        // (visible at the CLI level too — the reason bare `validate --format json`
        // is not a gated CLI case either), which is out of this task's scope.
        // DIVERGES under PD-122 (ADR 0022): the structuredContent findings follow
        // the flat closed contract, so the leaked internal link/diagnostic model
        // and per-variant fields the oracle emitted are gone.
        id: "mcp-tools-call-validate-code-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.validate","arguments":{"code":["frontmatter-required-field-missing"]}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A mutation `tools/call` FORECAST: `vault.set` with `confirm` absent is a
        // dry-run — the report carries the planned `frontmatter_changes` with
        // `applied: false` and `isError: false` (a forecast never throws in an SDK
        // that raises on isError, NRN-220). Write-free, so `mutating: false`. The
        // field (`project`) is a DECLARED typed-note field so the forecast is
        // warning-free — a warning would trip the unified-envelope divergence
        // (PD-111 covers `set --format json`; its cases list would need an MCP
        // extension before an MCP warning frame could gate, flagged not extended).
        id: "mcp-tools-call-set-forecast-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.set","arguments":{"target":"alpha","field":["project:myproj"]}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    Case {
        // A mutation `tools/call` REFUSAL: `vault.set --confirm` against a target
        // that does not resolve. The precondition refuses BEFORE any write, so the
        // report is `outcome: refused` + `isError: true` while the vault is
        // byte-identical — the NRN-220 coded-refusal-as-structured-result shape on
        // the MCP surface. A refusal writes nothing, so `mutating: false` is
        // correct even though `confirm: true`.
        id: "mcp-tools-call-set-confirm-refusal-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.set","arguments":{"target":"nonexistent-doc-xyz","field":["title:X"],"confirm":true}}}"#,
        ]),
        mutating: false,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
];

/// The mutation-edge fixture (NRN-371): the valid zoo plus the two non-mapping
/// frontmatter probes (`shapes/null-block.md`, `shapes/comment-block.md`),
/// isolated on the `mutate-edge` profile so the decided mapping-promotion
/// divergence never touches a shared zoo/clean case (the text-edge discipline).
const MUTATE_EDGE_1: Fixture = Fixture {
    profile_name: "mutate-edge",
    seed: 1,
};

/// The section-edge fixture (NRN-437): the valid zoo plus the two body-heading
/// probes (`shapes/setext.md`, `shapes/eof-heading.md`), isolated on the
/// `section-edge` profile so the decided section-op corruption fix never touches
/// a shared zoo/clean case (the text-edge / mutate-edge discipline).
const SECTION_EDGE_1: Fixture = Fixture {
    profile_name: "section-edge",
    seed: 1,
};

/// The wikilink-edge fixture (NRN-424): the valid zoo plus the embed /
/// code-fence-shadow / caret-stem backlink probes (`wl/*.md`), isolated on the
/// `wikilink-edge` profile so the decided wikilink-rewriter corruption fixes
/// never touch a shared zoo/clean case (the section-edge discipline).
const WIKILINK_EDGE_1: Fixture = Fixture {
    profile_name: "wikilink-edge",
    seed: 1,
};

/// The trace-id normalization every CONFIRMED-apply case appends: the pinned
/// oracle mints a random `trace:`/`trace_id` on apply, the rewrite emits an empty
/// one by contract, so without this an otherwise byte-equal apply would diverge
/// on the id alone (see [`Normalization::TraceId`]). Only on the applies that
/// MATCH — a diverged/refused/forecast case carries no id to normalize.
const TRACE_NORM: &[Normalization] = &[Normalization::TraceId];

/// A cascade-verb `--format json` forecast normalizes only the root-dependent
/// `plan_hash` (see [`Normalization::PlanHash`]) — no trace id on a forecast.
const PLAN_HASH_NORM: &[Normalization] = &[Normalization::PlanHash];

/// Mutation-verb parity for `set` / `new` (NRN-378 forecasts/refusals, extended
/// by NRN-388 with the CONFIRMED-apply path and report BODIES). Each case is
/// `mutating: true`, so the harness runs it against a FRESH per-case vault per
/// side and folds the two post-mutation TREES into the same match/diverged/drift
/// decision as the stdout/stderr/exit comparison — a forecast/refusal is proven
/// write-free on BOTH binaries, and a confirmed apply is proven to write
/// byte-identical trees on both. The confirmed-apply cases append [`TRACE_NORM`]
/// (the oracle's random telemetry id → the rewrite's empty id); everything else
/// in the applied report is deterministic and byte-compared as-is.
///
/// Three cases DIVERGE from the pinned oracle, each pinned by a ledger entry:
/// the unified `--format json` warning envelope (PD-111) and the NRN-371 null-/
/// comment-only frontmatter mapping-promotion (PD-112, two cases). Note the
/// `new` post-create re-validate pass is deliberately not yet ported (a tracked
/// follow-up), so a `new` apply whose created doc would trip a validate finding
/// (e.g. a `missing-required-field: title` on a titleless create) still diverges
/// on that warning — OUTSIDE this task's two adjudicated entries. The apply cases
/// here steer clear of it (each supplies `title`); the `new`-leaves-an-empty-block
/// half of the NRN-371 roundtrip is therefore NOT pinned as a gated case (it would
/// need a third, unauthorized entry), and the load-bearing NRN-371 behavior is
/// captured wholly by the set-on-null-block half below.
const MUTATE_CASES: &[Case] = &[
    // set: a nonexistent target is a clean pre-write refusal (exit 2), write-free.
    Case {
        id: "mutate-set-missing-target-zoo",
        argv: &["set", "no-such-doc-xyzzy", "status:done"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // set: plain (piped, no --yes) is an implicit dry-run — the plan preview.
    Case {
        id: "mutate-set-forecast-note-zoo",
        argv: &["set", "alpha", "summary:hello"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // set: explicit --dry-run plan output.
    Case {
        id: "mutate-set-dry-run-note-zoo",
        argv: &["set", "alpha", "summary:hello", "--dry-run"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // new: explicit-path forecast (Mode A) — `--title` is inert here (warns).
    Case {
        id: "mutate-new-explicit-forecast-zoo",
        argv: &["new", "notes/brand-new.md", "--title", "Brand New"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // new: --as a non-creatable rule (the zoo declares no `target:`) refuses.
    Case {
        id: "mutate-new-by-rule-not-creatable-zoo",
        argv: &["new", "--as", "typed-note"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // new: an existing destination without --force is a refusal (exit 2).
    Case {
        id: "mutate-new-exists-refusal-zoo",
        argv: &["new", "notes/alpha.md"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // new: --force over an existing destination, forecast (writes nothing).
    Case {
        id: "mutate-new-force-forecast-zoo",
        argv: &["new", "notes/alpha.md", "--force"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── NRN-388: confirmed applies + report bodies ───────────────────────────
    // set: a plain confirmed apply (records). The field-change line is compared
    // AND the mutated file is byte-compared via post-state — proving both sides
    // wrote the same bytes, not just printed the same plan. TRACE_NORM collapses
    // the oracle's random applied `trace:` id to the rewrite's empty one.
    Case {
        id: "mutate-set-apply-records-zoo",
        argv: &["set", "alpha", "summary:hello", "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // new: a multi-field confirmed apply (records). Pins the padded key/value
    // block and byte-compares the created file. `title` is supplied so the
    // create satisfies the global `title` requirement — steering clear of the
    // not-yet-ported post-create re-validate `missing-required-field` warning
    // (see the suite doc); `aaa` is an unknown field, so both sides emit the
    // donor-faithful records `unknown field` warning short-form (a MATCH).
    Case {
        id: "mutate-new-apply-records-zoo",
        argv: &[
            "new",
            "notes/created-doc.md",
            "--field",
            "title:Hello",
            "--field",
            "aaa:1",
            "--yes",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // set --push --format json, confirmed apply: pins the collapsed donor op
    // shape — a `--push` reports as `{"op":"set",...,"new":[…]}` (the whole list
    // after the append), not a distinct push op. Byte-compares the mutated file.
    Case {
        id: "mutate-set-push-apply-json-zoo",
        argv: &[
            "set",
            "alpha",
            "--push",
            "tags:newtag",
            "--yes",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // new with an unknown field under --force, --format json (forecast): the one
    // DECIDED warning-shape divergence (PD-111). The donor emits an inconsistent
    // per-kind warning object (`{field, kind}` for new); the rewrite emits its
    // unified envelope `{code, field?, message}`. stdout diverges on the warning
    // object; both write nothing (forecast) so the trees match. Pinned by PD-111.
    Case {
        id: "mutate-new-unknown-field-warning-json-zoo",
        argv: &[
            "new",
            "notes/warn-doc.md",
            "--field",
            "bogus:x",
            "--force",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // set: a value-not-allowed refusal, comparing the MESSAGE BODY (not just the
    // exit code). The offending value appears in the stderr line on both sides,
    // exit 2, write-free. A MATCH — the refusal surface is donor-faithful.
    Case {
        id: "mutate-set-value-not-allowed-body-zoo",
        argv: &["set", "task-001", "status:bogus", "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("tasks/task-001.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // set: a field-conflict refusal (same key across --field + --push). The
    // multi-line explainer body is compared, exit 2, write-free — a MATCH.
    Case {
        id: "mutate-set-field-conflict-body-zoo",
        argv: &[
            "set", "alpha", "--field", "tags:x", "--push", "tags:y", "--yes",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // NRN-371: a `set` against a bare-`null` frontmatter block. The oracle refuses
    // (`frontmatter is not a top-level mapping`, exit 2, write-free); the rewrite
    // promotes the null block to an empty mapping and applies (exit 0, tree
    // mutated). stdout + stderr + exit AND the post-state tree all diverge; pinned
    // by PD-112. This is the load-bearing half of the new→set null-frontmatter
    // roundtrip — a valid input that must yield a valid output.
    Case {
        id: "mutate-set-null-block-promote",
        argv: &["set", "null-block", "title:Hi", "--yes"],
        fixture: MUTATE_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("shapes/null-block.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // NRN-371 (same class as the null block): a `set` against a comment-only
    // frontmatter block. The oracle refuses identically; the rewrite preserves
    // the comment and appends the field. Also pinned by PD-112.
    Case {
        id: "mutate-set-comment-block-promote",
        argv: &["set", "comment-block", "title:Hi", "--yes"],
        fixture: MUTATE_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("shapes/comment-block.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── NRN-380: the cascade verbs (move / delete / rewrite-wikilink) ─────────
    // The zoo `cycle-a/b/c` docs form a deterministic 3-cycle: `notes/cycle-a.md`
    // links `[[cycle-b]]`, `cycle-b`→`[[cycle-c]]`, `cycle-c`→`[[cycle-a]]`. So
    // `cycle-b` has exactly ONE incoming backlink (from `cycle-a`) — a clean,
    // single-file cascade whose post-state proves both binaries rewrote the same
    // bytes.
    //
    // move: a confirmed single-file move with a backlink rewrite (records). The
    // `✓ moved … / ✓ rewrote 1 backlink across 1 file` summary is compared AND the
    // moved file + rewritten backlink are byte-compared via post-state. TRACE_NORM
    // collapses the oracle's random applied `trace:` id to the rewrite's empty one.
    Case {
        id: "mutate-move-apply-cascade-zoo",
        argv: &["move", "cycle-b", "notes/moved-b.md", "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // move: the `--dry-run` forecast (records) — the plan preview with the
    // backlink-rewrite count, write-free on both binaries. No trace footer on a
    // forecast, so NO_NORM.
    Case {
        id: "mutate-move-dry-run-forecast-zoo",
        argv: &["move", "cycle-b", "notes/moved-b.md", "--dry-run"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // move: the `--dry-run --format json` forecast — the pretty `ApplyReport`
    // serialization. This case pins the full report SHAPE (operations, cascade
    // counts, outcome, field presence) as the end-user contract; any intended
    // change moves through the divergence ledger. `plan_hash` embeds the absolute
    // vault_root (it is `MigrationPlan::canonical_hash()`), so it is genuinely
    // per-side-root-dependent and normalized here (PLAN_HASH_NORM) like the
    // sibling vault_root field — its EQUALITY given a shared root is proven
    // out-of-band + unit-pinned by `mutate::{move_doc::single_move_fields,
    // delete::delete_fields}` (the op FIELD SET that feeds the hash). A forecast
    // carries no trace id.
    Case {
        id: "mutate-move-dry-run-json-zoo",
        argv: &[
            "move",
            "cycle-b",
            "notes/moved-b.md",
            "--dry-run",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: PLAN_HASH_NORM,
        plan: None,
    },
    // delete: a confirmed apply with `--rewrite-to`, redirecting the one incoming
    // backlink to `cycle-c` before deleting (records). Post-state proves the
    // redirect wrote identically and the file is gone on both sides. TRACE_NORM.
    Case {
        id: "mutate-delete-rewrite-to-apply-zoo",
        argv: &["delete", "cycle-b", "--rewrite-to", "cycle-c", "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // delete: the backlink-policy refusal — `cycle-b` has an incoming link and
    // neither `--allow-broken-links` nor `--rewrite-to` is given, so both binaries
    // refuse `error: document has 1 incoming link(s) …` (exit 2), write-free.
    Case {
        id: "mutate-delete-backlinks-refusal-zoo",
        argv: &["delete", "cycle-b"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // rewrite-wikilink: a confirmed vault-wide `[[cycle-b]]` → `[[cycle-c]]`
    // rewrite (records). The `rewrote [[…]] → [[…]] in N ops` breakdown is
    // compared and the rewritten backlink is byte-compared via post-state.
    // TRACE_NORM.
    Case {
        id: "mutate-rewrite-wikilink-apply-zoo",
        argv: &["rewrite-wikilink", "cycle-b", "cycle-c", "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-b.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // ── NRN-424: the wikilink-rewriter unification divergences ────────────────
    // Confirmed applies against the wikilink-edge fixture. The oracle shares all
    // three rewriter bugs, so on these backlink shapes it writes DIFFERENT bytes
    // than the fix — the post-state tree diverges (stdout matches: the plan/report
    // is computed the same on both sides). Each exits 0, so TRACE_NORM collapses
    // the oracle's random applied `trace:` id to the rewrite's empty one, isolating
    // the divergence to the corrected content. Grouped by mechanism: PD-116
    // (embed marker), PD-117 (code opacity, both engines), PD-118 (caret target).
    //
    // NRN-431 — move cascade drops an embed's `!` and `|alias`. The oracle rewrites
    // `![[embed-target|Display]]` → `[[embed-moved]]`; the rewrite keeps
    // `![[embed-moved|Display]]`.
    Case {
        id: "wl-move-embed-backlink-diverge",
        argv: &["move", "embed-target", "wl/embed-moved.md", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/embed-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // NRN-432 (cascade) — the move cascade's first-occurrence `replacen` rewrites
    // the code-fenced `[[fence-target]]` sample and leaves the real prose backlink
    // dangling; the span-based rewrite skips the fence and rewrites the prose link.
    Case {
        id: "wl-move-code-fence-shadow-diverge",
        argv: &["move", "fence-target", "wl/fence-moved.md", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/fence-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // NRN-432 (verb) — the rewrite-wikilink whole-file scan rewrites BOTH the
    // fenced sample and the prose link; the code-aware rewrite touches only prose.
    Case {
        id: "wl-rewrite-wikilink-code-fence-shadow-diverge",
        argv: &["rewrite-wikilink", "fence-target", "fence-renamed", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/fence-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // NRN-433 — a bare `^` is an ordinary target char. The oracle splits `[[a^b]]`
    // on `^`, so the rewrite never matches and the file is left untouched (while
    // success is reported); the rewrite splits on `#` only and rewrites `[[a^b]]`.
    Case {
        id: "wl-rewrite-wikilink-caret-stem-diverge",
        argv: &["rewrite-wikilink", "a^b", "caret-renamed", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/caret-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // NRN-431 (delete variant, PD-116) — `delete --rewrite-to` redirects the embed
    // backlink through the SAME cascade helpers as the move case. The oracle
    // collapses `![[embed-target|Display]]` → `[[redirect-target]]`; the rewrite
    // preserves `![[redirect-target|Display]]`. Pins the delete engine of PD-116's
    // mechanism (mirroring how PD-117 pins both the cascade and the verb).
    Case {
        id: "wl-delete-embed-backlink-diverge",
        argv: &[
            "delete",
            "embed-target",
            "--rewrite-to",
            "redirect-target",
            "--yes",
        ],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/embed-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // PD-119 (decided-better) — interior-whitespace canonicalization on rewrite.
    // The reconstructed link emits the parser-trimmed target/alias, so a backlink
    // with whitespace around the pipe or padding inside the brackets is rewritten
    // to its canonical (tight) form. Byte-parity would require replicating the
    // oracle's own inconsistency (it drops the target-side space but keeps the
    // alias-side space); the padded-target match additionally fixes the oracle's
    // phantom no-op (its untrimmed bare_target defeats its own match).
    //
    // Cascade move over a spaced-pipe aliased backlink: the oracle keeps the
    // alias-side space (`[[spaced-moved| Display Name]]`), the rewrite trims it
    // (`[[spaced-moved|Display Name]]`).
    Case {
        id: "wl-move-spaced-alias-diverge",
        argv: &["move", "spaced-target", "wl/spaced-moved.md", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/spaced-alias-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // rewrite-wikilink over a padded (leading/trailing-whitespace) target link:
    // the oracle phantom-no-ops (`[[ padded-target ]]` untouched), the rewrite
    // matches on the trimmed target and rewrites it (`[[padded-renamed]]`).
    Case {
        id: "wl-rewrite-wikilink-padded-target-diverge",
        argv: &[
            "rewrite-wikilink",
            "padded-target",
            "padded-renamed",
            "--yes",
        ],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/padded-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // PD-120 (decided-better) — a rename to an UNREPRESENTABLE wikilink target (a
    // name carrying `|`/`#`/`[`/`]`) would re-parse as a different link shape and
    // corrupt the backlink. The oracle emits `[[a|b]]` (target `a`, alias `b`);
    // the rewrite refuses/skips, leaving the link intact. Both surfaces are
    // reachable (no upstream filename-portability refusal on the destination).
    //
    // rewrite-wikilink verb: the oracle applies (exit 0) and corrupts; the rewrite
    // refuses the whole op (exit 2, `unrepresentable-rewrite-target`), write-free.
    Case {
        id: "wl-rewrite-wikilink-unrepresentable-refusal",
        argv: &["rewrite-wikilink", "unrepr-target", "a|b", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/unrepr-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // move cascade: both exit 0 (the file moves to `wl/a|b.md`), but the oracle
    // corrupts the backlink to `[[a|b]]` while the rewrite skips it
    // (`would-corrupt-wikilink`), leaving `[[unrepr-target]]` stale-but-intact.
    Case {
        id: "wl-move-unrepresentable-skip-diverge",
        argv: &["move", "unrepr-target", "wl/a|b.md", "--yes"],
        fixture: WIKILINK_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("wl/unrepr-src.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
];

/// Mutation-verb parity for `edit` (NRN-379): the atomic content-anchored
/// body-edit verb. Each case is `mutating: true` — a forecast/refusal is proven
/// write-free on BOTH binaries, and the confirmed apply is proven to write a
/// byte-identical tree on both (post-state compared). The NRN-379 cases are all
/// `ported: true` and MATCH the oracle (pure byte-parity, no ledger entry); the
/// five NRN-437 section-edge cases at the end DIVERGE with a ledger entry
/// (PD-115) — the oracle corrupts a SETEXT heading / a heading at EOF, the
/// rewrite does not. The report shape,
/// records/json rendering, the format-independent refusal surface, and the ops
/// grammar (sugar desugar + `--edits-json`) were confirmed byte-identical to the
/// pinned oracle (v0.48.1). The confirmed-apply case appends [`TRACE_NORM`] (the
/// oracle's random applied `trace:` id → the rewrite's empty id); everything else
/// is deterministic and byte-compared as-is.
///
/// The anchors below reference `notes/alpha.md`'s zoo body: the intro paragraph
/// (`An introductory paragraph …`), `## Section One/Two/Three`, and their
/// `Content belonging to section …` lines.
const EDIT_CASES: &[Case] = &[
    // A confirmed apply combining a str_replace and a section op (records). The
    // change lines + `body: X → Y bytes` are compared AND the mutated file is
    // byte-compared via post-state — both sides wrote the same bytes.
    Case {
        id: "edit-apply-str-replace-section-zoo",
        argv: &[
            "edit",
            "alpha",
            "--edits-json",
            "[{\"op\":\"str_replace\",\"old\":\"An introductory paragraph for the alpha fixture document.\",\"new\":\"Rewritten intro.\"},{\"op\":\"append_to_section\",\"heading\":\"Section One\",\"content\":\"- appended line\"}]",
            "--yes",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // A dry-run forecast (records): the plan preview (`dry-run: edit …` + the
    // str_replace change line with its `1×` occurrence count + the `body: X → Y
    // bytes` line + `Apply with --yes`), proven write-free on both sides. Uses
    // `--edits-json` rather than single-op sugar deliberately: the harness runs
    // CLI cases with a closed stdin PIPE, and the oracle's sugar path refuses a
    // sugar op alongside a redirected-stdin fifo (its `stdin_carries_redirected_
    // payload` F1 guard). The `--edits-json` source is stdin-agnostic, so both
    // binaries behave identically under the harness; the sugar desugar itself is
    // covered by the CLI command unit tests.
    Case {
        id: "edit-dry-run-forecast-zoo",
        argv: &[
            "edit",
            "alpha",
            "--edits-json",
            "[{\"op\":\"str_replace\",\"old\":\"Content belonging to section two.\",\"new\":\"Replaced section two.\"}]",
            "--dry-run",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // A bad target is a clean pre-write refusal: `error: doc not found: <t>` on
    // stderr, exit 2, write-free (the format-independent edit refusal surface).
    // `--edits-json` for the same harness-stdin reason as the forecast case.
    Case {
        id: "edit-missing-target-zoo",
        argv: &[
            "edit",
            "no-such-doc-xyzzy",
            "--edits-json",
            "[{\"op\":\"str_replace\",\"old\":\"a\",\"new\":\"b\"}]",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // A malformed op (an unknown op discriminant in the JSON array): the CLI-side
    // parse refuses with `error: invalid edits JSON: <serde detail>`, exit 2,
    // write-free. The serde `unknown variant` detail (incl. the expected-variant
    // list) is byte-identical across the two binaries.
    Case {
        id: "edit-malformed-op-zoo",
        argv: &["edit", "alpha", "--edits-json", "[{\"op\":\"nope\"}]"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // The canonical JSON-ops-array source (`--edits-json`) rendered as a `--format
    // json` forecast: pins the whole EditReport JSON shape (struct field order,
    // the `edits` array with an omitted `occurrences` for a structural op, the
    // empty `trace_id` on a forecast), write-free.
    Case {
        id: "edit-json-ops-forecast-zoo",
        argv: &[
            "edit",
            "alpha",
            "--edits-json",
            "[{\"op\":\"delete_section\",\"heading\":\"Section Two\"}]",
            "--dry-run",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    },
    // ── NRN-437: SETEXT / heading-at-EOF section corruption (PD-115) ──────────
    // Confirmed applies against the section-edge fixture. The oracle shares the
    // body_start bug, so on these two body shapes it writes DIFFERENT bytes than
    // the fix: stdout (the `body: X → Y bytes` line) AND the post-state tree
    // diverge. Each case exits 0 on both sides, so TRACE_NORM collapses the
    // oracle's random applied `trace:` id to the rewrite's empty one, isolating
    // the divergence to the corrected content. Pinned by PD-115.
    //
    // SETEXT replace_section: the oracle consumes the `-----` underline (demoting
    // the heading to a paragraph); the rewrite keeps the underline.
    Case {
        id: "edit-setext-replace-section-diverge",
        argv: &[
            "edit",
            "setext",
            "--edits-json",
            "[{\"op\":\"replace_section\",\"heading\":\"Alpha\",\"content\":\"REPLACED.\"}]",
            "--yes",
        ],
        fixture: SECTION_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("shapes/setext.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // SETEXT insert_after_heading: the oracle inserts the line BEFORE the
    // underline (the underline then applies to the inserted text); the rewrite
    // inserts it after the underline, below the heading.
    Case {
        id: "edit-setext-insert-after-heading-diverge",
        argv: &[
            "edit",
            "setext",
            "--edits-json",
            "[{\"op\":\"insert_after_heading\",\"heading\":\"Alpha\",\"content\":\"LEAD.\"}]",
            "--yes",
        ],
        fixture: SECTION_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("shapes/setext.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // EOF-no-newline replace_section: the section is empty and runs to EOF
    // (body_start == end == len), so the replacement welds onto the marker on the
    // oracle (`## TailNEW.`); the rewrite supplies the missing line terminator
    // first (`## Tail\nNEW.`).
    Case {
        id: "edit-eof-heading-replace-section-diverge",
        argv: &[
            "edit",
            "eof-heading",
            "--edits-json",
            "[{\"op\":\"replace_section\",\"heading\":\"Tail\",\"content\":\"NEW.\"}]",
            "--yes",
        ],
        fixture: SECTION_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("shapes/eof-heading.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // EOF-no-newline append_to_section: the oracle welds the appended line onto
    // the heading marker (`## Tail- item`); the rewrite supplies the missing line
    // terminator first (`## Tail\n- item`).
    Case {
        id: "edit-eof-heading-append-to-section-diverge",
        argv: &[
            "edit",
            "eof-heading",
            "--edits-json",
            "[{\"op\":\"append_to_section\",\"heading\":\"Tail\",\"content\":\"- item\"}]",
            "--yes",
        ],
        fixture: SECTION_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("shapes/eof-heading.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
    // EOF-no-newline insert_after_heading: same welding bug as append; the oracle
    // produces `## TailLEAD.`, the rewrite `## Tail\nLEAD.`.
    Case {
        id: "edit-eof-heading-insert-after-heading-diverge",
        argv: &[
            "edit",
            "eof-heading",
            "--edits-json",
            "[{\"op\":\"insert_after_heading\",\"heading\":\"Tail\",\"content\":\"LEAD.\"}]",
            "--yes",
        ],
        fixture: SECTION_EDGE_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("shapes/eof-heading.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: None,
    },
];

/// Authored-plan parity for `apply` (NRN-394): the gap the verb's own port
/// (#206, NRN-393) left untested — every prior `apply`-exercising case ran a
/// plan a MUTATION VERB (`set`/`new`/`move`/…) generated at runtime, never one
/// an operator or automation client hand-authors and feeds `apply` directly.
/// Every case here supplies its own `MigrationPlan` JSON via `Case::plan` (a
/// [`PLAN_VAULT_ROOT_TOKEN`] template the runner materializes per side — see
/// that field's doc) and drives `apply` with the materialized file path
/// substituted for the [`PLAN_ARGV_PLACEHOLDER`] token in `argv`. All
/// `ported: true`; every case here MATCHES the oracle (pure byte-parity — no
/// ledger entry): the plan model (schema v2 + ADR 0015 owner-set
/// preconditions) and the `apply` verb's execution semantics are ported
/// donor-faithfully, so a hand-authored plan exercises no different a code
/// path than one a mutation verb would have generated.
///
/// `strip_bom` (a plan touching the BOM-recognition divergence, PD-104) is a
/// deliberate follow-on, not a gap here: it needs PR #207's BOM diagnostics
/// surface, which had not landed on this branch's base at authoring time (see
/// the git history) — adding it now would either fabricate an unmerged
/// dependency or misdescribe an untested surface as covered.
const APPLY_CASES: &[Case] = &[
    // A confirmed apply of a hand-authored single-op move plan (records). Same
    // shape `move_doc.rs` itself would generate — the zoo's `cycle-a/b/c` 3-cycle
    // (see the `mutate` suite's cascade cases) gives `cycle-c` exactly one
    // incoming link (from `cycle-b`, `Points to [[cycle-c]]`), so the low-level
    // `move_document` op's automatic backlink cascade fires even with no
    // separate `rewrite_link` op authored. Post-state byte-compares the moved
    // file AND the rewritten backlink, proving the raw plan bytes — not just a
    // CLI-synthesized one — apply identically on both binaries. TRACE_NORM
    // collapses the oracle's random applied `trace:` id to the rewrite's empty
    // one.
    Case {
        id: "apply-authored-move-plan-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-c.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    {
      "kind": "move_document",
      "fields": { "src": "notes/cycle-c.md", "dst": "notes/moved-c.md", "parents": false }
    }
  ]
}
"##,
        ),
    },
    // A hand-authored single-op `add_frontmatter` plan, forecast (`--dry-run
    // --format json`): pins the full `--format json` `ApplyReport` SHAPE for an
    // authored (not verb-synthesized) plan. `add_frontmatter` (not
    // `set_frontmatter`) targeting `priority` — a field genuinely absent from
    // `notes/alpha.md`'s frontmatter — deliberately: a low-level
    // `set_frontmatter`/`add_frontmatter` op with no `expected_old_value` CAS
    // check defaults to asserting the field is currently ABSENT (empirically
    // confirmed against the pinned oracle: the same op naming alpha's existing
    // `summary` field refuses `expected-old-value-mismatch`, "expected missing,
    // found …" — a real CAS barrier, not a fixture bug), so an authored plan
    // omitting the check must target a field the doc does not already carry.
    // `plan_hash` is `MigrationPlan::canonical_hash()` — genuinely
    // per-side-root-dependent (the plan embeds the absolute `vault_root`), same
    // as the cascade-verb `--dry-run --format json` cases, so `PLAN_HASH_NORM`
    // is needed alongside the universal `VaultRoot` default. Write-free on both
    // binaries.
    Case {
        id: "apply-authored-add-frontmatter-plan-dry-run-json-zoo",
        argv: &[
            "apply",
            PLAN_ARGV_PLACEHOLDER,
            "--dry-run",
            "--format",
            "json",
        ],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: PLAN_HASH_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    {
      "kind": "add_frontmatter",
      "fields": { "path": "notes/alpha.md", "field": "priority", "new_value": "high" }
    }
  ]
}
"##,
        ),
    },
    // ADR 0015: an owner-set precondition whose `expected_paths` names the WRONG
    // path for the real owner of stem `alpha` (`notes/alpha.md`) — a plan
    // authored against a stale planning-time snapshot. The barrier evaluates
    // before any operation writes (even under `--yes`), so the whole plan
    // refuses (`owner-set-mismatch`, exit 2) with every operation `not_run`,
    // write-free on both binaries — proving the precondition barrier itself,
    // not just operation execution, is donor-faithful for an authored plan.
    Case {
        id: "apply-authored-precondition-mismatch-refusal-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "preconditions": [
    {
      "id": "alpha-owner",
      "kind": "owner_set",
      "selector": { "stem": "alpha" },
      "expected_paths": ["notes/not-actually-alpha.md"]
    }
  ],
  "operations": [
    {
      "kind": "set_frontmatter",
      "fields": { "path": "notes/alpha.md", "field": "summary", "new_value": "should-not-apply" }
    }
  ]
}
"##,
        ),
    },
    // A plan whose `schema_version` (99) does not match
    // `MIGRATION_PLAN_SCHEMA_VERSION` (2) — the CLIENT-SIDE preamble refuses
    // BEFORE any wire activity (`unsupported-schema-version`, exit 2), so this
    // never even reaches a session; write-free on both binaries by construction.
    // `generated_at` OMITTED (the field is `Option`, `skip_serializing_if`/
    // `default` on both sides — confirmed accepted with no field at all, not
    // just a null).
    Case {
        id: "apply-authored-schema-version-refusal",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 99,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": []
}
"##,
        ),
    },
    // Two `create_document` ops sharing the SAME `{{seq}}` template
    // (`seq/task-{{seq}}.md`) in one plan, confirmed apply (records). `seq/` does
    // not exist in the zoo fixture, so `--parents` (`-p`) is required (a bare
    // create into a missing parent directory refuses, exit 2 — a `{{seq}}`
    // create is no exception). The apply-time resolver
    // (`seq_alloc::resolve_seq_create`) starts the counter at 1 for the first op
    // and — folding in that not-yet-on-disk allocation (NRN-101) — advances to 2
    // for the second, entirely within the single mutation-lock critical section
    // one plan apply runs under: post-state PROVES both `seq/task-1.md` (`Task
    // One`) and `seq/task-2.md` (`Task Two`) actually land on disk with the
    // right content, byte-identically on both binaries. Empirically confirmed
    // against the pinned oracle: the REPORT's own counters/summary lines are
    // visually confusing here — `applied: 1  skipped: 1` and BOTH ops' one-line
    // summaries read `create seq/task-1.md` (the second op's display line never
    // picks up its own apply-time-resolved id) — yet the actual vault tree
    // carries both files with the correct distinct frontmatter/body, and the
    // rewrite reproduces the identical counters and mislabeled summary text.
    // This is a REAL report-rendering defect (both create summaries print the
    // first op's resolved path), tracked as NRN-425; the case pins current
    // behavior until that fix lands with its ledger entry. TRACE_NORM for the
    // confirmed-apply trace id.
    Case {
        id: "apply-authored-sequenced-seq-creates-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes", "--parents"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: TRACE_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    {
      "kind": "create_document",
      "fields": {
        "path": "seq/task-{{seq}}.md",
        "new_value": {
          "frontmatter": { "title": "Task One", "type": "task" },
          "body": "# Task One\n"
        },
        "force": false
      }
    },
    {
      "kind": "create_document",
      "fields": {
        "path": "seq/task-{{seq}}.md",
        "new_value": {
          "frontmatter": { "title": "Task Two", "type": "task" },
          "body": "# Task Two\n"
        },
        "force": false
      }
    }
  ]
}
"##,
        ),
    },
    // ── NRN-405: authored-plan safety + diagnostic divergences ────────────────
    // A change op whose `fields.operation` DISAGREES with its `kind` (NRN-405).
    // `fields.operation` is the value that drives the executor's write dispatch,
    // so on the oracle a reviewed `kind: set_frontmatter` op carrying
    // `operation: remove_frontmatter` silently dispatches as a REMOVE — executing
    // a different operation than the reviewed plan declares. `priority` is absent
    // from `notes/alpha.md`, so the oracle's silent remove trips
    // `cannot-minimal-edit` ("field priority not present"), exit 2. The rewrite
    // refuses the mismatch outright — `error: op.fields.operation
    // 'remove_frontmatter' conflicts with op.kind 'set_frontmatter'`, exit 2 —
    // before any dispatch: a reviewed plan must execute as its `kind` declares.
    // stderr diverges; both write nothing. Pinned by PD-113.
    Case {
        id: "apply-authored-kind-operation-mismatch-refusal-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    {
      "kind": "set_frontmatter",
      "fields": { "path": "notes/alpha.md", "field": "priority", "new_value": "high", "operation": "remove_frontmatter" }
    }
  ]
}
"##,
        ),
    },
    // A malformed authored plan with an UNKNOWN op kind, `--format json` (NRN-405).
    // Both binaries refuse at plan expansion, exit 2, write-free, with the SAME
    // message (`unknown operation kind: no_such_kind`) — only the machine-branchable
    // `code` diverges: the oracle flattens the typed error to `internal-error`
    // ("norn has a bug"), the rewrite carries the diagnostic `unknown-operation-kind`
    // ("your plan names a kind norn doesn't know"). `--format json` is required to
    // surface the `code` — the records refusal prints only the (identical) message.
    // Pinned by PD-114.
    Case {
        id: "apply-authored-unknown-kind-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "no_such_kind", "fields": { "path": "notes/alpha.md" } }
  ]
}
"##,
        ),
    },
    // A malformed authored plan with a structural op MISSING a required field
    // (`move_document` with no `dst`), `--format json` (NRN-405). Same shape as the
    // unknown-kind case: both refuse at expansion, exit 2, write-free, identical
    // message (`move_document missing dst`); the oracle codes it `internal-error`,
    // the rewrite codes it `malformed-plan`. Pinned by PD-114.
    Case {
        id: "apply-authored-missing-field-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "move_document", "fields": { "src": "notes/alpha.md" } }
  ]
}
"##,
        ),
    },
    // A malformed authored plan whose `fields` are the right SHAPE (an object) but
    // carry a wrong-TYPED member — `"operation": 5` (NRN-405). It slips both the
    // object-shape and the kind/operation-mismatch guards (a non-string
    // `operation` is neither), then fails the decode into the change model. The
    // oracle flattens the bare serde error to `internal-error`; the rewrite carries
    // a typed `malformed-plan` and additionally names the kind
    // (`op.fields for set_frontmatter could not be decoded: <serde text>`), so both
    // the `code` and the message diverge. exit 2, write-free on both. The
    // class-completion of PD-114 (a wrong-typed member, not just an unknown kind or
    // missing field), pinned by the same entry.
    Case {
        id: "apply-authored-wrong-typed-field-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "set_frontmatter", "fields": { "path": "notes/alpha.md", "field": "priority", "new_value": "high", "operation": 5 } }
  ]
}
"##,
        ),
    },
    // ── ADR 0022: strict op-payload decode (F5 — refuse coercion) ──────────────
    // A structural op with a WRONG-TYPED boolean: `move_document` carrying
    // `"force": "true"` (a string, not a bool). The oracle's `.and_then(as_bool)
    // .unwrap_or(false)` silently coerces it to `false`, then proceeds to the
    // apply-time destination check — `notes/beta.md` already exists and `force` is
    // (silently) off, so it refuses `move destination already exists` (exit 2).
    // The rewrite refuses the malformed field itself, at plan decode, before any
    // dispatch: `op.fields for move_document could not be decoded: field `force`
    // must be a boolean` (`malformed-plan`, exit 2). Both write nothing; the
    // refusal reason (and message) diverges. Pinned by PD-121.
    Case {
        id: "apply-authored-wrong-typed-bool-refusal-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/beta.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "move_document", "fields": { "src": "notes/alpha.md", "dst": "notes/beta.md", "force": "true" } }
  ]
}
"##,
        ),
    },
    // A `delete_document` with a wrong-typed `rewrite_to` (`5`, a number). The
    // oracle's `.and_then(as_str)` drops the malformed value indistinguishably from
    // absent, so it deletes `notes/cycle-c.md` with NO redirect — silently applying
    // a destructive op the author fat-fingered (exit 0, the file is gone, leaving
    // `cycle-b`'s `[[cycle-c]]` backlink broken). The rewrite refuses the malformed
    // field at plan decode — `op.fields for delete_document could not be decoded:
    // field `rewrite_to` must be a string` (`malformed-plan`, exit 2), write-free.
    // The starkest F5 case: silent coercion turns a typo into data loss on the
    // oracle; the rewrite refuses. Exit codes AND the post-state tree diverge.
    // Pinned by PD-121. TRACE_NORM for the oracle's confirmed-apply trace id.
    Case {
        id: "apply-authored-wrong-typed-rewrite-to-refusal-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/cycle-c.md"),
        requires_code: None,
        normalize: TRACE_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "delete_document", "fields": { "path": "notes/cycle-c.md", "rewrite_to": 5 } }
  ]
}
"##,
        ),
    },
    // A section-edit op (`str_replace`) with a wrong-typed `document_hash` (`5`, a
    // number). The oracle's `.and_then(as_str).unwrap_or("")` coerces it to the
    // empty string (no compare-and-swap), then runs the replace — `old` names text
    // absent from `notes/alpha.md`, so it refuses `string not found` (exit 2). The
    // rewrite refuses the malformed field at plan decode — `op.fields for
    // str_replace could not be decoded: invalid type: integer `5`, expected a
    // string` (`malformed-plan`, exit 2), before the body is ever touched. Both
    // write nothing; the refusal reason diverges. Pinned by PD-121.
    Case {
        id: "apply-authored-wrong-typed-document-hash-refusal-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "str_replace", "fields": { "path": "notes/alpha.md", "old": "NONEXISTENT_ZZZ", "new": "x", "document_hash": 5 } }
  ]
}
"##,
        ),
    },
    // ── NRN-436: coded refusals for the bare-anyhow user-fault families ─────────
    // Each of these authored-plan refusals was a bare `anyhow::bail!` invisible to
    // the apply envelope's downcast ladder, so the oracle flattens every one to
    // `code: internal-error` ("norn has a bug") — a misleading machine-branchable
    // code for what is a malformed AUTHORED plan (a user fault). The rewrite gives
    // each family a typed error carrying a precise code. Exit 2 and the message
    // text are UNCHANGED on both sides; only the `code` (and, for the create-guard
    // family, an added `path`) diverges. `--format json` surfaces the envelope's
    // `code`; the records refusal prints only the (identical) message. All are
    // forecasts (`--format json`, no `--yes`) — the refusals are all pre-write, so
    // a forecast refuses exactly where a confirmed apply would, write-free on both.
    //
    // F3 create-guard — a `create_document` whose destination already exists and
    // no `force`. `notes/alpha.md` is a real zoo doc. Oracle: `internal-error`;
    // rewrite: `create-destination-exists` AND an added `path` echo. Pinned by
    // PD-125.
    Case {
        id: "apply-authored-create-destination-exists-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "create_document", "fields": { "path": "notes/alpha.md", "new_value": { "frontmatter": { "type": "note" }, "body": "x" }, "force": false } }
  ]
}
"##,
        ),
    },
    // F3 create-guard — a `create_document` into a parent directory that does not
    // exist, without `--parents`. `nope/` is absent from the zoo fixture. Oracle:
    // `internal-error`; rewrite: `create-parent-missing` AND an added `path`.
    // Pinned by PD-125.
    Case {
        id: "apply-authored-create-parent-missing-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "create_document", "fields": { "path": "nope/new.md", "new_value": { "frontmatter": { "type": "note" }, "body": "x" }, "force": false } }
  ]
}
"##,
        ),
    },
    // F3 create-guard — a `create_document` whose `new_value.frontmatter` is not a
    // JSON object (a string). A fault in the AUTHORED plan CONTENT, coded
    // `malformed-plan` (no path), not a create-family code. Oracle: `internal-error`;
    // rewrite: `malformed-plan`. Pinned by PD-125.
    Case {
        id: "apply-authored-create-nonobject-frontmatter-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "create_document", "fields": { "path": "newdoc.md", "new_value": { "frontmatter": "notanobject", "body": "x" } } }
  ]
}
"##,
        ),
    },
    // F1 precondition-validation — an owner-set precondition with an EMPTY stem
    // selector. The owner-set barrier evaluates before any write. Oracle:
    // `internal-error`; rewrite: `invalid-precondition`. Pinned by PD-125.
    Case {
        id: "apply-authored-empty-stem-precondition-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "preconditions": [
    { "id": "p", "kind": "owner_set", "selector": { "stem": "" }, "expected_paths": [] }
  ],
  "operations": [
    { "kind": "set_frontmatter", "fields": { "path": "notes/alpha.md", "field": "summary", "new_value": "x" } }
  ]
}
"##,
        ),
    },
    // F4 plan-structure — two operations sharing the same `id` (`dup`). The plan's
    // structure is malformed; refused at create-path resolution before any write.
    // Oracle: `internal-error`; rewrite: `malformed-plan`. Pinned by PD-125.
    Case {
        id: "apply-authored-duplicate-op-id-refusal-json-zoo",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        mutating: true,
        ported: true,
        expect_oracle_exit: 2,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some(
            r##"{
  "schema_version": 2,
  "vault_root": "{{VAULT_ROOT}}",
  "operations": [
    { "kind": "create_document", "id": "dup", "fields": { "path": "x.md", "new_value": { "frontmatter": {}, "body": "x" } } },
    { "kind": "create_document", "id": "dup", "fields": { "path": "y.md", "new_value": { "frontmatter": {}, "body": "y" } } }
  ]
}
"##,
        ),
    },
];

const SUITES: &[Suite] = &[
    Suite {
        name: "help",
        cases: HELP_CASES,
    },
    Suite {
        name: "errors",
        cases: ERROR_CASES,
    },
    Suite {
        name: "text-edge",
        cases: TEXT_EDGE_CASES,
    },
    Suite {
        name: "validate",
        cases: VALIDATE_CASES,
    },
    Suite {
        name: "repair",
        cases: REPAIR_CASES,
    },
    Suite {
        name: "read",
        cases: READ_CASES,
    },
    Suite {
        name: "describe",
        cases: DESCRIBE_CASES,
    },
    Suite {
        name: "mcp",
        cases: MCP_CASES,
    },
    Suite {
        name: "mutate",
        cases: MUTATE_CASES,
    },
    Suite {
        name: "edit",
        cases: EDIT_CASES,
    },
    Suite {
        name: "apply",
        cases: APPLY_CASES,
    },
];

/// All suites, in declaration order — that order is the run order and the
/// report order (determinism constraint, ADR 0018). Case-id uniqueness is
/// enforced at runtime by the runner via [`duplicate_case_id`], not just a
/// debug assertion.
pub fn suites() -> &'static [Suite] {
    SUITES
}

/// Every case id across every suite, in declaration order.
pub fn all_case_ids() -> Vec<&'static str> {
    suites()
        .iter()
        .flat_map(|s| s.cases.iter().map(|c| c.id))
        .collect()
}

/// Every `ported == true` case id, in declaration order. A ledger entry may
/// only cite these (see `crate::ledger`); phase 0: empty.
pub fn ported_case_ids() -> Vec<&'static str> {
    suites()
        .iter()
        .flat_map(|s| s.cases.iter().filter(|c| c.ported).map(|c| c.id))
        .collect()
}

/// The first case id that appears in more than one case across `suites`, or
/// `None` if every id is unique. The runner treats a duplicate as an error
/// (exit 2): a duplicate id would bind two cases to one ledger entry
/// silently. Runtime-enforced (not `debug_assert`-only) so release builds
/// catch it too.
pub fn duplicate_case_id(suites: &[Suite]) -> Option<&'static str> {
    let mut seen: Vec<&'static str> = Vec::new();
    for suite in suites {
        for case in suite.cases {
            if seen.contains(&case.id) {
                return Some(case.id);
            }
            seen.push(case.id);
        }
    }
    None
}

/// The first case id whose [`Case::plan`] and `argv` disagree about carrying
/// the authored-plan capability — either an authored plan (`plan.is_some()`)
/// whose argv names no [`PLAN_ARGV_PLACEHOLDER`] (the materialized file could
/// never reach the process), or an argv naming the placeholder with no plan
/// template to materialize it from (a dangling token the runner cannot fill in).
/// `None` when every case pairs the two consistently. Runtime-enforced (not
/// `debug_assert`-only), mirroring [`duplicate_case_id`]'s discipline.
pub fn plan_argv_mismatch(suites: &[Suite]) -> Option<&'static str> {
    for suite in suites {
        for case in suite.cases {
            // Exactly-one, per the `Case::plan` contract: zero tokens with a
            // plan is a dangling template; two or more would silently receive
            // the same path (and a literal "{PLAN}" argv value is unsupported
            // by design — the token is reserved).
            let placeholders = case
                .argv
                .iter()
                .filter(|a| **a == PLAN_ARGV_PLACEHOLDER)
                .count();
            if (case.plan.is_some() && placeholders != 1)
                || (case.plan.is_none() && placeholders != 0)
            {
                return Some(case.id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    static A: Case = Case {
        id: "dup",
        argv: &["count"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: false,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    };
    static B: Case = Case {
        id: "dup",
        argv: &["describe"],
        fixture: ZOO_1,
        stdin: None,
        mutating: false,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    };

    #[test]
    fn duplicate_case_id_flags_a_repeated_id_across_suites() {
        let suites = &[
            Suite {
                name: "one",
                cases: std::slice::from_ref(&A),
            },
            Suite {
                name: "two",
                cases: std::slice::from_ref(&B),
            },
        ];
        assert_eq!(duplicate_case_id(suites), Some("dup"));
    }

    #[test]
    fn duplicate_case_id_none_when_all_unique() {
        assert!(
            duplicate_case_id(suites()).is_none(),
            "the real catalog must have unique case ids"
        );
    }

    static PLAN_NO_PLACEHOLDER: Case = Case {
        id: "plan-no-placeholder",
        argv: &["apply", "plan.json", "--yes"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: true,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some("{}"),
    };
    static PLACEHOLDER_NO_PLAN: Case = Case {
        id: "placeholder-no-plan",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: true,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: None,
    };
    static PLAN_PAIRED: Case = Case {
        id: "plan-paired",
        argv: &["apply", PLAN_ARGV_PLACEHOLDER, "--yes"],
        fixture: CLEAN_1,
        stdin: None,
        mutating: true,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
        plan: Some("{}"),
    };

    #[test]
    fn plan_argv_mismatch_flags_a_plan_with_no_placeholder() {
        let suites = &[Suite {
            name: "one",
            cases: std::slice::from_ref(&PLAN_NO_PLACEHOLDER),
        }];
        assert_eq!(plan_argv_mismatch(suites), Some("plan-no-placeholder"));
    }

    #[test]
    fn plan_argv_mismatch_flags_a_placeholder_with_no_plan() {
        let suites = &[Suite {
            name: "one",
            cases: std::slice::from_ref(&PLACEHOLDER_NO_PLAN),
        }];
        assert_eq!(plan_argv_mismatch(suites), Some("placeholder-no-plan"));
    }

    #[test]
    fn plan_argv_mismatch_none_when_paired() {
        let suites = &[Suite {
            name: "one",
            cases: std::slice::from_ref(&PLAN_PAIRED),
        }];
        assert!(plan_argv_mismatch(suites).is_none());
    }

    #[test]
    fn plan_argv_mismatch_none_across_the_real_catalog() {
        assert!(
            plan_argv_mismatch(suites()).is_none(),
            "every real case must pair `plan` and the argv placeholder consistently"
        );
    }
}
