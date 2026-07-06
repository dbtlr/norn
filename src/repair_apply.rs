use std::fs;
use std::time::Duration;

use crate::core::GraphIndex;
use crate::standards::apply::{
    apply_delete, apply_file_changes, apply_link_rewrites, apply_move, apply_rewrite_link,
    changes_by_path, validate_plan_for_apply, ApplyError, CascadeRecord, CreateDocumentResult,
    DeleteResult, LinkAttempt, LinkFailResult, LinkRewriteResult, LinkSkipResult, MoveResult,
    RepairApplyWarning,
};
use crate::standards::{PlannedChange, RepairPlan};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Context passed to `apply_repair_plan` for flags that only affect specific
/// orchestrator passes (currently, `create_document` Pass 1e).
#[derive(Debug, Default, Clone)]
pub struct CreateApplyContext {
    /// When true and a `create_document` change's parent directory is missing,
    /// create the full path via `create_dir_all` instead of refusing.
    /// Threaded from `NewArgs::parents` / the `-p` / `--parents` flag.
    pub parents: bool,
    /// `files.ignore` glob patterns (from `VaultConfig`), re-checked against the
    /// RESOLVED `create_document` path (post `{{seq}}` resolution) before any
    /// write (NRN-138). `synth::build_plan`'s NRN-131 guard only sees the
    /// literal template path (e.g. `logs/{{seq}}.md`) at plan time, so a pattern
    /// that only matches the resolved filename (e.g. `logs/1.md`) would
    /// otherwise slip through. Empty for callers that don't populate it (their
    /// `create_document` changes are not `new`-synthesized, so there is no
    /// build-time guard for this to backstop).
    pub ignore: Vec<String>,
}

pub use crate::standards::apply::RepairApplyReport;

/// Pre-stamp an `op_planned` span per planned change so the applier's per-op
/// `action` events thread under the right span. Returns the `change_id → span`
/// map [`apply_repair_plan_with_context`] expects. Shared by the CLI and MCP
/// set / new / edit paths so span construction stays identical across surfaces.
pub(crate) fn build_op_spans(
    sink: &mut crate::telemetry::EventSink,
    changes: &[PlannedChange],
) -> std::collections::HashMap<String, String> {
    let mut spans = std::collections::HashMap::new();
    for change in changes {
        let span = sink.start_op(&change.operation, change.path.as_str(), None);
        spans.insert(change.change_id.clone(), span);
    }
    spans
}

fn check_hash(
    current_hashes: &std::collections::BTreeMap<Utf8PathBuf, String>,
    change: &PlannedChange,
) -> Result<()> {
    let current_hash = current_hashes.get(&change.path).ok_or_else(|| {
        anyhow::anyhow!(ApplyError::UnknownPath {
            path: change.path.clone(),
        })
    })?;
    if current_hash != &change.document_hash {
        return Err(anyhow::anyhow!(ApplyError::StaleDocumentHash {
            path: change.path.clone(),
            expected: change.document_hash.clone(),
            actual: current_hash.clone(),
        }));
    }
    Ok(())
}

fn count_planned_links(change: &PlannedChange) -> usize {
    change.link_risk.as_ref().map_or(0, |r| {
        r.stem_links.len() + r.path_qualified_wikilinks.len() + r.markdown_links.len()
    })
}

/// Re-attempt every failed backlink rewrite across all cascades, up to
/// `backoff.len()` rounds, sleeping `backoff[round]` BEFORE each round so a
/// transient condition affecting several files clears in one wait. Recovered
/// links migrate from `failed` to `rewritten`; survivors stay `failed` with
/// their latest reason. Returns the recovered `LinkRewriteResult`s so the
/// caller can extend the flat `rewritten_links` list.
///
/// Zero happy-path cost: if no cascade has a failure, returns immediately
/// without sleeping or invoking `attempt`.
fn retry_failed_cascades<F>(
    cascades: &mut [CascadeRecord],
    backoff: &[Duration],
    mut attempt: F,
) -> Vec<LinkRewriteResult>
where
    F: FnMut(&LinkFailResult) -> LinkAttempt,
{
    let mut promoted: Vec<LinkRewriteResult> = Vec::new();
    if cascades.iter().all(|c| c.failed.is_empty()) {
        return promoted;
    }
    for delay in backoff {
        if cascades.iter().all(|c| c.failed.is_empty()) {
            break;
        }
        if !delay.is_zero() {
            std::thread::sleep(*delay);
        }
        for cascade in cascades.iter_mut() {
            let still_failed = std::mem::take(&mut cascade.failed);
            for mut f in still_failed {
                match attempt(&f) {
                    LinkAttempt::Rewritten => {
                        let r = LinkRewriteResult {
                            file: f.file.clone(),
                            from: f.from.clone(),
                            to: f.to.clone(),
                        };
                        cascade.rewritten.push(r.clone());
                        promoted.push(r);
                    }
                    LinkAttempt::Skipped(reason) => {
                        cascade.skipped.push(LinkSkipResult {
                            file: f.file.clone(),
                            from: f.from.clone(),
                            to: f.to.clone(),
                            reason,
                        });
                    }
                    LinkAttempt::Failed(reason, detail) => {
                        f.reason = reason;
                        f.detail = detail;
                        cascade.failed.push(f);
                    }
                }
            }
        }
    }
    promoted
}

/// Thin wrapper over [`apply_repair_plan_with_context`] that forwards a discard
/// sink + empty span map. Production mutators (set/new/applier path) now open a
/// real sink and call the `_with_context` form directly; this remains as a
/// convenience for unit tests that don't exercise the event stream.
#[cfg(test)]
pub fn apply_repair_plan(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &RepairPlan,
    dry_run: bool,
) -> Result<RepairApplyReport> {
    let mut sink = crate::telemetry::EventSink::discard(
        crate::telemetry::IdGen::new(),
        crate::telemetry::Clock::System,
    );
    let spans = std::collections::HashMap::new();
    apply_repair_plan_with_context(
        cwd,
        index,
        plan,
        dry_run,
        &CreateApplyContext::default(),
        &mut sink,
        &spans,
    )
}

