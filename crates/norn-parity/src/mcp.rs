//! MCP stdio frame driving + frame-by-frame comparison (NRN-383, ADR 0018
//! phase 3): `crate::cases::Case::stdin` carries an ordered list of
//! JSON-RPC request frames for a case whose `argv` is `["mcp"]`; this
//! module drives `norn mcp` with them on both sides and compares the
//! responses frame-by-frame, feeding the SAME three-verdict machinery
//! (`crate::verdict`) every other case uses — no fourth state.
//!
//! Framing, verified empirically against the pinned oracle (v0.48.1, see the
//! crate's implementation notes): `norn mcp` speaks newline-delimited
//! JSON-RPC over stdio — one JSON object per line on both stdin and stdout,
//! no LSP-style `Content-Length` header. The process reads until stdin EOF
//! and exits on its own once every frame has been handled (confirmed by
//! writing a fixed frame set, closing stdin, and observing a clean exit —
//! never required killing). A driver still bounds the wait with a hard
//! timeout (`exec::run_argv_bounded`): a misbehaving side — a stub in a
//! test, or a real bug — that never exits must never hang the runner.
//! (NRN-324 tracks child-process bounding generally; this timeout is scoped
//! to MCP driving only, not a duplicate of that story.)
//!
//! Only stdout is compared. JSON-RPC is a stdout-only contract for `norn
//! mcp` (every request/response frame is a stdout line; stderr carries no
//! protocol content) — the CLI argv/stdout/stderr path already owns stderr
//! parity for every other surface, and MCP has no stderr contract of its own
//! to hold parity over.
//!
//! Both a response timeout and a premature EOF (the process exited before
//! answering every request frame) are runner errors ([`McpError`]), never
//! folded into a verdict — symmetrically for the oracle and the candidate
//! side. This mirrors the case-rot philosophy `run::RunError::
//! OracleExitMismatch` already applies to the oracle (a case whose oracle
//! side cannot be trusted to behave as declared cannot be compared
//! meaningfully) generalized to both sides: an MCP session that cannot
//! complete a full request/response handshake is an environment problem,
//! not a nuanced part of the parity comparison. `run::Mode::All` (the
//! never-CI-gating burn-down view) is the one place that softens this: it
//! catches an [`McpError`] per case and renders a runner-error row instead
//! of aborting the whole run, so a not-yet-ported candidate surface doesn't
//! discard the rest of the burn-down; `Gated`/`SelfCheck` keep the hard
//! abort (see `run::run_suites`).
//!
//! Beyond frame content, three more axes feed the same match/diverged/drift
//! decision (bundled in [`McpDivergence`], never a runner error): the two
//! sides' process EXIT codes (a rewrite that answers every frame correctly
//! but exits non-zero is not a Match), a response id on either side that
//! was never requested (an "extra" response), and an id either side
//! answered more than once (a "duplicate" response — adjudicated as a
//! divergence rather than a hard error: a repeated id is anomalous, but it
//! is still a *behavioral difference* a rewrite could genuinely diverge on,
//! and the ledger — not a blanket abort — is where that judgment call
//! belongs).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::exec::{self, ExecError};
use crate::normalize;

/// Local to MCP driving (see `exec::run_argv_bounded`'s doc): the real
/// oracle answers every smoke case in well under a second even cold, so this
/// leaves generous headroom for CI jitter while still bounding a hung stub
/// or a real bug to a few seconds instead of forever.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `serverInfo.version` (the crate version the running binary was built
/// at — differs release to release, and the rewrite's own crate version
/// will differ from the oracle's pin once it serves MCP) is the one
/// substitution applied before comparison, targeted by exact object-key
/// name wherever it occurs in the response tree.
///
/// `protocolVersion` is deliberately NOT normalized (dropped after review):
/// both sides here always echo back the SAME client-requested revision (see
/// the crate's implementation notes — the oracle only falls back to its own
/// default when the client sends a revision it does not recognize), so
/// every case matches without normalizing it. A future rewrite that
/// genuinely negotiates a different MCP protocol revision than the oracle
/// is exactly the kind of divergence the ledger exists to record — silently
/// erasing it here would bypass that decision gate rather than serve it.
const SERVER_VERSION_PLACEHOLDER: &str = "<MCP_SERVER_VERSION>";

