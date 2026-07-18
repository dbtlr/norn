//! Orchestration: validate the two binaries and the ledger, materialize
//! fixtures, run cases, and assemble a [`RunReport`]. Shared by the bin's
//! default/`--self-check`/`--all` modes and by the integration tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cases::{self, Suite};
use crate::exec::{self, ExecError};
use crate::fixtures::{FixtureCache, FixtureError};
use crate::ledger::{Ledger, LedgerError};
use crate::normalize;
use crate::verdict::{self, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every case, oracle vs. itself. Ledger not consulted (self-comparison
    /// cannot legitimately diverge); stale checks off.
    SelfCheck,
    /// `ported == true` suites only, oracle vs. rewrite. The default mode.
    Gated,
    /// Every suite regardless of `ported`, oracle vs. rewrite — the
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
    UnknownSuite(String),
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
            RunError::UnknownSuite(name) => write!(f, "unknown suite: {name}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Spawn `binary --version` and require it to succeed, returning the
/// version token (last whitespace-separated word of stdout, e.g. `norn
/// 0.48.0` -> `0.48.0`). Used for the oracle, whose version must match the
/// ledger's pinned `meta.oracle_version`.
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
    stdout
        .split_whitespace()
        .next_back()
        .map(|s| s.to_string())
        .ok_or_else(|| RunError::Binary {
            label,
            path: binary.display().to_string(),
            message: format!("--version produced no parseable output: {stdout:?}"),
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

fn select_suites<'a>(mode: Mode, suite_filter: &[String]) -> Result<Vec<&'a Suite>, RunError> {
    let all = cases::suites();
    if !suite_filter.is_empty() {
        for name in suite_filter {
            if !all.iter().any(|s| s.name == name) {
                return Err(RunError::UnknownSuite(name.clone()));
            }
        }
    }
    Ok(all
        .iter()
        .filter(|s| suite_filter.is_empty() || suite_filter.iter().any(|n| n == s.name))
        .filter(|s| !matches!(mode, Mode::Gated) || s.ported)
        .collect())
}

pub fn run(config: &RunConfig) -> Result<RunReport, RunError> {
    let oracle_version = require_version(config.oracle, "oracle")?;
    require_spawnable(config.rewrite, "rewrite")?;

    let known_case_ids: BTreeSet<&str> = cases::all_case_ids().into_iter().collect();
    let ledger = Ledger::load(config.ledger_path, &known_case_ids).map_err(RunError::Ledger)?;
    ledger
        .check_oracle_version(&oracle_version)
        .map_err(RunError::Ledger)?;

    let selected = select_suites(config.mode, config.suite_filter)?;

    let mut fixture_cache = FixtureCache::new().map_err(RunError::Fixture)?;
    let mut outcomes = Vec::new();
    let mut ran_ids: BTreeSet<&str> = BTreeSet::new();
    let mut diverged_ids: BTreeSet<&str> = BTreeSet::new();

    let candidate_binary: &Path = match config.mode {
        Mode::SelfCheck => config.oracle,
        Mode::Gated | Mode::All => config.rewrite,
    };
    let candidate_label: &'static str = match config.mode {
        Mode::SelfCheck => "oracle (self-check second run)",
        Mode::Gated | Mode::All => "rewrite",
    };

    for suite in &selected {
        for case in suite.cases {
            let vault: PathBuf = fixture_cache
                .vault_for(&case.fixture)
                .map_err(RunError::Fixture)?;

            let oracle_raw = exec::run_case(config.oracle, case, &vault).map_err(RunError::Exec)?;
            let oracle_norm = normalize::normalize_output(&oracle_raw, &vault, normalize::DEFAULT)
                .ok_or(RunError::Signaled {
                    binary_label: "oracle",
                    case_id: case.id,
                })?;

            let candidate_raw =
                exec::run_case(candidate_binary, case, &vault).map_err(RunError::Exec)?;
            let candidate_norm =
                normalize::normalize_output(&candidate_raw, &vault, normalize::DEFAULT).ok_or(
                    RunError::Signaled {
                        binary_label: candidate_label,
                        case_id: case.id,
                    },
                )?;

            let verdict = if verdict::outputs_match(&oracle_norm, &candidate_norm) {
                Verdict::Match
            } else if matches!(config.mode, Mode::SelfCheck) {
                Verdict::Drift
            } else {
                match ledger.entry_for_case(case.id) {
                    Some(entry) => Verdict::Diverged {
                        entry_id: entry.id.clone(),
                    },
                    None => Verdict::Drift,
                }
            };

            ran_ids.insert(case.id);
            if let Verdict::Diverged { .. } = &verdict {
                diverged_ids.insert(case.id);
            }
            outcomes.push(CaseOutcome {
                case_id: case.id,
                suite_name: suite.name,
                verdict,
            });
        }
    }

    let stale_entries: Vec<String> = if matches!(config.mode, Mode::SelfCheck) {
        Vec::new()
    } else {
        ledger
            .stale_entries(&ran_ids, &diverged_ids)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    };

    Ok(RunReport {
        outcomes,
        stale_entries,
        oracle_version,
    })
}
