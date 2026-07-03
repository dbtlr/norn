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

    // Pass 1: per-file frontmatter edits. changes_by_path skips
    // move_document, so the grouped map only contains set/remove/add
    // frontmatter changes.
    let grouped = changes_by_path(plan)?;

    let mut report = RepairApplyReport::new(plan, dry_run);

    let current_hashes: std::collections::BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();

    for (rel_path, changes) in &grouped {
        // Hash check against the first change in the group (all share the same
        // document_hash for a given path — changes_by_path rejects mismatches).
        check_hash(&current_hashes, changes[0])?;

        let absolute_path = cwd.join(rel_path);
        let original =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = apply_file_changes(&original, changes)?;

        if updated != original {
            report.changed_files.push(rel_path.clone());
            if !dry_run {
                fs::write(&absolute_path, updated)
                    .with_context(|| format!("write {absolute_path}"))?;
                for change in changes {
                    emit_op_action(sink, spans, change, Severity::Info, Vec::new());
                }
            }
        }
    }

    // Pass 1b: rewrite_link operations (broken wikilink target rewrites).
    // Hash check uses the index snapshot (same as Pass 1 frontmatter check).
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "rewrite_link")
    {
        check_hash(&current_hashes, change)?;

        if dry_run {
            // Record what would be rewritten without touching the file.
            if let (Some(from), Some(to)) = (
                change.expected_old_value.as_ref().and_then(|v| v.as_str()),
                change.new_value.as_ref().and_then(|v| v.as_str()),
            ) {
                report.rewritten_links.push(LinkRewriteResult {
                    file: change.path.clone(),
                    from: from.to_string(),
                    to: to.to_string(),
                });
                if !report.changed_files.contains(&change.path) {
                    report.changed_files.push(change.path.clone());
                }
            }
            continue;
        }

        let absolute_path = cwd.join(&change.path);
        let content =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = apply_rewrite_link(&content, change)?;
        if updated != content {
            fs::write(&absolute_path, &updated)
                .with_context(|| format!("write {absolute_path}"))?;
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
            if let (Some(from), Some(to)) = (
                change.expected_old_value.as_ref().and_then(|v| v.as_str()),
                change.new_value.as_ref().and_then(|v| v.as_str()),
            ) {
                report.rewritten_links.push(LinkRewriteResult {
                    file: change.path.clone(),
                    from: from.to_string(),
                    to: to.to_string(),
                });
            }
            emit_op_action(sink, spans, change, Severity::Info, Vec::new());
        }
    }

    // Pass 1c: delete_document operations.
    // Sequenced after rewrite_link so --rewrite-to redirects backlinks before
    // the target file disappears, and before move_document so delete-then-move
    // on the same path is impossible.
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "delete_document")
    {
        check_hash(&current_hashes, change)?;

        // Pass 1c.1: apply link rewrites if link_risk is attached (--rewrite-to case).
        // This runs BEFORE the delete so links can be rewritten in source docs.
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

        // Pass 1c.2: the actual file removal.
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

    // Pass 1d: replace_body operations. Whole-body rewrites with hash-check
    // discipline matching Passes 1a / 1b. Sequenced after delete_document so we
    // never attempt a body-replace on a file that was just removed, and before
    // move_document so the content is settled before any rename.
    for change in plan
        .changes
        .iter()
        .filter(|c| c.operation == "replace_body")
    {
        check_hash(&current_hashes, change)?;

        if dry_run {
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
            report.replaced_bodies.push(change.path.clone());
            continue;
        }

        let absolute_path = cwd.join(&change.path);
        let content =
            fs::read_to_string(&absolute_path).with_context(|| format!("read {absolute_path}"))?;
        let updated = crate::standards::apply::apply_replace_body(&content, change)?;
        if updated != content {
            fs::write(&absolute_path, &updated)
                .with_context(|| format!("write {absolute_path}"))?;
            if !report.changed_files.contains(&change.path) {
                report.changed_files.push(change.path.clone());
            }
            emit_op_action(sink, spans, change, Severity::Info, Vec::new());
        }
        report.replaced_bodies.push(change.path.clone());
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
        let tmp_path = {
            let mut p = full.clone();
            let stem = p.file_name().unwrap_or("doc").to_string();
            p.set_file_name(format!(".{stem}.tmp"));
            p
        };
        fs::write(tmp_path.as_std_path(), &contents)
            .with_context(|| format!("create_document: write temp file {tmp_path}"))?;
        fs::rename(tmp_path.as_std_path(), full.as_std_path()).with_context(|| {
            // Best-effort cleanup on rename failure.
            let _ = fs::remove_file(tmp_path.as_std_path());
            format!("create_document: rename temp to {full}")
        })?;

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

        let ctx = CreateApplyContext { parents: true };
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

        let create_ctx = CreateApplyContext { parents: false };
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

        let ctx = CreateApplyContext { parents: false };
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
}
