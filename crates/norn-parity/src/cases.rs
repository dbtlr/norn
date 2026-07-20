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
}

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
    },
    // Narrowed from bare `validate --format json` — see module docs: the
    // oracle's raw finding order is not rerun-stable when a document
    // carries more than one finding. `frontmatter-required-field-missing`
    // matches at most once per document in the zoo fixture, so this stays
    // deterministic while still exercising the raw (non-summary) findings
    // shape.
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
    },
    Case {
        // `--format jsonl` over the same single-per-document code: one finding
        // per line, in document (path) order — stable — and the per-line bytes
        // pin the `Finding` struct's own field order (distinct from `--format
        // json`'s alphabetical `json!` order).
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
    },
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
    },
];

/// describe ports for real (NRN-347): the structure view (folders + declared
/// rules + inbox + the full schema under `--format json`) and the contents
/// summary (`--data`/`--stats`/`--by`). All `ported: true`, must Match the
/// oracle. `--format json` pins the schema serialization (the full validate
/// config) byte-for-byte forever.
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
    },
    Case {
        // The structure view in full, incl. the serialized schema (validate
        // config) — pins the schema shape byte-for-byte.
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

/// MCP frame driving (NRN-383, ADR 0018 phase 3): drive `norn mcp`'s stdio
/// JSON-RPC surface with the ordered frames in `stdin` and compare responses
/// frame-by-frame (see `crate::mcp`), instead of the ordinary argv/stdout/
/// stderr comparison every other case uses. `ported: false` on both — the
/// rewrite's `mcp` subcommand is still `not_yet_ported` (prints a diagnostic
/// and exits, no stdio server) — so the gated default run skips these
/// cleanly via the existing `ported` filter; `--self-check` (oracle vs.
/// itself) still runs and must Match both.
const MCP_CASES: &[Case] = &[
    Case {
        // The MCP session lifecycle anchor: `initialize` (id 1) then
        // `tools/list` (id 2), with the standard `notifications/initialized`
        // in between (a notification — no `id`, no response expected, so
        // `crate::mcp::run_case` excludes it from pairing). Verified
        // empirically against the pinned oracle (v0.48.1): newline-delimited
        // JSON-RPC over stdio (one object per line, no LSP-style
        // `Content-Length` framing), and the process exits cleanly (exit 0)
        // once stdin reaches EOF — confirmed by writing exactly these frames,
        // closing stdin, and observing the process exit on its own rather
        // than needing to be killed.
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
    },
    Case {
        // A `tools/call` of a read tool (`vault.get`, the MCP counterpart of
        // `norn get`) against an existing fixture doc — proves the mechanics
        // end to end past the handshake: a real tool invocation, structured
        // JSON in the response's `structuredContent`, compared frame-by-frame
        // like the initialize/tools/list case above.
        id: "mcp-tools-call-get-alpha-zoo",
        argv: &["mcp"],
        fixture: MCP_FIXTURE,
        stdin: Some(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"norn-parity","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vault.get","arguments":{"targets":["alpha"]}}}"#,
        ]),
        mutating: false,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
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
    },
    // move: the `--dry-run --format json` forecast — the pretty `ApplyReport`
    // serialization. Locks the full report SHAPE (operations, cascade counts,
    // outcome, field presence) byte-exactly. `plan_hash` embeds the absolute
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
    },
];

/// Mutation-verb parity for `edit` (NRN-379): the atomic content-anchored
/// body-edit verb. Each case is `mutating: true` — a forecast/refusal is proven
/// write-free on BOTH binaries, and the confirmed apply is proven to write a
/// byte-identical tree on both (post-state compared). All `ported: true` and
/// MATCH the oracle (pure byte-parity, no ledger entry): the report shape,
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
}