/// One side to drive: the binary, its materialized vault (cwd), every
/// absolute spelling of that vault (for `VaultRoot` normalization, mirroring
/// `crate::normalize`), and a label for error messages.
pub struct DriveTarget<'a> {
    pub binary: &'a Path,
    pub vault: &'a Path,
    pub roots: &'a [&'a Path],
    pub label: &'static str,
}

/// A request frame's identity, extracted from `Case::stdin` before driving:
/// the JSON-RPC `id` (kept as its canonical JSON text, so a numeric and a
/// string id can never collide and compare exactly), the same for display,
/// and the `method` for reporting. A frame with no `id` (a notification,
/// e.g. `notifications/initialized`) produces no [`RequestMeta`] — JSON-RPC
/// promises it no response.
#[derive(Debug)]
struct RequestMeta {
    id_key: String,
    id_display: String,
    method: String,
}

/// One response frame that differs between oracle and candidate after
/// normalization. Reported as the id/method/pointer triple only — never a
/// full frame dump (`report::render`'s job, mirroring `poststate`'s concise
/// `ContentDelta`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDiff {
    pub id: String,
    pub method: String,
    /// A JSON-pointer-shaped path (`/result/serverInfo/version`) to the
    /// first point the two normalized response values differ.
    pub pointer: String,
}

/// A response id one side echoed that was never among the case's
/// id-bearing request frames — an unsolicited response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraResponseId {
    pub label: &'static str,
    pub id: String,
}

/// A response id one side answered more than once. See the module doc for
/// why this is adjudicated as a divergence rather than a runner error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateResponseId {
    pub label: &'static str,
    pub id: String,
    pub count: usize,
}

/// Everything one MCP case's driving found different from a clean Match,
/// bundled for reporting — mirrors `crate::poststate::PostStateDiff`'s
/// "present (and non-empty) only on a real difference" convention.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpDivergence {
    /// `Some((oracle, candidate))` when the two sides' process exit codes
    /// differ. The oracle side's exit is ALSO checked against
    /// `Case::expect_oracle_exit` by the caller (`run::RunError::
    /// OracleExitMismatch`, a runner error/case-rot guard, unaffected by
    /// this field) — this field instead compares the two sides' exits
    /// against EACH OTHER, so a candidate that answers every frame
    /// correctly but exits non-zero is not silently blessed as a Match.
    pub exit_mismatch: Option<(i32, i32)>,
    /// Every differing frame, in request declaration order.
    pub diffs: Vec<FrameDiff>,
    pub extra_response_ids: Vec<ExtraResponseId>,
    pub duplicate_response_ids: Vec<DuplicateResponseId>,
}

impl McpDivergence {
    pub fn is_empty(&self) -> bool {
        self.exit_mismatch.is_none()
            && self.diffs.is_empty()
            && self.extra_response_ids.is_empty()
            && self.duplicate_response_ids.is_empty()
    }
}

/// The result of driving one MCP case against both sides.
pub struct McpCaseResult {
    /// The oracle side's process exit code — the case-rot guard
    /// (`run::RunError::OracleExitMismatch`) checks this against
    /// `Case::expect_oracle_exit` exactly like the non-MCP path.
    pub oracle_exit: i32,
    pub candidate_exit: i32,
    pub divergence: McpDivergence,
}

impl McpCaseResult {
    /// `true` when the two sides are indistinguishable on every axis this
    /// module checks (frame content, exit code, extra/duplicate responses).
    pub fn matched(&self) -> bool {
        self.divergence.is_empty()
    }
}

