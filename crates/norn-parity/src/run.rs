//! Orchestration: validate the two binaries and the ledger, materialize
//! fixtures, run cases, and assemble a [`RunReport`]. Shared by the bin's
//! default/`--self-check`/`--all` modes and by the integration tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cases::{self, Case, Suite};
use crate::exec::{self, ExecError};
use crate::fixtures::{self, FixtureCache, FixtureError, Side};
use crate::ledger::{Ledger, LedgerError};
use crate::mcp;
use crate::normalize::{self, Normalization};
use crate::poststate::{self, PostStateDiff};
use crate::verdict::{self, Verdict};

/// The ledger/pin policy is a function of mode, stated once here:
///
/// - [`Mode::SelfCheck`] — **blind**: no ledger is loaded and the oracle
///   version pin is not enforced, so a case set can be vetted against a NEW
///   oracle before the ledger is updated. A self-comparison cannot diverge,
///   so the ledger has nothing to say; stale checks are off.
/// - [`Mode::Gated`] / [`Mode::All`] — **pinned + full ledger**: the ledger
///   is loaded, its `meta.oracle_version` is enforced against the oracle
///   binary, entries gate divergence, and stale entries fail the run.
///
/// (`--consistency` — handled in the bin, not here — is **pinned, meta
/// only**: it loads the ledger to enforce the pin but runs no cases.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every case, oracle vs. itself. Ledger not consulted (self-comparison
    /// cannot legitimately diverge); stale checks off; pin not enforced.
    SelfCheck,
    /// `ported == true` cases only, oracle vs. rewrite. The default mode.
    Gated,
    /// Every case regardless of `ported`, oracle vs. rewrite — the
    /// burn-down view; never wired into CI as a gate.
    All,
}

pub struct RunConfig<'a> {
    pub mode: Mode,
    pub oracle: &'a Path,
    pub rewrite: &'a Path,
    pub ledger_path: &'a Path,
    /// Suite names to restrict the run to; empty = no filter.
    pub suite_filter: &'a [String],
}

pub struct CaseOutcome {
    pub case_id: &'static str,
    pub suite_name: &'static str,
    pub verdict: Verdict,
    /// For a mutating case whose two post-mutation vault trees differed, the
    /// tree diff — surfaced in the report so a divergence is legible. `None`
    /// for a read case, or a mutating case whose trees matched. The diff is
    /// reporting detail only: the verdict already folded the tree difference
    /// into the same match/diverged/drift decision as an output difference.
    pub post_state: Option<PostStateDiff>,
    /// For an MCP case (`Case::stdin.is_some()`), every response frame that
    /// differed from the oracle after normalization — empty for a Match, and
    /// always empty for a non-MCP case. Reporting detail only, like
    /// `post_state`: the verdict already folded this into match/diverged/
    /// drift.
    pub mcp_diffs: Vec<mcp::FrameDiff>,
}

pub struct RunReport {
    pub outcomes: Vec<CaseOutcome>,
    pub stale_entries: Vec<String>,
    pub oracle_version: String,
}

