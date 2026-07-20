//! The mutation-verb request (`Params`) and response (`Report`) vocabulary —
//! `set` and `new` today, the pattern `edit`/`move`/`delete` follow.
//!
//! Pure serde types (no logic, no IO, no other norn crate). The owner receives a
//! `Params`, builds and — when `confirm` is set — applies a `MigrationPlan`
//! against its warm cache under the owner's single-writer serialization (ADR
//! 0011/0013/0017), and answers with the verb's `Report`; the CLI renders it.
//!
//! # The confirm/dry-run split
//!
//! A mutation carries `confirm` rather than a `dry_run` flag. `confirm = false`
//! is a forecast: the owner builds the plan and its report but writes nothing
//! (`applied = false`). `confirm = true` applies it. The CLI resolves confirm
//! from the mode ladder (`--dry-run`/`--yes`/`--format`/isatty) before send, so
//! the interactive preview→prompt→apply conversation stays client-side (ADR 0011
//! 2026-07-17 amendment) and the wire carries only the decided intent.
//!
//! # Refusal shape
//!
//! A clean pre-write refusal (a bad value, an owner-set mismatch, a missing
//! target) arrives as a report-shaped result with `outcome = refused` and a
//! coded [`CodedError`], never a bare protocol error — so a routed refusal
//! reconstructs as the exit-2 refusal the CLI renders, not a post-send-uncertain
//! (ADR 0011). Ordinary user rejections that never reached the plan (an unknown
//! rule, a malformed assignment) ride the shared owner `Rejected` frame.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The apply outcome of a mutation — mirrors the core `ApplyOutcome` on the
/// wire. `applied`/dry-run success is `Applied`; a clean pre-write decline is
/// `Refused` (paired with a [`CodedError`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOutcome {
    Applied,
    Refused,
}

/// A coded, report-shaped mutation error — the wire twin of the core
/// `ApplyError` envelope. `path` is present when the error is scoped to one
/// document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodedError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A non-fatal mutation warning carried in the report (unknown field, an
/// unresolved/ambiguous wikilink, a `--force` schema bypass, a title ignored by
/// an explicit-path create). `code` is a stable kebab discriminator, `field` the
/// affected frontmatter key when the warning is scoped to one, and `message` the
/// operator-facing detail.
///
/// # DELIBERATE DIVERGENCE from the donor (reason: discovered-inconsistency)
///
/// The donor's per-kind warning JSON is internally inconsistent — `set` tagged
/// its unknown-field warning `kind: "unknown_field"` (snake_case) while `new`
/// tagged the same class `"unknown-field"` (kebab), and different kinds carried
/// different member sets (`{kind, field, message}` vs `{kind, title}`). This is
/// donor slop, not a contract worth replicating. The rewrite emits ONE unified
/// envelope for every warning kind — `{ code: <kebab>, field?: <key>, message }`
/// — so a consumer parses one shape. A parity ledger entry (reason
/// discovered-inconsistency, unified) attaches the moment a warning-bearing
/// `--format json` parity case lands; the records short forms are donor-faithful
/// and already covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationWarning {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

// ── set ──────────────────────────────────────────────────────────────────────

/// A `set` request: the target plus the field mutations. `fields` is the merged
/// `--field`/positional `KEY=VALUE` token list (CLI-merged, order-preserving so
/// duplicate keys accumulate into an array); `field_json` carries `KEY=JSON`
/// tokens; `push`/`pop` are list-field mutations; `remove` deletes keys. `body`
/// carries the `--body-from-stdin` bytes, read CLI-side (the wire speaks the
/// body, not a stdin handle). `force` bypasses schema enforcement; `confirm`
/// applies (else forecast).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetParams {
    pub target: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub field_json: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub push: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub force: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub confirm: bool,
}

/// A `set` response — the donor `SetReport` (schema v2) shape. `--format json`
/// serializes this directly, so field order is the JSON contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetReport {
    pub schema_version: u32,
    /// The telemetry trace id. Empty on EVERY outcome — forecast, refusal, AND
    /// applied — until the durable telemetry store lands with the audit verb; the
    /// owner's discard sink deliberately does not mint a placeholder on apply, so
    /// the contract holds one shape and confirmed-apply parity needs no trace
    /// normalization. Donor-parity ids return when telemetry ports. Serialized
    /// right after `schema_version` to match the donor's compact struct-order JSON.
    #[serde(default)]
    pub trace_id: String,
    pub operation: String,
    pub target: String,
    /// Always serialized — the donor emits `"frontmatter_changes":[]` on a no-op
    /// (e.g. a pop that matched nothing), never omits the key.
    #[serde(default)]
    pub frontmatter_changes: Vec<FrontmatterChange>,
    pub body_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes_new: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes_old: Option<usize>,
    pub applied: bool,
    pub outcome: MutationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CodedError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<MutationWarning>,
}

/// One frontmatter change in a `set` report. `op` is the normalized action
/// (`set` / `remove` / `push` / `pop`); `old`/`new`/`value` are the JSON values
/// involved (which are populated depends on `op`); `found` reports whether a
/// `pop` located its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontmatterChange {
    pub op: String,
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<bool>,
}

// ── new ──────────────────────────────────────────────────────────────────────

