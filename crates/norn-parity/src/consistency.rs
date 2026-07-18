//! Oracle self-consistency checks (ADR 0018 "discovered inconsistency"
//! path): cross-command invariants checked against the oracle alone, on the
//! same fixture. A disagreement means the oracle contradicts itself — a
//! candidate ledger entry, not a rewrite-vs-oracle parity question — so
//! this module never touches the rewrite binary or the ledger.
//!
//! JSON parsing choice: hand-extracted via pragmatic string/bracket
//! scanning, the same approach `norn-fixtures/tests/oracle_smoke.rs` uses
//! for the summary `"codes"` map, rather than adding a `serde_json`
//! dev-dependency. The two shapes this module needs to read (`{"total":N}`
//! and a `"documents": [...]` array of objects) are simple enough that a
//! small brace/bracket-depth counter (string-literal aware, so braces
//! inside frontmatter string values never perturb the count) covers them
//! without a full parser.

use std::path::Path;

use crate::cases::{Fixture, CLEAN_1, ZOO_1};
use crate::exec::{self, ExecError};
use crate::fixtures::{FixtureCache, FixtureError, Side};

#[derive(Debug)]
pub enum ConsistencyError {
    Fixture(FixtureError),
    Exec(ExecError),
    Unparseable {
        check: &'static str,
        fixture: String,
        command: &'static str,
        raw: String,
    },
}

/// A failure raised by an individual invariant, before the runner attaches
/// the check name and fixture label.
enum CheckFailure {
    Exec(ExecError),
    Unparseable { command: &'static str, raw: String },
}

/// A single oracle self-consistency invariant bound to one fixture. The
/// descriptor slice ([`CHECKS`]) is iterated uniformly so each check-name
/// literal is stated exactly once.
struct Check {
    name: &'static str,
    fixture: Fixture,
    invariant: fn(&Path, &Path) -> Result<Option<String>, CheckFailure>,
}

/// The invariants, in report order (declaration order): both checks against
/// zoo, then both against clean.
const CHECKS: &[Check] = &[
    Check {
        name: "count-total-equals-find-rows",
        fixture: ZOO_1,
        invariant: check_count_matches_find,
    },
    Check {
        name: "summary-findings-equals-codes-sum",
        fixture: ZOO_1,
        invariant: check_summary_findings_equals_codes_sum,
    },
    Check {
        name: "count-total-equals-find-rows",
        fixture: CLEAN_1,
        invariant: check_count_matches_find,
    },
    Check {
        name: "summary-findings-equals-codes-sum",
        fixture: CLEAN_1,
        invariant: check_summary_findings_equals_codes_sum,
    },
];

impl std::fmt::Display for ConsistencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsistencyError::Fixture(e) => write!(f, "{e}"),
            ConsistencyError::Exec(e) => write!(f, "{e}"),
            ConsistencyError::Unparseable {
                check,
                fixture,
                command,
                raw,
            } => write!(
                f,
                "check `{check}` on fixture `{fixture}`: could not parse `{command}` output: {raw}"
            ),
        }
    }
}

impl std::error::Error for ConsistencyError {}

/// One disagreement surfaced by a consistency check — reported and treated
/// as a failing run (a candidate divergence-ledger entry per ADR 0018). The
/// fixture label is derived from the profile name and seed.
pub struct Finding {
    pub check: &'static str,
    pub fixture: String,
    pub message: String,
}

fn fixture_label(fixture: &Fixture) -> String {
    format!("{}-{}", fixture.profile_name, fixture.seed)
}

/// Run every descriptor in [`CHECKS`] oracle-only, in declaration order (the
/// report order). Each check materializes its fixture on the oracle side and
/// runs its invariant; a `Some(message)` is a disagreement.
pub fn run(oracle: &Path) -> Result<Vec<Finding>, ConsistencyError> {
    let mut cache = FixtureCache::new().map_err(ConsistencyError::Fixture)?;
    let mut findings = Vec::new();

    for check in CHECKS {
        // Oracle-only module: a single side suffices (no oracle-vs-candidate
        // comparison, so cross-binary symmetry is moot).
        let vault = cache
            .vault_for(&check.fixture, Side::Oracle)
            .map_err(ConsistencyError::Fixture)?;
        match (check.invariant)(oracle, &vault) {
            Ok(Some(message)) => findings.push(Finding {
                check: check.name,
                fixture: fixture_label(&check.fixture),
                message,
            }),
            Ok(None) => {}
            Err(CheckFailure::Exec(e)) => return Err(ConsistencyError::Exec(e)),
            Err(CheckFailure::Unparseable { command, raw }) => {
                return Err(ConsistencyError::Unparseable {
                    check: check.name,
                    fixture: fixture_label(&check.fixture),
                    command,
                    raw,
                })
            }
        }
    }

    Ok(findings)
}