/// Why an MCP case could not be driven to a comparable result — a runner
/// error (exit 2), never a verdict; see the module doc for why both a
/// timeout and a premature EOF apply symmetrically to either side.
#[derive(Debug)]
pub enum McpError {
    Exec {
        label: &'static str,
        source: ExecError,
    },
    /// A declared case frame (`Case::stdin`) is not valid JSON — a
    /// case-authoring bug (the frames are hand-written literals in
    /// `cases.rs`), never oracle/candidate behavior.
    MalformedCaseFrame { raw: String, message: String },
    /// A side's process ended (timed out, or exited) before every request
    /// frame's `id` had a paired response line — an incomplete session
    /// cannot be compared meaningfully.
    EofEarly {
        label: &'static str,
        expected: usize,
        received: usize,
        missing_ids: Vec<String>,
    },
    /// A side wrote a line to stdout that is not valid JSON.
    UnparsableResponse {
        label: &'static str,
        raw: String,
        message: String,
    },
    /// A side's process was terminated by a signal (not our own timeout
    /// kill) — no exit code to check or trust.
    Signaled { label: &'static str },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Exec { label, source } => write!(f, "{label}: {source}"),
            McpError::MalformedCaseFrame { raw, message } => write!(
                f,
                "a declared MCP case frame is not valid JSON: {message} (frame: {raw})"
            ),
            McpError::EofEarly {
                label,
                expected,
                received,
                missing_ids,
            } => write!(
                f,
                "{label} produced {received} of {expected} expected MCP responses before ending \
                 (missing ids: {}) — premature EOF, not a verdict",
                missing_ids.join(", ")
            ),
            McpError::UnparsableResponse {
                label,
                raw,
                message,
            } => write!(
                f,
                "{label} wrote a non-JSON line on MCP stdout: {message} (line: {raw})"
            ),
            McpError::Signaled { label } => {
                write!(f, "{label} was terminated by a signal running MCP frames")
            }
        }
    }
}

impl std::error::Error for McpError {}

/// Parse `frames` in declaration order, extracting the [`RequestMeta`] for
/// every frame that carries an `id`. A frame missing `id` is a notification
/// and is skipped — it produces no response line to pair.
fn request_metas(frames: &[&str]) -> Result<Vec<RequestMeta>, McpError> {
    let mut out = Vec::new();
    for frame in frames {
        let value: Value =
            serde_json::from_str(frame).map_err(|e| McpError::MalformedCaseFrame {
                raw: (*frame).to_string(),
                message: e.to_string(),
            })?;
        let Some(id) = value.get("id") else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(RequestMeta {
            id_key: serde_json::to_string(id).unwrap_or_default(),
            id_display: serde_json::to_string(id).unwrap_or_default(),
            method,
        });
    }
    Ok(out)
}

/// One side's complete, parseable MCP session: every response keyed by its
/// canonical `id` text (last-wins on a repeated id — [`extra_and_duplicates`]
/// is what flags the repeat as a divergence rather than silently
/// overwriting), the process exit code, and the ids that were unsolicited
/// or repeated.
struct SideResult {
    responses: BTreeMap<String, Value>,
    exit_code: i32,
    extra_ids: Vec<String>,
    duplicate_ids: Vec<(String, usize)>,
}

/// Partition `counts` (every response id seen, with how many times) against
/// `expected_ids` (the case's id-bearing request frames): ids never
/// requested (extra), and ids seen more than once (duplicate — regardless of
/// whether they were also requested). Both lists are in `counts`' key order
/// (a `BTreeMap`, so already sorted — deterministic regardless of the wire
/// order the side answered in).
fn extra_and_duplicates(
    counts: &BTreeMap<String, usize>,
    expected_ids: &BTreeSet<&str>,
) -> (Vec<String>, Vec<(String, usize)>) {
    let extra = counts
        .keys()
        .filter(|id| !expected_ids.contains(id.as_str()))
        .cloned()
        .collect();
    let duplicate = counts
        .iter()
        .filter(|(_, &count)| count > 1)
        .map(|(id, &count)| (id.clone(), count))
        .collect();
    (extra, duplicate)
}

