use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;

use crate::frontmatter::{
    extract_frontmatter, render_key, serialize_array_block_field, serialize_value_preserving_style,
    top_level_property_spans, QuoteError, ValueStyle,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::standards::repair::warnings::PlanWarning;
use crate::standards::repair::{
    PlannedChange, RepairPlan, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use crate::standards::summary::Summary;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("unsupported repair plan schema version: expected {expected}, got {got}; regenerate with `norn repair --plan`")]
    UnsupportedSchemaVersion { expected: u32, got: u32 },

    #[error("repair plan vault root does not match effective cwd: plan {plan}, cwd {cwd}")]
    VaultRootMismatch { plan: Utf8PathBuf, cwd: Utf8PathBuf },

    #[error("repair plan targets a document not in the index: {path}")]
    UnknownPath { path: Utf8PathBuf },

    #[error("stale repair plan for {path}: expected hash {expected}, found {actual}; regenerate with `norn repair --plan`")]
    StaleDocumentHash {
        path: Utf8PathBuf,
        expected: String,
        actual: String,
    },

    #[error("repair plan contains conflicting changes for {path} field {field}")]
    ConflictingFieldChange { path: Utf8PathBuf, field: String },

    #[error("repair plan contains conflicting document hash preconditions for {path}")]
    ConflictingHashes { path: Utf8PathBuf },

    #[error("stale repair plan for {path} field {field}: expected {expected}, found {actual}; regenerate with `norn repair --plan`")]
    ExpectedOldValueMismatch {
        path: Utf8PathBuf,
        field: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported repair operation for {path}: {operation}")]
    UnsupportedOperation {
        path: Utf8PathBuf,
        operation: String,
    },

    #[error("cannot minimal-edit frontmatter for {path}: {reason}")]
    CannotMinimalEdit { path: Utf8PathBuf, reason: String },

    #[error("frontmatter parse failed for {path}: {message}")]
    FrontmatterParseFailed { path: Utf8PathBuf, message: String },

    #[error("refusing write for {path}: post-edit frontmatter failed verification ({detail})")]
    PostImageVerificationFailed { path: Utf8PathBuf, detail: String },

    #[error("edit op failed for {path}: {message}")]
    EditFailed {
        path: camino::Utf8PathBuf,
        message: String,
    },
    #[error("set_frontmatter change missing new_value for {path}")]
    MissingNewValue { path: Utf8PathBuf },

    #[error(
        "field '{field}' already present in {path}; add_frontmatter refuses to overwrite (use set_frontmatter)"
    )]
    FieldAlreadyPresent { path: Utf8PathBuf, field: String },

    #[error("move source missing in filesystem: {path}")]
    MoveSourceMissing { path: Utf8PathBuf },

    #[error("move source is a symlink, not a regular file: {path}")]
    MoveSourceIsSymlink { path: Utf8PathBuf },

    #[error("move destination already exists: {destination}")]
    MoveDestinationExists { destination: Utf8PathBuf },

    #[error("delete source missing: {path}")]
    DeleteSourceMissing { path: Utf8PathBuf },

    #[error("delete source is a symlink, not a regular file: {path}")]
    DeleteSourceIsSymlink { path: Utf8PathBuf },

    #[error("repair plan edits {path} after an earlier {earlier_op} of the same path (change '{earlier_change_id}'); a content op cannot follow a delete/move of its target in the same plan — reorder the edit before the {earlier_op}")]
    ContentOpAfterVacate {
        path: Utf8PathBuf,
        earlier_op: String,
        earlier_change_id: String,
    },

    #[error(transparent)]
    Containment(#[from] ContainmentError),
}

impl ApplyError {
    /// A stable, machine-branchable KEBAB code for this failure variant (NRN-150).
    ///
    /// This is the contract an agent branches on — retry-vs-reread-vs-giveup —
    /// WITHOUT string-matching the human-facing `Display` prose. Codes are
    /// canonically kebab-case per norn's three-form-identity principle (code
    /// VALUES are kebab everywhere; JSON/MCP field KEYS stay snake_case). Never
    /// rename an existing code without a CHANGELOG breaking-change entry.
    pub fn code(&self) -> &'static str {
        match self {
            ApplyError::UnsupportedSchemaVersion { .. } => "unsupported-schema-version",
            ApplyError::VaultRootMismatch { .. } => "vault-root-mismatch",
            ApplyError::UnknownPath { .. } => "unknown-path",
            ApplyError::StaleDocumentHash { .. } => "stale-document-hash",
            ApplyError::ConflictingFieldChange { .. } => "conflicting-field-change",
            ApplyError::ConflictingHashes { .. } => "conflicting-hashes",
            ApplyError::ExpectedOldValueMismatch { .. } => "expected-old-value-mismatch",
            ApplyError::UnsupportedOperation { .. } => "unsupported-operation",
            ApplyError::CannotMinimalEdit { .. } => "cannot-minimal-edit",
            ApplyError::FrontmatterParseFailed { .. } => "frontmatter-parse-failed",
            ApplyError::PostImageVerificationFailed { .. } => "post-image-verification-failed",
            ApplyError::EditFailed { .. } => "edit-failed",
            ApplyError::MissingNewValue { .. } => "missing-new-value",
            ApplyError::FieldAlreadyPresent { .. } => "field-already-present",
            ApplyError::MoveSourceMissing { .. } => "move-source-missing",
            ApplyError::MoveSourceIsSymlink { .. } => "move-source-is-symlink",
            ApplyError::MoveDestinationExists { .. } => "move-destination-exists",
            ApplyError::DeleteSourceMissing { .. } => "delete-source-missing",
            ApplyError::DeleteSourceIsSymlink { .. } => "delete-source-is-symlink",
            ApplyError::ContentOpAfterVacate { .. } => "content-op-after-vacate",
            ApplyError::Containment(inner) => inner.code(),
        }
    }

    /// The vault-relative path this failure is about, when the variant carries one.
    /// Feeds the `path` field of the structured error envelope.
    pub fn path(&self) -> Option<&Utf8Path> {
        match self {
            ApplyError::UnknownPath { path }
            | ApplyError::StaleDocumentHash { path, .. }
            | ApplyError::ConflictingFieldChange { path, .. }
            | ApplyError::ConflictingHashes { path }
            | ApplyError::ExpectedOldValueMismatch { path, .. }
            | ApplyError::UnsupportedOperation { path, .. }
            | ApplyError::CannotMinimalEdit { path, .. }
            | ApplyError::FrontmatterParseFailed { path, .. }
            | ApplyError::PostImageVerificationFailed { path, .. }
            | ApplyError::EditFailed { path, .. }
            | ApplyError::MissingNewValue { path }
            | ApplyError::FieldAlreadyPresent { path, .. }
            | ApplyError::MoveSourceMissing { path }
            | ApplyError::MoveSourceIsSymlink { path }
            | ApplyError::DeleteSourceMissing { path }
            | ApplyError::DeleteSourceIsSymlink { path }
            | ApplyError::ContentOpAfterVacate { path, .. } => Some(path.as_path()),
            ApplyError::MoveDestinationExists { destination } => Some(destination.as_path()),
            ApplyError::Containment(inner) => Some(inner.target()),
            ApplyError::UnsupportedSchemaVersion { .. } | ApplyError::VaultRootMismatch { .. } => {
                None
            }
        }
    }

    /// A per-variant HINT that this failure *class* is typically raised at a
    /// pre-write validation site. **It is NOT the refused-vs-failed gate** — that
    /// gate is the runtime write-state fact threaded through the applier
    /// (`apply_repair_plan_with_context`'s `wrote_any`), because a single variant
    /// is structurally ambiguous: `stale-document-hash` / `unknown-path` are
    /// raised from BOTH the pre-write Phase-A1 content CAS AND the Phase-B delete
    /// pass (which runs AFTER Phase A2 has already written other ops in a mixed
    /// plan). Keying the byte-identical-refusal decision off this flag produced
    /// the NRN-150/183 byte-identity lie (a `refused` report over a mutated
    /// vault); the applier now decides on whether a write actually landed, so this
    /// method remains only as a coarse taxonomy hint and no longer gates output.
    pub fn is_precondition(&self) -> bool {
        match self {
            ApplyError::UnsupportedSchemaVersion { .. }
            | ApplyError::VaultRootMismatch { .. }
            | ApplyError::UnknownPath { .. }
            | ApplyError::StaleDocumentHash { .. }
            | ApplyError::ConflictingFieldChange { .. }
            | ApplyError::ConflictingHashes { .. }
            | ApplyError::ExpectedOldValueMismatch { .. }
            | ApplyError::UnsupportedOperation { .. }
            | ApplyError::CannotMinimalEdit { .. }
            | ApplyError::FrontmatterParseFailed { .. }
            | ApplyError::PostImageVerificationFailed { .. }
            | ApplyError::EditFailed { .. }
            | ApplyError::FieldAlreadyPresent { .. }
            | ApplyError::ContentOpAfterVacate { .. }
            | ApplyError::Containment(_) => true,
            ApplyError::MissingNewValue { .. }
            | ApplyError::MoveSourceMissing { .. }
            | ApplyError::MoveSourceIsSymlink { .. }
            | ApplyError::MoveDestinationExists { .. }
            | ApplyError::DeleteSourceMissing { .. }
            | ApplyError::DeleteSourceIsSymlink { .. } => false,
        }
    }
}

/// (NRN-145) Refusal from the shared vault-root containment gate. A vault is
/// self-contained: an op target that resolves outside the vault root — via an
/// absolute path, `..` parent-traversal, or a directory symlinked out of the
/// vault — is unsupported and refused before any write.
#[derive(Debug, Error)]
pub enum ContainmentError {
    #[error("refusing to operate on '{target}': absolute paths are not vault-relative; the vault root is self-contained and paths outside it are unsupported")]
    AbsolutePath { target: Utf8PathBuf },

    #[error("refusing to operate on '{target}': a '..' component escapes the vault root; the vault is self-contained and paths outside it are unsupported")]
    ParentTraversal { target: Utf8PathBuf },

    #[error("refusing to operate on '{target}': path resolves outside the vault root; the vault is self-contained and symlinks out of the vault are unsupported")]
    EscapesVault { target: Utf8PathBuf },

    #[error("refusing to operate on '{target}': cannot verify vault-root containment ({detail})")]
    Unresolvable { target: Utf8PathBuf, detail: String },
}

impl ContainmentError {
    /// Stable KEBAB code per variant (NRN-150). Namespaced under `containment-`
    /// so a consumer can branch on the whole class with a prefix match.
    pub fn code(&self) -> &'static str {
        match self {
            ContainmentError::AbsolutePath { .. } => "containment-absolute-path",
            ContainmentError::ParentTraversal { .. } => "containment-parent-traversal",
            ContainmentError::EscapesVault { .. } => "containment-escapes-vault",
            ContainmentError::Unresolvable { .. } => "containment-unresolvable",
        }
    }

    /// The offending target path (always present on a containment refusal).
    pub fn target(&self) -> &Utf8Path {
        match self {
            ContainmentError::AbsolutePath { target }
            | ContainmentError::ParentTraversal { target }
            | ContainmentError::EscapesVault { target }
            | ContainmentError::Unresolvable { target, .. } => target.as_path(),
        }
    }
}

/// (NRN-145) Refuse an op target that would resolve outside the vault root. The
/// shared containment gate for the mutation stack (create / move / delete / edit
/// / backlink-cascade targets) and `norn new`'s path validation — one
/// implementation, no parallel logic.
///
/// The check is lexical first (cheap): an absolute path or any `..` component is
/// refused up front. Then:
///
/// - If the target ALREADY EXISTS (a backlink-cascade rewrite source, a
///   move/delete source — these always exist), the target ITSELF is
///   canonicalized and confirmed prefix-contained in `canonical_root`. This is
///   the F1 fix (NRN-145 follow-up): a symlink FILE inside the vault whose
///   PARENT is legitimately in-vault but which itself resolves outside would
///   pass a parent-only check, then a bare `fs::write`/`fs::read_to_string`
///   would follow it straight through to the outside file. Canonicalizing the
///   target closes that.
/// - Otherwise (a create/move destination that does not yet exist, so there is
///   nothing to canonicalize at the target itself), the op target's PARENT
///   directory is resolved and its nearest EXISTING ancestor is canonicalized
///   and confirmed prefix-contained in `canonical_root`. Canonicalizing the
///   parent resolves a directory symlinked out of the vault — the case the
///   lexical check alone bypasses. Canonicalizing the nearest existing
///   ancestor means `-p`/`--parents` creation of a not-yet-existing subtree
///   cannot be used to sidestep the gate.
///
/// `canonical_root` is the caller's canonicalization of the vault root; it is
/// canonicalized ONCE per apply (not per op) and never on a read path.
pub fn ensure_within_vault(
    vault_root: &Utf8Path,
    canonical_root: &std::path::Path,
    target: &Utf8Path,
) -> Result<(), ContainmentError> {
    if target.is_absolute() {
        return Err(ContainmentError::AbsolutePath {
            target: target.to_owned(),
        });
    }
    if target
        .components()
        .any(|c| matches!(c, camino::Utf8Component::ParentDir))
    {
        return Err(ContainmentError::ParentTraversal {
            target: target.to_owned(),
        });
    }

    let joined = vault_root.join(target);

    if joined.as_std_path().exists() {
        let canonical_target =
            joined
                .as_std_path()
                .canonicalize()
                .map_err(|e| ContainmentError::Unresolvable {
                    target: target.to_owned(),
                    detail: e.to_string(),
                })?;
        if !canonical_target.starts_with(canonical_root) {
            return Err(ContainmentError::EscapesVault {
                target: target.to_owned(),
            });
        }
        return Ok(());
    }

    let parent = joined.parent().unwrap_or(vault_root);
    let existing = nearest_existing_ancestor(parent);
    let canonical_parent =
        existing
            .as_std_path()
            .canonicalize()
            .map_err(|e| ContainmentError::Unresolvable {
                target: target.to_owned(),
                detail: e.to_string(),
            })?;
    if !canonical_parent.starts_with(canonical_root) {
        return Err(ContainmentError::EscapesVault {
            target: target.to_owned(),
        });
    }
    Ok(())
}