/// `count` total equals the number of rows `find --format json --all`
/// returns. `--all` bypasses `find`'s default 10-row page — without it the
/// two commands are not comparable.
fn check_count_matches_find(oracle: &Path, vault: &Path) -> Result<Option<String>, CheckFailure> {
    let count_out = exec::run_argv(oracle, &["count", "--format", "json"], None, vault)
        .map_err(CheckFailure::Exec)?;
    let count_text = String::from_utf8_lossy(&count_out.stdout);
    let total = parse_int_field(&count_text, "total").ok_or_else(|| CheckFailure::Unparseable {
        command: "count --format json",
        raw: count_text.to_string(),
    })?;

    // `--all` alone only satisfies find's "opt in to a full-vault dump"
    // requirement — the default 10-row page still applies underneath it.
    // `--no-limit` is required too, or this check would just be comparing
    // `count`'s total against a constant 10 for any fixture with more than
    // 10 documents (empirically discovered: both starter fixtures have far
    // more than 10 docs, and the row count without `--no-limit` was a flat
    // 10 on both).
    let find_out = exec::run_argv(
        oracle,
        &["find", "--format", "json", "--all", "--no-limit"],
        None,
        vault,
    )
    .map_err(CheckFailure::Exec)?;
    let find_text = String::from_utf8_lossy(&find_out.stdout);
    let rows = count_documents(&find_text).ok_or_else(|| CheckFailure::Unparseable {
        command: "find --format json --all --no-limit",
        raw: find_text.to_string(),
    })?;

    if total == rows as i64 {
        Ok(None)
    } else {
        Ok(Some(format!(
            "count --format json total ({total}) != find --format json --all --no-limit row count ({rows})"
        )))
    }
}

/// `validate --summary --format json` findings total equals the sum of its
/// `codes` map values.
fn check_summary_findings_equals_codes_sum(
    oracle: &Path,
    vault: &Path,
) -> Result<Option<String>, CheckFailure> {
    let out = exec::run_argv(
        oracle,
        &["validate", "--summary", "--format", "json"],
        None,
        vault,
    )
    .map_err(CheckFailure::Exec)?;
    let text = String::from_utf8_lossy(&out.stdout);

    let findings_total =
        parse_int_field(&text, "findings").ok_or_else(|| CheckFailure::Unparseable {
            command: "validate --summary --format json",
            raw: text.to_string(),
        })?;
    let codes_sum = sum_codes_map(&text).ok_or_else(|| CheckFailure::Unparseable {
        command: "validate --summary --format json",
        raw: text.to_string(),
    })?;

    if findings_total == codes_sum {
        Ok(None)
    } else {
        Ok(Some(format!(
            "validate --summary findings ({findings_total}) != sum of codes map values ({codes_sum})"
        )))
    }
}

// The JSON scanners below (`parse_int_field`, `sum_codes_map`,
// `count_documents`, `find_array_start`, `find_object_start`) read the
// specific output shapes of the PINNED oracle version (see the ledger's
// `meta.oracle_version`). Their validity is scoped to that version; every
// oracle pin bump must revalidate them against the new output. They are
// deliberately not a general JSON parser (ADR 0018: no `serde_json`
// dependency for the harness).

/// Parses the integer value of `"key": N` (or `"key":N`) at any nesting
/// level — sufficient for the flat `total`/`findings` counters this module
/// reads; not a general JSON-path lookup.
fn parse_int_field(json: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\"");
    let key_pos = json.find(&needle)?;
    let after_key = &json[key_pos + needle.len()..];
    let colon_rel = after_key.find(':')?;
    let after_colon = after_key[colon_rel + 1..].trim_start();
    let end = after_colon
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(after_colon.len());
    after_colon[..end].parse::<i64>().ok()
}