/// Drive `target` with `frames` (newline-joined verbatim onto stdin, exactly
/// as a real MCP client would write them) and return every response line
/// parsed as JSON, keyed by its canonical `id` text, plus the process exit
/// code and any extra/duplicate ids. Errors (via [`McpError`]) if the
/// process times out, is signaled, a response line is not valid JSON, or it
/// ends without answering every id in `expected`.
fn collect_responses(
    target: &DriveTarget,
    argv: &[&str],
    frames: &[&str],
    expected: &[RequestMeta],
) -> Result<SideResult, McpError> {
    let payload: String = frames.iter().map(|f| format!("{f}\n")).collect();
    let raw = exec::run_argv_bounded(target.binary, argv, Some(&payload), target.vault, TIMEOUT)
        .map_err(|source| McpError::Exec {
            label: target.label,
            source,
        })?;

    let stdout = String::from_utf8_lossy(&raw.stdout);
    let mut responses = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value =
            serde_json::from_str(line).map_err(|e| McpError::UnparsableResponse {
                label: target.label,
                raw: line.to_string(),
                message: e.to_string(),
            })?;
        if let Some(id) = value.get("id") {
            let key = serde_json::to_string(id).unwrap_or_default();
            *counts.entry(key.clone()).or_insert(0) += 1;
            responses.insert(key, value);
        }
    }

    let missing: Vec<String> = expected
        .iter()
        .filter(|m| !responses.contains_key(&m.id_key))
        .map(|m| m.id_display.clone())
        .collect();
    if !missing.is_empty() {
        return Err(McpError::EofEarly {
            label: target.label,
            expected: expected.len(),
            // Count only EXPECTED ids that arrived — an unsolicited response
            // must not inflate this, or the message contradicts missing_ids.
            received: expected.len() - missing.len(),
            missing_ids: missing,
        });
    }

    let expected_ids: BTreeSet<&str> = expected.iter().map(|m| m.id_key.as_str()).collect();
    let (extra_ids, duplicate_ids) = extra_and_duplicates(&counts, &expected_ids);

    let exit_code = raw.exit_code.ok_or(McpError::Signaled {
        label: target.label,
    })?;
    Ok(SideResult {
        responses,
        exit_code,
        extra_ids,
        duplicate_ids,
    })
}

/// The placeholder every `generated_at` timestamp normalizes to. A repair /
/// apply `MigrationPlan` stamps a wall-clock `generated_at` at plan time, so two
/// runs of the SAME binary (self-check) — and the oracle vs. the rewrite — differ
/// on it alone. It is excluded from the plan's content hash (so `plan_hash`
/// already matches), but the raw plan JSON still embeds it; this substitution
/// neutralizes it exactly like `serverInfo.version`, by exact object-key name.
const GENERATED_AT_PLACEHOLDER: &str = "<PLAN_GENERATED_AT>";

/// The placeholder every `plan_hash` normalizes to. A cascade verb's
/// `ApplyReport.plan_hash` is `MigrationPlan::canonical_hash()`, which depends on
/// the plan's absolute `vault_root` — so two runs against fixtures materialized
/// in DIFFERENT temp dirs (both self-check sides, and oracle vs. rewrite) always
/// differ on it. The CLI forecast cases already normalize it (`PLAN_HASH_NORM`);
/// this is the MCP-frame equivalent, by exact object-key name.
const PLAN_HASH_PLACEHOLDER: &str = "<PLAN_HASH>";

/// Recursively normalize a parsed MCP response: every string leaf gets the
/// same `VaultRoot` substitution `normalize::normalize_text` applies to raw
/// CLI stdout/stderr (each side stripped by its OWN vault-root spellings, so
/// a path a tool happens to echo never registers as a divergence), plus three
/// targeted substitutions by exact object-key name (see the module-level doc
/// for why it is safe to normalize, and why `protocolVersion` deliberately
/// is NOT): `serverInfo.version`, a plan's `generated_at` timestamp, and a
/// cascade report's root-dependent `plan_hash`. A `content` array's `text`
/// fields get one further, POSITION-SCOPED treatment — see
/// [`normalize_content_array`].
fn normalize_value(value: &Value, vault_roots: &[&Path]) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let normalized = if k == "serverInfo" && v.is_object() {
                    normalize_server_info(v)
                } else if k == "generated_at" && v.is_string() {
                    Value::String(GENERATED_AT_PLACEHOLDER.to_string())
                } else if k == "plan_hash" && v.is_string() {
                    Value::String(PLAN_HASH_PLACEHOLDER.to_string())
                } else if k == "content" && v.is_array() {
                    normalize_content_array(v, vault_roots)
                } else {
                    normalize_value(v, vault_roots)
                };
                out.insert(k.clone(), normalized);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| normalize_value(v, vault_roots))
                .collect(),
        ),
        Value::String(s) => Value::String(normalize::normalize_text(
            s,
            vault_roots,
            normalize::DEFAULT,
        )),
        other => other.clone(),
    }
}

