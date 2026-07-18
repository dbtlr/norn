//! The divergence ledger (`docs/parity-ledger.toml`, ADR 0018): every
//! intended old->new behavior difference, decision-gated. A differing case
//! passes ONLY when an entry here covers it; a differing case with no entry
//! is drift.
//!
//! Parsing goes through the `toml` crate's untyped `Table`/`Value` API
//! (no `serde` derive) — correctness of the gate is trust-critical, so
//! hand-rolling a TOML parser was ruled out per the implementation spec.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    DecidedBetter,
    DiscoveredInconsistency,
}

impl Reason {
    fn parse(s: &str) -> Option<Reason> {
        match s {
            "decided-better" => Some(Reason::DecidedBetter),
            "discovered-inconsistency" => Some(Reason::DiscoveredInconsistency),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub surface: String,
    pub cases: Vec<String>,
    pub old: String,
    pub new: String,
    pub reason: Reason,
    pub decision: String,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub oracle_version: String,
}

#[derive(Clone, Debug)]
pub struct Ledger {
    pub meta: Meta,
    /// Declaration order — report ordering must stay deterministic.
    pub entries: Vec<Entry>,
    /// case id -> index into `entries`, built while checking the
    /// one-entry-per-case invariant so verdict lookups are O(log n).
    case_index: std::collections::BTreeMap<String, usize>,
}

#[derive(Debug)]
pub enum LedgerError {
    Io {
        path: String,
        message: String,
    },
    Parse {
        path: String,
        message: String,
    },
    MissingMetaTable,
    MissingField {
        context: String,
        field: &'static str,
    },
    WrongType {
        context: String,
        field: &'static str,
        expected: &'static str,
    },
    UnknownReason {
        entry: String,
        value: String,
    },
    DuplicateEntryId(String),
    /// An entry with `cases = []` maps to nothing and is skipped forever by
    /// the stale check — dead weight that can only mislead. Rejected at load.
    EmptyCases {
        entry: String,
    },
    UnknownCaseId {
        entry: String,
        case: String,
    },
    UnportedCaseId {
        entry: String,
        case: String,
    },
    CaseCitedByMultipleEntries {
        case: String,
        first_entry: String,
        second_entry: String,
    },
    OracleVersionMismatch {
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io { path, message } => write!(f, "cannot read ledger {path}: {message}"),
            LedgerError::Parse { path, message } => {
                write!(f, "cannot parse ledger {path} as TOML: {message}")
            }
            LedgerError::MissingMetaTable => write!(f, "ledger is missing the [meta] table"),
            LedgerError::MissingField { context, field } => {
                write!(f, "{context} is missing required field `{field}`")
            }
            LedgerError::WrongType {
                context,
                field,
                expected,
            } => write!(f, "{context} field `{field}` must be {expected}"),
            LedgerError::UnknownReason { entry, value } => write!(
                f,
                "entry {entry}: unknown reason `{value}` (expected `decided-better` or `discovered-inconsistency`)"
            ),
            LedgerError::DuplicateEntryId(id) => write!(f, "duplicate ledger entry id: {id}"),
            LedgerError::UnknownCaseId { entry, case } => {
                write!(f, "entry {entry} cites unknown case id `{case}`")
            }
            LedgerError::EmptyCases { entry } => write!(
                f,
                "entry {entry} cites no cases — every entry must cover at least one case"
            ),
            LedgerError::UnportedCaseId { entry, case } => write!(
                f,
                "entry {entry} cites case `{case}` whose surface is not yet ported — \
                 divergence can only be observed on a ported surface, so an entry for an \
                 unported one is premature"
            ),
            LedgerError::CaseCitedByMultipleEntries {
                case,
                first_entry,
                second_entry,
            } => write!(
                f,
                "case `{case}` is cited by more than one entry ({first_entry}, {second_entry}) — \
                 a diverged verdict must resolve to exactly one entry"
            ),
            LedgerError::OracleVersionMismatch { expected, actual } => write!(
                f,
                "oracle --version reported `{actual}`, but the ledger's [meta] oracle_version is `{expected}` — \
                 refusing to compare against an unpinned oracle"
            ),
        }
    }
}

impl std::error::Error for LedgerError {}

fn get_str(table: &toml::Table, context: &str, field: &'static str) -> Result<String, LedgerError> {
    match table.get(field) {
        None => Err(LedgerError::MissingField {
            context: context.to_string(),
            field,
        }),
        Some(toml::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(LedgerError::WrongType {
            context: context.to_string(),
            field,
            expected: "a string",
        }),
    }
}

fn get_str_array(
    table: &toml::Table,
    context: &str,
    field: &'static str,
) -> Result<Vec<String>, LedgerError> {
    match table.get(field) {
        None => Err(LedgerError::MissingField {
            context: context.to_string(),
            field,
        }),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                toml::Value::String(s) => Ok(s.clone()),
                _ => Err(LedgerError::WrongType {
                    context: context.to_string(),
                    field,
                    expected: "an array of strings",
                }),
            })
            .collect(),
        Some(_) => Err(LedgerError::WrongType {
            context: context.to_string(),
            field,
            expected: "an array of strings",
        }),
    }
}

impl Ledger {
    /// Parse and structurally validate ledger TOML text: missing required
    /// fields, an unknown `reason`, a duplicate entry id, a case id cited by
    /// more than one entry (the load-time corollary of a Diverged verdict
    /// needing to resolve to exactly one entry), an entry citing a case id
    /// absent from `known_case_ids`, and — the phase-0 rot guard — an entry
    /// citing a known case id that is not in `ported_case_ids`. An entry for
    /// an unported surface is premature: divergence can only be observed once
    /// that surface is ported (until then a gated run executes zero of its
    /// cases and the stale check never fires), so it is rejected at load.
    pub fn parse(
        text: &str,
        known_case_ids: &BTreeSet<&str>,
        ported_case_ids: &BTreeSet<&str>,
    ) -> Result<Ledger, LedgerError> {
        let root: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| LedgerError::Parse {
                path: "<in-memory>".to_string(),
                message: e.to_string(),
            })?;

