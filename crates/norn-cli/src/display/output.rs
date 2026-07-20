//! The value a command's `run` returns for the display layer to render (NRN-370).
//!
//! A verb never writes stdout. It resolves its report, folds it (with the
//! presentation parameters the renderer needs — the `--col` projection, the
//! explicit `--format`, the verb's tty/piped default pair) into an [`Output`], and
//! returns it. The single [`emit`](crate::display::emit) call in the dispatch loop
//! turns that `Output` into bytes. A verb that hits a user error returns a
//! [`Diagnostic`](crate::display::Diagnostic) instead, rendered on stderr through
//! the one presenter path.

use norn_config::RegisteredVault;
use norn_wire::{CountReport, DescribeReport, FindReport, GetReport, ValidateReport};

use super::format::{Format, FormatSpec};

/// A command's renderable outcome. Each report-bearing variant wraps the wire
/// report unchanged and carries the presentation parameters the renderer needs;
/// [`emit`](crate::display::emit) resolves the format, palette, and exit code.
pub enum Output {
    /// `find`: the paged document report plus its `--col` projection.
    Find(FindView),
    /// `get`: the resolved records plus `--col` / `--section` projection.
    Get(GetView),
    /// `count`: the total / grouped distribution.
    Count(CountView),
    /// `describe`: the structure + optional data summary.
    Describe(DescribeView),
    /// `validate`: the findings, summary body, and run counts.
    Validate(ValidateView),
    /// `vault list`: the registered vaults.
    VaultList(VaultListView),
    /// A single stdout confirmation line, written verbatim plus a newline, exit 0
    /// — `vault` register / set / unregister / no-changes.
    Line(String),
    /// A usage help page written to stderr, exit 2 — `find`'s bare no-predicate
    /// gate (a full-vault dump is almost always a mistake).
    Usage(Vec<u8>),
}

/// `find`'s renderable report plus projection parameters.
pub struct FindView {
    pub report: FindReport,
    pub cols: Vec<String>,
    pub all_cols: bool,
    /// The `--sort` field, if any — the record renderer highlights a matching
    /// frontmatter row.
    pub sort_field: Option<String>,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}

/// `get`'s renderable report plus projection parameters.
pub struct GetView {
    pub report: GetReport,
    pub cols: Vec<String>,
    pub sections: Vec<String>,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}

/// `count`'s renderable report.
pub struct CountView {
    pub report: CountReport,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}

/// `describe`'s renderable report.
pub struct DescribeView {
    pub report: DescribeReport,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}

/// `validate`'s renderable report plus the `--summary` view toggle. The findings
/// arrive pre-filtered from the owner; the renderer projects them into records /
/// summary / json / jsonl / paths.
pub struct ValidateView {
    pub report: ValidateReport,
    /// `--summary`: emit grouped counts instead of per-finding blocks (records)
    /// or the full findings array (json).
    pub summary: bool,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}

/// `vault list`'s registered vaults.
pub struct VaultListView {
    pub vaults: Vec<RegisteredVault>,
    pub explicit: Option<Format>,
    pub spec: FormatSpec,
}