/// Normalize a tool result's `content` array — rmcp's human-readable mirror of
/// `structuredContent`, `[{"type": "text", "text": <compact-JSON string>}]`.
/// Only a `content[].text` string gets the STRUCTURAL re-normalization: when it
/// parses as a JSON object/array, re-run [`normalize_value`] over the parsed
/// value and re-serialize, so the two renderings of the same root-dependent
/// keys (`plan_hash`, `generated_at` — embedded here as text, out of the
/// key-based rules' reach in `normalize_value`) end up consistent. Every other
/// string leaf in the response — including every other field of a `content`
/// item — gets only the plain text/vault-root strip, never a JSON-parse
/// reinterpretation; scoping the structural rewrite to this one field position
/// is deliberate, so a frontmatter value or error message that happens to
/// parse as JSON elsewhere in the tree is never restructured.
fn normalize_content_array(items: &Value, vault_roots: &[&Path]) -> Value {
    let Value::Array(items) = items else {
        return normalize_value(items, vault_roots);
    };
    Value::Array(
        items
            .iter()
            .map(|item| match item {
                Value::Object(map) => {
                    let mut out = serde_json::Map::with_capacity(map.len());
                    for (k, v) in map {
                        let normalized = if k == "text" {
                            normalize_content_text(v, vault_roots)
                        } else {
                            normalize_value(v, vault_roots)
                        };
                        out.insert(k.clone(), normalized);
                    }
                    Value::Object(out)
                }
                other => normalize_value(other, vault_roots),
            })
            .collect(),
    )
}

/// The one position in an MCP response where a string leaf is deliberately
/// re-parsed and structurally re-normalized — see [`normalize_content_array`].
fn normalize_content_text(v: &Value, vault_roots: &[&Path]) -> Value {
    match v {
        Value::String(s) => {
            let stripped = normalize::normalize_text(s, vault_roots, normalize::DEFAULT);
            match serde_json::from_str::<Value>(&stripped) {
                Ok(inner @ (Value::Object(_) | Value::Array(_))) => {
                    let normalized = normalize_value(&inner, vault_roots);
                    Value::String(serde_json::to_string(&normalized).unwrap_or(stripped))
                }
                _ => Value::String(stripped),
            }
        }
        other => normalize_value(other, vault_roots),
    }
}

/// Replace only `serverInfo.version` (the `name` field, `"norn"`, is a
/// stable identity worth comparing verbatim — NRN-187).
fn normalize_server_info(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = map.clone();
            if out.contains_key("version") {
                out.insert(
                    "version".to_string(),
                    Value::String(SERVER_VERSION_PLACEHOLDER.to_string()),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// The JSON-pointer-shaped path to the first point `a` and `b` differ, or
/// `None` if they are equal. Object keys are visited in sorted order (a
/// `serde_json::Map`'s default iteration, preserved here explicitly via
/// `BTreeMap`-like key union) so the result is deterministic regardless of
/// each side's own key-insertion order.
fn first_diff_pointer(a: &Value, b: &Value) -> Option<String> {
    let mut path = Vec::new();
    first_diff_pointer_inner(a, b, &mut path)
}

fn first_diff_pointer_inner(a: &Value, b: &Value, path: &mut Vec<String>) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Object(ao), Value::Object(bo)) => {
            let mut keys: std::collections::BTreeSet<&String> = ao.keys().collect();
            keys.extend(bo.keys());
            for k in keys {
                match (ao.get(k), bo.get(k)) {
                    (Some(av), Some(bv)) => {
                        path.push(k.clone());
                        if let Some(p) = first_diff_pointer_inner(av, bv, path) {
                            return Some(p);
                        }
                        path.pop();
                    }
                    _ => {
                        path.push(k.clone());
                        let p = pointer_string(path);
                        path.pop();
                        return Some(p);
                    }
                }
            }
            None
        }
        (Value::Array(aa), Value::Array(ba)) => {
            let len = aa.len().max(ba.len());
            for i in 0..len {
                match (aa.get(i), ba.get(i)) {
                    (Some(av), Some(bv)) => {
                        path.push(i.to_string());
                        if let Some(p) = first_diff_pointer_inner(av, bv, path) {
                            return Some(p);
                        }
                        path.pop();
                    }
                    _ => {
                        path.push(i.to_string());
                        let p = pointer_string(path);
                        path.pop();
                        return Some(p);
                    }
                }
            }
            None
        }
        _ => Some(pointer_string(path)),
    }
}

