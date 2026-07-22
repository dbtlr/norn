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
use norn_wire::ApplyReport;
use norn_wire::{
    CountReport, DescribeReport, EditReport, FindReport, GetReport, NewReport, SetReport,
    ValidateReport,
};

use super::format::FormatChoice;

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
    /// `repair`: the findings-derived `MigrationPlan` (bare summary or `--plan`).
    Repair(RepairView),
    /// `vault list`: the registered vaults.
    VaultList(VaultListView),
    /// `set`: the frontmatter change report (forecast / applied / refused).
    Set(SetMutationView),
    /// `new`: the document-creation report (forecast / applied / refused).
    New(NewMutationView),
    /// `edit`: the body-edit report (forecast / applied / refused).
    Edit(EditMutationView),
    /// `move`: the cascade `ApplyReport` (forecast / applied / refused).
    Move(MoveMutationView),
    /// `delete`: the cascade `ApplyReport` (forecast / applied / refused).
    Delete(DeleteMutationView),
    /// `rewrite-wikilink`: the cascade `ApplyReport` (forecast / applied / refused).
    RewriteWikilink(RewriteWikilinkView),
    /// `apply`: the executed plan's `ApplyReport` (forecast / applied / refused).
    Apply(ApplyMutationView),
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
    pub format: FormatChoice,
}

/// `get`'s renderable report plus projection parameters.
pub struct GetView {
    pub report: GetReport,
    pub cols: Vec<String>,
    pub sections: Vec<String>,
    /// The `--sort` field, if any (NRN-374: drives the unknown-sort-field
    /// warning, the `get` counterpart to `FindView::sort_field`).
    pub sort_field: Option<String>,
    pub format: FormatChoice,
}

/// `count`'s renderable report.
pub struct CountView {
    pub report: CountReport,
    pub format: FormatChoice,
}

/// `describe`'s renderable report.
pub struct DescribeView {
    pub report: DescribeReport,
    /// The `--by` fields requested, raw (un-normalized) (NRN-374: drives the
    /// unknown-`--by`-field warning — `report.data.fields` already carries the
    /// normalized, occurrence-filtered set to compare against).
    pub by: Vec<String>,
    pub format: FormatChoice,
}

/// `validate`'s renderable report plus the `--summary` view toggle. The findings
/// arrive pre-filtered from the owner; the renderer projects them into records /
/// summary / json / jsonl / paths.
pub struct ValidateView {
    pub report: ValidateReport,
    /// `--summary`: emit grouped counts instead of per-finding blocks (records)
    /// or the full findings array (json).
    pub summary: bool,
    pub format: FormatChoice,
}

/// `repair`'s renderable report plus the surface knobs. Bare `norn repair`
/// prints the findings summary; `--plan` emits the `MigrationPlan` in the
/// requested `format` (report / json / paths) and/or writes it to `--out`. The
/// exit code is `report.has_diagnostic_errors` for both (the donor's
/// `exit_code_for`), independent of the triage filters.
pub struct RepairView {
    pub report: norn_wire::RepairReport,
    /// `--plan`: emit the `MigrationPlan` instead of the bare findings summary.
    pub plan: bool,
    /// `--format` for `--plan` (report / json / paths); `None` defaults to report
    /// on a tty, json when piped.
    pub format: Option<crate::cli::RepairPlanFormat>,
    /// `--out`: write the JSON plan to this path (independent of `--format`;
    /// stdout stays silent when `--out` is set without `--format`).
    pub out: Option<std::path::PathBuf>,
    /// Active triage/confidence/skip-reason flags, for the report-format
    /// apply-guidance command lines.
    pub filter_flags: Vec<String>,
}

/// `vault list`'s registered vaults.
pub struct VaultListView {
    pub vaults: Vec<RegisteredVault>,
    pub format: FormatChoice,
}

/// `set`'s renderable report. Only `records` and `json` are valid; the renderer
/// maps a refused report (`outcome = refused`) to exit 2 with the coded error
/// envelope, and an applied/forecast report to exit 0.
pub struct SetMutationView {
    pub report: SetReport,
    pub format: FormatChoice,
}

/// `new`'s renderable report. Same records/json + exit-code contract as
/// [`SetMutationView`].
pub struct NewMutationView {
    pub report: NewReport,
    pub format: FormatChoice,
}

/// `edit`'s renderable report. Only `records` and `json` are valid. A refused
/// report (`outcome = refused`) renders as `error: <message>` on stderr at exit
/// 2 for BOTH formats (the donor's pre-existing format-independent refusal
/// asymmetry — unlike `set`/`new`, which emit a structured JSON refusal); an
/// applied/forecast report renders at exit 0.
pub struct EditMutationView {
    pub report: EditReport,
    pub format: FormatChoice,
}

/// `move`'s renderable report. The cascade verbs render the shared
/// [`ApplyReport`] the donor emits: `--format json` is its pretty serialization;
/// records is the donor's single/folder move summary. A refused report renders
/// the coded error envelope (json) or `error: <msg>` (records) and exits 2.
pub struct MoveMutationView {
    pub report: ApplyReport,
    /// The raw source argument, echoed in the records summary.
    pub src: String,
    /// The raw destination argument, echoed in the records summary.
    pub dst: String,
    pub format: FormatChoice,
}

/// `delete`'s renderable report.
pub struct DeleteMutationView {
    pub report: ApplyReport,
    /// The raw target argument, echoed in the records summary.
    pub doc: String,
    pub format: FormatChoice,
}

/// `rewrite-wikilink`'s renderable report.
pub struct RewriteWikilinkView {
    pub report: ApplyReport,
    pub old: String,
    pub new: String,
    pub format: FormatChoice,
    /// `--out`: write the (always-JSON) report to this file, silencing stdout.
    pub out: Option<String>,
}

/// `apply`'s renderable report. Unlike the other cascade verbs, `apply` renders
/// the donor's generic apply-report summary (`apply <status>` + counts +
/// preconditions + per-op + warnings): `--format json` is the report's pretty
/// serialization; records is that summary. A refused report renders the coded
/// error envelope (envelope-only refusals) or, for an owner-set precondition
/// mismatch, the full summary with the preconditions block — both at exit 2.
pub struct ApplyMutationView {
    pub report: ApplyReport,
    pub format: FormatChoice,
    /// `--out`: write the (always-JSON) report to this file, silencing stdout.
    pub out: Option<String>,
}
