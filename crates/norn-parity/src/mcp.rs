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
//! Both a response timeout and a premature EOF (the process exited before
//! answering every request frame) are runner errors ([`McpError`]), never
//! folded into a verdict — symmetrically for the oracle and the candidate
//! side. This mirrors the case-rot philosophy `run::RunError::
//! OracleExitMismatch` already applies to the oracle (a case whose oracle
//! side cannot be trusted to behave as declared cannot be compared
//! meaningfully) generalized to both sides: an MCP session that cannot
//! complete a full request/response handshake is an environment problem,
//! not a nuanced part of the parity comparison. In practice this only
//! matters for `--all` (never CI-gating, per `run::Mode`'s doc): every MCP
//! case is declared `ported: false` today (the rewrite's `mcp` subcommand
//! is `not_yet_ported`), so the default gated run — and CI — never drives
//! the candidate side at all.

use std::collections::BTreeMap;
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

/// The two volatile fields normalized before comparison, both inside an
/// `initialize` response: `serverInfo.version` (the crate version the
/// running binary was built at — differs release to release, and the
/// rewrite's own crate version will differ from the oracle's pin once it
/// serves MCP) and `protocolVersion` (the MCP spec revision the server
/// negotiated — a client-requested value the server may not echo verbatim,
/// see the module's implementation notes; normalizing it is what lets a
/// future rewrite that negotiates a different revision still compare
/// cleanly on everything else). Both substitutions are targeted by exact
/// object-key name, applied anywhere they occur in the response tree (not
/// just at a hardcoded path), matching `normalize::Normalization`'s
/// documented-substitution discipline.
const SERVER_VERSION_PLACEHOLDER: &str = "<MCP_SERVER_VERSION>";
const PROTOCOL_VERSION_PLACEHOLDER: &str = "<MCP_PROTOCOL_VERSION>";

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

/// The result of driving one MCP case against both sides.
pub struct McpCaseResult {
    /// The oracle side's process exit code — the case-rot guard
    /// (`run::RunError::OracleExitMismatch`) checks this against
    /// `Case::expect_oracle_exit` exactly like the non-MCP path.
    pub oracle_exit: i32,
    /// Every differing frame, in request declaration order. Empty means
    /// every paired response matched after normalization.
    pub diffs: Vec<FrameDiff>,
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

/// Drive `target` with `frames` (newline-joined verbatim onto stdin, exactly
/// as a real MCP client would write them) and return every response line
/// parsed as JSON, keyed by its canonical `id` text, plus the process exit
/// code. Errors (via [`McpError`]) if the process times out, is signaled, a
/// response line is not valid JSON, or it ends without answering every id
/// in `expected`.
fn collect_responses(
    target: &DriveTarget,
    argv: &[&str],
    frames: &[&str],
    expected: &[RequestMeta],
) -> Result<(BTreeMap<String, Value>, i32), McpError> {
    let payload: String = frames.iter().map(|f| format!("{f}\n")).collect();
    let raw = exec::run_argv_bounded(target.binary, argv, Some(&payload), target.vault, TIMEOUT)
        .map_err(|source| McpError::Exec {
            label: target.label,
            source,
        })?;

    let stdout = String::from_utf8_lossy(&raw.stdout);
    let mut responses = BTreeMap::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value =
            serde_json::from_str(line).map_err(|e| McpError::UnparsableResponse {
                label: target.label,
                raw: line.to_string(),
                message: e.to_string(),
            })?;
        if let Some(id) = value.get("id") {
            responses.insert(serde_json::to_string(id).unwrap_or_default(), value);
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
            received: responses.len(),
            missing_ids: missing,
        });
    }

    let exit_code = raw.exit_code.ok_or(McpError::Signaled {
        label: target.label,
    })?;
    Ok((responses, exit_code))
}

/// Recursively normalize a parsed MCP response: every string leaf gets the
/// same `VaultRoot` substitution `normalize::normalize_text` applies to raw
/// CLI stdout/stderr (each side stripped by its OWN vault-root spellings, so
/// a path a tool happens to echo never registers as a divergence), plus two
/// targeted substitutions by exact object-key name (see the module-level doc
/// for why each is safe to normalize): `protocolVersion` and
/// `serverInfo.version`.
fn normalize_value(value: &Value, vault_roots: &[&Path]) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let normalized = if k == "protocolVersion" && v.is_string() {
                    Value::String(PROTOCOL_VERSION_PLACEHOLDER.to_string())
                } else if k == "serverInfo" && v.is_object() {
                    normalize_server_info(v)
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
/// the paired responses frame-by-frame under normalization. Errors (via
/// [`McpError`]) before ever reaching a verdict if either side cannot
/// produce a complete, parseable session (see the module doc); otherwise
/// returns the oracle's exit code (for the caller's case-rot guard) plus
/// every differing frame.
pub fn run_case(
    argv: &[&str],
    frames: &[&str],
    oracle: DriveTarget,
    candidate: DriveTarget,
) -> Result<McpCaseResult, McpError> {
    let requests = request_metas(frames)?;

    let (oracle_responses, oracle_exit) = collect_responses(&oracle, argv, frames, &requests)?;
    let (candidate_responses, _candidate_exit) =
        collect_responses(&candidate, argv, frames, &requests)?;

    let mut diffs = Vec::new();
    for req in &requests {
        let a = oracle_responses
            .get(&req.id_key)
            .expect("collect_responses already confirmed every expected id is present");
        let b = candidate_responses
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

    Ok(McpCaseResult { oracle_exit, diffs })
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
    fn normalize_value_replaces_server_version_and_protocol_version() {
        let value: Value = serde_json::from_str(
            r#"{"protocolVersion":"2024-11-05","serverInfo":{"name":"norn","version":"0.48.1"}}"#,
        )
        .unwrap();
        let normalized = normalize_value(&value, &[]);
        assert_eq!(
            normalized["protocolVersion"],
            Value::String(PROTOCOL_VERSION_PLACEHOLDER.to_string())
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
}