fn pointer_string(path: &[String]) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", path.join("/"))
    }
}

/// Drive `argv`/`frames` against both `oracle` and `candidate`, and compare
/// the paired responses frame-by-frame under normalization, plus the two
/// sides' exit codes and any extra/duplicate response ids (see the module
/// doc). Errors (via [`McpError`]) before ever reaching a result if either
/// side cannot produce a complete, parseable session.
pub fn run_case(
    argv: &[&str],
    frames: &[&str],
    oracle: DriveTarget,
    candidate: DriveTarget,
) -> Result<McpCaseResult, McpError> {
    let requests = request_metas(frames)?;

    let oracle_side = collect_responses(&oracle, argv, frames, &requests)?;
    let candidate_side = collect_responses(&candidate, argv, frames, &requests)?;

    let mut diffs = Vec::new();
    for req in &requests {
        let a = oracle_side
            .responses
            .get(&req.id_key)
            .expect("collect_responses already confirmed every expected id is present");
        let b = candidate_side
            .responses
            .get(&req.id_key)
            .expect("collect_responses already confirmed every expected id is present");
        let a_norm = normalize_value(a, oracle.roots);
        let b_norm = normalize_value(b, candidate.roots);
        if a_norm != b_norm {
            let pointer = first_diff_pointer(&a_norm, &b_norm).unwrap_or_else(|| "/".to_string());
            diffs.push(FrameDiff {
                id: req.id_display.clone(),
                method: req.method.clone(),
                pointer,
            });
        }
    }

    let extra_response_ids = oracle_side
        .extra_ids
        .iter()
        .map(|id| ExtraResponseId {
            label: oracle.label,
            id: id.clone(),
        })
        .chain(candidate_side.extra_ids.iter().map(|id| ExtraResponseId {
            label: candidate.label,
            id: id.clone(),
        }))
        .collect();

    let duplicate_response_ids = oracle_side
        .duplicate_ids
        .iter()
        .map(|(id, count)| DuplicateResponseId {
            label: oracle.label,
            id: id.clone(),
            count: *count,
        })
        .chain(
            candidate_side
                .duplicate_ids
                .iter()
                .map(|(id, count)| DuplicateResponseId {
                    label: candidate.label,
                    id: id.clone(),
                    count: *count,
                }),
        )
        .collect();

    let exit_mismatch = (oracle_side.exit_code != candidate_side.exit_code)
        .then_some((oracle_side.exit_code, candidate_side.exit_code));

    Ok(McpCaseResult {
        oracle_exit: oracle_side.exit_code,
        candidate_exit: candidate_side.exit_code,
        divergence: McpDivergence {
            exit_mismatch,
            diffs,
            extra_response_ids,
            duplicate_response_ids,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_metas_skips_frames_with_no_id() {
        let frames = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":"two","method":"tools/list"}"#,
        ];
        let metas = request_metas(&frames).unwrap();
        assert_eq!(metas.len(), 2, "the notification produces no RequestMeta");
        assert_eq!(metas[0].id_key, "1");
        assert_eq!(metas[0].method, "initialize");
        assert_eq!(metas[1].id_key, "\"two\"");
        assert_eq!(metas[1].method, "tools/list");
    }

    #[test]
    fn request_metas_rejects_malformed_json() {
        let frames = ["not json at all"];
        let err = request_metas(&frames).unwrap_err();
        assert!(matches!(err, McpError::MalformedCaseFrame { .. }));
    }

    #[test]
    fn numeric_and_string_ids_never_collide() {
        // id 1 (number) and id "1" (string) are DISTINCT JSON-RPC ids —
        // canonical JSON text as the key (not e.g. `to_string()` on the
        // Value, which would collapse `1` and `"1"` to the same text minus
        // quoting) keeps them apart.
        let frames = [
            r#"{"jsonrpc":"2.0","id":1,"method":"a"}"#,
            r#"{"jsonrpc":"2.0","id":"1","method":"b"}"#,
        ];
        let metas = request_metas(&frames).unwrap();
        assert_ne!(metas[0].id_key, metas[1].id_key);
    }

    #[test]
    fn normalize_value_replaces_server_version_but_leaves_protocol_version_untouched() {
        let value: Value = serde_json::from_str(
            r#"{"protocolVersion":"2024-11-05","serverInfo":{"name":"norn","version":"0.48.1"}}"#,
        )
        .unwrap();
        let normalized = normalize_value(&value, &[]);
        assert_eq!(
            normalized["protocolVersion"],
            Value::String("2024-11-05".to_string()),
            "protocolVersion is deliberately not normalized (F5) — a real \
             negotiated-revision divergence belongs in the ledger, not erased here"
        );
        assert_eq!(
            normalized["serverInfo"]["version"],
            Value::String(SERVER_VERSION_PLACEHOLDER.to_string())
        );
        assert_eq!(
            normalized["serverInfo"]["name"],
            Value::String("norn".to_string()),
            "the server name is a stable identity, not normalized away"
        );
    }

    #[test]
    fn normalize_value_replaces_plan_generated_at_timestamp() {
        // A repair/apply plan's wall-clock `generated_at` differs run-to-run
        // (self-check) and oracle-vs-rewrite; it is normalized by exact key name
        // exactly like `serverInfo.version`, so an otherwise byte-equal plan does
        // not diverge on the timestamp alone.
        let value: Value = serde_json::from_str(
            r#"{"result":{"structuredContent":{"report":{"plan":{"generated_at":"2026-07-23T12:00:00Z","operations":[]}}}}}"#,
        )
        .unwrap();
        let normalized = normalize_value(&value, &[]);
        assert_eq!(
            normalized["result"]["structuredContent"]["report"]["plan"]["generated_at"],
            Value::String(GENERATED_AT_PLACEHOLDER.to_string())
        );
    }

    #[test]
    fn normalize_value_replaces_root_dependent_plan_hash() {
        // A cascade report's `plan_hash` folds in the absolute `vault_root`, so
        // it always differs across fixtures in different temp dirs; the targeted
        // key normalization neutralizes it so an otherwise byte-equal apply report
        // does not diverge on the hash alone.
        let value: Value = serde_json::from_str(
            r#"{"result":{"structuredContent":{"report":{"plan_hash":"abc123","applied":0}}}}"#,
        )
        .unwrap();
        let normalized = normalize_value(&value, &[]);
        assert_eq!(
            normalized["result"]["structuredContent"]["report"]["plan_hash"],
            Value::String(PLAN_HASH_PLACEHOLDER.to_string())
        );
    }

    #[test]
    fn normalize_value_strips_vault_root_from_nested_strings() {
        let value: Value = serde_json::from_str(
            r#"{"result":{"documents":[{"path":"/tmp/vault-abc/notes/alpha.md"}]}}"#,
        )
        .unwrap();
        let root = Path::new("/tmp/vault-abc");
        let normalized = normalize_value(&value, &[root]);
        assert_eq!(
            normalized["result"]["documents"][0]["path"],
            Value::String("<VAULT>/notes/alpha.md".to_string())
        );
    }

    #[test]
    fn normalize_value_structurally_renorms_content_text_but_not_other_strings() {
        // `content[].text` is the ONE position that gets the structural
        // JSON-parse-and-renormalize treatment (rmcp's second, text-embedded
        // rendering of `structuredContent`) — a `generated_at`/`plan_hash` inside
        // it neutralizes exactly like the key-based rules above.
        let value: Value = serde_json::from_str(
            r#"{"result":{"content":[{"type":"text","text":"{\"plan_hash\":\"abc123\",\"generated_at\":\"2026-07-23T12:00:00Z\"}"}],"structuredContent":{"note":"{\"plan_hash\":\"abc123\"}"}}}"#,
        )
        .unwrap();
        let normalized = normalize_value(&value, &[]);
        let text = normalized["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(PLAN_HASH_PLACEHOLDER) && text.contains(GENERATED_AT_PLACEHOLDER),
            "content[].text is re-parsed and structurally normalized, got: {text}"
        );
        // A string leaf OUTSIDE `content[].text` that happens to parse as JSON
        // (here, `structuredContent.note`) is left as plain stripped text, never
        // reinterpreted — the scoping this test pins.
        assert_eq!(
            normalized["result"]["structuredContent"]["note"],
            Value::String(r#"{"plan_hash":"abc123"}"#.to_string()),
            "a JSON-looking string outside content[].text must not be \
             structurally re-normalized"
        );
    }

    #[test]
    fn first_diff_pointer_reports_the_object_key() {
        let a: Value = serde_json::from_str(r#"{"result":{"total":1}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"result":{"total":2}}"#).unwrap();
        assert_eq!(
            first_diff_pointer(&a, &b),
            Some("/result/total".to_string())
        );
    }

    #[test]
    fn first_diff_pointer_reports_the_array_index() {
        let a: Value = serde_json::from_str(r#"{"result":{"tools":[]}}"#).unwrap();
        let b: Value =
            serde_json::from_str(r#"{"result":{"tools":[{"name":"vault.other"}]}}"#).unwrap();
        assert_eq!(
            first_diff_pointer(&a, &b),
            Some("/result/tools/0".to_string())
        );
    }

    #[test]
    fn first_diff_pointer_none_when_equal() {
        let a: Value = serde_json::from_str(r#"{"result":{"total":1}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"result":{"total":1}}"#).unwrap();
        assert_eq!(first_diff_pointer(&a, &b), None);
    }

    fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn extra_and_duplicates_flags_an_unsolicited_id() {
        let expected: BTreeSet<&str> = ["1", "2"].into_iter().collect();
        let (extra, duplicate) =
            extra_and_duplicates(&counts(&[("1", 1), ("2", 1), ("99", 1)]), &expected);
        assert_eq!(extra, vec!["99".to_string()]);
        assert!(duplicate.is_empty());
    }

    #[test]
    fn extra_and_duplicates_flags_a_repeated_id() {
        let expected: BTreeSet<&str> = ["1", "2"].into_iter().collect();
        let (extra, duplicate) = extra_and_duplicates(&counts(&[("1", 1), ("2", 2)]), &expected);
        assert!(extra.is_empty());
        assert_eq!(duplicate, vec![("2".to_string(), 2)]);
    }

    #[test]
    fn extra_and_duplicates_empty_when_every_id_expected_and_seen_once() {
        let expected: BTreeSet<&str> = ["1", "2"].into_iter().collect();
        let (extra, duplicate) = extra_and_duplicates(&counts(&[("1", 1), ("2", 1)]), &expected);
        assert!(extra.is_empty());
        assert!(duplicate.is_empty());
    }

    #[test]
    fn extra_and_duplicates_a_repeated_unsolicited_id_is_both() {
        let expected: BTreeSet<&str> = ["1"].into_iter().collect();
        let (extra, duplicate) = extra_and_duplicates(&counts(&[("1", 1), ("99", 2)]), &expected);
        assert_eq!(extra, vec!["99".to_string()]);
        assert_eq!(duplicate, vec![("99".to_string(), 2)]);
    }

    #[test]
    fn mcp_divergence_is_empty_iff_every_axis_is_clean() {
        assert!(McpDivergence::default().is_empty());
        assert!(!McpDivergence {
            exit_mismatch: Some((0, 1)),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn mcp_case_result_matched_delegates_to_divergence() {
        let clean = McpCaseResult {
            oracle_exit: 0,
            candidate_exit: 0,
            divergence: McpDivergence::default(),
        };
        assert!(clean.matched());

        let dirty = McpCaseResult {
            oracle_exit: 0,
            candidate_exit: 1,
            divergence: McpDivergence {
                exit_mismatch: Some((0, 1)),
                ..Default::default()
            },
        };
        assert!(!dirty.matched());
    }
}