impl RunReport {
    /// 0 when every case matched or diverged-with-citation and no ledger
    /// entry went stale; 1 when any case drifted or any entry is stale.
    /// (Runner errors that keep a report from ever being built exit 2 —
    /// see [`RunError`].)
    pub fn exit_code(&self) -> u8 {
        let any_drift = self
            .outcomes
            .iter()
            .any(|o| matches!(o.verdict, Verdict::Drift));
        if any_drift || !self.stale_entries.is_empty() {
            1
        } else {
            0
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    Binary {
        label: &'static str,
        path: String,
        message: String,
    },
    OracleVersionProbeFailed {
        path: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    Ledger(LedgerError),
    Fixture(FixtureError),
    Exec(ExecError),
    /// A binary was killed by a signal rather than exiting — cannot be
    /// compared as a verdict, so the whole run aborts.
    Signaled {
        binary_label: &'static str,
        case_id: &'static str,
    },
    /// A binary emitted invalid UTF-8. Parity surfaces are text protocols;
    /// lossy conversion would let two DIFFERENT invalid byte sequences both
    /// become U+FFFD and falsely compare equal, so this aborts instead.
    NonUtf8Output {
        binary_label: &'static str,
        case_id: &'static str,
        stream: &'static str,
    },
    UnknownSuite(String),
    /// Two cases share an id — a duplicate would bind both to one ledger
    /// entry silently. Enforced at runtime (not `debug_assert`-only).
    DuplicateCaseId(&'static str),
    /// The oracle exited with a code other than the case's
    /// `expect_oracle_exit` — catches silent case rot before it can Match
    /// quietly.
    OracleExitMismatch {
        case_id: &'static str,
        expected: i32,
        actual: i32,
    },
    /// A case declared a `requires_doc` / `requires_code` the generated
    /// fixture does not satisfy — the argv would exercise nothing meaningful.
    UnmetRequirement {
        case_id: &'static str,
        requirement: fixtures::Requirement,
    },
    /// A mutating case's post-mutation vault tree could not be read to build
    /// the post-state snapshot — an environment/IO failure, not a verdict, so
    /// the run aborts (exit 2) rather than guessing at a comparison.
    PostStateSnapshot {
        case_id: &'static str,
        side_label: &'static str,
        message: String,
    },
    /// An MCP case (`crate::mcp`) could not be driven to a comparable
    /// result on one side (a timeout, a premature EOF, a malformed frame) —
    /// a runner error, never a verdict; see that module's doc for why.
    Mcp {
        case_id: &'static str,
        source: mcp::McpError,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Binary {
                label,
                path,
                message,
            } => write!(f, "{label} binary {path} is not runnable: {message}"),
            RunError::OracleVersionProbeFailed {
                path,
                exit_code,
                stderr,
            } => write!(
                f,
                "oracle {path} --version did not succeed (exit {exit_code:?}): {stderr}"
            ),
            RunError::Ledger(e) => write!(f, "{e}"),
            RunError::Fixture(e) => write!(f, "{e}"),
            RunError::Exec(e) => write!(f, "{e}"),
            RunError::Signaled {
                binary_label,
                case_id,
            } => write!(
                f,
                "{binary_label} was terminated by a signal running case `{case_id}` — cannot compare"
            ),
            RunError::NonUtf8Output {
                binary_label,
                case_id,
                stream,
            } => write!(
                f,
                "{binary_label} emitted invalid UTF-8 on {stream} running case `{case_id}` — cannot compare"
            ),
            RunError::UnknownSuite(name) => write!(f, "unknown suite: {name}"),
            RunError::DuplicateCaseId(id) => {
                write!(f, "duplicate case id across suites: `{id}`")
            }
            RunError::OracleExitMismatch {
                case_id,
                expected,
                actual,
            } => write!(
                f,
                "case `{case_id}`: oracle exited {actual}, expected {expected} — likely case rot \
                 (the fixture or oracle surface changed under the argv)"
            ),
            RunError::UnmetRequirement {
                case_id,
                requirement,
            } => write!(f, "case `{case_id}`: {requirement}"),
            RunError::PostStateSnapshot {
                case_id,
                side_label,
                message,
            } => write!(
                f,
                "case `{case_id}`: failed to snapshot the {side_label} post-mutation vault tree: {message}"
            ),
            RunError::Mcp { case_id, source } => write!(f, "case `{case_id}`: {source}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Scan `--version` stdout for the first semver-shaped token — the first
/// whitespace-separated word that, after stripping an optional leading `v`,
/// begins `digits.digits.digits`. Tolerant of a leading program name, build
/// metadata, and a `v` prefix: `norn 0.48.0`, `norn v0.48.0`, and
/// `norn 0.48.0 (abc 2026-01-01)` all yield `0.48.0`.
pub fn parse_version_token(stdout: &str) -> Option<String> {
    for token in stdout.split_whitespace() {
        let candidate = token.strip_prefix('v').unwrap_or(token);
        if is_semver_prefixed(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// `true` if `s` starts with `digits.digits.digits` (a semver core prefix;
/// trailing pre-release/build metadata is allowed to follow).
fn is_semver_prefixed(s: &str) -> bool {
    let mut groups = 0;
    let mut rest = s;
    loop {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return false;
        }
        groups += 1;
        rest = &rest[digits.len()..];
        if groups == 3 {
            return true;
        }
        match rest.strip_prefix('.') {
            Some(r) => rest = r,
            None => return false,
        }
    }
}

/// Spawn `binary --version`, require it to succeed, and return its
/// semver-shaped token (see [`parse_version_token`]). Used for the oracle,
/// whose version must match the ledger's pinned `meta.oracle_version`.
fn require_version(binary: &Path, label: &'static str) -> Result<String, RunError> {
    let raw = exec::probe_version(binary).map_err(|e| RunError::Binary {
        label,
        path: binary.display().to_string(),
        message: e.to_string(),
    })?;
    match raw.exit_code {
        Some(0) => {}
        other => {
            return Err(RunError::OracleVersionProbeFailed {
                path: binary.display().to_string(),
                exit_code: other,
                stderr: String::from_utf8_lossy(&raw.stderr).to_string(),
            })
        }
    }
    let stdout = String::from_utf8_lossy(&raw.stdout);
    parse_version_token(&stdout).ok_or_else(|| RunError::Binary {
        label,
        path: binary.display().to_string(),
        message: format!("--version produced no semver-shaped token: {stdout:?}"),
    })
}

/// The rewrite binary only needs to exist and be spawnable — the phase-0
/// skeleton's `--version` prints a notice and exits 2, and that is
/// accepted; only its existence is required (ADR 0018 phase-0 reality).
fn require_spawnable(binary: &Path, label: &'static str) -> Result<(), RunError> {
    exec::probe_version(binary)
        .map(|_| ())
        .map_err(|e| RunError::Binary {
            label,
            path: binary.display().to_string(),
            message: e.to_string(),
        })
}

/// The `(suite_name, case)` pairs a mode + filter selects from `suites`, in
/// declaration order. Gated mode selects only `ported` CASES (not suites);
/// self-check and all take every case. `suite_filter` restricts by suite
/// name and errors on an unknown name.
fn select_cases(
    mode: Mode,
    suite_filter: &[String],
    suites: &'static [Suite],
) -> Result<Vec<(&'static str, &'static Case)>, RunError> {
    if !suite_filter.is_empty() {
        for name in suite_filter {
            if !suites.iter().any(|s| s.name == name) {
                return Err(RunError::UnknownSuite(name.clone()));
            }
        }
    }
    let mut selected = Vec::new();
    for suite in suites {
        if !suite_filter.is_empty() && !suite_filter.iter().any(|n| n == suite.name) {
            continue;
        }
        for case in suite.cases {
            if matches!(mode, Mode::Gated) && !case.ported {
                continue;
            }
            selected.push((suite.name, case));
        }
    }
    Ok(selected)
}

/// Run against the production case catalog ([`cases::suites`]).
pub fn run(config: &RunConfig) -> Result<RunReport, RunError> {
    run_suites(config, cases::suites())
}

/// The seam under [`run`]: run against an explicit `suites` slice so tests
/// can drive the real orchestration end-to-end over a synthetic catalog
/// (e.g. a ported case to exercise the Diverged/stale paths, which the
/// phase-0 production catalog — zero ported cases — cannot).
fn normalize_run_error(
    e: normalize::NormalizeError,
    binary_label: &'static str,
    case_id: &'static str,
) -> RunError {
    match e {
        normalize::NormalizeError::Signaled => RunError::Signaled {
            binary_label,
            case_id,
        },
        normalize::NormalizeError::NonUtf8 { stream } => RunError::NonUtf8Output {
            binary_label,
            case_id,
            stream,
        },
    }
}

pub fn run_suites(config: &RunConfig, suites: &'static [Suite]) -> Result<RunReport, RunError> {
    if let Some(dup) = cases::duplicate_case_id(suites) {
        return Err(RunError::DuplicateCaseId(dup));
    }

    let self_check = matches!(config.mode, Mode::SelfCheck);

    let oracle_version = require_version(config.oracle, "oracle")?;
    // Self-check never runs the rewrite binary (candidate := oracle), so its
    // absence must not block vetting a case set — don't require it.
    if !self_check {
        require_spawnable(config.rewrite, "rewrite")?;
    }

    // Ledger/pin policy is mode-scoped (see the `Mode` doc comment):
    // self-check is blind (no ledger, no pin); gated/all load the full ledger
    // and enforce the pin.
    let ledger: Option<Ledger> = if self_check {
        None
    } else {
        let known: BTreeSet<&str> = suites
            .iter()
            .flat_map(|s| s.cases.iter().map(|c| c.id))
            .collect();
        let ported: BTreeSet<&str> = suites
            .iter()
            .flat_map(|s| s.cases.iter().filter(|c| c.ported).map(|c| c.id))
            .collect();
        let loaded = Ledger::load(config.ledger_path, &known, &ported).map_err(RunError::Ledger)?;
        loaded
            .check_oracle_version(&oracle_version)
            .map_err(RunError::Ledger)?;
        Some(loaded)
    };

    let selected = select_cases(config.mode, config.suite_filter, suites)?;

    let mut fixture_cache = FixtureCache::new().map_err(RunError::Fixture)?;
    let mut outcomes = Vec::new();
    let mut ran_ids: BTreeSet<&str> = BTreeSet::new();
    let mut diverged_ids: BTreeSet<&str> = BTreeSet::new();

    let candidate_binary: &Path = if self_check {
        config.oracle
    } else {
        config.rewrite
    };
    let candidate_label: &'static str = if self_check {
        "oracle (self-check second run)"
    } else {
        "rewrite"
    };

    for (suite_name, case) in selected {
        // Per-SIDE vaults: each binary gets its own copy so neither sees the
        // other's cache mutations (finding 5). A mutating case takes that one
        // step further — a FRESH per-case copy per side (`fresh_for`), so its
        // writes never leak into another case and both sides start identical.
        let fresh_for = if case.mutating { Some(case.id) } else { None };
        let oracle_vault = fixture_cache
            .materialize(&case.fixture, Side::Oracle, fresh_for)
            .map_err(RunError::Fixture)?;
        let candidate_vault = fixture_cache
            .materialize(&case.fixture, Side::Candidate, fresh_for)
            .map_err(RunError::Fixture)?;

        // Requirements: the argv depends on this doc/code actually existing in
        // the generated fixture — unmet is a runner error, not a quiet Match.
        if let Some(manifest) = fixture_cache.manifest_for(&case.fixture) {
            if let Some(requirement) =
                fixtures::unmet_requirement(manifest, case.requires_doc, case.requires_code)
            {
                return Err(RunError::UnmetRequirement {
                    case_id: case.id,
                    requirement,
                });
            }
        }

        let oracle_roots: Vec<&Path> = oracle_vault
            .spellings
            .iter()
            .map(PathBuf::as_path)
            .collect();
        let candidate_roots: Vec<&Path> = candidate_vault
            .spellings
            .iter()
            .map(PathBuf::as_path)
            .collect();

        let (verdict, post_state, mcp_diffs) = if let Some(frames) = case.stdin {
            // An MCP case: driven and compared frame-by-frame by `crate::mcp`,
            // never through the ordinary argv/stdout/stderr path below (see
            // that module's doc for the framing + timeout/EOF-early
            // semantics).
            let oracle_target = mcp::DriveTarget {
                binary: config.oracle,
                vault: &oracle_vault.path,
                roots: &oracle_roots,
                label: "oracle",
            };
            let candidate_target = mcp::DriveTarget {
                binary: candidate_binary,
                vault: &candidate_vault.path,
                roots: &candidate_roots,
                label: candidate_label,
            };
            let result = mcp::run_case(case.argv, frames, oracle_target, candidate_target)
                .map_err(|source| RunError::Mcp {
                    case_id: case.id,
                    source,
                })?;

            // Case-rot guard, exactly as the non-MCP path below: the oracle
            // side must exit exactly as declared.
            if result.oracle_exit != case.expect_oracle_exit {
                return Err(RunError::OracleExitMismatch {
                    case_id: case.id,
                    expected: case.expect_oracle_exit,
                    actual: result.oracle_exit,
                });
            }

            let matched = result.diffs.is_empty();
            let entry_id = ledger
                .as_ref()
                .and_then(|l| l.entry_for_case(case.id))
                .map(|e| e.id.as_str());
            let verdict = verdict::classify(matched, self_check, entry_id);
            (verdict, None, result.diffs)
        } else {
            let steps: Vec<Normalization> = normalize::DEFAULT
                .iter()
                .chain(case.normalize)
                .copied()
                .collect();

            let oracle_raw =
                exec::run_case(config.oracle, case, &oracle_vault.path).map_err(RunError::Exec)?;
            let oracle_norm = normalize::normalize_output(&oracle_raw, &oracle_roots, &steps)
                .map_err(|e| normalize_run_error(e, "oracle", case.id))?;

            // Case-rot guard: the oracle side must exit exactly as declared —
            // an error/error pair that would Match quietly is caught here first.
            if oracle_norm.exit_code != case.expect_oracle_exit {
                return Err(RunError::OracleExitMismatch {
                    case_id: case.id,
                    expected: case.expect_oracle_exit,
                    actual: oracle_norm.exit_code,
                });
            }

            let candidate_raw = exec::run_case(candidate_binary, case, &candidate_vault.path)
                .map_err(RunError::Exec)?;
            let candidate_norm =
                normalize::normalize_output(&candidate_raw, &candidate_roots, &steps)
                    .map_err(|e| normalize_run_error(e, candidate_label, case.id))?;

            // A mutating case additionally compares the two post-mutation vault
            // trees. A tree difference folds into the SAME match/diverged/drift
            // decision as an output difference (no fourth verdict); the diff is
            // kept only to make a divergence legible in the report.
            let post_state = if case.mutating {
                let oracle_snap = poststate::snapshot(&oracle_vault.path).map_err(|e| {
                    RunError::PostStateSnapshot {
                        case_id: case.id,
                        side_label: "oracle",
                        message: e.to_string(),
                    }
                })?;
                let candidate_snap = poststate::snapshot(&candidate_vault.path).map_err(|e| {
                    RunError::PostStateSnapshot {
                        case_id: case.id,
                        side_label: candidate_label,
                        message: e.to_string(),
                    }
                })?;
                poststate::compare(
                    &oracle_snap,
                    &oracle_roots,
                    &candidate_snap,
                    &candidate_roots,
                )
            } else {
                None
            };

            let matched =
                verdict::outputs_match(&oracle_norm, &candidate_norm) && post_state.is_none();
            let entry_id = ledger
                .as_ref()
                .and_then(|l| l.entry_for_case(case.id))
                .map(|e| e.id.as_str());
            let verdict = verdict::classify(matched, self_check, entry_id);
            (verdict, post_state, Vec::new())
        };

        ran_ids.insert(case.id);
        if let Verdict::Diverged { .. } = &verdict {
            diverged_ids.insert(case.id);
        }
        outcomes.push(CaseOutcome {
            case_id: case.id,
            suite_name,
            verdict,
            post_state,
            mcp_diffs,
        });
    }

    let stale_entries: Vec<String> = match &ledger {
        None => Vec::new(),
        Some(l) => l
            .stale_entries(&ran_ids, &diverged_ids)
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    };

    Ok(RunReport {
        outcomes,
        stale_entries,
        oracle_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version() {
        assert_eq!(
            parse_version_token("norn 0.48.0"),
            Some("0.48.0".to_string())
        );
    }

    #[test]
    fn parses_v_prefixed_version() {
        assert_eq!(
            parse_version_token("norn v0.48.0"),
            Some("0.48.0".to_string())
        );
    }

    #[test]
    fn parses_version_with_build_metadata() {
        assert_eq!(
            parse_version_token("norn 0.48.0 (abc 2026-01-01)"),
            Some("0.48.0".to_string())
        );
    }

    #[test]
    fn none_when_no_semver_token() {
        assert_eq!(parse_version_token("norn unknown"), None);
    }
}