/// Sums the integer values of the single-level `"codes": { "a": 1, "b": 2 }`
/// object. Assumes flat integer values (true for `validate --summary`'s
/// `codes` map) — not a general JSON object summer.
fn sum_codes_map(json: &str) -> Option<i64> {
    // Whitespace-tolerant: matches `"codes":{`, `"codes": {`, `"codes"  :  {`
    // — the pinned oracle emits `"codes": {`, but the scanner should not be
    // brittle to spacing changes within an otherwise-unchanged shape.
    let open = find_object_start(json, "codes")?;
    let rest = &json[open + 1..];
    let end = rest.find('}')?;
    let body = &rest[..end];
    let mut sum = 0i64;
    for line in body.lines() {
        let line = line.trim().trim_end_matches(',');
        if line.is_empty() {
            continue;
        }
        let (_key, value) = line.split_once(':')?;
        sum += value.trim().parse::<i64>().ok()?;
    }
    Some(sum)
}

/// Counts the top-level object elements of the `"documents": [...]` array.
/// String-literal aware (quote + backslash-escape tracking) so braces or
/// brackets inside a frontmatter string value never perturb the depth
/// count; a new element is any `{` seen at array depth 1.
fn count_documents(json: &str) -> Option<usize> {
    let open = find_array_start(json, "documents")?;
    let bytes = json.as_bytes();
    let mut i = open + 1;
    let mut depth: i32 = 1;
    let mut count = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                if depth == 1 {
                    count += 1;
                }
                depth += 1;
            }
            '[' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(count);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The byte index of the `[` opening the array value of `"key":` in `json`.
fn find_array_start(json: &str, key: &str) -> Option<usize> {
    find_value_open(json, key, b'[')
}

/// The byte index of the `{` opening the object value of `"key":` in `json`.
fn find_object_start(json: &str, key: &str) -> Option<usize> {
    find_value_open(json, key, b'{')
}

/// The byte index of the `open` delimiter beginning the value of `"key":`,
/// tolerating arbitrary whitespace between the key, the colon, and the
/// delimiter.
fn find_value_open(json: &str, key: &str, open: u8) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let key_pos = json.find(&needle)?;
    let after_key = &json[key_pos + needle.len()..];
    let colon_rel = after_key.find(':')?;
    let after_colon = &after_key[colon_rel + 1..];
    let delim_rel = after_colon.find(|c: char| !c.is_whitespace())?;
    if after_colon.as_bytes().get(delim_rel) != Some(&open) {
        return None;
    }
    Some(key_pos + needle.len() + colon_rel + 1 + delim_rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_total() {
        assert_eq!(parse_int_field("{\"total\":82}", "total"), Some(82));
    }

    #[test]
    fn parses_pretty_findings() {
        let json = "{\n  \"findings\": 18,\n  \"codes\": {}\n}";
        assert_eq!(parse_int_field(json, "findings"), Some(18));
    }

    #[test]
    fn sums_empty_codes_map() {
        assert_eq!(sum_codes_map("{\n  \"codes\": {}\n}"), Some(0));
    }

    #[test]
    fn sums_populated_codes_map() {
        let json = "{\n  \"codes\": {\n    \"a\": 1,\n    \"b\": 4\n  }\n}";
        assert_eq!(sum_codes_map(json), Some(5));
    }

    #[test]
    fn sums_codes_map_with_nonstandard_needle_spacing() {
        // Whitespace-tolerant needle: a compact `"codes":{` and an
        // extra-spaced `"codes"  :  {` both locate the object (the pinned
        // oracle emits `"codes": {`, and the map body stays one entry per
        // line as the oracle pretty-prints it).
        let compact_needle = "{\n  \"codes\":{\n    \"a\": 1,\n    \"b\": 4\n  }\n}";
        assert_eq!(sum_codes_map(compact_needle), Some(5));
        let spaced_needle = "{\n  \"codes\"  :  {\n    \"a\": 2\n  }\n}";
        assert_eq!(sum_codes_map(spaced_needle), Some(2));
    }

    #[test]
    fn counts_empty_documents_array() {
        assert_eq!(count_documents("{\"documents\": []}"), Some(0));
    }

    #[test]
    fn counts_documents_ignoring_braces_in_string_values() {
        let json = r#"{
  "documents": [
    { "frontmatter": { "title": "has a { brace } and [ bracket ]" }, "path": "a.md" },
    { "frontmatter": { "title": "second" }, "path": "b.md" }
  ]
}"#;
        assert_eq!(count_documents(json), Some(2));
    }

    #[test]
    fn counts_documents_with_escaped_quotes_in_string_values() {
        let json = r#"{"documents": [{"frontmatter": {"title": "a \"quoted\" word { not a brace"}, "path": "a.md"}]}"#;
        assert_eq!(count_documents(json), Some(1));
    }
}
