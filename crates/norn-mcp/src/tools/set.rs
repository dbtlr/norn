//! `vault.set` — update one document's frontmatter (and optionally its body),
//! schema-aware. DRY-RUN by default; `confirm: true` writes.
//!
//! The param struct mirrors `norn set`'s mutation flags; the handler routes to the
//! owner and wraps the wire [`SetReport`] in the donor's `{ report: … }` envelope,
//! deriving the MCP `isError` bit from the report's outcome (a confirmed refusal /
//! failure is `isError: true`; a dry-run forecast never is).

use norn_wire::{MutationOutcome, SetParams as WireSetParams, SetReport};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.set`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SetParams {
    /// Target document (stem or path), as `norn set` accepts.
    pub target: String,

    /// Frontmatter fields to set, as raw `KEY=JSON` tokens, repeatable. Applied
    /// in order and fed verbatim into `norn set --field-json KEY=JSON`: each value
    /// is JSON-parsed and schema-validated. A key repeated across tokens
    /// accumulates into an array. Empty list = no frontmatter change.
    #[serde(default)]
    pub field_json: Vec<String>,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable. The value is
    /// string-coerced against the schema exactly like `norn set --field KEY=VALUE`.
    #[serde(default)]
    pub field: Vec<String>,

    /// Append to a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable — fed verbatim into `norn set --push KEY=VALUE`.
    #[serde(default)]
    pub push: Vec<String>,

    /// Remove from a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable — fed verbatim into `norn set --pop KEY=VALUE`.
    #[serde(default)]
    pub pop: Vec<String>,

    /// Frontmatter keys to remove entirely. Silent no-op for missing keys.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Wholesale body replacement — the MCP analogue of `norn set
    /// --body-from-stdin`. Absent = body unchanged.
    #[serde(default)]
    pub body: Option<String>,

    /// Bypass schema enforcement (type validation + required-field protection),
    /// mirroring `norn set --force`.
    #[serde(default)]
    pub force: bool,

    /// Apply the mutation. **Defaults to `false` (dry-run): the call returns the
    /// planned change with `applied = false` and writes nothing.** Pass `true` to
    /// acquire the vault mutation lock and write the change to disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.set` — the wire report wrapped as a generic
/// `Value` under `report` (the report carries a path type with no `JsonSchema`
/// impl), giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SetOutput {
    /// The full `norn set` report: the applied flag, frontmatter changes, body
    /// change, outcome, and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: SetParams) -> WireSetParams {
    WireSetParams {
        target: p.target,
        fields: p.field,
        field_json: p.field_json,
        push: p.push,
        pop: p.pop,
        remove: p.remove,
        body: p.body,
        force: p.force,
        confirm: p.confirm,
    }
}

/// Wrap the wire report in the MCP envelope. `isError` is derived from the same
/// outcome→exit vocabulary the donor uses: a CONFIRM apply whose outcome is not
/// clean (`refused`) is `isError: true`; a dry-run forecast never is.
pub(crate) fn envelope(confirm: bool, report: SetReport) -> MutationResult<SetOutput> {
    let is_error = confirm && matches!(report.outcome, MutationOutcome::Refused);
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_flag(SetOutput { report: value }, is_error)
}
