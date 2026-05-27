use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use vault_frontmatter::{
    extract_frontmatter, serialize_array_block_for_new_field, serialize_value_preserving_style,
    top_level_property_spans, QuoteError, ValueStyle,
};

use crate::standards::findings::Finding;
use crate::standards::repair::warnings::PlanWarning;
use crate::standards::repair::{
    PlannedChange, RepairPlan, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use crate::standards::summarize;
use crate::standards::summary::Summary;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("unsupported repair plan schema version: expected {expected}, got {got}; regenerate with `vault repair plan`")]
    UnsupportedSchemaVersion { expected: u32, got: u32 },

    #[error("repair plan vault root does not match effective cwd: plan {plan}, cwd {cwd}")]
    VaultRootMismatch { plan: Utf8PathBuf, cwd: Utf8PathBuf },

    #[error("repair plan targets a document not in the index: {path}")]
    UnknownPath { path: Utf8PathBuf },

    #[error("stale repair plan for {path}: expected hash {expected}, found {actual}; regenerate with `vault repair plan`")]
    StaleDocumentHash {
        path: Utf8PathBuf,
        expected: String,
        actual: String,
    },

    #[error("repair plan contains conflicting changes for {path} field {field}")]
    ConflictingFieldChange { path: Utf8PathBuf, field: String },

    #[error("repair plan contains conflicting document hash preconditions for {path}")]
    ConflictingHashes { path: Utf8PathBuf },

    #[error("stale repair plan for {path} field {field}: expected {expected}, found {actual}; regenerate with `vault repair plan`")]
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
            replaced_bodies: Vec::new(),
            warnings: Vec::new(),
            plan_context: RepairApplyPlanContext {
                skipped: plan.summary.skipped.clone(),
            },
            verification: None,
        }
    }

    pub fn with_verification(mut self, findings: &[Finding]) -> Self {
        let summary = summarize(findings);
        self.verification = Some(RepairApplyVerification {
            remaining_findings: summary.findings,
            summary,
        });
        self
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
        "move_document" | "rewrite_link" | "delete_document" | "replace_body" | "create_document"
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

pub fn apply_file_changes(content: &str, changes: &[&PlannedChange]) -> Result<String, ApplyError> {
    let path = if let Some(change) = changes.first() {
        change.path.clone()
    } else {
        return Ok(content.to_string());
    };

    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    let Some(frontmatter_range) = frontmatter_range else {
        return Err(ApplyError::CannotMinimalEdit {
            path,
            reason: "document has no frontmatter".into(),
        });
    };
    if !diagnostics.is_empty() {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: diagnostics
                .iter()
                .map(|d| d.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(frontmatter_value) = frontmatter else {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: "frontmatter could not be parsed".into(),
        });
    };
    let Some(current_object) = frontmatter_value.as_object() else {
        return Err(ApplyError::CannotMinimalEdit {
            path,
            reason: "frontmatter is not a top-level mapping".into(),
        });
    };

    let spans = top_level_property_spans(content, frontmatter_range.clone());

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
                        reason: format!("field {field} not present in frontmatter"),
                    });
                };
                let new_value = change
                    .new_value
                    .as_ref()
                    .ok_or_else(|| ApplyError::MissingNewValue { path: path.clone() })?;

                // Block-sequence arrays: value_range is None; replace the
                // entire line_range (which covers `key:\n  - item\n …`) with
                // a freshly serialized block.
                if span.style == ValueStyle::BlockSequence {
                    if let Value::Array(items) = new_value {
                        let block_items =
                            serialize_array_block_for_new_field(items).map_err(|e| {
                                ApplyError::CannotMinimalEdit {
                                    path: path.clone(),
                                    reason: e.to_string(),
                                }
                            })?;
                        let replacement = format!("{field}:\n{block_items}");
                        edits.push((span.line_range.clone(), replacement));
                        continue;
                    }
                    // Scalar value into a block-sequence field — refuse.
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!(
                            "field {field} has style {:?}; set_frontmatter requires a scalar value",
                            span.style
                        ),
                    });
                }

                let Some(value_range) = span.value_range.clone() else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!(
                            "field {field} has style {:?}; set_frontmatter requires a scalar value",
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
                        reason: format!("field {field} not present in frontmatter"),
                    });
                };
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
                        // readable in Markdown frontmatter.
                        let block_items =
                            serialize_array_block_for_new_field(items).map_err(|e| {
                                ApplyError::CannotMinimalEdit {
                                    path: path.clone(),
                                    reason: e.to_string(),
                                }
                            })?;
                        format!("{leading_newline}{field}:\n{block_items}")
                    }
                    _ => {
                        let rendered =
                            serialize_value_preserving_style(new_value, ValueStyle::Plain)
                                .map_err(|e| ApplyError::CannotMinimalEdit {
                                    path: path.clone(),
                                    reason: e.to_string(),
                                })?;
                        format!("{leading_newline}{field}: {rendered}\n")
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
    Ok(out)
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

/// Reads every file containing an AffectedLink and replaces the raw link
/// text with the precomputed rewritten replacement. Silent skip if the raw
/// doesn't match (file drift between plan and apply); --verify catches any
/// unresolved links.
pub fn apply_link_rewrites(
    cwd: &Utf8Path,
    change: &PlannedChange,
) -> Result<Vec<LinkRewriteResult>, ApplyError> {
    let mut results = Vec::new();
    let risk = match &change.link_risk {
        Some(r) => r,
        None => return Ok(results),
    };
    let all = risk
        .stem_links
        .iter()
        .chain(risk.path_qualified_wikilinks.iter())
        .chain(risk.markdown_links.iter());
    for affected in all {
        let abs = cwd.join(&affected.source_path);
        let original =
            fs::read_to_string(abs.as_std_path()).map_err(|e| ApplyError::CannotMinimalEdit {
                path: affected.source_path.clone(),
                reason: format!("read backlinker failed: {e}"),
            })?;
        let updated = original.replacen(&affected.raw, &affected.rewritten, 1);
        if updated == original {
            continue;
        }
        fs::write(abs.as_std_path(), &updated).map_err(|e| ApplyError::CannotMinimalEdit {
            path: affected.source_path.clone(),
            reason: format!("write backlinker failed: {e}"),
        })?;
        results.push(LinkRewriteResult {
            file: affected.source_path.clone(),
            from: affected.raw.clone(),
            to: affected.rewritten.clone(),
        });
    }
    Ok(results)
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

    let mut diagnostics = Vec::new();
    let (_, frontmatter_range, _, body_start) = extract_frontmatter(content, &mut diagnostics);

    match frontmatter_range {
        Some(_) => {
            // Preserve everything up to (and including) the closing `---\n`,
            // then replace the body.
            let mut result = String::with_capacity(body_start + new_body.len());
            result.push_str(&content[..body_start]);
            result.push_str(new_body);
            Ok(result)
        }
        None => Ok(new_body.to_string()),
    }
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
        let content = "---\nworkspace: \"[[vault-cli]]\"\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[vault-cli]]")),
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
        let content = "---\nworkspace: '[[vault-cli]]'\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[vault-cli]]")),
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
        assert!(matches!(err, ApplyError::CannotMinimalEdit { .. }));
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
    fn apply_add_frontmatter_empty_array_inserts_key_only() {
        let content = "---\ntitle: Foo\n---\nbody\n";
        let change = make_change("a.md", "aliases", "h1", "add_frontmatter", Some(json!([])));
        let result = apply_change(content, &change).unwrap();
        assert!(
            result.contains("aliases:\n"),
            "expected key line with no items: {result}"
        );
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
        };
        assert!(apply_replace_body(content, &change).is_err());
    }

    #[test]
    fn apply_delete_removes_file() {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-apply-delete-")
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
        };

        let result = apply_delete(root, &change).unwrap();
        assert_eq!(result.path, doc_rel);
        assert!(!root.join(&doc_rel).as_std_path().exists());
    }

    #[test]
    fn apply_delete_missing_source_errors() {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-apply-delete-missing-")
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
            .prefix("vault-cli-apply-delete-symlink-")
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
            .prefix("vault-cli-apply-move-force-")
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
            .prefix("vault-cli-apply-move-noforce-")
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
        };

        let err = apply_move(root, &change).unwrap_err();
        match err {
            ApplyError::MoveDestinationExists { destination } => {
                assert_eq!(destination, dst_rel)
            }
            other => panic!("expected MoveDestinationExists, got {other:?}"),
        }
    }
}