        let meta_table = root
            .get("meta")
            .and_then(|v| v.as_table())
            .ok_or(LedgerError::MissingMetaTable)?;
        let oracle_version = get_str(meta_table, "[meta]", "oracle_version")?;
        let meta = Meta { oracle_version };

        let raw_entries: &[toml::Value] = match root.get("entry") {
            None => &[],
            Some(toml::Value::Array(items)) => items,
            Some(_) => {
                return Err(LedgerError::WrongType {
                    context: "top level".to_string(),
                    field: "entry",
                    expected: "an array of tables",
                })
            }
        };

        let mut entries = Vec::with_capacity(raw_entries.len());
        let mut seen_ids: BTreeSet<String> = BTreeSet::new();
        let mut case_index: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();

        for (index, raw) in raw_entries.iter().enumerate() {
            let table = raw.as_table().ok_or_else(|| LedgerError::WrongType {
                context: format!("entry[{index}]"),
                field: "entry",
                expected: "a table",
            })?;
            let context = format!("entry[{index}]");
            let id = get_str(table, &context, "id")?;
            let context = format!("entry `{id}`");
            let surface = get_str(table, &context, "surface")?;
            let cases = get_str_array(table, &context, "cases")?;
            if cases.is_empty() {
                return Err(LedgerError::EmptyCases { entry: id });
            }
            let old = get_str(table, &context, "old")?;
            let new = get_str(table, &context, "new")?;
            let reason_str = get_str(table, &context, "reason")?;
            let decision = get_str(table, &context, "decision")?;

            let reason = Reason::parse(&reason_str).ok_or_else(|| LedgerError::UnknownReason {
                entry: id.clone(),
                value: reason_str.clone(),
            })?;

            if !seen_ids.insert(id.clone()) {
                return Err(LedgerError::DuplicateEntryId(id));
            }

            for case in &cases {
                if !known_case_ids.contains(case.as_str()) {
                    return Err(LedgerError::UnknownCaseId {
                        entry: id.clone(),
                        case: case.clone(),
                    });
                }
                if !ported_case_ids.contains(case.as_str()) {
                    return Err(LedgerError::UnportedCaseId {
                        entry: id.clone(),
                        case: case.clone(),
                    });
                }
                if let Some(&existing) = case_index.get(case) {
                    return Err(LedgerError::CaseCitedByMultipleEntries {
                        case: case.clone(),
                        first_entry: entries_id_at(&entries, existing),
                        second_entry: id.clone(),
                    });
                }
                case_index.insert(case.clone(), entries.len());
            }

            entries.push(Entry {
                id,
                surface,
                cases,
                old,
                new,
                reason,
                decision,
            });
        }

        Ok(Ledger {
            meta,
            entries,
            case_index,
        })
    }

    pub fn load(
        path: &Path,
        known_case_ids: &BTreeSet<&str>,
        ported_case_ids: &BTreeSet<&str>,
    ) -> Result<Ledger, LedgerError> {
        let text = fs::read_to_string(path).map_err(|e| LedgerError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::parse(&text, known_case_ids, ported_case_ids).map_err(|e| match e {
            LedgerError::Parse { message, .. } => LedgerError::Parse {
                path: path.display().to_string(),
                message,
            },
            other => other,
        })
    }

    /// The pinned oracle version this ledger was authored against must
    /// match the oracle binary actually being run — protects against
    /// comparing rewrite-vs-rewrite or a floated oracle.
    pub fn check_oracle_version(&self, actual: &str) -> Result<(), LedgerError> {
        if self.meta.oracle_version != actual {
            return Err(LedgerError::OracleVersionMismatch {
                expected: self.meta.oracle_version.clone(),
                actual: actual.to_string(),
            });
        }
        Ok(())
    }

    /// The entry citing `case_id`, if any. A diverged case with no entry
    /// here is drift; with an entry here, the verdict is diverged, citing
    /// this entry's id.
    pub fn entry_for_case(&self, case_id: &str) -> Option<&Entry> {
        self.case_index.get(case_id).map(|&i| &self.entries[i])
    }

    /// Entry ids that are stale after this run: cited by at least one case
    /// that ran (`ran`), but none of those cases actually diverged
    /// (`diverged`). ADR 0018: "entries cannot rot" — an entry whose cases
    /// all currently match must fail the run just as loudly as an
    /// uncovered drift.
    pub fn stale_entries(&self, ran: &BTreeSet<&str>, diverged: &BTreeSet<&str>) -> Vec<&str> {
        let mut stale = Vec::new();
        for entry in &self.entries {
            let cited_and_ran: Vec<&str> = entry
                .cases
                .iter()
                .map(|s| s.as_str())
                .filter(|c| ran.contains(c))
                .collect();
            if cited_and_ran.is_empty() {
                continue;
            }
            let any_diverged = cited_and_ran.iter().any(|c| diverged.contains(c));
            if !any_diverged {
                stale.push(entry.id.as_str());
            }
        }
        stale
    }
}

fn entries_id_at(entries: &[Entry], index: usize) -> String {
    entries
        .get(index)
        .map(|e| e.id.clone())
        .unwrap_or_else(|| format!("<entry #{index}>"))
}