/// Emit an action event for a change if its span is known; no-op otherwise.
/// Status is always `applied`; callers pass any extra attributes (e.g. the
/// move destination).
fn emit_op_action(
    sink: &mut crate::telemetry::EventSink,
    spans: &std::collections::HashMap<String, String>,
    change: &PlannedChange,
    sev: crate::telemetry::Severity,
    extra: Vec<(&'static str, String)>,
) {
    use crate::telemetry::event;
    if let Some(span) = spans.get(&change.change_id) {
        let span = span.clone();
        let mut attrs = vec![
            (event::ATTR_OP_KIND, change.operation.clone()),
            (event::ATTR_TARGET, change.path.to_string()),
            (event::ATTR_STATUS, "applied".to_string()),
        ];
        attrs.extend(extra);
        sink.action(
            &span,
            sev,
            &event::action_event_name(&change.operation),
            format!("applied {} on {}", change.operation, change.path),
            attrs,
        );
    }
}

/// Whether `op` is a content-mutating op — one handled by Phase A rather than a
/// lifecycle op (create/delete/move). Delegates to [`content_class`] so there is
/// one definition of the content-op vocabulary.
fn is_content_op(op: &str) -> bool {
    content_class(op).is_some()
}

/// Reject a plan that edits a document AFTER an earlier `delete_document` or
/// `move_document` (its SOURCE) of the same path in plan order. Phase A (content)
/// always runs before Phase B (delete/move), so a plan authored as "delete/move
/// X, then edit X" would be silently reordered into edit-then-vacate — masking an
/// incoherent intent (a document cannot be edited after it has been removed or
/// moved away). The reverse order (edit X, then delete/move X) is the legitimate
/// edit-then-vacate and stays valid. `create_document` at a vacated path is
/// coherent (delete-then-recreate) and is not a content op, so it is not guarded.
fn reject_content_op_after_vacate(plan: &RepairPlan) -> Result<()> {
    // path -> (operation, change_id) of the earlier delete/move that vacated it.
    let mut vacated: std::collections::BTreeMap<&Utf8Path, (&str, &str)> =
        std::collections::BTreeMap::new();
    for change in &plan.changes {
        let op = change.operation.as_str();
        if matches!(op, "delete_document" | "move_document") {
            vacated.insert(change.path.as_path(), (op, change.change_id.as_str()));
        } else if is_content_op(op) {
            if let Some(&(earlier_op, earlier_id)) = vacated.get(change.path.as_path()) {
                return Err(anyhow::anyhow!(ApplyError::ContentOpAfterVacate {
                    path: change.path.clone(),
                    earlier_op: earlier_op.to_string(),
                    earlier_change_id: earlier_id.to_string(),
                }));
            }
        }
    }
    Ok(())
}

/// Crash-atomic write: serialize `contents` to a sibling temp file
/// (`.{stem}.tmp`) then `fs::rename` it into place (atomic on POSIX). A SIGKILL /
/// power loss / `ENOSPC` mid-write truncates only the throwaway temp, never the
/// live document — which is exactly the half-mutation NRN-139 exists to prevent.
/// Best-effort temp cleanup on rename failure. Shared by the Phase A2 content
/// write and the `create_document` pass so there is a single implementation.
fn atomic_write(full: &Utf8Path, contents: &str) -> Result<()> {
    let tmp_path = {
        let mut p = full.to_path_buf();
        let stem = p.file_name().unwrap_or("doc").to_string();
        p.set_file_name(format!(".{stem}.tmp"));
        p
    };
    fs::write(tmp_path.as_std_path(), contents)
        .with_context(|| format!("write temp file {tmp_path}"))?;
    fs::rename(tmp_path.as_std_path(), full.as_std_path()).with_context(|| {
        // Best-effort cleanup on rename failure.
        let _ = fs::remove_file(tmp_path.as_std_path());
        format!("rename temp to {full}")
    })?;
    Ok(())
}

/// Record `path` in `report.changed_files` if not already present (the list is
/// de-duplicated).
fn record_changed_file(report: &mut RepairApplyReport, path: &Utf8Path) {
    if !report.changed_files.iter().any(|p| p.as_path() == path) {
        report.changed_files.push(path.to_path_buf());
    }
}

/// The content-mutating ops for one document, bucketed by class. A document
/// touched by more than one class (canonically: a frontmatter `set` plus an
/// `append_to_section`) composes into ONE read + ONE write — see
/// [`compose_content_ops`].
#[derive(Default)]
struct ContentOps<'a> {
    frontmatter: Vec<&'a PlannedChange>,
    rewrite_links: Vec<&'a PlannedChange>,
    replace_bodies: Vec<&'a PlannedChange>,
    edit_ops: Vec<&'a PlannedChange>,
}

/// The region class a content op mutates. Ordered as the composition chain runs:
/// frontmatter → rewrite_link → replace_body → section-edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentClass {
    Frontmatter,
    RewriteLink,
    ReplaceBody,
    EditOps,
}

/// Classify a plan op into its content region, or `None` for a lifecycle op
/// (create/delete/move). The single source of truth for the content-op
/// vocabulary — shared by the Phase A bucketing dispatch and the vacate guard.
fn content_class(op: &str) -> Option<ContentClass> {
    Some(match op {
        "set_frontmatter" | "remove_frontmatter" | "add_frontmatter" => ContentClass::Frontmatter,
        "rewrite_link" => ContentClass::RewriteLink,
        "replace_body" => ContentClass::ReplaceBody,
        "str_replace"
        | "replace_section"
        | "append_to_section"
        | "delete_section"
        | "insert_before_heading"
        | "insert_after_heading" => ContentClass::EditOps,
        _ => return None,
    })
}

/// One transform invocation within a composed file: the planned changes it
/// covers and whether it altered the content it was handed. Frontmatter and edit
/// ops each run as ONE grouped transform (so `changes` may hold several); link
/// rewrites and body replacements run per-change. `changed` drives no-op
/// suppression — a byte-identical transform emits no action and adds no
/// `changed_files` entry.
#[derive(Debug)]
struct ComposedUnit<'a> {
    changes: Vec<&'a PlannedChange>,
    class: ContentClass,
    changed: bool,
}

/// The composed result for one document: the fully-transformed content plus the
/// per-transform record. `content == original` exactly when the whole
/// composition was a no-op (no unit changed).
#[derive(Debug)]
struct ComposedFile<'a> {
    content: String,
    units: Vec<ComposedUnit<'a>>,
}

/// Chain every content-mutating op for one document into a single composed
/// string. Classes apply in the fixed region order frontmatter → rewrite_link →
/// replace_body → section-edits against the *evolving* content (each transform
/// re-splits the frontmatter boundary from the string it is handed, so chaining
/// — not byte-range merging against the original — is what keeps downstream
/// offsets correct). Any transform failure propagates so the caller can abort the
/// whole content phase before writing anything (NRN-139: no half-mutated file).
fn compose_content_ops<'a>(
    path: &Utf8Path,
    ops: ContentOps<'a>,
    original: &str,
) -> Result<ComposedFile<'a>> {
    let ContentOps {
        frontmatter,
        rewrite_links,
        replace_bodies,
        edit_ops,
    } = ops;
    let mut content = original.to_string();
    let mut units: Vec<ComposedUnit<'a>> = Vec::new();

    // Frontmatter: one grouped transform over all set/remove/add changes.
    if !frontmatter.is_empty() {
        let updated = apply_file_changes(&content, &frontmatter)?;
        let changed = updated != content;
        units.push(ComposedUnit {
            changes: frontmatter,
            class: ContentClass::Frontmatter,
            changed,
        });
        content = updated;
    }

    // rewrite_link: per-change whole-file scans (matches the prior per-pass
    // granularity so each rewrite's changed/no-op status is tracked independently).
    for &change in &rewrite_links {
        let updated = apply_rewrite_link(&content, change)?;
        let changed = updated != content;
        units.push(ComposedUnit {
            changes: vec![change],
            class: ContentClass::RewriteLink,
            changed,
        });
        content = updated;
    }

    // replace_body: per-change whole-body rewrites.
    for &change in &replace_bodies {
        let updated = crate::standards::apply::apply_replace_body(&content, change)?;
        let changed = updated != content;
        units.push(ComposedUnit {
            changes: vec![change],
            class: ContentClass::ReplaceBody,
            changed,
        });
        content = updated;
    }

    // Section/body edit ops: one grouped transform running the shared
    // `edit::transform` engine in plan order against the evolving body.
    if !edit_ops.is_empty() {
        let decoded: Vec<crate::edit::ops::EditOp> = edit_ops
            .iter()
            .map(|c| {
                let payload = c
                    .new_value
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("edit op missing payload for {}", c.path))?;
                serde_json::from_value::<crate::edit::ops::EditOp>(payload.clone())
                    .map_err(|e| anyhow::anyhow!("edit op decode for {}: {e}", c.path))
            })
            .collect::<Result<Vec<_>>>()?;
        let updated = crate::standards::apply::apply_edit_ops(&content, &decoded, path)?;
        let changed = updated != content;
        units.push(ComposedUnit {
            changes: edit_ops,
            class: ContentClass::EditOps,
            changed,
        });
        content = updated;
    }

    // NRN-141: a content rewrite (rewrite_link rewrites `[[...]]` anywhere in
    // the file, frontmatter values included) can break the frontmatter without
    // going through the frontmatter editor's own post-image gate. Refuse the
    // document unwritten if the composition degraded a previously-parsing
    // frontmatter block.
    if content != original {
        crate::standards::apply::verify_frontmatter_not_degraded(path, original, &content)?;
    }

    Ok(ComposedFile { content, units })
}

