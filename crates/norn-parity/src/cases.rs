//! The parity case/suite catalog (ADR 0018).
//!
//! Every [`Suite`] is `ported: false` today — the rewrite binary is still
//! the phase-0 skeleton (prints a notice, exits 2; see `crates/norn/src/main.rs`).
//! The default gated bin run therefore filters to zero cases and reports
//! "0 suites gated", exit 0. These suites are proven sound today via
//! `--self-check` (oracle vs. itself) and flip to `ported: true` suite by
//! suite as phases 1-3 port their surfaces.
//!
//! Every case here was run against the installed oracle (v0.48.0) and
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
    /// Unique across all suites (enforced by a debug assertion in
    /// [`suites`] and exercised by the ledger's unknown-case-id check).
    pub id: &'static str,
    pub argv: &'static [&'static str],
    pub fixture: Fixture,
    /// Future MCP frame driving (phase 3, stdin-fed JSON-RPC frames). `None`
    /// everywhere today — no case exercises `norn mcp` yet.
    pub stdin: Option<&'static str>,
}

/// A named group of [`Case`]s. `ported` gates whether the default bin run
/// includes this suite (phase 0: always `false`, see module docs).
pub struct Suite {
    pub name: &'static str,
    pub ported: bool,
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

const HELP_CASES: &[Case] = &[
    Case {
        id: "help-bare",
        argv: &["--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
    },
    Case {
        id: "help-validate",
        argv: &["validate", "--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
    },
    Case {
        id: "help-find",
        argv: &["find", "--help"],
        fixture: HELP_FIXTURE,
        stdin: None,
    },
];

const VALIDATE_CASES: &[Case] = &[
    Case {
        id: "validate-summary-clean",
        argv: &["validate", "--summary", "--format", "json"],
        fixture: CLEAN_1,
        stdin: None,
    },
    Case {
        id: "validate-summary-zoo",
        argv: &["validate", "--summary", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
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
    },
];

const READ_CASES: &[Case] = &[
    Case {
        id: "read-count-clean",
        argv: &["count"],
        fixture: CLEAN_1,
        stdin: None,
    },
    Case {
        id: "read-find-json-zoo",
        // `--all`: bare `find` with no predicate prints help and exits 2 by
        // design (see module docs).
        argv: &["find", "--format", "json", "--all"],
        fixture: ZOO_1,
        stdin: None,
    },
    Case {
        id: "read-find-col-title-clean",
        argv: &["find", "--col", "title", "--format", "json", "--all"],
        fixture: CLEAN_1,
        stdin: None,
    },
    Case {
        id: "read-get-alpha-zoo",
        // Stem form, not `notes/alpha` — see module docs.
        argv: &["get", "alpha", "--format", "json"],
        fixture: ZOO_1,
        stdin: None,
    },
];

const DESCRIBE_CASES: &[Case] = &[Case {
    id: "describe-zoo",
    argv: &["describe"],
    fixture: ZOO_1,
    stdin: None,
}];

const SUITES: &[Suite] = &[
    Suite {
        name: "help",
        ported: false,
        cases: HELP_CASES,
    },
    Suite {
        name: "validate",
        ported: false,
        cases: VALIDATE_CASES,
    },
    Suite {
        name: "read",
        ported: false,
        cases: READ_CASES,
    },
    Suite {
        name: "describe",
        ported: false,
        cases: DESCRIBE_CASES,
    },
];

/// All suites, in declaration order — that order is the run order and the
/// report order (determinism constraint, ADR 0018).
pub fn suites() -> &'static [Suite] {
    debug_assert!(
        all_case_ids_unique(SUITES),
        "case ids must be unique across all suites"
    );
    SUITES
}

/// Every case id across every suite, in declaration order.
pub fn all_case_ids() -> Vec<&'static str> {
    suites()
        .iter()
        .flat_map(|s| s.cases.iter().map(|c| c.id))
        .collect()
}

fn all_case_ids_unique(suites: &[Suite]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for suite in suites {
        for case in suite.cases {
            if seen.contains(&case.id) {
                return false;
            }
            seen.push(case.id);
        }
    }
    true
}