/// The nearest ancestor of `path` (inclusive) that exists on disk, following
/// symlinks. Terminates because the vault root — an ancestor of every
/// lexically-contained target — always exists.
fn nearest_existing_ancestor(path: &Utf8Path) -> &Utf8Path {
    let mut cur = path;
    loop {
        if cur.as_std_path().exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return cur,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveResult {
    pub from: Utf8PathBuf,
    pub to: Utf8PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkRewriteResult {
    pub file: Utf8PathBuf,
    pub from: String,
    pub to: String,
}

/// Why a planned backlink rewrite did not land. Benign cases (no filesystem
/// error occurred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSkipReason {
    /// The on-disk link text changed between plan and apply (the planned
    /// `raw` was not found in the file), so the rewrite was a no-op.
    Drifted,
    /// The backlinker file no longer exists (deleted or moved away by
    /// something outside this run); there is nothing to rewrite. Not retried.
    SourceMissing,
    /// The rewritten link text would break the backlinker's frontmatter (the
    /// new target carries YAML-structural bytes and the rewrite lands inside a
    /// frontmatter value, degrading a parsing mapping). The rewrite is skipped
    /// — the stale link remains, detectable by `validate` and repairable — a
    /// skipped rewrite is recoverable, a corrupted document is not (NRN-141).
    WouldCorruptFrontmatter,
}

impl LinkSkipReason {
    pub fn code(self) -> &'static str {
        match self {
            LinkSkipReason::Drifted => "drifted",
            LinkSkipReason::SourceMissing => "source_missing",
            LinkSkipReason::WouldCorruptFrontmatter => "would_corrupt_frontmatter",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LinkSkipResult {
    pub file: Utf8PathBuf,
    pub from: String,
    pub to: String,
    pub reason: LinkSkipReason,
}

/// Why a backlink rewrite hit a real filesystem problem (as opposed to a
/// benign skip). Everything in this set is retryable by the cleanup pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFailReason {
    /// Reading the backlinker file failed (non-NotFound io error).
    ReadFailed,
    /// Writing the rewritten backlinker file failed (non-NotFound io error).
    WriteFailed,
}

impl LinkFailReason {
    pub fn code(self) -> &'static str {
        match self {
            LinkFailReason::ReadFailed => "read_failed",
            LinkFailReason::WriteFailed => "write_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LinkFailResult {
    pub file: Utf8PathBuf,
    pub from: String,
    pub to: String,
    pub reason: LinkFailReason,
    /// The underlying io error string, for the human-facing `what`.
    pub detail: String,
}

/// Result of applying a move/delete backlink cascade: what landed and what
/// was skipped (with a reason). Replaces the bare `Vec<LinkRewriteResult>`
/// so deviations are recorded, not silently dropped.
#[derive(Debug, Clone, Default)]
pub struct LinkRewriteOutcome {
    pub rewritten: Vec<LinkRewriteResult>,
    pub skipped: Vec<LinkSkipResult>,
    pub failed: Vec<LinkFailResult>,
}

/// One backlink cascade attributable to a single move/delete change, keyed by
/// the change's source path. Internal plumbing consumed by the applier when it
/// builds the per-op `CascadeSummary`; never serialized to user JSON.
#[derive(Debug, Clone)]
pub struct CascadeRecord {
    pub source_path: camino::Utf8PathBuf,
    pub planned: usize,
    pub rewritten: Vec<LinkRewriteResult>,
    pub skipped: Vec<LinkSkipResult>,
    pub failed: Vec<LinkFailResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairApplyWarning {
    pub path: Utf8PathBuf,
    #[serde(flatten)]
    pub warning: PlanWarning,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub changed_files: Vec<Utf8PathBuf>,
    pub applied_changes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub moved_files: Vec<MoveResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub deleted_documents: Vec<DeleteResult>,
    /// Documents created by `create_document` ops (Pass 1e).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub created_documents: Vec<CreateDocumentResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rewritten_links: Vec<LinkRewriteResult>,
    /// Per-change backlink cascades (move/delete). Internal; not serialized —
    /// the applier folds these into per-op `CascadeSummary` in the ApplyReport.
    #[serde(skip)]
    pub cascades: Vec<CascadeRecord>,
    /// Paths whose body was wholly replaced by a `replace_body` change (Pass 1d).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub replaced_bodies: Vec<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<RepairApplyWarning>,
    pub plan_context: RepairApplyPlanContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<RepairApplyVerification>,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyPlanContext {
    pub skipped: SkippedSummary,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyVerification {
    pub remaining_findings: usize,
    pub summary: Summary,
}

impl RepairApplyReport {
    pub fn new(plan: &RepairPlan, dry_run: bool) -> Self {
        Self {
            schema_version: plan.schema_version,
            dry_run,
            changed_files: Vec::new(),
            applied_changes: plan.changes.len(),
            moved_files: Vec::new(),
            deleted_documents: Vec::new(),
            created_documents: Vec::new(),
            rewritten_links: Vec::new(),
            cascades: Vec::new(),
            replaced_bodies: Vec::new(),
            warnings: Vec::new(),
            plan_context: RepairApplyPlanContext {
                skipped: plan.summary.skipped.clone(),
            },
            verification: None,
        }
    }
}

pub fn validate_plan_for_apply(cwd: &Utf8PathBuf, plan: &RepairPlan) -> Result<(), ApplyError> {
    if plan.schema_version != REPAIR_PLAN_SCHEMA_VERSION {
        return Err(ApplyError::UnsupportedSchemaVersion {
            expected: REPAIR_PLAN_SCHEMA_VERSION,
            got: plan.schema_version,
        });
    }
    if &plan.vault_root != cwd {
        return Err(ApplyError::VaultRootMismatch {
            plan: plan.vault_root.clone(),
            cwd: cwd.clone(),
        });
    }
    Ok(())
}

/// Returns true for operations that are handled by dedicated orchestrator passes
/// (Pass 1b, 1c, 1d, 1e, 2, 3) rather than the per-file frontmatter edit pass. These
/// are skipped in `changes_by_path` rather than rejected as unsupported.
fn is_orchestrator_pass_op(operation: &str) -> bool {
    matches!(
        operation,
        "move_document"
            | "rewrite_link"
            | "delete_document"
            | "replace_body"
            | "create_document"
            | "str_replace"
            | "replace_section"
            | "append_to_section"
            | "delete_section"
            | "insert_before_heading"
            | "insert_after_heading"
    )
}

pub fn changes_by_path(
    plan: &RepairPlan,
) -> Result<BTreeMap<Utf8PathBuf, Vec<&PlannedChange>>, ApplyError> {
    let mut grouped: BTreeMap<Utf8PathBuf, Vec<&PlannedChange>> = BTreeMap::new();
    let mut seen_fields = BTreeSet::new();

    for change in &plan.changes {
        // move_document, rewrite_link, and delete_document are handled by
        // the orchestrator separately — they are not per-file frontmatter
        // edits, so they are skipped here rather than rejected.
        if is_orchestrator_pass_op(&change.operation) {
            continue;
        }
        if !matches!(
            change.operation.as_str(),
            "set_frontmatter" | "remove_frontmatter" | "add_frontmatter"
        ) {
            return Err(ApplyError::UnsupportedOperation {
                path: change.path.clone(),
                operation: change.operation.clone(),
            });
        }
        let field = change
            .field
            .as_deref()
            .ok_or_else(|| ApplyError::UnsupportedOperation {
                path: change.path.clone(),
                operation: format!("{} without field", change.operation),
            })?;
        let key = (change.path.clone(), field.to_string());
        if !seen_fields.insert(key) {
            return Err(ApplyError::ConflictingFieldChange {
                path: change.path.clone(),
                field: field.to_string(),
            });
        }
        grouped.entry(change.path.clone()).or_default().push(change);
    }

    for (path, changes) in &grouped {
        let hash = &changes[0].document_hash;
        if changes.iter().any(|change| &change.document_hash != hash) {
            return Err(ApplyError::ConflictingHashes { path: path.clone() });
        }
    }

    Ok(grouped)
}

/// A sequence-valued field (block or flow). Both have `value_range = None` and
/// are `set` by replacing the whole `line_range` with a freshly serialized
/// field, so a flow list stays flow and a block list stays block.
fn is_sequence_style(style: ValueStyle) -> bool {
    matches!(style, ValueStyle::BlockSequence | ValueStyle::FlowSequence)
}

/// The trailing same-line YAML comment (with its leading whitespace, excluding
/// the newline) on the single line `content[line_range]`, or `""` when there is
/// none. Only single-line ranges are handled — a multi-line flow value's
/// comment recovery is out of scope, so its comment is dropped as before.
/// A `#` inside a quoted scalar is never a comment (NRN-141).
fn single_line_trailing_comment(content: &str, line_range: &Range<usize>) -> String {
    let body = content[line_range.clone()].trim_end_matches(['\r', '\n']);
    // A multi-line range (unclosed flow spanning continuation lines) is out of
    // scope; leave its comment untouched (dropped, as before).
    if body.contains('\n') {
        return String::new();
    }
    trailing_line_comment(body).unwrap_or("").to_string()
}

/// Locates a trailing YAML comment in a single newline-free `line`. Returns the
/// slice from the whitespace preceding `#` to end of line (e.g. `"  # note"`),
/// or `None`. A comment opener is a `#` preceded by whitespace and NOT inside a
/// single- or double-quoted scalar (honoring `''` and `\` escapes), matching
/// where YAML actually begins a comment.
fn trailing_line_comment(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut comment_start = None;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                // A doubled `''` is an escaped quote, still inside the scalar.
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                // Skip the escaped byte (e.g. `\"`).
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'#' if i > 0 && matches!(bytes[i - 1], b' ' | b'\t') => {
                    comment_start = Some(i);
                    break;
                }
                _ => {}
            }
        }
        i += 1;
    }
    let start = comment_start?;
    // Back up over the whitespace run that precedes the `#` so it is preserved.
    let mut ws = start;
    while ws > 0 && matches!(bytes[ws - 1], b' ' | b'\t') {
        ws -= 1;
    }
    Some(&line[ws..])
}

pub fn apply_file_changes(content: &str, changes: &[&PlannedChange]) -> Result<String, ApplyError> {
    let path = if let Some(change) = changes.first() {
        change.path.clone()
    } else {
        return Ok(content.to_string());
    };

    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    let Some(frontmatter_range) = frontmatter_range else {
        // A malformed or unclosed block (no located range but a diagnostic) is a
        // parse failure to surface — not something to prepend a second block onto.
        if !diagnostics.is_empty() {
            return Err(ApplyError::FrontmatterParseFailed {
                path,
                message: join_diagnostics(&diagnostics),
            });
        }
        // NRN-120: a document with no frontmatter block gets an empty one
        // synthesized so a new field can be added to initialize it (schema
        // backfill on legacy files). The added fields flow through the normal
        // add_frontmatter insertion path against the empty block — which is
        // exactly what `set`/`new` on a missing field routes to.
        let synthesized = format!("---\n---\n{content}");
        return apply_file_changes(&synthesized, changes);
    };
    if !diagnostics.is_empty() {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: join_diagnostics(&diagnostics),
        });
    }
    let Some(frontmatter_value) = frontmatter else {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: "frontmatter could not be parsed".into(),
        });
    };
    let empty_object = serde_json::Map::new();
    let Some(current_object) = frontmatter_as_mapping(
        &frontmatter_value,
        content,
        &frontmatter_range,
        &empty_object,
    ) else {
        return Err(ApplyError::CannotMinimalEdit {
            path,
            reason: "frontmatter is not a top-level mapping".into(),
        });
    };

    let spans = top_level_property_spans(content, frontmatter_range.clone(), current_object);

    // The mapping the composed ops intend to produce: the parsed original with
    // each op's semantic effect applied. The post-image gate at the end compares
    // the re-parsed result against this (NRN-141).
    let mut expected: serde_json::Map<String, Value> = current_object.clone();

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();

    for change in changes {
        let field = change
            .field
            .as_deref()
            .ok_or_else(|| ApplyError::UnsupportedOperation {
                path: path.clone(),
                operation: format!("{} without field", change.operation),
            })?;
        let current_value = current_object.get(field);

        let span = spans.iter().find(|s| s.name == field);

        match change.operation.as_str() {
            "set_frontmatter" => {
                check_expected_old_value(&path, field, &change.expected_old_value, current_value)?;
                let Some(span) = span else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: span_absent_reason(field, current_object),
                    });
                };
                let new_value = change
                    .new_value
                    .as_ref()
                    .ok_or_else(|| ApplyError::MissingNewValue { path: path.clone() })?;
                // Mirror the op's semantic effect for the post-image gate below.
                expected.insert(field.to_string(), new_value.clone());

                // Sequence styles (block AND flow) carry no scalar value_range;
                // replace the field's whole serde-aligned line_range with a
                // freshly serialized field, preserving the original collection
                // style (block stays block, flow stays flow). This is what makes
                // a flow list like `tags: ["a", "b"]` editable without fragile
                // flow-item span parsing.
                if is_sequence_style(span.style) {
                    let Value::Array(items) = new_value else {
                        return Err(ApplyError::CannotMinimalEdit {
                            path: path.clone(),
                            reason: format!(
                                "field {field} is a sequence; set_frontmatter requires an array value"
                            ),
                        });
                    };
                    let replacement = if span.style == ValueStyle::FlowSequence {
                        let rendered = serialize_value_preserving_style(new_value, span.style)
                            .map_err(|e| ApplyError::CannotMinimalEdit {
                                path: path.clone(),
                                reason: e.to_string(),
                            })?;
                        // A flow `set` rewrites the whole line, so a trailing
                        // same-line comment would be dropped. Recover it (with its
                        // leading whitespace) for the single-line case (NRN-141).
                        let comment = single_line_trailing_comment(content, &span.line_range);
                        format!("{}: {rendered}{comment}\n", render_key(field))
                    } else {
                        serialize_array_block_field(field, items).map_err(|e| {
                            ApplyError::CannotMinimalEdit {
                                path: path.clone(),
                                reason: e.to_string(),
                            }
                        })?
                    };
                    edits.push((span.line_range.clone(), replacement));
                    continue;
                }

                let Some(value_range) = span.value_range.clone() else {
                    // A mapping, block scalar, anchor/alias/tag, or refused
                    // multi-line value: no editable scalar span. Decline in
                    // place (an accurate message — not "requires a scalar").
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!(
                            "field {field} value (style {:?}) cannot be minimally edited in place",
                            span.style
                        ),
                    });
                };
                let replacement = serialize_value_preserving_style(new_value, span.style).map_err(
                    |e| match e {
                        QuoteError::StructuredOriginalStyle(_)
                        | QuoteError::NonScalarValue
                        | QuoteError::ArrayIntoScalar => ApplyError::CannotMinimalEdit {
                            path: path.clone(),
                            reason: e.to_string(),
                        },
                        QuoteError::Unrepresentable { .. } => ApplyError::CannotMinimalEdit {
                            path: path.clone(),
                            reason: e.to_string(),
                        },
                    },
                )?;
                edits.push((value_range, replacement));
            }
            "remove_frontmatter" => {
                check_expected_old_value(&path, field, &change.expected_old_value, current_value)?;
                let Some(span) = span else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: span_absent_reason(field, current_object),
                    });
                };
                // Mirror the op's semantic effect for the post-image gate below.
                expected.remove(field);
                edits.push((span.line_range.clone(), String::new()));
            }
            "add_frontmatter" => {
                // add_frontmatter refuses to overwrite an existing field; the
                // caller must use set_frontmatter for that. We check the span
                // list (presence in source) since current_object may not
                // contain a field whose value style we cannot edit.
                if span.is_some() {
                    return Err(ApplyError::FieldAlreadyPresent {
                        path: path.clone(),
                        field: field.to_string(),
                    });
                }
                // expected_old_value semantics for add_frontmatter: None or
                // Null means "expected absent." Anything else is a contract
                // violation.
                if let Some(expected) = &change.expected_old_value {
                    if !expected.is_null() {
                        return Err(ApplyError::ExpectedOldValueMismatch {
                            path: path.clone(),
                            field: field.to_string(),
                            expected: format!("{expected}"),
                            actual: "missing".to_string(),
                        });
                    }
                }
                let new_value = change
                    .new_value
                    .as_ref()
                    .ok_or_else(|| ApplyError::MissingNewValue { path: path.clone() })?;
                // Mirror the op's semantic effect for the post-image gate below.
                expected.insert(field.to_string(), new_value.clone());
                // Insert at end of frontmatter block. extract_frontmatter
                // returns a range over the YAML content (between the leading
                // and trailing `---` lines). It ends at the byte just after
                // the final newline of the YAML, so we can splice a new line
                // here without disturbing the closing `---`.
                let insertion = frontmatter_range.end;
                let leading_newline =
                    if insertion == 0 || content.as_bytes().get(insertion - 1) == Some(&b'\n') {
                        ""
                    } else {
                        "\n"
                    };
                let line_to_insert = match new_value {
                    Value::Array(items) => {
                        // Default to block style for new array fields — more
                        // readable in Markdown frontmatter (an empty array
                        // renders as `field: []`).
                        let rendered = serialize_array_block_field(field, items).map_err(|e| {
                            ApplyError::CannotMinimalEdit {
                                path: path.clone(),
                                reason: e.to_string(),
                            }
                        })?;
                        format!("{leading_newline}{rendered}")
                    }
                    _ => {
                        let rendered =
                            serialize_value_preserving_style(new_value, ValueStyle::Plain)
                                .map_err(|e| ApplyError::CannotMinimalEdit {
                                    path: path.clone(),
                                    reason: e.to_string(),
                                })?;
                        format!("{leading_newline}{}: {rendered}\n", render_key(field))
                    }
                };
                edits.push((insertion..insertion, line_to_insert));
            }
            "move_document" => {
                // Handled by `apply_move`, not the per-file edit pass.
                // Reaching here means the caller bypassed `changes_by_path`.
                return Err(ApplyError::UnsupportedOperation {
                    path: path.clone(),
                    operation: "move_document".to_string(),
                });
            }
            other => {
                return Err(ApplyError::UnsupportedOperation {
                    path: path.clone(),
                    operation: other.to_string(),
                });
            }
        }
    }

    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = content.to_string();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }

    // NRN-141 post-image gate: the composed frontmatter must re-parse to exactly
    // the mapping the ops intended. A splice that produced unparseable YAML (a
    // duplicate key, an unclosed flow) or silently dropped/renamed a key (an
    // unquoted `#foo:` line YAML reads as a comment) is caught here and refused
    // before any write — a wrong write corrupts a document, a refusal does not.
    verify_post_image(&path, &out, &expected)?;

    Ok(out)
}

