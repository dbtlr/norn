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
    /// Future MCP frame driving (phase 3, stdin-fed JSON-RPC frames). `None`
    /// everywhere today — no case exercises `norn mcp` yet.
    pub stdin: Option<&'static str>,
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
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
    },
];

const VALIDATE_CASES: &[Case] = &[
    Case {
        id: "validate-summary-clean",
        argv: &["validate", "--summary", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
        ported: false,
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
        ported: false,
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
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: None,
        // The `--code` filter is only meaningful if the zoo fixture actually
        // emits this code; tie the argv to the manifest that generates it.
        requires_code: Some("frontmatter-required-field-missing"),
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
        ported: true,
        expect_oracle_exit: 0,
        requires_doc: None,
        requires_code: None,
        normalize: NO_NORM,
    },
    Case {
        id: "read-get-alpha-zoo",
        // Stem form, not `notes/alpha` — see module docs. The resolved target
        // is the vault-relative doc below; tie the argv to the manifest.
        argv: &["get", "alpha", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
        ported: false,
        expect_oracle_exit: 0,
        requires_doc: Some("notes/alpha.md"),
        requires_code: None,
        normalize: NO_NORM,
    },
];

const DESCRIBE_CASES: &[Case] = &[Case {
    id: "describe-zoo",
    argv: &["describe"],
    fixture: ZOO_1,
    stdin: None,
    ported: false,
    expect_oracle_exit: 0,
    requires_doc: None,
    requires_code: None,
    normalize: NO_NORM,
}];

const SUITES: &[Suite] = &[
    Suite {
        name: "help",
        cases: HELP_CASES,
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