/// Like `apply_repair_plan` but with additional context for `create_document`
/// operations (e.g., the `-p` / `--parents` flag) and a telemetry sink + a
/// `change_id -> op span id` map for emitting per-action events.
pub fn apply_repair_plan_with_context(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &RepairPlan,
    dry_run: bool,
    ctx: &CreateApplyContext,
    sink: &mut crate::telemetry::EventSink,
    spans: &std::collections::HashMap<String, String>,
) -> Result<RepairApplyReport> {
    use crate::telemetry::event;
    use crate::telemetry::Severity;
    validate_plan_for_apply(cwd, plan)?;
    // Guard incoherent plans BEFORE any write: a content op cannot target a path
    // that an earlier delete/move in the same plan already vacated (NRN-139).
    reject_content_op_after_vacate(plan)?;

    // `changes_by_path` validates the frontmatter changes (rejecting conflicting
    // field edits / divergent hashes / unsupported ops) and skips the
    // orchestrator-pass ops. We call it purely as that validation gate — Phase A
    // below re-buckets the content ops itself so all four content classes compose
    // into ONE read + ONE write per document.
    changes_by_path(plan)?;

    let mut report = RepairApplyReport::new(plan, dry_run);

    let current_hashes: std::collections::BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();

    // ── Phase A: content composition (NRN-139) ────────────────────────────────
    // Every content-mutating class (frontmatter set/remove/add, rewrite_link,
    // replace_body, and the section/body edit ops) is FILE-MAJOR: a document
    // touched by more than one class is read once, transformed by chaining the
    // classes in the fixed region order frontmatter → rewrite_link →
    // replace_body → section-edits, and written once. This closes the latent
    // half-mutation window where a frontmatter write in one pass could land
    // before a later pass's failing edit aborted the plan.
    //
    // A1 (compute) runs every transform under whole-doc CAS BEFORE any write. A
    // hash drift or a failing transform (missing heading, non-unique str_replace,
    // etc.) aborts the whole content phase here, before the first byte is written.

    // Bucket content ops per document, recording first-appearance path order for
    // determinism. Lifecycle ops (create/delete/move) classify as `None` and are
    // handled by the Phase B passes.
    let mut content_order: Vec<Utf8PathBuf> = Vec::new();
    let mut content_ops: std::collections::BTreeMap<Utf8PathBuf, ContentOps> =
        std::collections::BTreeMap::new();
    for change in &plan.changes {
        let Some(class) = content_class(&change.operation) else {
            continue;
        };
        if !content_ops.contains_key(&change.path) {
            content_order.push(change.path.clone());
        }
        let bucket = content_ops.entry(change.path.clone()).or_default();
        match class {
            ContentClass::Frontmatter => bucket.frontmatter.push(change),
            ContentClass::RewriteLink => bucket.rewrite_links.push(change),
            ContentClass::ReplaceBody => bucket.replace_bodies.push(change),
            ContentClass::EditOps => bucket.edit_ops.push(change),
        }
    }

    // A1: compute every file's composed content up front.
    let mut composed: Vec<(Utf8PathBuf, ComposedFile)> = Vec::with_capacity(content_order.len());
    for path in &content_order {
        // Drain the bucket so its `Vec<&PlannedChange>`s move into the composed
        // units instead of being cloned.
        let ops = content_ops
            .remove(path)
            .expect("content_order paths are keys of content_ops");
        // Hash-check EVERY content change for this path (not just the first): a
        // divergent same-path hash must abort. There is exactly one current hash
        // per path, so when the changes agree this is equivalent to a single
        // check — collapsing the per-pass checks is behavior-preserving.
        for change in ops
            .frontmatter
            .iter()
            .chain(ops.rewrite_links.iter())
            .chain(ops.replace_bodies.iter())
            .chain(ops.edit_ops.iter())
            .copied()
        {
            check_hash(&current_hashes, change)?;
        }
        let absolute_path = cwd.join(path);
        let original =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let file = compose_content_ops(path, ops, &original)?;
        composed.push((path.clone(), file));
    }

    // A2: write every changed file (one write per document) and record report +
    // telemetry. All computation/validation already succeeded, so no partial
    // write is possible.
    for (path, file) in &composed {
        let overall_changed = file.units.iter().any(|u| u.changed);

        // changed_files: the union of the prior passes' pushes — present once iff
        // the composed file actually changed.
        if overall_changed {
            record_changed_file(&mut report, path);
        }

        // Per-class report forecasts, preserving the prior per-pass shapes.
        for unit in &file.units {
            match unit.class {
                ContentClass::RewriteLink => {
                    let change = unit.changes[0];
                    // Dry-run forecasts every rewrite (with from/to) regardless of
                    // whether it would change the file; apply records only the
                    // rewrites that actually landed.
                    if dry_run || unit.changed {
                        if let (Some(from), Some(to)) = (
                            change.expected_old_value.as_ref().and_then(|v| v.as_str()),
                            change.new_value.as_ref().and_then(|v| v.as_str()),
                        ) {
                            report.rewritten_links.push(LinkRewriteResult {
                                file: change.path.clone(),
                                from: from.to_string(),
                                to: to.to_string(),
                            });
                            if dry_run {
                                record_changed_file(&mut report, &change.path);
                            }
                        }
                    }
                }
                ContentClass::ReplaceBody => {
                    let change = unit.changes[0];
                    // replace_body is always recorded in replaced_bodies (both
                    // modes); dry-run additionally forces changed_files.
                    if dry_run {
                        record_changed_file(&mut report, &change.path);
                    }
                    report.replaced_bodies.push(change.path.clone());
                }
                ContentClass::Frontmatter | ContentClass::EditOps => {}
            }
        }

        if dry_run || !overall_changed {
            continue;
        }

        let absolute_path = cwd.join(path);
        atomic_write(&absolute_path, &file.content)
            .with_context(|| format!("write {absolute_path}"))?;
        // One action per contributing change_id: a unit that was a byte-identical
        // no-op emits nothing.
        for unit in &file.units {
            if !unit.changed {
                continue;
            }
            for change in unit.changes.iter().copied() {
                emit_op_action(sink, spans, change, Severity::Info, Vec::new());
            }
        }
    }

    // ── Phase B: lifecycle post-passes ────────────────────────────────────────
    // create_document, delete_document (+ --rewrite-to cascade), and
    // move_document (+ backlink rewrites + retry) run AFTER the content phase.
    // They are whole-file ops with cross-FILE cascades that must resolve from the
    // now-settled content state.

    // Delete pass: sequenced after the content phase (so rewrite_link content is
    // settled and --rewrite-to redirects backlinks before the target disappears)
    // and before move_document (so delete-then-move on the same path is
    // impossible).
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "delete_document")
    {
        check_hash(&current_hashes, change)?;

        // Apply link rewrites if link_risk is attached (--rewrite-to case). This
        // runs BEFORE the delete so links can be rewritten in source docs.
        if change.link_risk.is_some() {
            let planned = count_planned_links(change);
            if dry_run {
                let mut rewritten: Vec<LinkRewriteResult> = Vec::new();
                if let Some(risk) = &change.link_risk {
                    for affected in risk
                        .stem_links
                        .iter()
                        .chain(risk.path_qualified_wikilinks.iter())
                        .chain(risk.markdown_links.iter())
                    {
                        rewritten.push(LinkRewriteResult {
                            file: affected.source_path.clone(),
                            from: affected.raw.clone(),
                            to: affected.rewritten.clone(),
                        });
                    }
                }
                report.rewritten_links.extend(rewritten.clone());
                report.cascades.push(CascadeRecord {
                    source_path: change.path.clone(),
                    planned,
                    rewritten,
                    skipped: Vec::new(),
                    failed: Vec::new(),
                });
            } else {
                let outcome = apply_link_rewrites(cwd, change)?;
                report.rewritten_links.extend(outcome.rewritten.clone());
                report.cascades.push(CascadeRecord {
                    source_path: change.path.clone(),
                    planned,
                    rewritten: outcome.rewritten,
                    skipped: outcome.skipped,
                    failed: outcome.failed,
                });
            }
        }

        // The actual file removal.
        if !dry_run {
            let result = apply_delete(cwd, change)?;
            report.deleted_documents.push(result);
            emit_op_action(sink, spans, change, Severity::Info, Vec::new());
        } else {
            report.deleted_documents.push(DeleteResult {
                path: change.path.clone(),
            });
        }
    }

    // Pass 1e: create_document operations. Sequenced after all mutation passes
    // (set/remove frontmatter, rewrite_link, delete, replace_body) and before
    // move_document, so we never move a document that was just created and then
    // immediately renamed.
    //
    // NRN-101: `{{seq}}` ids allocated earlier in THIS plan but not yet on disk
    // (dry-run never writes; apply writes just-in-time) are tracked here so a
    // later seq-create in the same plan doesn't re-predict an id already claimed
    // by an earlier one. Without this, a dry-run of a multi-create plan reports
    // duplicate ids while apply produces distinct ones.
    let mut allocated_this_plan: Vec<Utf8PathBuf> = Vec::new();
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "create_document")
    {
        // create_document has no document_hash precondition (the file doesn't
        // exist yet). Skip the hash-check used by other passes.

        // NRN-101: resolve an incremental `{{seq}}` token to the next id via
        // filesystem max+1. This runs under the mutation lock the caller holds
        // around apply, so two concurrent creates serialize — the second observes
        // the first's file and gets a distinct sequential id. No new lock is
        // introduced: the NRN-87 warm daemon will own this same boundary and can
        // swap the impl behind it untouched.
        let resolved_path = if crate::seq_alloc::has_seq(&change.path) {
            let dir = cwd.join(change.path.parent().unwrap_or_else(|| Utf8Path::new("")));
            let mut siblings = crate::seq_alloc::dir_file_names(&dir)
                .with_context(|| format!("create_document: scan {dir} for {{{{seq}}}}"))?;
            // Fold in ids already claimed by earlier same-directory seq-creates
            // in this plan (not necessarily on disk yet).
            for prior in &allocated_this_plan {
                if prior.parent() == change.path.parent() {
                    if let Some(name) = prior.file_name() {
                        siblings.push(name.to_string());
                    }
                }
            }
            let resolved = crate::seq_alloc::resolve_seq(&change.path, &siblings);
            // `{{seq}}` is only resolvable once, in the file name. If any token
            // survives (it appeared in a directory component, or more than once),
            // refuse rather than write a path with a literal `{{seq}}` in it. The
            // `new` path already refuses this at generate time; this backstops
            // hand-authored migrate plans that bypass `generate_path`.
            if crate::seq_alloc::has_seq(&resolved) {
                return Err(anyhow::anyhow!(
                    "create_document: `{{{{seq}}}}` is only supported once, in the file name of a rule target: {}",
                    change.path
                ));
            }
            allocated_this_plan.push(resolved.clone());
            resolved
        } else {
            change.path.clone()
        };

        // NRN-138: re-check `files.ignore` against the RESOLVED path. For a
        // literal (non-`{{seq}}`) path this repeats `synth::build_plan`'s
        // NRN-131 guard (a no-op — that guard already refused an ignored path
        // before a plan reached the applier); for a `{{seq}}`-templated path
        // this is the first check the resolved filename ever sees, since
        // `{{seq}}` only becomes concrete here. Must run BEFORE any write.
        if crate::graph::is_ignored(&resolved_path, &ctx.ignore) {
            return Err(anyhow::anyhow!(
                "create_document: resolved path {} matches `files.ignore` — refusing to create",
                resolved_path
            ));
        }

        let nv = change.new_value.as_ref().ok_or_else(|| {
            anyhow::anyhow!(ApplyError::MissingNewValue {
                path: resolved_path.clone(),
            })
        })?;
        let fm_obj = nv
            .get("frontmatter")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                anyhow::anyhow!("create_document: missing or non-object frontmatter in new_value")
            })?;
        let body = nv.get("body").and_then(|v| v.as_str()).unwrap_or("");

        let full = cwd.join(&resolved_path);

        // Pre-flight (defense in depth — preflight/synth should have caught these).
        if full.as_std_path().exists() && !change.force {
            return Err(anyhow::anyhow!(
                "create_document: destination already exists (use --force to overwrite): {}",
                resolved_path
            ));
        }
        if let Some(parent) = full.parent() {
            if !parent.as_std_path().exists() {
                if ctx.parents {
                    if !dry_run {
                        fs::create_dir_all(parent.as_std_path()).with_context(|| {
                            format!("create_document: create parent dirs for {}", resolved_path)
                        })?;
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "create_document: parent directory does not exist (use -p / --parents to auto-create): {}",
                        resolved_path
                    ));
                }
            }
        }

        if dry_run {
            report.created_documents.push(CreateDocumentResult {
                path: resolved_path.clone(),
            });
            if !report.changed_files.contains(&resolved_path) {
                report.changed_files.push(resolved_path.clone());
            }
            continue;
        }

        // Serialize the document.
        let fm_btree: std::collections::BTreeMap<String, serde_json::Value> =
            fm_obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let contents = crate::frontmatter::serialize_new_document(&fm_btree, body)
            .map_err(|e| anyhow::anyhow!("create_document: serialize failed: {e}"))?;

        // Atomic write: write to a sibling temp file, then rename into place.
        atomic_write(&full, &contents)
            .with_context(|| format!("create_document: write {resolved_path}"))?;

        report.created_documents.push(CreateDocumentResult {
            path: resolved_path.clone(),
        });
        if !report.changed_files.contains(&resolved_path) {
            report.changed_files.push(resolved_path.clone());
        }
        // Audit the action against the resolved path, not the `{{seq}}` template
        // (NRN-101). Same change_id, so it still hangs off the op_planned span.
        let resolved_change = PlannedChange {
            path: resolved_path.clone(),
            ..change.clone()
        };
        emit_op_action(sink, spans, &resolved_change, Severity::Info, Vec::new());
    }

    // Collect move_document changes for passes 2 and 3.
    let move_changes: Vec<&PlannedChange> = plan
        .changes
        .iter()
        .filter(|c| c.operation == "move_document")
        .collect();

    // Pass 2: filesystem moves.
    let mut moves: Vec<MoveResult> = Vec::new();
    for change in &move_changes {
        if dry_run {
            if let Some(destination) = change.destination.as_ref() {
                moves.push(MoveResult {
                    from: change.path.clone(),
                    to: destination.clone(),
                });
            }
        } else {
            moves.push(apply_move(cwd, change)?);
            let extra = change
                .destination
                .as_ref()
                .map(|dst| vec![(event::ATTR_TARGET_TO, dst.to_string())])
                .unwrap_or_default();
            emit_op_action(sink, spans, change, Severity::Info, extra);
        }
    }

    // Pass 3: link rewrites (only after every move succeeded).
    let mut rewrites: Vec<LinkRewriteResult> = Vec::new();
    for change in &move_changes {
        let planned = count_planned_links(change);
        if dry_run {
            // Dry-run forecast: every planned link is reported as a would-be
            // rewrite (applied == planned, skipped == 0).
            let mut rewritten: Vec<LinkRewriteResult> = Vec::new();
            if let Some(risk) = &change.link_risk {
                for affected in risk
                    .stem_links
                    .iter()
                    .chain(risk.path_qualified_wikilinks.iter())
                    .chain(risk.markdown_links.iter())
                {
                    rewritten.push(LinkRewriteResult {
                        file: affected.source_path.clone(),
                        from: affected.raw.clone(),
                        to: affected.rewritten.clone(),
                    });
                }
            }
            rewrites.extend(rewritten.clone());
            report.cascades.push(CascadeRecord {
                source_path: change.path.clone(),
                planned,
                rewritten,
                skipped: Vec::new(),
                failed: Vec::new(),
            });
        } else {
            let outcome = apply_link_rewrites(cwd, change)?;
            rewrites.extend(outcome.rewritten.clone());
            report.cascades.push(CascadeRecord {
                source_path: change.path.clone(),
                planned,
                rewritten: outcome.rewritten,
                skipped: outcome.skipped,
                failed: outcome.failed,
            });
        }
    }

    // Cleanup-retry pass: transient FS failures get up to 3 retry rounds
    // (100/300/900ms backoff) before they're left as dangling links.
    if !dry_run {
        let backoff = [
            Duration::from_millis(100),
            Duration::from_millis(300),
            Duration::from_millis(900),
        ];
        // Coarse retry signal: count failures BEFORE the retry pass so we can
        // emit a single `norn.retry` event capturing that the retry pass ran
        // (and added latency). Per-round events are a future refinement.
        let failed_before: usize = report.cascades.iter().map(|c| c.failed.len()).sum();
        let recovered = retry_failed_cascades(&mut report.cascades, &backoff, |f| {
            crate::standards::apply::rewrite_one_backlink(cwd.as_path(), &f.file, &f.from, &f.to)
        });
        report.rewritten_links.extend(recovered);

        if failed_before > 0 {
            sink.lifecycle(
                event::EVENT_RETRY,
                Severity::Warn,
                format!("retried {failed_before} failed backlink rewrite(s) over up to 3 rounds"),
                vec![(event::ATTR_RETRY_ROUND, "3".to_string())],
            );
        }

        // Cascade rewrite_link actions emitted from FINAL settled state (after
        // retry), so a promoted-then-clean link reports as applied, not failed.
        // The cascade source path owns the op span (move_document /
        // delete_document); rewrite_link actions hang off that span.
        let mut cascade_span: std::collections::HashMap<&camino::Utf8Path, &str> =
            std::collections::HashMap::new();
        for change in plan
            .changes
            .iter()
            .filter(|c| c.operation == "move_document" || c.operation == "delete_document")
        {
            if let Some(s) = spans.get(&change.change_id) {
                cascade_span.insert(change.path.as_path(), s.as_str());
            }
        }

        for cascade in &report.cascades {
            let Some(span) = cascade_span.get(cascade.source_path.as_path()).copied() else {
                continue;
            };
            for r in &cascade.rewritten {
                sink.action(
                    span,
                    Severity::Info,
                    &event::action_event_name("rewrite_link"),
                    format!("rewrote {} ({} -> {})", r.file, r.from, r.to),
                    vec![
                        (event::ATTR_OP_KIND, "rewrite_link".to_string()),
                        (event::ATTR_TARGET, r.file.to_string()),
                        (event::ATTR_LINK_FROM, r.from.clone()),
                        (event::ATTR_LINK_TO, r.to.clone()),
                        (event::ATTR_STATUS, "applied".to_string()),
                    ],
                );
            }
            for s in &cascade.skipped {
                sink.action(
                    span,
                    Severity::Warn,
                    &event::action_event_name("rewrite_link"),
                    format!(
                        "skipped {} ({} -> {}): {}",
                        s.file,
                        s.from,
                        s.to,
                        s.reason.code()
                    ),
                    vec![
                        (event::ATTR_OP_KIND, "rewrite_link".to_string()),
                        (event::ATTR_TARGET, s.file.to_string()),
                        (event::ATTR_LINK_FROM, s.from.clone()),
                        (event::ATTR_LINK_TO, s.to.clone()),
                        (event::ATTR_STATUS, "skipped".to_string()),
                        (event::ATTR_REASON_CODE, s.reason.code().to_string()),
                    ],
                );
            }
            for f in &cascade.failed {
                sink.action(
                    span,
                    Severity::Error,
                    &event::action_event_name("rewrite_link"),
                    format!(
                        "failed {} ({} -> {}): {} {}",
                        f.file,
                        f.from,
                        f.to,
                        f.reason.code(),
                        f.detail
                    ),
                    vec![
                        (event::ATTR_OP_KIND, "rewrite_link".to_string()),
                        (event::ATTR_TARGET, f.file.to_string()),
                        (event::ATTR_LINK_FROM, f.from.clone()),
                        (event::ATTR_LINK_TO, f.to.clone()),
                        (event::ATTR_STATUS, "failed".to_string()),
                        (event::ATTR_REASON_CODE, f.reason.code().to_string()),
                        (event::ATTR_REASON_MESSAGE, f.detail.clone()),
                    ],
                );
            }
        }
    }

    let warnings: Vec<RepairApplyWarning> = move_changes
        .iter()
        .flat_map(|c| {
            c.warnings.iter().map(|w| RepairApplyWarning {
                path: c.path.clone(),
                warning: w.clone(),
            })
        })
        .collect();

    report.moved_files = moves;
    // Extend (not replace): Pass 1b may have already populated rewritten_links
    // with rewrite_link results; Pass 3 appends move-induced backlink rewrites.
    report.rewritten_links.extend(rewrites);
    report.warnings = warnings;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::{
        PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary,
        REPAIR_PLAN_SCHEMA_VERSION,
    };

    /// A throwaway in-memory sink for tests that exercise the orchestrator
    /// directly (no event assertions here — those live in the integration test).
    fn discard_sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-05-29T00:00:00.000Z"),
        )
    }

    /// Build a minimal on-disk vault with a single document and return the
    /// temp dir, the vault root as a `Utf8PathBuf`, and the `GraphIndex`.
    fn make_vault_with_doc(
        prefix: &str,
        doc_rel: &str,
        body: &str,
    ) -> (tempfile::TempDir, camino::Utf8PathBuf, GraphIndex, String) {
        let tmp = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        // Write a minimal vault config so build_index doesn't complain.
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join(doc_rel), body).unwrap();
        let index = crate::graph::build_index(&root).unwrap();
        let hash = index
            .documents
            .iter()
            .find(|d| d.path == doc_rel)
            .unwrap()
            .hash
            .clone();
        (tmp, root, index, hash)
    }

    fn delete_plan(vault_root: &camino::Utf8PathBuf, doc_rel: &str, hash: &str) -> RepairPlan {
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "delete-foo".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
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
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        }
    }

    fn rewrite_link_plan(
        vault_root: &camino::Utf8PathBuf,
        doc_rel: &str,
        hash: &str,
        old_target: &str,
        new_target: &str,
    ) -> RepairPlan {
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "rewrite-test".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: "link-target-missing".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "rewrite_link".into(),
                field: None,
                expected_old_value: Some(serde_json::json!(old_target)),
                new_value: Some(serde_json::json!(new_target)),
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        }
    }

    fn remove_field_plan(
        vault_root: &camino::Utf8PathBuf,
        doc_rel: &str,
        hash: &str,
        field: &str,
        expected_old: serde_json::Value,
    ) -> RepairPlan {
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "remove-test".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: "operator-mutation".into(),
                finding_rule: None,
                repair_rule: "vault-set".into(),
                operation: "remove_frontmatter".into(),
                field: Some(field.to_string()),
                expected_old_value: Some(expected_old),
                new_value: None,
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        }
    }

    #[test]
    fn remove_last_frontmatter_field_applies_through_compose_path() {
        // NRN-141 round 3: removing the ONLY frontmatter field leaves an empty
        // `---\n---\n` block, which re-parses as YAML null. verify_post_image
        // accepts that as the empty mapping, but the compose seam's degradation
        // check tested raw is-a-mapping on the composed side and refused this
        // perfectly valid edit whole. The composed side must accept
        // mapping-or-empty-block.
        let doc = "---\nstatus: draft\n---\nbody\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-orch-remove-last-", "doc.md", doc);
        let plan = remove_field_plan(&root, "doc.md", &hash, "status", serde_json::json!("draft"));

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false)
            .expect("removing the last frontmatter field must apply");
        assert_eq!(report.changed_files.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("doc.md")).unwrap(),
            "---\n---\nbody\n"
        );
    }

    #[test]
    fn rewrite_link_that_breaks_frontmatter_is_refused_unwritten() {
        // NRN-141: apply_rewrite_link rewrites `[[...]]` across the WHOLE file,
        // frontmatter values included, without the frontmatter editor's own
        // post-image gate. A new target carrying YAML-structural bytes (a `"`
        // inside the quoted-wikilink convention) turns `up: "[[Parent]]"` into
        // `up: "[[Parent "Two"]]"` — unparseable YAML that collapses the whole
        // mapping to null on the next read. The compose seam's parse-degradation
        // check must refuse the document unwritten.
        let doc = "---\nup: \"[[Parent]]\"\n---\nsee [[Parent]]\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-orch-rewrite-fmbreak-", "doc.md", doc);
        let plan = rewrite_link_plan(&root, "doc.md", &hash, "Parent", "Parent \"Two\"");

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false)
            .expect_err("a frontmatter-breaking rewrite must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("doc.md") && msg.contains("frontmatter"),
            "refusal must name the doc and the broken frontmatter, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("doc.md")).unwrap(),
            doc,
            "file must be byte-identical after the refusal"
        );
    }

    #[test]
    fn rewrite_link_benign_frontmatter_rewrite_still_applies() {
        // The degradation check is deliberately weaker than mapping-equality:
        // a rewrite legitimately changes frontmatter values, so a structural-
        // char-free target applies cleanly (frontmatter and body both rewritten).
        let doc = "---\nup: \"[[Parent]]\"\n---\nsee [[Parent]]\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-orch-rewrite-benign-", "doc.md", doc);
        let plan = rewrite_link_plan(&root, "doc.md", &hash, "Parent", "parent-two");

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();
        assert_eq!(report.changed_files.len(), 1);
        let written = std::fs::read_to_string(root.join("doc.md")).unwrap();
        assert_eq!(
            written,
            "---\nup: \"[[parent-two]]\"\n---\nsee [[parent-two]]\n"
        );
        // The rewritten frontmatter still parses to a mapping.
        let mut diags = Vec::new();
        let (fm, _, _, _) = crate::frontmatter::extract_frontmatter(&written, &mut diags);
        assert!(diags.is_empty());
        assert_eq!(
            fm.unwrap().get("up"),
            Some(&serde_json::json!("[[parent-two]]"))
        );
    }

    #[test]
    fn delete_pass_removes_file() {
        let (_tmp, root, index, hash) = make_vault_with_doc(
            "norn-orch-delete-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        let plan = delete_plan(&root, "foo.md", &hash);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        assert_eq!(report.deleted_documents.len(), 1);
        assert_eq!(
            report.deleted_documents[0].path,
            camino::Utf8PathBuf::from("foo.md")
        );
        assert!(!root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_dry_run_does_not_remove_file() {
        let (_tmp, root, index, hash) = make_vault_with_doc(
            "norn-orch-delete-dry-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        let plan = delete_plan(&root, "foo.md", &hash);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ true).unwrap();

        // Dry run: entry is recorded but file must still exist.
        assert_eq!(report.deleted_documents.len(), 1);
        assert_eq!(
            report.deleted_documents[0].path,
            camino::Utf8PathBuf::from("foo.md")
        );
        assert!(root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_rejects_stale_hash() {
        let (_tmp, root, index, _hash) = make_vault_with_doc(
            "norn-orch-delete-stale-",
            "foo.md",
            "---\ntype: note\n---\n# Foo\n",
        );
        // Use an intentionally wrong hash.
        let plan = delete_plan(&root, "foo.md", "definitely-wrong-hash");

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stale") || msg.contains("hash"),
            "expected stale-hash error, got: {msg}"
        );
        // File must be untouched.
        assert!(root.join("foo.md").as_std_path().exists());
    }

    #[test]
    fn delete_pass_with_rewrite_to_rewrites_then_deletes() {
        use crate::standards::classify_link_risk;
        let tmp = tempfile::Builder::new()
            .prefix("norn-orch-delete-rewrite-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n[[b]]\n").unwrap();
        std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
        let index = crate::graph::build_index(&root).unwrap();

        let b_doc = index
            .documents
            .iter()
            .find(|d| d.path.as_str() == "b.md")
            .unwrap();
        let risk = classify_link_risk(
            &camino::Utf8PathBuf::from("b.md"),
            &camino::Utf8PathBuf::from("c.md"),
            &index.documents,
            &index.files,
        );

        let plan = RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "delete-b".into(),
                path: "b.md".into(),
                document_hash: b_doc.hash.clone(),
                finding_code: "operator-request".into(),
                finding_rule: None,
                repair_rule: "operator-request".into(),
                operation: "delete_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: None,
                link_risk: Some(risk),
                warnings: Vec::new(),
                force: false,
                parents: false,
            }],
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        };

        let report = apply_repair_plan(&root, &index, &plan, false).unwrap();
        assert_eq!(report.deleted_documents.len(), 1);
        assert!(!root.join("b.md").as_std_path().exists());
        let a_content = std::fs::read_to_string(root.join("a.md")).unwrap();
        assert!(
            a_content.contains("[[c]]"),
            "a.md should now link to c: {a_content}"
        );
    }

    // ── create_document tests ─────────────────────────────────────────────────

    fn make_empty_vault(prefix: &str) -> (tempfile::TempDir, camino::Utf8PathBuf, GraphIndex) {
        let tmp = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        let index = crate::graph::build_index(&root).unwrap();
        (tmp, root, index)
    }

    fn create_plan(
        vault_root: &camino::Utf8PathBuf,
        rel_path: &str,
        fm: serde_json::Map<String, serde_json::Value>,
        body: &str,
        force: bool,
    ) -> RepairPlan {
        let new_value = serde_json::json!({
            "frontmatter": serde_json::Value::Object(fm),
            "body": body,
        });
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 1,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "create-test".into(),
                path: rel_path.into(),
                document_hash: String::new(),
                finding_code: "imperative-create".into(),
                finding_rule: None,
                repair_rule: "vault-new".into(),
                operation: "create_document".into(),
                field: None,
                expected_old_value: None,
                new_value: Some(new_value),
                destination: None,
                link_risk: None,
                warnings: vec![],
                force,
                parents: false,
            }],
            skipped_findings: vec![],
            footnotes: vec![],
        }
    }

    #[test]
    fn apply_create_document_writes_file() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "foo.md", fm, "Hello\n", false);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        assert_eq!(report.created_documents.len(), 1);
        assert_eq!(
            report.created_documents[0].path,
            camino::Utf8PathBuf::from("foo.md")
        );
        let full = root.join("foo.md");
        assert!(full.as_std_path().exists(), "file should exist after apply");
        let written = std::fs::read_to_string(full.as_std_path()).unwrap();
        assert!(written.starts_with("---\n"), "got: {written}");
        assert!(written.contains("type: note"), "got: {written}");
        assert!(written.contains("Hello\n"), "got: {written}");
    }

    #[test]
    fn apply_create_document_dry_run_does_not_write_file() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-dry-");
        let plan = create_plan(&root, "foo.md", serde_json::Map::new(), "", false);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ true).unwrap();

        assert_eq!(report.created_documents.len(), 1);
        assert!(
            !root.join("foo.md").as_std_path().exists(),
            "dry-run must not create file"
        );
    }

    #[test]
    fn apply_create_document_refuses_existing_path_without_force() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-exists-");
        std::fs::write(root.join("foo.md").as_std_path(), "existing\n").unwrap();
        let plan = create_plan(&root, "foo.md", serde_json::Map::new(), "", false);

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exists") || msg.contains("force"),
            "expected exists/force error, got: {msg}"
        );
        // Original file must be untouched.
        let content = std::fs::read_to_string(root.join("foo.md").as_std_path()).unwrap();
        assert_eq!(content, "existing\n");
    }

    #[test]
    fn apply_create_document_overwrites_with_force() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-force-");
        std::fs::write(root.join("foo.md").as_std_path(), "old content\n").unwrap();
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "foo.md", fm, "new\n", true);

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        assert_eq!(report.created_documents.len(), 1);
        let written = std::fs::read_to_string(root.join("foo.md").as_std_path()).unwrap();
        assert!(written.contains("new\n"), "got: {written}");
        assert!(!written.contains("old content"), "got: {written}");
    }

    // ── files.ignore re-check on resolved `{{seq}}` path (NRN-138) ─────────────

    #[test]
    fn apply_create_document_refuses_ignored_resolved_seq_path() {
        // `files.ignore` constrains the RESOLVED seq filename (`logs/1.md`),
        // not the literal template (`logs/{{seq}}.md`) synth-time saw. The
        // applier must re-check post-resolution and refuse before any write.
        let (_tmp, root, index) = make_empty_vault("vault-apply-seq-ignored-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "logs/{{seq}}.md", fm, "Hello\n", false);

        let ctx = CreateApplyContext {
            parents: true,
            ignore: vec!["logs/1.md".to_string()],
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let err = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("logs/1.md") && msg.contains("files.ignore"),
            "expected resolved-path + files.ignore mention, got: {msg}"
        );
        assert!(
            !root.join("logs/1.md").as_std_path().exists(),
            "ignored resolved seq path must not be written"
        );
        assert!(
            !root.join("logs").as_std_path().exists(),
            "the ignore check must run before parent-dir creation"
        );
    }

    #[test]
    fn apply_create_document_creates_non_ignored_resolved_seq_path() {
        // Regression guard: a `{{seq}}` create whose resolved path is NOT
        // covered by `files.ignore` must still create normally.
        let (_tmp, root, index) = make_empty_vault("vault-apply-seq-ok-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "logs/{{seq}}.md", fm, "Hello\n", false);

        let ctx = CreateApplyContext {
            parents: true,
            ignore: vec!["other/**".to_string()],
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let report = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans,
        )
        .unwrap();

        assert_eq!(report.created_documents.len(), 1);
        assert_eq!(
            report.created_documents[0].path,
            camino::Utf8PathBuf::from("logs/1.md")
        );
        let full = root.join("logs/1.md");
        assert!(full.as_std_path().exists(), "file should exist after apply");
        let written = std::fs::read_to_string(full.as_std_path()).unwrap();
        assert!(written.contains("Hello\n"), "got: {written}");
    }

    // ── -p / --parents tests ───────────────────────────────────────────────────

    #[test]
    fn apply_create_document_creates_parent_dirs_when_p() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-parents-");
        let plan = create_plan(
            &root,
            "deep/nested/dir/foo.md",
            serde_json::Map::new(),
            "",
            false,
        );

        let ctx = CreateApplyContext {
            parents: true,
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let report = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans,
        )
        .unwrap();

        assert_eq!(report.created_documents.len(), 1);
        assert!(
            root.join("deep/nested/dir/foo.md").as_std_path().exists(),
            "file should exist"
        );
    }

    #[test]
    fn move_populates_cascade_record_from_actuals() {
        let tmp = tempfile::Builder::new()
            .prefix("cascade-rec-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
        std::fs::write(root.join("d.md"), "---\ntype: note\n---\nsee [[a]]\n").unwrap();
        let index = crate::graph::build_index(&root).unwrap();

        let cfg = crate::move_doc::PreflightConfig {
            src: "a.md",
            dst: "b.md",
            force: false,
            no_link_rewrite: false,
            vault_root: &root,
            index: &index,
        };
        let plan = crate::move_doc::preflight_and_plan(cfg).unwrap();

        let create_ctx = CreateApplyContext {
            parents: false,
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let report = apply_repair_plan_with_context(
            &root,
            &index,
            &plan,
            false,
            &create_ctx,
            &mut sink,
            &spans,
        )
        .unwrap();

        let rec = report
            .cascades
            .iter()
            .find(|c| c.source_path.as_str() == "a.md")
            .expect("cascade record for the move");
        assert_eq!(rec.planned, 1);
        assert_eq!(rec.rewritten.len(), 1);
        assert_eq!(rec.skipped.len(), 0);
        assert_eq!(rec.rewritten[0].file.as_str(), "d.md");
    }

    #[test]
    fn apply_create_document_refuses_missing_parent_without_p() {
        let (_tmp, root, index) = make_empty_vault("vault-apply-no-parents-");
        let plan = create_plan(
            &root,
            "deep/nested/foo.md",
            serde_json::Map::new(),
            "",
            false,
        );

        let ctx = CreateApplyContext {
            parents: false,
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let err = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parent") || msg.contains("-p"),
            "expected parent-missing error, got: {msg}"
        );
    }

    // ── retry_failed_cascades tests ───────────────────────────────────────────

    #[test]
    fn retry_promotes_transient_failure_to_rewritten() {
        use crate::standards::apply::{CascadeRecord, LinkAttempt, LinkFailReason, LinkFailResult};
        use std::time::Duration;
        let mut cascades = vec![CascadeRecord {
            source_path: "m.md".into(),
            planned: 1,
            rewritten: vec![],
            skipped: vec![],
            failed: vec![LinkFailResult {
                file: "b.md".into(),
                from: "[[old]]".into(),
                to: "[[new]]".into(),
                reason: LinkFailReason::WriteFailed,
                detail: "Resource temporarily unavailable".into(),
            }],
        }];
        let mut calls = 0;
        let promoted = retry_failed_cascades(
            &mut cascades,
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            |_f| {
                calls += 1;
                if calls == 1 {
                    LinkAttempt::Failed(LinkFailReason::WriteFailed, "still busy".into())
                } else {
                    LinkAttempt::Rewritten
                }
            },
        );
        assert_eq!(
            promoted.len(),
            1,
            "recovered link returned for rewritten_links"
        );
        assert!(
            cascades[0].failed.is_empty(),
            "recovered link removed from failed"
        );
        assert_eq!(
            cascades[0].rewritten.len(),
            1,
            "recovered link moved to rewritten"
        );
    }

    #[test]
    fn retry_is_a_noop_when_there_are_no_failures() {
        use crate::standards::apply::{
            CascadeRecord, LinkAttempt, LinkFailReason, LinkRewriteResult,
        };
        use std::time::Duration;
        let mut cascades = vec![CascadeRecord {
            source_path: "m.md".into(),
            planned: 1,
            rewritten: vec![LinkRewriteResult {
                file: "b.md".into(),
                from: "[[old]]".into(),
                to: "[[new]]".into(),
            }],
            skipped: vec![],
            failed: vec![],
        }];
        let mut called = false;
        let promoted = retry_failed_cascades(
            &mut cascades,
            &[Duration::from_secs(999)], // would hang if entered
            |_f| {
                called = true;
                LinkAttempt::Failed(LinkFailReason::WriteFailed, "nope".into())
            },
        );
        assert!(
            !called,
            "no failures => attempt closure never called, no sleep"
        );
        assert!(promoted.is_empty());
    }

    #[test]
    fn retry_survivor_stays_failed_after_all_rounds() {
        use crate::standards::apply::{CascadeRecord, LinkAttempt, LinkFailReason, LinkFailResult};
        use std::time::Duration;
        let mut cascades = vec![CascadeRecord {
            source_path: "m.md".into(),
            planned: 1,
            rewritten: vec![],
            skipped: vec![],
            failed: vec![LinkFailResult {
                file: "b.md".into(),
                from: "[[old]]".into(),
                to: "[[new]]".into(),
                reason: LinkFailReason::WriteFailed,
                detail: "busy".into(),
            }],
        }];
        let promoted = retry_failed_cascades(
            &mut cascades,
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            |_f| LinkAttempt::Failed(LinkFailReason::WriteFailed, "still busy".into()),
        );
        assert!(promoted.is_empty());
        assert_eq!(
            cascades[0].failed.len(),
            1,
            "survivor remains failed after 3 rounds"
        );
    }

    // ── NRN-139: file-major content composition ───────────────────────────────

    fn set_frontmatter_change(
        path: &str,
        hash: &str,
        field: &str,
        old: serde_json::Value,
        new: serde_json::Value,
    ) -> PlannedChange {
        PlannedChange {
            change_id: format!("set-{field}"),
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "set_frontmatter".into(),
            field: Some(field.to_string()),
            expected_old_value: Some(old),
            new_value: Some(new),
            destination: None,
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        }
    }

    fn append_to_section_change(
        path: &str,
        hash: &str,
        heading: &str,
        content: &str,
    ) -> PlannedChange {
        let payload = serde_json::json!({
            "op": "append_to_section",
            "heading": heading,
            "content": content,
        });
        PlannedChange {
            change_id: format!("append-{heading}"),
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: "append_to_section".into(),
            field: None,
            expected_old_value: None,
            new_value: Some(payload),
            destination: None,
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        }
    }

    fn plan_with(vault_root: &camino::Utf8PathBuf, changes: Vec<PlannedChange>) -> RepairPlan {
        RepairPlan {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: changes.len(),
                skipped: SkippedSummary::default(),
            },
            changes,
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        }
    }

    /// The pure composition helper chains a frontmatter set and a section edit on
    /// one document into a SINGLE composed string (one write in Phase A2), with a
    /// changed unit recorded per contributing transform.
    #[test]
    fn compose_content_ops_chains_frontmatter_and_edit_into_one_output() {
        let original = "---\nstatus: todo\n---\n## History\n- created\n";
        let fm = set_frontmatter_change(
            "note.md",
            "h",
            "status",
            serde_json::json!("todo"),
            serde_json::json!("done"),
        );
        let edit = append_to_section_change("note.md", "h", "History", "- done");
        let ops = ContentOps {
            frontmatter: vec![&fm],
            rewrite_links: Vec::new(),
            replace_bodies: Vec::new(),
            edit_ops: vec![&edit],
        };

        let composed =
            compose_content_ops(camino::Utf8Path::new("note.md"), ops, original).unwrap();

        // A single composed string carries BOTH mutations — proving Phase A2
        // performs exactly one write for the multi-class document.
        assert!(
            composed.content.contains("status: done"),
            "{}",
            composed.content
        );
        assert!(
            composed.content.contains("- created") && composed.content.contains("- done"),
            "{}",
            composed.content
        );
        assert_eq!(
            composed.units.len(),
            2,
            "one unit per contributing transform"
        );
        assert!(composed.units.iter().all(|u| u.changed));
    }

    /// A no-op transform is detected (unit.changed == false) so A2 suppresses the
    /// write / action.
    #[test]
    fn compose_content_ops_detects_noop() {
        let original = "---\nstatus: done\n---\nbody\n";
        // set status to the value it already holds → byte-identical.
        let fm = set_frontmatter_change(
            "note.md",
            "h",
            "status",
            serde_json::json!("done"),
            serde_json::json!("done"),
        );
        let ops = ContentOps {
            frontmatter: vec![&fm],
            rewrite_links: Vec::new(),
            replace_bodies: Vec::new(),
            edit_ops: Vec::new(),
        };
        let composed =
            compose_content_ops(camino::Utf8Path::new("note.md"), ops, original).unwrap();
        assert_eq!(composed.content, original);
        assert!(!composed.units[0].changed, "no-op frontmatter set");
    }

    /// A failing edit transform (missing heading) propagates as Err from the
    /// helper, so the orchestrator aborts BEFORE any write.
    #[test]
    fn compose_content_ops_propagates_transform_failure() {
        let original = "---\nstatus: todo\n---\n## History\n- created\n";
        let fm = set_frontmatter_change(
            "note.md",
            "h",
            "status",
            serde_json::json!("todo"),
            serde_json::json!("done"),
        );
        let edit = append_to_section_change("note.md", "h", "NoSuchHeading", "- x");
        let ops = ContentOps {
            frontmatter: vec![&fm],
            rewrite_links: Vec::new(),
            replace_bodies: Vec::new(),
            edit_ops: vec![&edit],
        };
        let err = compose_content_ops(camino::Utf8Path::new("note.md"), ops, original).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("History")
                || msg.to_lowercase().contains("heading")
                || msg.contains("NoSuchHeading"),
            "expected a missing-heading edit failure, got: {msg}"
        );
    }

    /// End-to-end per-document atomicity (the NRN-139 headline): a plan with a
    /// frontmatter `set` AND a section edit on ONE doc. When the section edit's
    /// anchor is missing, the WHOLE content phase aborts before any write — the
    /// frontmatter change is NOT flushed, so the file is byte-identical to the
    /// original (no half-mutation across the two classes on one file).
    #[test]
    fn content_phase_is_atomic_on_edit_failure_for_one_doc() {
        let initial = "---\nstatus: todo\n---\n## History\n- created\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-nrn139-atomic-", "note.md", initial);

        let plan = plan_with(
            &root,
            vec![
                set_frontmatter_change(
                    "note.md",
                    &hash,
                    "status",
                    serde_json::json!("todo"),
                    serde_json::json!("done"),
                ),
                // Missing heading → apply_edit_ops fails during A1 compute.
                append_to_section_change("note.md", &hash, "NoSuchHeading", "- done"),
            ],
        );

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("heading")
                || err.to_string().contains("NoSuchHeading")
                || err.to_string().contains("History"),
            "expected an edit-anchor failure, got: {err}"
        );

        // The frontmatter mutation must NOT have been written: the file is exactly
        // the original bytes.
        let on_disk = std::fs::read_to_string(root.join("note.md")).unwrap();
        assert_eq!(
            on_disk, initial,
            "content phase aborted before writing; file must be byte-identical"
        );
    }

    /// The success path of the same canonical case: both the frontmatter set and
    /// the section append land in the single composed write.
    #[test]
    fn content_phase_applies_frontmatter_and_edit_for_one_doc() {
        let initial = "---\nstatus: todo\n---\n## History\n- created\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-nrn139-both-", "note.md", initial);

        let plan = plan_with(
            &root,
            vec![
                set_frontmatter_change(
                    "note.md",
                    &hash,
                    "status",
                    serde_json::json!("todo"),
                    serde_json::json!("done"),
                ),
                append_to_section_change("note.md", &hash, "History", "- done"),
            ],
        );

        let report = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();
        assert!(report
            .changed_files
            .contains(&camino::Utf8PathBuf::from("note.md")));

        let on_disk = std::fs::read_to_string(root.join("note.md")).unwrap();
        assert!(on_disk.contains("status: done"), "{on_disk}");
        assert!(
            on_disk.contains("- created") && on_disk.contains("- done"),
            "{on_disk}"
        );
    }

    /// The Phase A2 content write is crash-atomic (temp + rename, mirroring
    /// `create_document`): the composed content lands correctly AND no `.tmp`
    /// sibling is left behind after a successful write.
    #[test]
    fn content_write_is_atomic_and_leaves_no_temp() {
        let initial = "---\nstatus: todo\n---\nbody\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-nrn139-atomicwrite-", "note.md", initial);

        let plan = plan_with(
            &root,
            vec![set_frontmatter_change(
                "note.md",
                &hash,
                "status",
                serde_json::json!("todo"),
                serde_json::json!("done"),
            )],
        );

        apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        // Content landed via the atomic rename.
        let on_disk = std::fs::read_to_string(root.join("note.md")).unwrap();
        assert!(on_disk.contains("status: done"), "{on_disk}");

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

    // ── NRN-139: reject a content op after a delete/move of the same path ──────

    fn op_change(change_id: &str, path: &str, operation: &str) -> PlannedChange {
        PlannedChange {
            change_id: change_id.into(),
            path: path.into(),
            document_hash: String::new(),
            finding_code: "operator-request".into(),
            finding_rule: None,
            repair_rule: "operator-request".into(),
            operation: operation.into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: Vec::new(),
            force: false,
            parents: false,
        }
    }

    fn dummy_root() -> camino::Utf8PathBuf {
        camino::Utf8PathBuf::from("/vault")
    }

    #[test]
    fn guard_rejects_content_op_after_delete_of_same_path() {
        let plan = plan_with(
            &dummy_root(),
            vec![
                op_change("del", "x.md", "delete_document"),
                op_change("set", "x.md", "set_frontmatter"),
            ],
        );
        let err = reject_content_op_after_vacate(&plan).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("x.md"), "error must name the path: {msg}");
    }

    #[test]
    fn guard_rejects_content_op_after_move_of_same_path() {
        let plan = plan_with(
            &dummy_root(),
            vec![
                op_change("mv", "x.md", "move_document"),
                op_change("edit", "x.md", "append_to_section"),
            ],
        );
        let err = reject_content_op_after_vacate(&plan).unwrap_err();
        assert!(err.to_string().contains("x.md"), "{err}");
    }

    #[test]
    fn guard_allows_content_op_before_delete_of_same_path() {
        // edit-then-delete is the legitimate reverse order: it executes as given.
        let plan = plan_with(
            &dummy_root(),
            vec![
                op_change("set", "x.md", "set_frontmatter"),
                op_change("del", "x.md", "delete_document"),
            ],
        );
        reject_content_op_after_vacate(&plan).unwrap();
    }

    #[test]
    fn guard_allows_create_after_delete_of_same_path() {
        // delete-then-recreate is coherent; create_document is not a content op.
        let plan = plan_with(
            &dummy_root(),
            vec![
                op_change("del", "x.md", "delete_document"),
                op_change("new", "x.md", "create_document"),
            ],
        );
        reject_content_op_after_vacate(&plan).unwrap();
    }
}