/// (NRN-141) Re-parse the composed frontmatter and confirm it equals `expected`
/// — the parsed original with each op's semantic effect applied. Parsed mappings
/// are compared, so key order and formatting are irrelevant. On a parse failure
/// or a mismatch, refuse: the span locator is a best-effort scanner (NRN-140
/// replaces it), so this is the trust-preserving backstop that converts a
/// corrupting write into a clean decline. Runs on every path through the
/// frontmatter-op editor ([`apply_file_changes`]), including a document whose
/// frontmatter was freshly synthesized. Content rewrites that mutate frontmatter
/// values *outside* this editor (`rewrite_link`) are covered by the weaker
/// parse-degradation check [`verify_frontmatter_not_degraded`] at the applier's
/// compose seam.
fn verify_post_image(
    path: &Utf8PathBuf,
    content: &str,
    expected: &serde_json::Map<String, Value>,
) -> Result<(), ApplyError> {
    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    if !diagnostics.is_empty() {
        return Err(ApplyError::PostImageVerificationFailed {
            path: path.clone(),
            detail: format!(
                "result no longer parses: {}",
                join_diagnostics(&diagnostics)
            ),
        });
    }
    let empty = serde_json::Map::new();
    let actual = match (&frontmatter, &frontmatter_range) {
        // The mapping-or-empty normalization mirrors the input gate; an emptied
        // block re-parsing as YAML null counts as the empty mapping.
        (Some(value), Some(range)) => frontmatter_as_mapping(value, content, range, &empty)
            .ok_or_else(|| ApplyError::PostImageVerificationFailed {
                path: path.clone(),
                detail: "result frontmatter is no longer a top-level mapping".into(),
            })?,
        // A document with no frontmatter block at all has the empty mapping.
        (None, None) => &empty,
        _ => {
            return Err(ApplyError::PostImageVerificationFailed {
                path: path.clone(),
                detail: "result frontmatter is no longer a top-level mapping".into(),
            });
        }
    };
    if actual != expected {
        return Err(ApplyError::PostImageVerificationFailed {
            path: path.clone(),
            detail: "result frontmatter does not match the intended fields".into(),
        });
    }
    Ok(())
}

/// Normalizes a parsed frontmatter value to its mapping form: a mapping is
/// itself; a YAML-null parse over an empty or whitespace-only block is the
/// empty mapping (an initializable `---\n---\n` block, NRN-120). Anything else
/// — an explicit `null`/`~` scalar, a sequence, a bare scalar — is not a
/// mapping and yields `None` (splicing keys around it would produce invalid
/// YAML, so callers refuse). Shared by `apply_file_changes`' input gate and
/// `verify_post_image` so both ends of an edit agree on what counts as a
/// mapping.
fn frontmatter_as_mapping<'a>(
    value: &'a Value,
    content: &str,
    frontmatter_range: &Range<usize>,
    empty: &'a serde_json::Map<String, Value>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        Value::Null if content[frontmatter_range.clone()].trim().is_empty() => Some(empty),
        _ => None,
    }
}

/// Joins frontmatter parse diagnostics into one `; `-separated message.
fn join_diagnostics(diagnostics: &[crate::core::Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

/// (NRN-141) Parse-degradation check for content rewrites. `apply_rewrite_link`
/// rewrites `[[...]]` across the whole file — frontmatter values included —
/// without going through the frontmatter editor's own post-image gate, so a
/// rewrite target carrying YAML-structural bytes can silently break the block
/// (collapsing every field to null on the next read). If the BASELINE
/// frontmatter (the content entering the rewrite: the post-frontmatter-ops
/// state at the applier's compose seam, or a backlinker's on-disk bytes in the
/// move/delete cascade) parsed as a mapping and the rewritten result no longer
/// does, the rewrite is refused. Deliberately weaker than mapping-equality:
/// link rewrites legitimately change values, so only parse degradation refuses
/// — and a document whose frontmatter was already broken (or absent, or a
/// non-mapping) stays rewritable.
pub(crate) fn verify_frontmatter_not_degraded(
    path: &Utf8Path,
    original: &str,
    composed: &str,
) -> Result<(), ApplyError> {
    let mut diagnostics = Vec::new();
    let (original_fm, _, _, _) = extract_frontmatter(original, &mut diagnostics);
    if !diagnostics.is_empty() || !matches!(original_fm, Some(Value::Object(_))) {
        return Ok(());
    }
    let mut diagnostics = Vec::new();
    let (composed_fm, composed_range, _, _) = extract_frontmatter(composed, &mut diagnostics);
    if !diagnostics.is_empty() {
        return Err(ApplyError::PostImageVerificationFailed {
            path: path.to_path_buf(),
            detail: format!(
                "a content rewrite broke the frontmatter: {}",
                join_diagnostics(&diagnostics)
            ),
        });
    }
    // The composed side accepts a mapping OR an emptied block: a plan that
    // removes the last frontmatter field legitimately leaves `---\n---\n`,
    // which re-parses as null — verify_post_image already accepted it as the
    // empty mapping, and refusing it here would decline a valid edit whole.
    // Anything else — a non-empty non-mapping, or a vanished block — degrades.
    let empty = serde_json::Map::new();
    let composed_ok = match (&composed_fm, &composed_range) {
        (Some(value), Some(range)) => {
            frontmatter_as_mapping(value, composed, range, &empty).is_some()
        }
        _ => false,
    };
    if !composed_ok {
        return Err(ApplyError::PostImageVerificationFailed {
            path: path.to_path_buf(),
            detail: "a content rewrite broke the frontmatter (no longer a top-level mapping)"
                .into(),
        });
    }
    Ok(())
}

/// The reason for a span-absent `set`/`remove`, told truthfully from the parsed
/// mapping (in scope at both call sites). A field that IS a parsed key but has no
/// editable span was declined wholesale — an ambiguous-key document the span
/// locator refused (NRN-141 guard). Reporting that as "not present" is false and
/// dangerous: it invites an `add_frontmatter` that would splice a duplicate key.
/// A field genuinely absent keeps the plain not-present message (NRN-141 / V10).
fn span_absent_reason(field: &str, current_object: &serde_json::Map<String, Value>) -> String {
    if current_object.contains_key(field) {
        format!(
            "field {field} is present but cannot be minimal-edited in place \
             (document declined: a frontmatter key cannot be reliably located; see NRN-140)"
        )
    } else {
        format!("field {field} not present in frontmatter")
    }
}

fn check_expected_old_value(
    path: &Utf8PathBuf,
    field: &str,
    expected: &Option<Value>,
    actual: Option<&Value>,
) -> Result<(), ApplyError> {
    match (expected, actual) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (None, None) => Ok(()),
        (None, Some(Value::Null)) => Ok(()),
        (Some(expected), Some(actual)) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: format!("{expected}"),
            actual: format!("{actual}"),
        }),
        (Some(expected), None) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: format!("{expected}"),
            actual: "missing".to_string(),
        }),
        (None, Some(actual)) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: "missing".to_string(),
            actual: format!("{actual}"),
        }),
    }
}