/// A `new` request: the creation-mode inputs plus overrides. Exactly one of
/// `path` (Mode A, explicit path) or `as_rule` (Mode B, by named rule) is set;
/// neither → Mode C (inbox, requires `title`). `vars` are `KEY=VALUE` template
/// variables; `fields`/`field_json` are frontmatter overrides; `body` carries
/// `--body-from-stdin` bytes. `parents` auto-creates parent dirs; `force`
/// overwrites an existing destination and skips coercion; `confirm` applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub field_json: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub parents: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub force: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub confirm: bool,
}

/// A `new` response — the donor `NewReport` (schema v2) shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReport {
    pub schema_version: u32,
    /// Empty until durable telemetry lands (see [`SetReport::trace_id`]).
    #[serde(default)]
    pub trace_id: String,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub applied: bool,
    pub outcome: MutationOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontmatter_created: Vec<FrontmatterCreated>,
    pub body_bytes: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<MutationWarning>,
    /// The `{{seq}}`-predicted path (NRN-101), when the target carried a `{{seq}}`
    /// token and this is a forecast (the real id is allocated at apply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CodedError>,
}

/// One frontmatter field a `new` created, with the rule/source that supplied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontmatterCreated {
    pub field: String,
    pub value: Value,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

// ── edit ─────────────────────────────────────────────────────────────────────

/// An `edit` request: the target plus the CLI-resolved ops. `edits` is the
/// serialized JSON array of section-edit ops (`[{"op":"str_replace",…}, …]`),
/// resolved CLI-side (sugar-desugared, or read from `--edits-json`/`--ops-file`/
/// stdin) and re-serialized onto the wire so the owner's transform runs on the
/// SAME resolved array. Carried as the JSON text (not a typed `Vec`) so the
/// param stays a pure-serde `Eq` type — the typed op vocabulary
/// (`norn_core::edit::ops::EditOp`) is a `norn-core` engine type `norn-wire` may
/// not name. `expected_hash` is the opt-in compare-and-swap precondition (the
/// document's full-content blake3 hex); `confirm` applies (else forecast).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditParams {
    pub target: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub edits: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub confirm: bool,
}

/// An `edit` response — the donor `EditReport` (schema v1) shape. Same outer
/// envelope as [`SetReport`] with an `edits` array (one entry per applied op)
/// replacing `frontmatter_changes`. `--format json` serializes this directly, so
/// the field order below IS the JSON contract (donor struct order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditReport {
    pub schema_version: u32,
    /// Empty on EVERY outcome until durable telemetry lands (see
    /// [`SetReport::trace_id`]).
    #[serde(default)]
    pub trace_id: String,
    pub operation: String,
    pub target: String,
    pub edits: Vec<EditChange>,
    pub body_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes_old: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes_new: Option<usize>,
    pub applied: bool,
    pub outcome: MutationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CodedError>,
}

/// One applied edit op in an `edit` report. `op` is the op discriminant
/// (`str_replace` / `append_to_section` / …), `anchor` its human-readable anchor
/// summary, `occurrences` the match count for a `str_replace` (absent for a
/// structural op), and `applied` whether the batch was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditChange {
    pub op: String,
    pub anchor: String,
    pub matched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<usize>,
    pub applied: bool,
}

/// The `edit` report schema version (donor `edit::report::SCHEMA_VERSION`).
pub const EDIT_REPORT_SCHEMA_VERSION: u32 = 1;

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_set_params_serialize_to_target_only() {
        let p = SetParams {
            target: "a.md".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({ "target": "a.md" })
        );
    }

    // Pins the PD-111 unified warning envelope's exact JSON keys. A parity
    // diverged-verdict is keyed by case id and cannot see WHICH divergence the
    // candidate emitted — this is the independent positive assertion.
    #[test]
    fn warning_envelope_serializes_to_the_unified_shape() {
        let w = MutationWarning {
            code: "unknown-field".into(),
            field: Some("status".into()),
            message: "field 'status' not declared in schema".into(),
        };
        assert_eq!(
            serde_json::to_value(&w).unwrap(),
            json!({
                "code": "unknown-field",
                "field": "status",
                "message": "field 'status' not declared in schema"
            })
        );
    }

    #[test]
    fn set_report_round_trips() {
        let r = SetReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "set".into(),
            target: "a.md".into(),
            frontmatter_changes: vec![FrontmatterChange {
                op: "set".into(),
                field: "status".into(),
                old: Some(json!("draft")),
                new: Some(json!("done")),
                value: None,
                found: None,
            }],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied: true,
            outcome: MutationOutcome::Applied,
            error: None,
            warnings: vec![],
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: SetReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn outcome_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(MutationOutcome::Refused).unwrap(),
            json!("refused")
        );
    }

    #[test]
    fn new_params_pick_one_mode() {
        let p = NewParams {
            as_rule: Some("task".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({ "as_rule": "task" })
        );
    }

    #[test]
    fn new_report_round_trips() {
        let r = NewReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "new".into(),
            path: Some("notes/a.md".into()),
            applied: false,
            outcome: MutationOutcome::Applied,
            frontmatter_created: vec![FrontmatterCreated {
                field: "type".into(),
                value: json!("note"),
                source: "default".into(),
                rule: Some("notes".into()),
            }],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: NewReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