/// Performs the filesystem move for a `move_document` PlannedChange.
/// Refuses with precondition errors if source is missing/symlink or
/// destination exists. Falls back to copy+remove if rename fails
/// (typically cross-device).
pub fn apply_move(cwd: &Utf8Path, change: &PlannedChange) -> Result<MoveResult, ApplyError> {
    let source_rel = &change.path;
    let dest_rel = change
        .destination
        .as_ref()
        .ok_or_else(|| ApplyError::UnsupportedOperation {
            path: source_rel.clone(),
            operation: "move_document missing destination".to_string(),
        })?;

    let source_abs = cwd.join(source_rel);
    let dest_abs = cwd.join(dest_rel);

    let metadata = fs::symlink_metadata(source_abs.as_std_path()).map_err(|_| {
        ApplyError::MoveSourceMissing {
            path: source_rel.clone(),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ApplyError::MoveSourceIsSymlink {
            path: source_rel.clone(),
        });
    }
    if dest_abs.as_std_path().exists() {
        if change.force {
            // Best-effort atomicity: remove destination, then attempt rename.
            // If rename fails after this, destination is gone with no rollback.
            // Future improvement: snapshot-and-restore for true atomicity.
            fs::remove_file(dest_abs.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
                path: dest_rel.clone(),
                reason: format!("force-remove destination failed: {e}"),
            })?;
        } else {
            return Err(ApplyError::MoveDestinationExists {
                destination: dest_rel.clone(),
            });
        }
    }
    if let Some(parent) = dest_abs.parent() {
        fs::create_dir_all(parent.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
            path: dest_rel.clone(),
            reason: format!("create parent dir failed: {e}"),
        })?;
    }

    match fs::rename(source_abs.as_std_path(), dest_abs.as_std_path()) {
        Ok(()) => Ok(MoveResult {
            from: source_rel.clone(),
            to: dest_rel.clone(),
        }),
        Err(_) => {
            // Cross-device fallback
            fs::copy(source_abs.as_std_path(), dest_abs.as_std_path()).map_err(|e| {
                ApplyError::CannotMinimalEdit {
                    path: dest_rel.clone(),
                    reason: format!("copy failed: {e}"),
                }
            })?;
            fs::remove_file(source_abs.as_std_path()).map_err(|e| {
                ApplyError::CannotMinimalEdit {
                    path: source_rel.clone(),
                    reason: format!("remove source after copy failed: {e}"),
                }
            })?;
            Ok(MoveResult {
                from: source_rel.clone(),
                to: dest_rel.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResult {
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDocumentResult {
    pub path: Utf8PathBuf,
}

/// Performs the filesystem removal for a `delete_document` PlannedChange.
/// Refuses with precondition errors if source is missing or is a symlink.
pub fn apply_delete(cwd: &Utf8Path, change: &PlannedChange) -> Result<DeleteResult, ApplyError> {
    let source_rel = &change.path;
    let source_abs = cwd.join(source_rel);

    let metadata = fs::symlink_metadata(source_abs.as_std_path()).map_err(|_| {
        ApplyError::DeleteSourceMissing {
            path: source_rel.clone(),
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ApplyError::DeleteSourceIsSymlink {
            path: source_rel.clone(),
        });
    }

    fs::remove_file(source_abs.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
        path: source_rel.clone(),
        reason: format!("delete failed: {e}"),
    })?;

    Ok(DeleteResult {
        path: source_rel.clone(),
    })
}

/// Crash-atomic write: serialize `contents` to a sibling temp file
/// (`.{stem}.tmp`) then `fs::rename` it into place (atomic on POSIX). A SIGKILL /
/// power loss / `ENOSPC` mid-write truncates only the throwaway temp, never the
/// live document — which is exactly the half-mutation NRN-139 exists to prevent.
/// Best-effort temp cleanup on rename failure. Shared by the Phase A2 content
/// write, the `create_document` pass, and (NRN-146) backlink-cascade rewrites
/// in `rewrite_one_backlink` so there is a single implementation for every
/// document write path. A side effect for cascade callers: renaming into place
/// REPLACES a symlink at `full` rather than writing through it, closing the
/// symlink-file cascade class NRN-145 could otherwise only gate at preflight.
///
/// Mode preservation: renaming a temp file over `full` replaces its inode, so
/// the replacement would otherwise pick up fresh umask-based permissions
/// rather than inheriting whatever mode the file it replaces had — silently
/// downgrading a permission-hardened document (e.g. `chmod 600`, see
/// docs/cache.md) on every incidental content rewrite or cascade touch. When
/// `full` already exists, stat it first and carry its mode over to the temp
/// file before the rename so the replacement's mode matches the original.
/// When `full` does not exist (a fresh `create_document`), there is nothing to
/// preserve — the temp file's default (umask-based) permissions are correct.
/// Best-effort only: a metadata-read or chmod failure falls back to the
/// unmodified temp permissions rather than failing the write — preserving
/// mode is hardening, not a new way for a rewrite to fail. Ownership/ACLs are
/// out of scope: unlike mode bits, they cannot be portably preserved without
/// root, so this covers the meaningful, portable subset.
pub(crate) fn atomic_write(full: &Utf8Path, contents: &str) -> std::io::Result<()> {
    let tmp_path = {
        let mut p = full.to_path_buf();
        let stem = p.file_name().unwrap_or("doc").to_string();
        p.set_file_name(format!(".{stem}.tmp"));
        p
    };
    fs::write(tmp_path.as_std_path(), contents)?;
    #[cfg(unix)]
    if let Ok(existing) = fs::metadata(full.as_std_path()) {
        let _ = fs::set_permissions(tmp_path.as_std_path(), existing.permissions());
    }
    if let Err(e) = fs::rename(tmp_path.as_std_path(), full.as_std_path()) {
        // Best-effort cleanup on rename failure.
        let _ = fs::remove_file(tmp_path.as_std_path());
        return Err(e);
    }
    Ok(())
}

/// Outcome of attempting one backlink rewrite. The caller sorts these into
/// the `LinkRewriteOutcome` buckets and the retry pass re-runs this on failures.
pub(crate) enum LinkAttempt {
    Rewritten,
    Skipped(LinkSkipReason),
    /// reason + io error detail
    Failed(LinkFailReason, String),
}

/// Read `source_path`, replace the first occurrence of `raw` with `rewritten`,
/// write it back via `atomic_write` (NRN-146: temp file + rename, the same
/// crash-atomic guarantee as every other document write). Categorizes
/// outcomes; never panics, never aborts.
/// - NotFound (read or write) -> Skipped(SourceMissing): the file moved on.
/// - other io error on read   -> Failed(ReadFailed, detail)
/// - other io error on write  -> Failed(WriteFailed, detail)
/// - planned text not present -> Skipped(Drifted)
/// - would break the backlinker's parsing frontmatter
///   -> Skipped(WouldCorruptFrontmatter), unwritten
/// - success                  -> Rewritten
pub(crate) fn rewrite_one_backlink(
    cwd: &Utf8Path,
    source_path: &Utf8Path,
    raw: &str,
    rewritten: &str,
) -> LinkAttempt {
    let abs = cwd.join(source_path);
    let original = match fs::read_to_string(abs.as_std_path()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LinkAttempt::Skipped(LinkSkipReason::SourceMissing);
        }
        Err(e) => return LinkAttempt::Failed(LinkFailReason::ReadFailed, e.to_string()),
    };
    let updated = original.replacen(raw, rewritten, 1);
    if updated == original {
        return LinkAttempt::Skipped(LinkSkipReason::Drifted);
    }
    // NRN-141: a rewrite landing inside a frontmatter value can break the
    // backlinker's block when the new target carries YAML-structural bytes
    // (a move destination is a free-form filename). Skip rather than corrupt —
    // and rather than abort the whole cascade mid-move: the stale link stays
    // detectable by `validate` and repairable, while a nulled mapping is not.
    if verify_frontmatter_not_degraded(source_path, &original, &updated).is_err() {
        return LinkAttempt::Skipped(LinkSkipReason::WouldCorruptFrontmatter);
    }
    match atomic_write(&abs, &updated) {
        Ok(()) => LinkAttempt::Rewritten,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            LinkAttempt::Skipped(LinkSkipReason::SourceMissing)
        }
        Err(e) => LinkAttempt::Failed(LinkFailReason::WriteFailed, e.to_string()),
    }
}

/// Reads every file containing an AffectedLink and replaces the raw link
/// text with the precomputed rewritten replacement. Collect-and-continue:
/// benign deviations are skipped (with a reason), real FS errors are recorded
/// as failures (retried later by the cleanup pass) — neither aborts the apply.
pub fn apply_link_rewrites(
    cwd: &Utf8Path,
    change: &PlannedChange,
) -> Result<LinkRewriteOutcome, ApplyError> {
    let mut outcome = LinkRewriteOutcome::default();
    let risk = match &change.link_risk {
        Some(r) => r,
        None => return Ok(outcome),
    };
    let all = risk
        .stem_links
        .iter()
        .chain(risk.path_qualified_wikilinks.iter())
        .chain(risk.markdown_links.iter());
    for affected in all {
        match rewrite_one_backlink(
            cwd,
            &affected.source_path,
            &affected.raw,
            &affected.rewritten,
        ) {
            LinkAttempt::Rewritten => outcome.rewritten.push(LinkRewriteResult {
                file: affected.source_path.clone(),
                from: affected.raw.clone(),
                to: affected.rewritten.clone(),
            }),
            LinkAttempt::Skipped(reason) => outcome.skipped.push(LinkSkipResult {
                file: affected.source_path.clone(),
                from: affected.raw.clone(),
                to: affected.rewritten.clone(),
                reason,
            }),
            LinkAttempt::Failed(reason, detail) => outcome.failed.push(LinkFailResult {
                file: affected.source_path.clone(),
                from: affected.raw.clone(),
                to: affected.rewritten.clone(),
                reason,
                detail,
            }),
        }
    }
    Ok(outcome)
}

/// Apply a `rewrite_link` operation to source-doc content. Rewrites every
/// wikilink in the source whose target equals `expected_old_value` to use
/// `new_value`, preserving display text, anchor, and block-ref suffixes.
/// Replaces the body of a document wholesale, preserving the frontmatter block
/// (opening `---`, YAML content, and closing `---`) exactly as-is. If the
/// document has no frontmatter, the entire content is replaced by `new_value`.
///
/// Returns `ApplyError::MissingNewValue` when `change.new_value` is absent or
/// not a string.
///
/// Caller is responsible for hash verification before invoking this.
pub fn apply_replace_body(content: &str, change: &PlannedChange) -> Result<String, ApplyError> {
    let new_body = change
        .new_value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApplyError::MissingNewValue {
            path: change.path.clone(),
        })?;

    let (prefix, _body) = split_frontmatter_body(content);
    Ok(format!("{prefix}{new_body}"))
}

/// Split `content` into the `(prefix, body)` around the frontmatter block: the
/// prefix is everything up to and including the closing `---\n` (empty when the
/// document has no frontmatter), and `body` is the remainder. Reassemble a
/// body-preserving edit as `format!("{prefix}{new_body}")`. The single source of
/// truth for the frontmatter/body boundary, shared by [`apply_replace_body`] and
/// [`apply_edit_ops`] so they can't diverge.
fn split_frontmatter_body(content: &str) -> (&str, &str) {
    let mut diagnostics = Vec::new();
    let (_, frontmatter_range, _, body_start) = extract_frontmatter(content, &mut diagnostics);
    match frontmatter_range {
        Some(_) => (&content[..body_start], &content[body_start..]),
        None => ("", content),
    }
}

/// Apply a sequence of section/body [`EditOp`](crate::edit::ops::EditOp)s to
/// `content` at apply time, preserving frontmatter.
///
/// Mirrors [`apply_replace_body`]'s frontmatter-preserving splice, but computes
/// the new body via the shared `edit::transform` engine — the same code path
/// `norn edit` uses — so a plan-applied section edit and `norn edit` produce
/// byte-identical results. Ops apply sequentially, each against the prior's
/// output. The caller verifies the document hash before invoking (whole-doc CAS).
pub fn apply_edit_ops(
    content: &str,
    ops: &[crate::edit::ops::EditOp],
    path: &camino::Utf8Path,
) -> Result<String, ApplyError> {
    let (prefix, body) = split_frontmatter_body(content);
    let transformed =
        crate::edit::transform::apply_edits(body, ops).map_err(|e| ApplyError::EditFailed {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    Ok(format!("{prefix}{}", transformed.new_body))
}

///
/// Caller is responsible for hash verification before invoking this.
///
/// # Known limitation
///
/// The parser does not skip code-fenced content. If the same target appears
/// both in prose (flagged by validate) and inside a ``` ... ``` block (not
/// flagged), apply will rewrite BOTH occurrences. Validate's link extractor
/// skips code fences via `ignored_wikilink_ranges` in vault-links, but this
/// rewrite path does not. Reuse of `crate::links::parse_wikilinks` here would
/// require byte-span based rewriting; deferred to a follow-up.
pub fn apply_rewrite_link(content: &str, change: &PlannedChange) -> Result<String, ApplyError> {
    let old_target = change
        .expected_old_value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApplyError::UnsupportedOperation {
            path: change.path.clone(),
            operation: "rewrite_link without expected_old_value".to_string(),
        })?;
    let new_target = change
        .new_value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApplyError::MissingNewValue {
            path: change.path.clone(),
        })?;

    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("[[") {
        // Copy chunk before this candidate.
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(close) = after_open.find("]]") else {
            // Unclosed wikilink — copy the rest verbatim and stop.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let inner = &after_open[..close];

        // Parse inner = target [| label] with optional #anchor / ^block-ref on target.
        let (target_with_modifiers, label) = match inner.split_once('|') {
            Some((t, l)) => (t, Some(l)),
            None => (inner, None),
        };
        // Split target from suffix (#anchor or ^block-ref).
        let (bare_target, suffix) = split_target_suffix(target_with_modifiers);

        if bare_target == old_target {
            out.push_str("[[");
            out.push_str(new_target);
            if let Some(s) = suffix {
                out.push_str(s);
            }
            if let Some(l) = label {
                out.push('|');
                out.push_str(l);
            }
            out.push_str("]]");
        } else {
            // Not our match — copy verbatim.
            out.push_str("[[");
            out.push_str(inner);
            out.push_str("]]");
        }

        rest = &after_open[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn split_target_suffix(s: &str) -> (&str, Option<&str>) {
    // Suffix starts at the first '#' or '^', whichever comes first.
    let hash = s.find('#');
    let caret = s.find('^');
    let split_at = match (hash, caret) {
        (Some(h), Some(c)) => Some(h.min(c)),
        (Some(h), None) => Some(h),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    };
    match split_at {
        Some(i) => (&s[..i], Some(&s[i..])),
        None => (s, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::repair::{RepairPlanFilters, RepairPlanSummary, SkippedSummary};
    use serde_json::json;

    /// NRN-150: every `ApplyError` variant reports a stable, non-empty KEBAB
    /// code, and the CAS-drift variants report the exact codes the MMR-202
    /// acceptance branches on. Asserting kebab-shape (lowercase / digits /
    /// hyphens, no underscores) here pins the canonical-form mandate so a future
    /// snake_case addition fails the build.
    #[test]
    fn apply_error_codes_are_stable_kebab() {
        fn is_kebab(s: &str) -> bool {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !s.starts_with('-')
                && !s.ends_with('-')
        }

        let p = || Utf8PathBuf::from("doc.md");
        let variants: Vec<(ApplyError, &str)> = vec![
            (
                ApplyError::UnsupportedSchemaVersion {
                    expected: 1,
                    got: 2,
                },
                "unsupported-schema-version",
            ),
            (
                ApplyError::VaultRootMismatch {
                    plan: p(),
                    cwd: p(),
                },
                "vault-root-mismatch",
            ),
            (ApplyError::UnknownPath { path: p() }, "unknown-path"),
            (
                ApplyError::StaleDocumentHash {
                    path: p(),
                    expected: "a".into(),
                    actual: "b".into(),
                },
                "stale-document-hash",
            ),
            (
                ApplyError::ConflictingFieldChange {
                    path: p(),
                    field: "f".into(),
                },
                "conflicting-field-change",
            ),
            (
                ApplyError::ConflictingHashes { path: p() },
                "conflicting-hashes",
            ),
            (
                ApplyError::ExpectedOldValueMismatch {
                    path: p(),
                    field: "f".into(),
                    expected: "a".into(),
                    actual: "b".into(),
                },
                "expected-old-value-mismatch",
            ),
            (
                ApplyError::UnsupportedOperation {
                    path: p(),
                    operation: "op".into(),
                },
                "unsupported-operation",
            ),
            (
                ApplyError::CannotMinimalEdit {
                    path: p(),
                    reason: "r".into(),
                },
                "cannot-minimal-edit",
            ),
            (
                ApplyError::FrontmatterParseFailed {
                    path: p(),
                    message: "m".into(),
                },
                "frontmatter-parse-failed",
            ),
            (
                ApplyError::PostImageVerificationFailed {
                    path: p(),
                    detail: "d".into(),
                },
                "post-image-verification-failed",
            ),
            (
                ApplyError::EditFailed {
                    path: p(),
                    message: "m".into(),
                },
                "edit-failed",
            ),
            (
                ApplyError::MissingNewValue { path: p() },
                "missing-new-value",
            ),
            (
                ApplyError::FieldAlreadyPresent {
                    path: p(),
                    field: "f".into(),
                },
                "field-already-present",
            ),
            (
                ApplyError::MoveSourceMissing { path: p() },
                "move-source-missing",
            ),
            (
                ApplyError::MoveSourceIsSymlink { path: p() },
                "move-source-is-symlink",
            ),
            (
                ApplyError::MoveDestinationExists { destination: p() },
                "move-destination-exists",
            ),
            (
                ApplyError::DeleteSourceMissing { path: p() },
                "delete-source-missing",
            ),
            (
                ApplyError::DeleteSourceIsSymlink { path: p() },
                "delete-source-is-symlink",
            ),
            (
                ApplyError::ContentOpAfterVacate {
                    path: p(),
                    earlier_op: "delete_document".into(),
                    earlier_change_id: "c0".into(),
                },
                "content-op-after-vacate",
            ),
            (
                ApplyError::Containment(ContainmentError::AbsolutePath { target: p() }),
                "containment-absolute-path",
            ),
            (
                ApplyError::Containment(ContainmentError::ParentTraversal { target: p() }),
                "containment-parent-traversal",
            ),
            (
                ApplyError::Containment(ContainmentError::EscapesVault { target: p() }),
                "containment-escapes-vault",
            ),
            (
                ApplyError::Containment(ContainmentError::Unresolvable {
                    target: p(),
                    detail: "d".into(),
                }),
                "containment-unresolvable",
            ),
        ];

        let mut seen = std::collections::BTreeSet::new();
        for (err, expected) in &variants {
            assert_eq!(err.code(), *expected, "code drift for {err:?}");
            assert!(is_kebab(err.code()), "non-kebab code: {}", err.code());
            assert!(seen.insert(err.code()), "duplicate code: {}", err.code());
        }

        // CAS-drift (retryable) codes the MMR-202 acceptance keys on.
        assert_eq!(
            ApplyError::StaleDocumentHash {
                path: p(),
                expected: "a".into(),
                actual: "b".into()
            }
            .code(),
            "stale-document-hash"
        );
        assert_eq!(
            ApplyError::ExpectedOldValueMismatch {
                path: p(),
                field: "f".into(),
                expected: "a".into(),
                actual: "b".into()
            }
            .code(),
            "expected-old-value-mismatch"
        );
    }

    /// The CAS-drift variants are precondition (byte-identical) refusals; the
    /// Phase-B lifecycle variants are not.
    #[test]
    fn precondition_classification() {
        let p = || Utf8PathBuf::from("d.md");
        assert!(ApplyError::StaleDocumentHash {
            path: p(),
            expected: "a".into(),
            actual: "b".into()
        }
        .is_precondition());
        assert!(ApplyError::ExpectedOldValueMismatch {
            path: p(),
            field: "f".into(),
            expected: "a".into(),
            actual: "b".into()
        }
        .is_precondition());
        assert!(
            ApplyError::Containment(ContainmentError::AbsolutePath { target: p() })
                .is_precondition()
        );
        assert!(!ApplyError::MoveDestinationExists { destination: p() }.is_precondition());
        assert!(!ApplyError::DeleteSourceMissing { path: p() }.is_precondition());
    }

    fn empty_plan(schema_version: u32, vault_root: &str) -> RepairPlan {
        RepairPlan {
            schema_version,
            vault_root: vault_root.into(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 0,
                skipped: SkippedSummary::default(),
            },
            changes: vec![],
            skipped_findings: vec![],
            footnotes: vec![],
        }
    }

    fn make_change(
        path: &str,
        field: &str,
        hash: &str,
        operation: &str,
        new_value: Option<Value>,
    ) -> PlannedChange {
        PlannedChange {
            change_id: "test-change-id".to_string(),
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: "frontmatter-disallowed-value".into(),
            finding_rule: None,
            repair_rule: "test".into(),
            operation: operation.to_string(),
            field: Some(field.to_string()),
            expected_old_value: None,
            new_value,
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        }
    }

    #[test]
    fn validate_plan_rejects_unsupported_schema_version() {
        let plan = empty_plan(99, "/vault");
        let err = validate_plan_for_apply(&"/vault".into(), &plan).unwrap_err();
        assert!(matches!(
            err,
            ApplyError::UnsupportedSchemaVersion {
                expected: REPAIR_PLAN_SCHEMA_VERSION,
                got: 99,
            }
        ));
    }

    #[test]
    fn validate_plan_rejects_vault_root_mismatch() {
        let plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/other");
        let err = validate_plan_for_apply(&"/vault".into(), &plan).unwrap_err();
        assert!(matches!(err, ApplyError::VaultRootMismatch { .. }));
    }

    #[test]
    fn validate_plan_accepts_matching_schema_and_root() {
        let plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        validate_plan_for_apply(&"/vault".into(), &plan).unwrap();
    }

    #[test]
    fn changes_by_path_groups_by_path() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("done")),
            ),
            make_change("a.md", "kind", "h1", "remove_frontmatter", None),
            make_change(
                "b.md",
                "status",
                "h2",
                "set_frontmatter",
                Some(json!("done")),
            ),
        ];
        let grouped = changes_by_path(&plan).unwrap();
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&Utf8PathBuf::from("a.md")].len(), 2);
        assert_eq!(grouped[&Utf8PathBuf::from("b.md")].len(), 1);
    }

    #[test]
    fn changes_by_path_rejects_conflicting_field_changes() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("done")),
            ),
            make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            ),
        ];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::ConflictingFieldChange { .. }));
    }

    #[test]
    fn changes_by_path_rejects_conflicting_hashes_for_same_path() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("done")),
            ),
            make_change("a.md", "kind", "h2", "remove_frontmatter", None),
        ];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::ConflictingHashes { .. }));
    }

    #[test]
    fn changes_by_path_rejects_unsupported_operation() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![make_change("a.md", "status", "h1", "rename_file", None)];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::UnsupportedOperation { .. }));
    }

    fn apply_change(content: &str, change: &PlannedChange) -> Result<String, ApplyError> {
        apply_file_changes(content, &[change])
    }

    /// Parse the frontmatter block of an applied result into a JSON object so
    /// tests can assert a key reads back byte-exactly with its new value.
    fn frontmatter_json(content: &str) -> serde_json::Value {
        let yaml = content
            .strip_prefix("---\n")
            .and_then(|rest| rest.split("\n---\n").next())
            .expect("frontmatter block");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(yaml).expect("applied frontmatter must parse");
        serde_json::to_value(parsed).expect("yaml → json")
    }

    #[test]
    fn set_frontmatter_replaces_plain_scalar_value() {
        let content = "---\nstatus: someday\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nstatus: completed\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_double_quoted_style() {
        let content = "---\nworkspace: \"[[norn]]\"\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[norn]]")),
            new_value: Some(json!("[[other]]")),
            ..make_change(
                "a.md",
                "workspace",
                "h1",
                "set_frontmatter",
                Some(json!("[[other]]")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nworkspace: \"[[other]]\"\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_single_quoted_style() {
        let content = "---\nworkspace: '[[norn]]'\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[norn]]")),
            new_value: Some(json!("[[other]]")),
            ..make_change(
                "a.md",
                "workspace",
                "h1",
                "set_frontmatter",
                Some(json!("[[other]]")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nworkspace: '[[other]]'\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_same_line_comment() {
        let content = "---\nstatus: someday  # legacy\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nstatus: completed  # legacy\n---\n");
    }

    #[test]
    fn remove_frontmatter_deletes_full_line() {
        let content = "---\ntitle: hi\nkind: legacy\nstatus: done\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("legacy")),
            ..make_change("a.md", "kind", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: hi\nstatus: done\n---\n");
    }

    #[test]
    fn remove_frontmatter_can_delete_block_value_lines() {
        let content = "---\ntitle: hi\naliases:\n  - one\n  - two\nstatus: done\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["one", "two"])),
            ..make_change("a.md", "aliases", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: hi\nstatus: done\n---\n");
    }

    #[test]
    fn set_frontmatter_rejects_block_sequence_target() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["one", "two"])),
            ..make_change(
                "a.md",
                "aliases",
                "h1",
                "set_frontmatter",
                Some(json!("one")),
            )
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::CannotMinimalEdit { .. }));
    }

    #[test]
    fn apply_rejects_expected_old_value_mismatch() {
        let content = "---\nstatus: completed\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("backlog")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            )
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::ExpectedOldValueMismatch { .. }));
    }

    #[test]
    fn apply_treats_yaml_null_as_absent_for_expected_old_value() {
        let content = "---\nstatus: ~\n---\n";
        let change = PlannedChange {
            expected_old_value: None,
            new_value: Some(json!("backlog")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert!(result.contains("status: backlog"));
    }

    #[test]
    fn apply_preserves_markdown_body_exactly() {
        let content =
            "---\nstatus: someday\n---\n# Heading\n\nParagraph with `code` and **bold**.\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        let body_start = result.find("# Heading").unwrap();
        assert_eq!(
            &result[body_start..],
            "# Heading\n\nParagraph with `code` and **bold**.\n"
        );
    }

    #[test]
    fn apply_returns_cannot_minimal_edit_for_missing_field() {
        let content = "---\ntitle: hi\n---\n";
        let change = make_change("a.md", "status", "h1", "remove_frontmatter", None);
        let err = apply_change(content, &change).unwrap_err();
        let ApplyError::CannotMinimalEdit { reason, .. } = &err else {
            panic!("expected CannotMinimalEdit, got {err:?}");
        };
        assert!(
            reason.contains("not present in frontmatter"),
            "a genuinely absent field keeps the not-present message, got: {reason}"
        );
    }

    #[test]
    fn apply_synthesizes_frontmatter_on_frontmatterless_document() {
        // NRN-120: a legacy document with no frontmatter block gets one
        // synthesized so schema fields can be initialized through norn.
        let content = "just a body\n";
        let change = make_change(
            "a.md",
            "title",
            "h1",
            "add_frontmatter",
            Some(json!("Legacy")),
        );
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: Legacy\n---\njust a body\n");
    }

    #[test]
    fn apply_synthesizes_frontmatter_on_empty_document() {
        let content = "";
        let change = make_change("a.md", "title", "h1", "add_frontmatter", Some(json!("X")));
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: X\n---\n");
    }

    #[test]
    fn apply_initializes_empty_frontmatter_block() {
        // An existing but empty `---\n---\n` block also accepts field
        // initialization (parses as YAML null → treated as an empty mapping).
        let content = "---\n---\nbody\n";
        let change = make_change("a.md", "title", "h1", "add_frontmatter", Some(json!("X")));
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: X\n---\nbody\n");
    }

    #[test]
    fn apply_refuses_explicit_null_scalar_block() {
        // An explicit `null` scalar block is NOT an empty block: splicing a key
        // after it would produce invalid YAML, so it must stay a hard error
        // rather than be treated as an empty mapping (review Finding 1).
        let content = "---\nnull\n---\nbody\n";
        let change = make_change("a.md", "title", "h1", "add_frontmatter", Some(json!("X")));
        let err = apply_change(content, &change).unwrap_err();
        assert!(
            matches!(err, ApplyError::CannotMinimalEdit { .. }),
            "expected CannotMinimalEdit, got {err:?}"
        );
    }

    #[test]
    fn apply_initializes_whitespace_only_frontmatter_block() {
        // A block whose content is only whitespace parses as null but IS empty —
        // it stays initializable (the pre-existing whitespace line is preserved
        // verbatim; the appended field still parses correctly).
        let content = "---\n   \n---\nbody\n";
        let change = make_change("a.md", "title", "h1", "add_frontmatter", Some(json!("X")));
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\n   \ntitle: X\n---\nbody\n");
    }

    #[test]
    fn apply_reports_parse_failure_not_missing_frontmatter_for_unclosed_block() {
        // An unclosed block must surface as a parse failure, not the misleading
        // "document has no frontmatter" (and must not get a second block prepended).
        let content = "---\ntitle: hi\nno closing fence\n";
        let change = make_change("a.md", "status", "h1", "add_frontmatter", Some(json!("x")));
        let err = apply_change(content, &change).unwrap_err();
        assert!(
            matches!(err, ApplyError::FrontmatterParseFailed { .. }),
            "expected FrontmatterParseFailed, got {err:?}"
        );
    }

    #[test]
    fn apply_add_frontmatter_array_inserts_block_style() {
        let content = "---\ntitle: Foo\n---\nbody\n";
        let change = make_change(
            "a.md",
            "aliases",
            "h1",
            "add_frontmatter",
            Some(json!(["alpha", "beta"])),
        );
        let result = apply_change(content, &change).unwrap();
        assert!(
            result.contains("aliases:\n  - alpha\n  - beta"),
            "expected block-style array in result: {result}"
        );
        assert!(result.contains("title: Foo"));
        assert!(result.contains("body"));
    }

    #[test]
    fn apply_add_frontmatter_empty_array_inserts_empty_flow_list() {
        // NRN-141: a bare `aliases:` line reads back as null, not `[]`. An empty
        // array now serializes as `aliases: []` so the field round-trips to the
        // empty list it was written as.
        let content = "---\ntitle: Foo\n---\nbody\n";
        let change = make_change("a.md", "aliases", "h1", "add_frontmatter", Some(json!([])));
        let result = apply_change(content, &change).unwrap();
        assert!(
            result.contains("aliases: []\n"),
            "expected an explicit empty list: {result}"
        );
        let mut diags = Vec::new();
        let (fm, _, _, _) = extract_frontmatter(&result, &mut diags);
        assert!(diags.is_empty(), "result must re-parse cleanly: {result}");
        assert_eq!(fm.unwrap().get("aliases"), Some(&json!([])));
    }

    #[test]
    fn apply_set_block_list_to_empty_round_trips_as_empty_list() {
        // NRN-141: `set aliases=[]` on a block list must write `aliases: []`
        // (not a bare `aliases:` that reads back as null).
        let content = "---\naliases:\n  - old\ntitle: hi\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["old"])),
            ..make_change("a.md", "aliases", "h1", "set_frontmatter", Some(json!([])))
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\naliases: []\ntitle: hi\n---\nbody\n");
        let mut diags = Vec::new();
        let (fm, _, _, _) = extract_frontmatter(&result, &mut diags);
        assert!(diags.is_empty());
        assert_eq!(fm.unwrap().get("aliases"), Some(&json!([])));
    }

    #[test]
    fn apply_set_frontmatter_array_on_existing_block_replaces_items() {
        let content = "---\naliases:\n  - old\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["old"])),
            ..make_change(
                "a.md",
                "aliases",
                "h1",
                "set_frontmatter",
                Some(json!(["alpha", "beta"])),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert!(
            result.contains("aliases:\n  - alpha\n  - beta"),
            "expected new block items: {result}"
        );
        assert!(
            !result.contains("- old"),
            "old item should be removed: {result}"
        );
    }

    #[test]
    fn apply_set_frontmatter_array_on_existing_flow_replaces_inline() {
        let content = "---\naliases: [old]\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["old"])),
            ..make_change(
                "a.md",
                "aliases",
                "h1",
                "set_frontmatter",
                Some(json!(["alpha", "beta"])),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert!(
            result.contains("aliases: [alpha, beta]"),
            "expected inline flow array: {result}"
        );
        assert!(!result.contains("old"), "old item should be gone: {result}");
    }

    #[test]
    fn apply_set_frontmatter_quoted_flow_list_is_editable() {
        // Round 3 regression fix: a quote inside `[...]` used to over-refuse the
        // whole field. It now edits through whole-line replacement, and the new
        // value re-parses to the requested array.
        let content = "---\ntags: [\"a\", \"b\"]\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["a", "b"])),
            ..make_change(
                "a.md",
                "tags",
                "h1",
                "set_frontmatter",
                Some(json!(["Foo Bar", "c"])),
            )
        };
        let result = apply_change(content, &change).unwrap();
        // Re-extract the new value and assert it equals the requested array —
        // whatever quoting the serializer chose must be valid.
        let mut diags = Vec::new();
        let (fm, _, _, _) = extract_frontmatter(&result, &mut diags);
        assert!(diags.is_empty(), "result must re-parse cleanly: {result}");
        assert_eq!(
            fm.unwrap().get("tags").unwrap(),
            &json!(["Foo Bar", "c"]),
            "flow list must round-trip: {result}"
        );
    }

    #[test]
    fn apply_set_flow_list_preserves_trailing_comment() {
        // NRN-141 P3: a flow-list `set` replaces the whole line via line_range,
        // which dropped a trailing same-line comment. The comment (with its
        // leading whitespace) is now re-appended after the replacement.
        let content = "---\ntags: [a, b]  # note\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["a", "b"])),
            ..make_change("a.md", "tags", "h1", "set_frontmatter", Some(json!(["x"])))
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntags: [x]  # note\n---\nbody\n");
    }

    #[test]
    fn apply_set_flow_list_does_not_treat_quoted_hash_as_comment() {
        // A `#` inside a quoted scalar is not a comment opener — it must not be
        // captured as a trailing comment, and a real trailing comment after the
        // quoted `#` must still be preserved.
        let content = "---\ntags: [\"a#b\"]  # real\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["a#b"])),
            ..make_change("a.md", "tags", "h1", "set_frontmatter", Some(json!(["c"])))
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntags: [c]  # real\n---\nbody\n");
    }

    #[test]
    fn apply_set_flow_list_quoted_hash_only_captures_no_comment() {
        // With only a quoted `#` and no real comment, nothing is appended and
        // the value round-trips cleanly (no spurious `# ...` bytes).
        let content = "---\ntags: [\"a#b\"]\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["a#b"])),
            ..make_change(
                "a.md",
                "tags",
                "h1",
                "set_frontmatter",
                Some(json!(["c#d"])),
            )
        };
        let result = apply_change(content, &change).unwrap();
        let mut diags = Vec::new();
        let (fm, _, _, _) = extract_frontmatter(&result, &mut diags);
        assert!(diags.is_empty(), "result must re-parse cleanly: {result}");
        assert_eq!(fm.unwrap().get("tags").unwrap(), &json!(["c#d"]));
    }

    #[test]
    fn apply_remove_frontmatter_deletes_whole_list_of_mappings_block() {
        // Phantom-boundary fix: `- name:` item lines are not confirmed keys, so
        // `contacts`'s line_range covers the whole block — remove takes it all.
        let content = "---\ncontacts:\n- name: Bob\n- name: Alice\nowner: x\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!([{"name": "Bob"}, {"name": "Alice"}])),
            ..make_change("a.md", "contacts", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nowner: x\n---\nbody\n", "no orphaned items");
    }

    #[test]
    fn apply_set_key_escape_collision_does_not_clobber_other_field() {
        // `"\x61"` (serde `a`) collides with a literal `x61` at the scanner
        // level; both ambiguous keys refuse, so setting `x61` cannot touch `a`.
        let content = "---\n\"\\x61\": v\nx61: w\n---\nbody\n";
        // expected_old_value matches serde's x61 so the precondition passes and
        // we exercise the span-level refusal specifically.
        let change = PlannedChange {
            expected_old_value: Some(json!("w")),
            ..make_change("a.md", "x61", "h1", "set_frontmatter", Some(json!("NEW")))
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(
            matches!(err, ApplyError::CannotMinimalEdit { .. }),
            "ambiguous key must refuse at the span level, got {err:?}"
        );
        // And crucially, serde's `a` field is untouched (no write happened).
        assert!(apply_change(content, &change).is_err());
    }

    #[test]
    fn apply_refuses_whole_doc_when_a_serde_key_is_unlocatable() {
        // NRN-141: `"\x61"` (serde `a`) is mis-decoded by the scanner, so serde
        // key `a` has no candidate line. Setting the well-formed `title` on the
        // same document must refuse (the whole-doc span refusal), never write —
        // otherwise `a` would be absorbed into `title`'s line and clobbered.
        let content = "---\ntitle: hi\n\"\\x61\": 1\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("hi")),
            ..make_change("a.md", "title", "h1", "set_frontmatter", Some(json!("bye")))
        };
        let err = apply_change(content, &change).unwrap_err();
        let ApplyError::CannotMinimalEdit { reason, .. } = &err else {
            panic!("expected CannotMinimalEdit, got {err:?}");
        };
        // V10: `title` IS present in the parsed mapping — the refusal must say
        // so, not claim it absent (which would invite an add_frontmatter that
        // splices a duplicate).
        assert!(
            reason.contains("present but cannot be minimal-edited"),
            "a located-but-declined field must not report as absent, got: {reason}"
        );
        // No write path is reached — a remove of the same field also refuses at
        // the span-absent site with the same truthful diagnostic. (expected_old
        // is supplied so the precondition passes and we reach that site.)
        let remove = PlannedChange {
            expected_old_value: Some(json!("hi")),
            ..make_change("a.md", "title", "h1", "remove_frontmatter", None)
        };
        let remove_err = apply_change(content, &remove).unwrap_err();
        let ApplyError::CannotMinimalEdit { reason, .. } = &remove_err else {
            panic!("expected CannotMinimalEdit, got {remove_err:?}");
        };
        assert!(
            reason.contains("present but cannot be minimal-edited"),
            "remove of a located-but-declined field must not report as absent, got: {reason}"
        );
    }

    #[test]
    fn apply_set_frontmatter_scalar_into_scalar_still_works() {
        let content = "---\nstatus: draft\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("draft")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("active")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nstatus: active\n---\nbody\n");
    }

    #[test]
    fn apply_add_frontmatter_inserts_missing_field() {
        let content = "---\ntitle: hi\n---\n# body\n";
        let change = make_change(
            "task.md",
            "kind",
            "h1",
            "add_frontmatter",
            Some(json!("research")),
        );
        let result = apply_change(content, &change).unwrap();
        assert!(result.contains("kind: research"));
        assert!(result.contains("title: hi"));
        assert!(result.contains("# body"));
    }

    #[test]
    fn apply_add_frontmatter_refuses_when_field_present() {
        let content = "---\ntitle: hi\nkind: oldvalue\n---\n# body\n";
        let change = make_change(
            "task.md",
            "kind",
            "h1",
            "add_frontmatter",
            Some(json!("newvalue")),
        );
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::FieldAlreadyPresent { .. }));
    }

    #[test]
    fn apply_add_frontmatter_quotes_special_values() {
        let content = "---\ntitle: hi\n---\n";
        let change = make_change(
            "task.md",
            "workspace",
            "h1",
            "add_frontmatter",
            Some(json!("[[demo]]")),
        );
        let result = apply_change(content, &change).unwrap();
        assert!(result.contains("workspace: '[[demo]]'"));
    }

    #[test]
    fn apply_rewrite_link_replaces_bare_wikilink() {
        let original = "---\ntitle: x\n---\n\nSee [[Norn Brand]] for details.\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[norn-brand]]"));
        assert!(!updated.contains("[[Norn Brand]]"));
    }

    #[test]
    fn apply_rewrite_link_preserves_display_text() {
        let original = "Reference: [[Norn Brand|the brand spec]] here.\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[norn-brand|the brand spec]]"));
    }

    #[test]
    fn apply_rewrite_link_preserves_anchor() {
        let original = "See [[Norn Brand#colors]].\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[norn-brand#colors]]"));
    }

    #[test]
    fn apply_rewrite_link_preserves_block_ref() {
        let original = "See [[Norn Brand^block-id]].\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[norn-brand^block-id]]"));
    }

    #[test]
    fn apply_rewrite_link_replaces_all_occurrences() {
        let original = "[[Norn Brand]] and [[Norn Brand]] again.\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert_eq!(updated.matches("[[norn-brand]]").count(), 2);
        assert!(!updated.contains("[[Norn Brand]]"));
    }

    #[test]
    fn apply_rewrite_link_leaves_unmatched_wikilinks_alone() {
        let original = "See [[Other Doc]] and [[Norn Brand]].\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[Other Doc]]"));
        assert!(updated.contains("[[norn-brand]]"));
    }

    #[test]
    fn apply_rewrite_link_preserves_anchor_then_block_ref_combination() {
        let original = "See [[Norn Brand#^block-id]] for details.\n";
        let change = PlannedChange {
            change_id: "test".into(),
            path: "doc.md".into(),
            document_hash: "test-hash".into(),
            finding_code: "link-target-missing".into(),
            finding_rule: None,
            repair_rule: "built-in:closest-match-stem".into(),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(Value::String("Norn Brand".into())),
            new_value: Some(Value::String("norn-brand".into())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let updated = apply_rewrite_link(original, &change).unwrap();
        assert!(updated.contains("[[norn-brand#^block-id]]"));
    }

    #[test]
    fn apply_replace_body_replaces_body_preserves_frontmatter() {
        let content = "---\ntitle: Foo\n---\nold body line 1\nold body line 2\n";
        let change = PlannedChange {
            change_id: "test".to_string(),
            path: "test.md".into(),
            document_hash: "ignored".to_string(),
            finding_code: "operator-mutation".to_string(),
            finding_rule: None,
            repair_rule: "vault-set".to_string(),
            operation: "replace_body".to_string(),
            field: None,
            expected_old_value: None,
            new_value: Some(Value::String("new body content\n".to_string())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let result =
            apply_replace_body(content, &change).expect("apply_replace_body should succeed");
        assert_eq!(result, "---\ntitle: Foo\n---\nnew body content\n");
    }

    #[test]
    fn apply_replace_body_handles_doc_with_no_frontmatter() {
        let content = "raw body line 1\nraw body line 2\n";
        let change = PlannedChange {
            change_id: "test".to_string(),
            path: "test.md".into(),
            document_hash: "ignored".to_string(),
            finding_code: "operator-mutation".to_string(),
            finding_rule: None,
            repair_rule: "vault-set".to_string(),
            operation: "replace_body".to_string(),
            field: None,
            expected_old_value: None,
            new_value: Some(Value::String("new body\n".to_string())),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let result =
            apply_replace_body(content, &change).expect("apply_replace_body should succeed");
        assert_eq!(result, "new body\n");
    }

    #[test]
    fn apply_replace_body_returns_error_when_new_value_missing() {
        let content = "---\ntitle: Foo\n---\nbody\n";
        let change = PlannedChange {
            change_id: "test".to_string(),
            path: "test.md".into(),
            document_hash: "ignored".to_string(),
            finding_code: "operator-mutation".to_string(),
            finding_rule: None,
            repair_rule: "vault-set".to_string(),
            operation: "replace_body".to_string(),
            field: None,
            expected_old_value: None,
            new_value: None, // missing!
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        assert!(apply_replace_body(content, &change).is_err());
    }

    #[test]
    fn apply_delete_removes_file() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let doc_rel = camino::Utf8PathBuf::from("foo.md");
        std::fs::write(root.join(&doc_rel), "---\ntype: note\n---\n# Foo\n").unwrap();

        let change = PlannedChange {
            change_id: "delete-foo".into(),
            path: doc_rel.clone(),
            document_hash: "irrelevant".into(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "delete_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        };

        let result = apply_delete(root, &change).unwrap();
        assert_eq!(result.path, doc_rel);
        assert!(!root.join(&doc_rel).as_std_path().exists());
    }

    #[test]
    fn apply_delete_missing_source_errors() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-missing-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let doc_rel = camino::Utf8PathBuf::from("missing.md");

        let change = PlannedChange {
            change_id: "delete-missing".into(),
            path: doc_rel.clone(),
            document_hash: "irrelevant".into(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "delete_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        };

        let err = apply_delete(root, &change).unwrap_err();
        match err {
            ApplyError::DeleteSourceMissing { path } => assert_eq!(path, doc_rel),
            other => panic!("expected DeleteSourceMissing, got {other:?}"),
        }
    }

    #[test]
    fn apply_delete_refuses_symlink() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-delete-symlink-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let real_rel = camino::Utf8PathBuf::from("real.md");
        let link_rel = camino::Utf8PathBuf::from("link.md");
        std::fs::write(root.join(&real_rel), "real").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join(&real_rel), root.join(&link_rel)).unwrap();

        #[cfg(unix)]
        {
            let change = PlannedChange {
                change_id: "delete-symlink".into(),
                path: link_rel.clone(),
                document_hash: "irrelevant".into(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "delete_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            };

            let err = apply_delete(root, &change).unwrap_err();
            match err {
                ApplyError::DeleteSourceIsSymlink { path } => assert_eq!(path, link_rel),
                other => panic!("expected DeleteSourceIsSymlink, got {other:?}"),
            }
        }
    }

    #[test]
    fn apply_move_with_force_overwrites_destination() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-move-force-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let src_rel = camino::Utf8PathBuf::from("src.md");
        let dst_rel = camino::Utf8PathBuf::from("dst.md");
        std::fs::write(root.join(&src_rel), "src content").unwrap();
        std::fs::write(root.join(&dst_rel), "dst content").unwrap();

        let change = PlannedChange {
            change_id: "force-test".into(),
            path: src_rel.clone(),
            document_hash: "irrelevant".into(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(dst_rel.clone()),
            link_risk: None,
            warnings: Vec::new(),
            force: true,
            parents: false,
        };

        let result = apply_move(root, &change).unwrap();
        assert_eq!(result.from, src_rel);
        assert_eq!(result.to, dst_rel);
        // dst now has src's content; src is gone.
        assert_eq!(
            std::fs::read_to_string(root.join(&dst_rel)).unwrap(),
            "src content"
        );
        assert!(!root.join(&src_rel).as_std_path().exists());
    }

    #[test]
    fn apply_move_without_force_refuses_existing_destination() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-apply-move-noforce-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let src_rel = camino::Utf8PathBuf::from("src.md");
        let dst_rel = camino::Utf8PathBuf::from("dst.md");
        std::fs::write(root.join(&src_rel), "src").unwrap();
        std::fs::write(root.join(&dst_rel), "dst").unwrap();

        let change = PlannedChange {
            change_id: "noforce-test".into(),
            path: src_rel.clone(),
            document_hash: "irrelevant".into(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(dst_rel.clone()),
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        };

        let err = apply_move(root, &change).unwrap_err();
        match err {
            ApplyError::MoveDestinationExists { destination } => {
                assert_eq!(destination, dst_rel)
            }
            other => panic!("expected MoveDestinationExists, got {other:?}"),
        }
    }

    #[test]
    fn apply_link_rewrites_records_drift_skip_with_reason() {
        let tmp = tempfile::Builder::new()
            .prefix("apply-skip-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        // Backlinker on disk references [[c]], but the plan expects [[a]] → drift.
        std::fs::write(root.join("d.md"), "see [[c]] here\n").unwrap();

        let change = PlannedChange {
            change_id: "test-skip-id".into(),
            path: camino::Utf8PathBuf::from("a.md"),
            document_hash: String::new(),
            finding_code: "move_document".into(),
            finding_rule: None,
            repair_rule: "test".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(camino::Utf8PathBuf::from("b.md")),
            link_risk: Some(crate::standards::repair::link_risk::LinkRisk {
                stem_changed: true,
                directory_changed: false,
                stem_links: vec![crate::standards::repair::link_risk::AffectedLink {
                    source_path: camino::Utf8PathBuf::from("d.md"),
                    raw: "[[a]]".into(),
                    kind: crate::core::LinkKind::Wikilink,
                    source_span: None,
                    rewritten: "[[b]]".into(),
                }],
                path_qualified_wikilinks: vec![],
                markdown_links: vec![],
            }),
            warnings: vec![],
            force: false,
            parents: false,
        };

        let outcome = apply_link_rewrites(root, &change).unwrap();
        assert_eq!(
            outcome.rewritten.len(),
            0,
            "drifted link must not be rewritten"
        );
        assert_eq!(
            outcome.skipped.len(),
            1,
            "drifted link must be recorded as skipped"
        );
        assert_eq!(outcome.skipped[0].file.as_str(), "d.md");
        assert_eq!(outcome.skipped[0].reason.code(), "drifted");
        assert_eq!(
            std::fs::read_to_string(root.join("d.md")).unwrap(),
            "see [[c]] here\n"
        );
    }

    #[test]
    fn link_fail_and_skip_reason_codes_are_stable() {
        assert_eq!(LinkFailReason::ReadFailed.code(), "read_failed");
        assert_eq!(LinkFailReason::WriteFailed.code(), "write_failed");
        assert_eq!(LinkSkipReason::Drifted.code(), "drifted");
        assert_eq!(LinkSkipReason::SourceMissing.code(), "source_missing");
        assert_eq!(
            LinkSkipReason::WouldCorruptFrontmatter.code(),
            "would_corrupt_frontmatter"
        );
    }

    #[test]
    fn cascade_skips_backlink_rewrite_that_would_corrupt_frontmatter() {
        // NRN-141 round 3: a move/delete backlink cascade rewrites `[[...]]`
        // inside OTHER documents' frontmatter with a raw replace + write, no
        // degradation check. Moving a doc to a stem with a YAML-structural byte
        // (`Parent "Two"` — a legal filename) rewrote B's `up: "[[Parent]]"`
        // into unparseable YAML, silently nulling B's whole mapping. The
        // corrupting rewrite is now SKIPPED (B's bytes untouched, reported with
        // a truthful reason — a stale link is detectable and repairable; a
        // corrupted doc is not), while safe rewrites in the same cascade still
        // land.
        let tmp = tempfile::Builder::new()
            .prefix("norn-cascade-fmguard-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let b_doc = "---\nup: \"[[Parent]]\"\n---\nbody\n";
        std::fs::write(root.join("b.md"), b_doc).unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\nsee [[Parent]]\n").unwrap();

        let change = PlannedChange {
            change_id: "move-parent".into(),
            path: camino::Utf8PathBuf::from("Parent.md"),
            document_hash: String::new(),
            finding_code: "move_document".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(camino::Utf8PathBuf::from("Parent \"Two\".md")),
            link_risk: Some(crate::standards::repair::link_risk::LinkRisk {
                stem_changed: true,
                directory_changed: false,
                stem_links: vec![
                    crate::standards::repair::link_risk::AffectedLink {
                        source_path: camino::Utf8PathBuf::from("b.md"),
                        raw: "[[Parent]]".into(),
                        kind: crate::core::LinkKind::Wikilink,
                        source_span: None,
                        rewritten: "[[Parent \"Two\"]]".into(),
                    },
                    crate::standards::repair::link_risk::AffectedLink {
                        source_path: camino::Utf8PathBuf::from("c.md"),
                        raw: "[[Parent]]".into(),
                        kind: crate::core::LinkKind::Wikilink,
                        source_span: None,
                        rewritten: "[[Parent \"Two\"]]".into(),
                    },
                ],
                path_qualified_wikilinks: vec![],
                markdown_links: vec![],
            }),
            warnings: vec![],
            force: false,
            parents: false,
        };

        let outcome = apply_link_rewrites(root, &change).unwrap();
        assert_eq!(
            outcome.skipped.len(),
            1,
            "the frontmatter-corrupting rewrite must be skipped, got {outcome:?}"
        );
        assert_eq!(outcome.skipped[0].file.as_str(), "b.md");
        assert_eq!(
            outcome.skipped[0].reason.code(),
            "would_corrupt_frontmatter"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.md")).unwrap(),
            b_doc,
            "b.md must be byte-identical"
        );
        // The safe body-only rewrite in the same cascade still lands.
        assert_eq!(outcome.rewritten.len(), 1);
        assert_eq!(outcome.rewritten[0].file.as_str(), "c.md");
        assert_eq!(
            std::fs::read_to_string(root.join("c.md")).unwrap(),
            "---\ntype: note\n---\nsee [[Parent \"Two\"]]\n"
        );
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn cascade_safe_stem_rewrites_frontmatter_backlink_as_before() {
        // Control: a structural-char-free stem rewrites a frontmatter wikilink
        // backlink exactly as before (no spurious skip).
        let tmp = tempfile::Builder::new()
            .prefix("norn-cascade-fmsafe-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::write(root.join("b.md"), "---\nup: \"[[Parent]]\"\n---\nbody\n").unwrap();

        let change = make_move_change_with_backlinker("b.md", "[[Parent]]", "[[parent-two]]");
        let outcome = apply_link_rewrites(root, &change).unwrap();
        assert_eq!(outcome.rewritten.len(), 1, "got {outcome:?}");
        assert!(outcome.skipped.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("b.md")).unwrap(),
            "---\nup: \"[[parent-two]]\"\n---\nbody\n"
        );
    }

    #[test]
    fn link_rewrite_outcome_default_has_empty_failed() {
        let o = LinkRewriteOutcome::default();
        assert!(o.failed.is_empty());
    }

    /// Build a minimal `PlannedChange` whose `link_risk` points at one
    /// `AffectedLink` with `source_path` relative to the vault root.
    fn make_move_change_with_backlinker(
        source_path: &str,
        raw: &str,
        rewritten: &str,
    ) -> PlannedChange {
        PlannedChange {
            change_id: "test-id".into(),
            path: camino::Utf8PathBuf::from("a.md"),
            document_hash: String::new(),
            finding_code: "move_document".into(),
            finding_rule: None,
            repair_rule: "test".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(camino::Utf8PathBuf::from("b.md")),
            link_risk: Some(crate::standards::repair::link_risk::LinkRisk {
                stem_changed: true,
                directory_changed: false,
                stem_links: vec![crate::standards::repair::link_risk::AffectedLink {
                    source_path: camino::Utf8PathBuf::from(source_path),
                    raw: raw.into(),
                    kind: crate::core::LinkKind::Wikilink,
                    source_span: None,
                    rewritten: rewritten.into(),
                }],
                path_qualified_wikilinks: vec![],
                markdown_links: vec![],
            }),
            warnings: vec![],
            force: false,
            parents: false,
        }
    }

    #[test]
    fn apply_link_rewrites_skips_source_missing_without_failing() {
        // Backlinker absent on disk -> read NotFound -> Skipped(SourceMissing), NOT failed.
        let tmp = tempfile::Builder::new()
            .prefix("apply-missing-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        // Do NOT create "ghost.md" — it must be absent to trigger NotFound.
        let change = make_move_change_with_backlinker("ghost.md", "[[a]]", "[[b]]");

        let outcome = apply_link_rewrites(root, &change).unwrap();
        assert_eq!(
            outcome.rewritten.len(),
            0,
            "absent backlinker must not be rewritten"
        );
        assert_eq!(
            outcome.failed.len(),
            0,
            "NotFound must not be counted as a failure"
        );
        assert_eq!(
            outcome.skipped.len(),
            1,
            "absent backlinker must be recorded as skipped"
        );
        assert_eq!(outcome.skipped[0].file.as_str(), "ghost.md");
        assert_eq!(outcome.skipped[0].reason.code(), "source_missing");
    }

    #[test]
    fn rewrite_one_backlink_rewrites_and_drifts() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-rowb-rewrite-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let rel = camino::Utf8Path::new("b.md");
        std::fs::write(root.join(rel), "see [[old]] here\n").unwrap();

        // Success path: raw text present → Rewritten; file updated.
        let result = rewrite_one_backlink(root, rel, "[[old]]", "[[new]]");
        assert!(
            matches!(result, LinkAttempt::Rewritten),
            "expected Rewritten, got something else"
        );
        let content = std::fs::read_to_string(root.join(rel)).unwrap();
        assert!(
            content.contains("[[new]]"),
            "file should contain [[new]] after rewrite: {content}"
        );
        assert!(
            !content.contains("[[old]]"),
            "file should not contain [[old]] after rewrite: {content}"
        );

        // Drift path: raw text no longer present → Skipped(Drifted).
        let result2 = rewrite_one_backlink(root, rel, "[[absent]]", "[[whatever]]");
        assert!(
            matches!(result2, LinkAttempt::Skipped(LinkSkipReason::Drifted)),
            "expected Skipped(Drifted) when raw text absent"
        );
    }

    /// (NRN-146) The backlink-cascade write is crash-atomic (temp + rename,
    /// mirroring the Phase A2 content write and `create_document`): the
    /// rewritten content lands correctly AND no `.tmp` sibling is left behind
    /// after a successful rewrite — the observable that distinguishes
    /// `atomic_write` from a bare `fs::write`.
    #[test]
    fn rewrite_one_backlink_is_atomic_and_leaves_no_temp() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-rowb-atomic-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let rel = camino::Utf8Path::new("b.md");
        std::fs::write(root.join(rel), "see [[old]] here\n").unwrap();

        let result = rewrite_one_backlink(root, rel, "[[old]]", "[[new]]");
        assert!(
            matches!(result, LinkAttempt::Rewritten),
            "expected Rewritten, got something else"
        );

        // Content landed via the atomic rename.
        let content = std::fs::read_to_string(root.join(rel)).unwrap();
        assert!(
            content.contains("[[new]]") && !content.contains("[[old]]"),
            "rewrite should be fully applied: {content}"
        );

        // No sibling temp left behind: the temp+rename mechanism cleaned up.
        let leftovers: Vec<String> = std::fs::read_dir(root.as_std_path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.') && n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp sibling should remain after a successful atomic write; found: {leftovers:?}"
        );
    }

    /// (NRN-146 regression) `atomic_write`'s temp-write-then-rename replaces the
    /// destination's inode outright, so the replacement previously got fresh
    /// umask-based permissions rather than inheriting the mode of the file it
    /// replaced — a confidentiality regression for a permission-hardened
    /// backlinker (e.g. 0600) run through an incidental move/delete cascade.
    /// `atomic_write` must stat the existing destination and carry its mode
    /// over to the replacement before the rename.
    #[test]
    #[cfg(unix)]
    fn rewrite_one_backlink_preserves_destination_mode() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::Builder::new()
            .prefix("norn-rowb-mode-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let rel = camino::Utf8Path::new("b.md");
        let full = root.join(rel);
        std::fs::write(&full, "see [[old]] here\n").unwrap();
        std::fs::set_permissions(&full, Permissions::from_mode(0o600)).unwrap();

        let before = std::fs::metadata(&full).unwrap().permissions().mode() & 0o777;
        assert_eq!(before, 0o600, "precondition: file must start at 0600");

        let result = rewrite_one_backlink(root, rel, "[[old]]", "[[new]]");
        assert!(
            matches!(result, LinkAttempt::Rewritten),
            "expected Rewritten, got something else"
        );

        let after = std::fs::metadata(&full).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            after, 0o600,
            "cascade rewrite must preserve the destination's original mode \
             (before: {before:o}, after: {after:o})"
        );
    }

    // (NRN-146) `rewrite_one_backlink` now writes via `atomic_write`: a sibling
    // temp file is created and renamed over the destination, so a read-only
    // *destination file* no longer blocks the write — `rename(2)` doesn't
    // consult the replaced file's permission bits, only the containing
    // directory's. The failure surface this test exercises moved with it: a
    // read-only *directory* still blocks the write, because no new directory
    // entry (the temp file) can be created in it.
    #[test]
    #[cfg(unix)]
    fn rewrite_one_backlink_write_failed_on_readonly_dir() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::Builder::new()
            .prefix("norn-rowb-readonly-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let rel = camino::Utf8Path::new("b.md");
        std::fs::write(root.join(rel), "see [[old]] here\n").unwrap();

        // Make the containing directory read-only (no new entries creatable).
        std::fs::set_permissions(root.as_std_path(), Permissions::from_mode(0o555)).unwrap();

        // Probe: if we can still create a file in the directory, we're root or
        // perms aren't enforced — skip the assertion rather than false-failing.
        let probe_path = root.join(".rowb-perm-probe");
        let probe_writable = std::fs::write(probe_path.as_std_path(), "x").is_ok();
        let _ = std::fs::remove_file(probe_path.as_std_path());
        if probe_writable {
            std::fs::set_permissions(root.as_std_path(), Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let result = rewrite_one_backlink(root, rel, "[[old]]", "[[new]]");

        // Restore permissions so the tempdir's own cleanup can remove it.
        std::fs::set_permissions(root.as_std_path(), Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(result, LinkAttempt::Failed(LinkFailReason::WriteFailed, _)),
            "expected Failed(WriteFailed, _) for a read-only directory"
        );
    }

    #[test]
    fn apply_link_rewrites_records_hard_failure_and_continues() {
        // Two backlinkers: first is a DIRECTORY at its path (read_to_string fails,
        // non-NotFound); second is a real file containing [[a]].
        // Expected: outcome.failed == 1, outcome.rewritten == 1 (loop continued).
        let tmp = tempfile::Builder::new()
            .prefix("apply-hardfail-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();

        // Create a directory at "dir-backlinker.md" to make read_to_string fail
        // with a non-NotFound error (IsADirectory / Other).
        std::fs::create_dir(root.join("dir-backlinker.md")).unwrap();

        // Create a real file that contains the raw link text.
        std::fs::write(root.join("good-backlinker.md"), "see [[a]] here\n").unwrap();

        // Build a change with two AffectedLinks: directory first, good file second.
        let change = PlannedChange {
            change_id: "test-hardfail-id".into(),
            path: camino::Utf8PathBuf::from("a.md"),
            document_hash: String::new(),
            finding_code: "move_document".into(),
            finding_rule: None,
            repair_rule: "test".into(),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(camino::Utf8PathBuf::from("b.md")),
            link_risk: Some(crate::standards::repair::link_risk::LinkRisk {
                stem_changed: true,
                directory_changed: false,
                stem_links: vec![
                    crate::standards::repair::link_risk::AffectedLink {
                        source_path: camino::Utf8PathBuf::from("dir-backlinker.md"),
                        raw: "[[a]]".into(),
                        kind: crate::core::LinkKind::Wikilink,
                        source_span: None,
                        rewritten: "[[b]]".into(),
                    },
                    crate::standards::repair::link_risk::AffectedLink {
                        source_path: camino::Utf8PathBuf::from("good-backlinker.md"),
                        raw: "[[a]]".into(),
                        kind: crate::core::LinkKind::Wikilink,
                        source_span: None,
                        rewritten: "[[b]]".into(),
                    },
                ],
                path_qualified_wikilinks: vec![],
                markdown_links: vec![],
            }),
            warnings: vec![],
            force: false,
            parents: false,
        };

        let outcome = apply_link_rewrites(root, &change).unwrap();
        assert_eq!(
            outcome.failed.len(),
            1,
            "directory-at-path must be recorded as a failure"
        );
        assert_eq!(outcome.failed[0].file.as_str(), "dir-backlinker.md");
        assert_eq!(outcome.failed[0].reason.code(), "read_failed");
        assert_eq!(
            outcome.rewritten.len(),
            1,
            "good backlinker must still be rewritten (loop continued)"
        );
        assert_eq!(outcome.rewritten[0].file.as_str(), "good-backlinker.md");
        assert_eq!(outcome.skipped.len(), 0);
        // Verify the good file was actually rewritten on disk.
        assert_eq!(
            std::fs::read_to_string(root.join("good-backlinker.md")).unwrap(),
            "see [[b]] here\n"
        );
    }

    // ── NRN-141 post-image verification gate ──────────────────────────────────

    #[test]
    fn apply_v2_phantom_remove_is_refused_not_corrupted() {
        // V2: `"\x62ar"` decodes to serde `bar`, but the scanner reads `x62ar`,
        // and the flow value `foo: [a,` / `"b]c",` / `bar: v]` hides an interior
        // `bar: v]` phantom. On the buggy absorb, that phantom is confirmed as
        // `bar`'s span, so `remove bar` deletes to block end and leaves an
        // unclosed flow (`foo: [a,` / `"b]c",`) that re-parses to null — silent
        // corruption. This asserts the durable property (a refusal, no write);
        // the refusal mechanism shifts from the post-image gate (this commit) to
        // the quote-aware absorb guard (NRN-141 absorb fix) — see the guard-
        // specific test below.
        let content = "---\nfoo: [a,\n\"b]c\",\nbar: v]\n\"\\x62ar\": realvalue\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("realvalue")),
            ..make_change("a.md", "bar", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change);
        assert!(
            result.is_err(),
            "unclosed-flow corruption must be refused, got {result:?}"
        );
    }

    #[test]
    fn apply_hashfoo_collection_set_applies_with_key_requoting() {
        // NRN-142: a `"#foo"` key that requires quoting now re-emits quoted on a
        // collection `set`, so the rebuilt line round-trips instead of reading
        // back as a comment (pre-v0.44 corruption; v0.44 refused). The key reads
        // back byte-exact with its new array value.
        let content = "---\n\"#foo\": [a, b]\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["a", "b"])),
            ..make_change("a.md", "#foo", "h1", "set_frontmatter", Some(json!(["x"])))
        };
        let result = apply_change(content, &change).expect("quote-needing key must apply");
        let fm = frontmatter_json(&result);
        assert_eq!(fm["#foo"], json!(["x"]));
    }

    #[test]
    fn apply_colon_key_collection_set_applies_with_key_requoting() {
        // NRN-142: a `"a: b"` key emits a quoted key line on a collection `set`,
        // so the post-image is valid YAML and round-trips (pre-v0.44 produced
        // invalid `a: b: [..]`; v0.44 refused). Sibling fields stay intact.
        let content = "---\n\"a: b\": [x]\nkeep: kept\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["x"])),
            ..make_change("a.md", "a: b", "h1", "set_frontmatter", Some(json!(["y"])))
        };
        let result = apply_change(content, &change).expect("quote-needing key must apply");
        let fm = frontmatter_json(&result);
        assert_eq!(fm["a: b"], json!(["y"]));
        assert_eq!(fm["keep"], json!("kept"));
    }

    #[test]
    fn apply_add_frontmatter_quote_needing_key_applies() {
        // NRN-142: add_frontmatter with a key that requires quoting emits a quoted
        // key line, so the spliced field round-trips and sibling fields survive.
        let content = "---\ntitle: hi\n---\n# body\n";
        let change = make_change("a.md", "#foo", "h1", "add_frontmatter", Some(json!("bar")));
        let result = apply_change(content, &change).expect("quote-needing key add must apply");
        let fm = frontmatter_json(&result);
        assert_eq!(fm["#foo"], json!("bar"));
        assert_eq!(fm["title"], json!("hi"));
    }

    // ── NRN-141/NRN-142 post-image gate regression pins ──────────────────────
    // Direct pins on both refusal arms of `verify_post_image` (the key-quoting
    // vectors that used to exercise them now apply cleanly). A future
    // gate-weakening refactor must fail these.

    #[test]
    fn verify_post_image_refuses_mapping_mismatch() {
        // The composed frontmatter parses fine but is not the intended mapping —
        // the semantic-mismatch arm must fire.
        let mut expected = serde_json::Map::new();
        expected.insert("status".to_string(), json!("done"));
        let content = "---\nstatus: draft\n---\nbody\n";
        let err = verify_post_image(&Utf8PathBuf::from("a.md"), content, &expected).unwrap_err();
        assert!(
            matches!(err, ApplyError::PostImageVerificationFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn verify_post_image_refuses_unparseable_frontmatter() {
        // The composed frontmatter no longer parses — the parse-failure arm must
        // fire regardless of what was intended.
        let expected = serde_json::Map::new();
        let content = "---\nkey: [unclosed\n---\nbody\n";
        let err = verify_post_image(&Utf8PathBuf::from("a.md"), content, &expected).unwrap_err();
        assert!(
            matches!(err, ApplyError::PostImageVerificationFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn verify_post_image_accepts_matching_mapping() {
        // Positive control: a post-image equal to the intended mapping passes.
        let mut expected = serde_json::Map::new();
        expected.insert("status".to_string(), json!("done"));
        let content = "---\nstatus: done\n---\nbody\n";
        verify_post_image(&Utf8PathBuf::from("a.md"), content, &expected)
            .expect("matching post-image must pass");
    }

    #[test]
    fn apply_gate_refuses_key_no_quoting_can_represent() {
        // Integration pin: `PostImageVerificationFailed` still surfaces through
        // `apply_file_changes`. A field name past YAML's 1024-byte simple-key
        // parse limit cannot round-trip at ANY quoting rank, so render_key's
        // terminal fallback splices a key line the reader rejects — the gate
        // must convert that write into a refusal.
        let content = "---\ntitle: hi\n---\nbody\n";
        let long_key = "k".repeat(1100);
        let change = make_change("a.md", &long_key, "h1", "add_frontmatter", Some(json!("v")));
        let result = apply_change(content, &change);
        assert!(
            matches!(result, Err(ApplyError::PostImageVerificationFailed { .. })),
            "unrepresentable key must be refused by the gate, got {result:?}"
        );
    }

    #[test]
    fn apply_post_image_gate_allows_ordinary_multi_op_doc() {
        // The gate must not false-positive: a normal set + remove + add batch
        // composes to a document that re-parses to exactly the intended mapping,
        // so it applies unchanged in behavior.
        let content = "---\ntitle: hi\nstatus: draft\nkind: old\n---\nbody\n";
        let changes = [
            PlannedChange {
                expected_old_value: Some(json!("draft")),
                ..make_change(
                    "a.md",
                    "status",
                    "h1",
                    "set_frontmatter",
                    Some(json!("done")),
                )
            },
            PlannedChange {
                expected_old_value: Some(json!("old")),
                ..make_change("a.md", "kind", "h1", "remove_frontmatter", None)
            },
            make_change("a.md", "author", "h1", "add_frontmatter", Some(json!("me"))),
        ];
        let refs: Vec<&PlannedChange> = changes.iter().collect();
        let result = apply_file_changes(content, &refs).expect("ordinary batch must apply");
        assert_eq!(
            result,
            "---\ntitle: hi\nstatus: done\nauthor: me\n---\nbody\n"
        );
    }

    // ── NRN-141 quote-aware flow absorb ───────────────────────────────────────

    #[test]
    fn apply_v3a_quoted_flow_sibling_set_applies_after_absorb_fix() {
        // V3a: a flow mapping whose closing `}` is shadowed inside `a: "}"`. The
        // buggy absorb stopped at that quoted `}`, exposing the interior
        // `next: 1` as a second `next` candidate and refusing the whole
        // (well-formed) document. Quote-aware absorb steps over the quoted `}`,
        // so the sibling `title` is uniquely located and editable again.
        let content = "---\ntitle: hi\nmeta: {\na: \"}\",\nnext: 1\n}\nnext: 2\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("hi")),
            ..make_change("a.md", "title", "h1", "set_frontmatter", Some(json!("bye")))
        };
        let result = apply_change(content, &change).expect("well-formed sibling set must apply");
        assert_eq!(
            result,
            "---\ntitle: bye\nmeta: {\na: \"}\",\nnext: 1\n}\nnext: 2\n---\nbody\n"
        );
    }

    #[test]
    fn apply_v2_phantom_remove_refused_by_guard_after_absorb_fix() {
        // Companion to `apply_v2_phantom_remove_is_refused_not_corrupted`: after
        // the quote-aware absorb, the interior `bar: v]` is no longer a
        // candidate, so serde key `bar` is unlocatable and the guard empties
        // spans — `remove bar` refuses at the span layer (CannotMinimalEdit),
        // earlier than the post-image gate, never reaching a corrupting edit.
        let content = "---\nfoo: [a,\n\"b]c\",\nbar: v]\n\"\\x62ar\": realvalue\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("realvalue")),
            ..make_change("a.md", "bar", "h1", "remove_frontmatter", None)
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(
            matches!(err, ApplyError::CannotMinimalEdit { .. }),
            "the absorb fix must refuse at the guard, got {err:?}"
        );
    }

    #[test]
    fn apply_set_sibling_of_flow_with_key_line_quote_applies() {
        // NRN-141 round 2 (a): the flow's first item opens a double quote on
        // the KEY line and closes it on the continuation, where the real `]`
        // follows. With the quote state seeded from the key line, the absorb
        // stops at that `]` and the sibling `title` stays uniquely located —
        // an unseeded absorb misread the closing `"` as opening, skipped the
        // `]`, absorbed `title`, and refused this valid document whole.
        let content = "---\nfoo: [\"a,\nb\", c]\ntitle: hi\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("hi")),
            ..make_change("a.md", "title", "h1", "set_frontmatter", Some(json!("bye")))
        };
        let result = apply_change(content, &change).expect("sibling set must apply");
        assert_eq!(result, "---\nfoo: [\"a,\nb\", c]\ntitle: bye\n---\nbody\n");
    }

    #[test]
    fn apply_remove_shielded_closer_phantom_refused_at_guard() {
        // NRN-141 round 2 (b): the key-line quote spans the continuation and
        // shields the `]` in `b]: c`, so the absorb must run through it and
        // keep the interior `phantom: v]` absorbed — leaving the mis-decoded
        // serde key `phantom` (`"\x70hantom"`) with zero candidates and the
        // whole doc refused at the guard (CannotMinimalEdit). The unseeded
        // absorb stopped at the shielded `]`, re-exposed the phantom, and the
        // remove corrupted the doc (caught only by the post-image gate).
        let content = "---\ntags: [\"a,\nb]: c\",\nphantom: v]\n\"\\x70hantom\": real\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("real")),
            ..make_change("a.md", "phantom", "h1", "remove_frontmatter", None)
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(
            matches!(err, ApplyError::CannotMinimalEdit { .. }),
            "the seeded absorb must refuse at the guard, got {err:?}"
        );
    }

    #[test]
    fn apply_set_sibling_of_flow_with_key_line_comment_applies() {
        // NRN-141 round 3 (a): the key line's trailing `# "x` comment is
        // comment text, not content — the `"` inside it must not shield the
        // real `]` on the continuation. With the comment-aware scan, `title`
        // stays uniquely located and editable; the comment-blind scan absorbed
        // it and refused this valid document whole.
        let content = "---\nfoo: [ # \"x\n  a, b ]\ntitle: hi\n---\nbody\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("hi")),
            ..make_change("a.md", "title", "h1", "set_frontmatter", Some(json!("bye")))
        };
        let result = apply_change(content, &change).expect("sibling set must apply");
        assert_eq!(
            result,
            "---\nfoo: [ # \"x\n  a, b ]\ntitle: bye\n---\nbody\n"
        );
    }
}
