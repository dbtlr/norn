//! The repair-specific apply orchestrator.
//!
//! `apply_repair_plan` runs a `ApplyBatch`'s changes in ordered passes —
//! document creation, moves, deletes, link rewrites, file edits — over the
//! low-level primitives in `standards::apply`, collecting per-op results and
//! skip reasons. The `repair` and `new` commands drive it; the general `apply`
//! command uses `applier.rs` instead. Containment and mutation-lock checks live
//! in the primitives it calls, not here.

use std::fs;
use std::time::Duration;

use crate::apply::fsops::{self, apply_delete, apply_move, ensure_within_vault};
use crate::apply::transaction::{self, Composition, DriftPolicy};
use crate::domain::GraphIndex;
use crate::standards::apply::{
    apply_file_changes, apply_link_rewrites, apply_rewrite_link, apply_strip_bom, changes_by_path,
    validate_plan_for_apply, ApplyError, CascadeRecord, CreateDocumentResult, DeleteResult,
    LinkAttempt, LinkFailResult, LinkRewriteResult, LinkSkipResult, MoveResult, RepairApplyWarning,
};
use crate::standards::{ApplyBatch, ApplyOp};
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
    /// NRN-265: the caller declares that every `create_document` change arrives
    /// with its `{{seq}}` template ALREADY resolved to a concrete path.
    /// Allocation semantics live in one shared helper
    /// (`seq_alloc::resolve_seq_create`), so the flag's job is narrower than
    /// drift detection: under this declaration a surviving `{{seq}}` token
    /// means the applier's pre-resolution step — and with it the NRN-264
    /// owner-set barrier, which validated the RESOLVED paths — did not run, so
    /// the delegate fails closed rather than allocate a path that barrier never
    /// validated.
    ///
    /// Per-caller reachability of the Pass-1e seq branch (authoritative copy):
    ///   - MigrationPlan applier (`applier.rs`): sets `true` — pre-resolves via
    ///     `resolve_create_paths` under the owner-set barrier and rewrites
    ///     `change.path` before delegating, so the branch is unreachable there.
    ///   - `new` (CLI `new/mod.rs`, MCP `mcp/tools/new.rs`): `false` — passes
    ///     the `{{seq}}` template unresolved; the branch is load-bearing.
    ///   - set / edit (CLI `lib.rs`, MCP `set.rs`/`edit.rs`): `false` (default)
    ///     — they emit no `create_document` ops and never reach the branch.
    pub creates_preresolved: bool,
}

pub use crate::standards::apply::RepairApplyReport;

/// Pre-stamp an `op_planned` span per planned change so the applier's per-op
/// `action` events thread under the right span. Returns the `change_id → span`
/// map [`apply_repair_plan_with_context`] expects. Shared by the CLI and MCP
/// set / new / edit paths so span construction stays identical across surfaces.
///
/// Unused until the frontmatter mutation verbs (`set` / `new` / `edit`) land —
/// the plan applier builds its own spans inline. Kept live rather than deferring
/// this helper's port along with those verbs.
#[allow(dead_code)]
pub(crate) fn build_op_spans(
    sink: &mut crate::telemetry::EventSink,
    changes: &[ApplyOp],
) -> std::collections::HashMap<String, String> {
    let mut spans = std::collections::HashMap::new();
    for change in changes {
        let span = sink.start_op(&change.operation, change.path.as_str(), None);
        spans.insert(change.change_id.clone(), span);
    }
    spans
}

/// Refuse a plan whose target is not tracked in the index. The whole-document
/// hash CAS itself now happens against the file's ACTUAL bytes inside the
/// transaction engine (`transaction::fingerprint_cas`), not against the
/// GraphIndex snapshot — the index can be stale relative to the file, so the
/// file-bytes CAS strictly catches more staleness. This retains only the
/// index-membership half of the old `check_hash`: a plan targeting a path the
/// index never saw is an `unknown-path` refusal.
fn ensure_known_path(
    current_hashes: &std::collections::BTreeMap<Utf8PathBuf, String>,
    change: &ApplyOp,
) -> Result<()> {
    if !current_hashes.contains_key(&change.path) {
        return Err(anyhow::anyhow!(ApplyError::UnknownPath {
            path: change.path.clone(),
        }));
    }
    Ok(())
}

/// The plan `document_hash` for one path, taken from its first content op.
/// Empty string when operator-originated (no CAS).
///
/// `changes_by_path`'s ConflictingHashes check only covers FRONTMATTER ops
/// (set/add/remove) — strip_bom, rewrite_link, replace_body, and the section
/// edit ops are skipped by it. So a divergent same-path hash is guaranteed
/// impossible only across frontmatter ops; a hand-authored plan mixing content
/// classes with different hashes on one path would CAS against whichever op is
/// first here. That is acceptable: any wrong hash refuses as stale, and
/// verb-synthesized plans (the only ones with real hashes) always carry one
/// uniform hash per path.
fn content_plan_hash<'a>(ops: &ContentOps<'a>) -> &'a str {
    ops.strip_bom
        .iter()
        .chain(ops.frontmatter.iter())
        .chain(ops.rewrite_links.iter())
        .chain(ops.replace_bodies.iter())
        .chain(ops.edit_ops.iter())
        .map(|c| c.document_hash.as_str())
        .next()
        .unwrap_or("")
}

/// Whether every content op for a file is declarative (frontmatter set/add/
/// remove, strip_bom) — the auto-retry-on-drift class. A single content-anchored
/// op (rewrite_link, replace_body, or a section/heading edit) makes the whole
/// file content-anchored: re-landing on drifted bytes could destroy an external
/// edit, so it refuses on drift instead.
fn is_declarative_only(ops: &ContentOps<'_>) -> bool {
    ops.rewrite_links.is_empty() && ops.replace_bodies.is_empty() && ops.edit_ops.is_empty()
}

/// Clone the per-file op buckets (cheap: the vectors hold `&ApplyOp`) so
/// the transaction engine can re-run composition on a retry round without
/// consuming the originals.
fn clone_ops<'a>(ops: &ContentOps<'a>) -> ContentOps<'a> {
    ContentOps {
        strip_bom: ops.strip_bom.clone(),
        frontmatter: ops.frontmatter.clone(),
        rewrite_links: ops.rewrite_links.clone(),
        replace_bodies: ops.replace_bodies.clone(),
        edit_ops: ops.edit_ops.clone(),
    }
}

fn count_planned_links(change: &ApplyOp) -> usize {
    change.link_risk.as_ref().map_or(0, |r| {
        r.stem_links.len() + r.path_qualified_wikilinks.len() + r.markdown_links.len()
    })
}

/// Re-attempt every failed backlink rewrite across all cascades, up to
/// `backoff.len()` rounds, sleeping `backoff[round]` BEFORE each round so a
/// transient condition affecting several files clears in one wait. Recovered
/// links move from `failed` to `rewritten`; survivors stay `failed` with
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
    plan: &ApplyBatch,
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
        None,
    )
}

/// Emit an action event for a change if its span is known; no-op otherwise.
/// Status is always `applied`; callers pass any extra attributes (e.g. the
/// move destination).
fn emit_op_action(
    sink: &mut crate::telemetry::EventSink,
    spans: &std::collections::HashMap<String, String>,
    change: &ApplyOp,
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
fn reject_content_op_after_vacate(plan: &ApplyBatch) -> Result<()> {
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

/// (NRN-142) Post-serialization gate for `create_document`: the freshly
/// serialized document must re-parse through the same read pipeline
/// (`extract_frontmatter`) to a top-level mapping before it is written. YAML
/// null over an empty/whitespace-only block counts as the empty mapping (the
/// `---\n---\n` a fieldless create emits), mirroring the apply-path
/// normalization. Everything else — a parse diagnostic, or a non-mapping
/// frontmatter value — refuses the create with the document named, before any
/// byte reaches disk.
fn verify_created_document(resolved_path: &Utf8Path, contents: &str) -> Result<()> {
    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) =
        norn_frontmatter::frontmatter::extract_frontmatter(contents, &mut diagnostics);
    if !diagnostics.is_empty() {
        let detail = diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow::anyhow!(
            "create_document: refusing write {resolved_path}: serialized frontmatter failed to parse: {detail}"
        ));
    }
    let is_mapping = match (&frontmatter, &frontmatter_range) {
        (Some(serde_json::Value::Object(_)), Some(_)) => true,
        (Some(serde_json::Value::Null), Some(range)) => contents[range.clone()].trim().is_empty(),
        (None, None) => true, // no frontmatter block at all: nothing to corrupt
        _ => false,
    };
    if !is_mapping {
        return Err(anyhow::anyhow!(
            "create_document: refusing write {resolved_path}: serialized frontmatter failed to parse as a top-level mapping"
        ));
    }
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
    strip_bom: Vec<&'a ApplyOp>,
    frontmatter: Vec<&'a ApplyOp>,
    rewrite_links: Vec<&'a ApplyOp>,
    replace_bodies: Vec<&'a ApplyOp>,
    edit_ops: Vec<&'a ApplyOp>,
}

/// The region class a content op mutates. Ordered as the composition chain runs:
/// strip_bom → frontmatter → rewrite_link → replace_body → section-edits.
/// `strip_bom` (NRN-385) runs first: it only ever removes the document's
/// leading 3 bytes, an offset every other class's edits sit past, so its
/// position in the chain is really about intent (document-level normalization
/// before content edits) rather than a correctness requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentClass {
    StripBom,
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
        "strip_bom" => ContentClass::StripBom,
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
    changes: Vec<&'a ApplyOp>,
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
        strip_bom,
        frontmatter,
        rewrite_links,
        replace_bodies,
        edit_ops,
    } = ops;
    let mut content = original.to_string();
    let mut units: Vec<ComposedUnit<'a>> = Vec::new();

    // strip_bom: at most one meaningful change per doc (idempotent if more —
    // see `apply_strip_bom`), so a per-change loop composes safely either way.
    for &change in &strip_bom {
        let updated = apply_strip_bom(&content, change)?;
        let changed = updated != content;
        units.push(ComposedUnit {
            changes: vec![change],
            class: ContentClass::StripBom,
            changed,
        });
        content = updated;
    }

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
    if !rewrite_links.is_empty() {
        // NRN-141: rewrite_link rewrites `[[...]]` anywhere in the file,
        // frontmatter values included, without the frontmatter editor's own
        // post-image gate — a target carrying YAML-structural bytes can break
        // the block. The degradation baseline is the state ENTERING the rewrite
        // stage (post-frontmatter-ops), so frontmatter created by ops earlier
        // in this same plan is protected too; the check only runs when link
        // rewrites are actually composed (the other classes splice onto the
        // verbatim frontmatter prefix and cannot degrade it).
        let rewrite_baseline = content.clone();
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
        if content != rewrite_baseline {
            crate::standards::apply::verify_frontmatter_not_degraded(
                path,
                &rewrite_baseline,
                &content,
            )?;
        }
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
                let payload = c.new_value.as_ref().ok_or_else(|| {
                    anyhow::Error::from(
                        crate::standards::apply::PlanStructureError::EditPayloadMissing {
                            path: c.path.clone(),
                        },
                    )
                })?;
                serde_json::from_value::<crate::edit::ops::EditOp>(payload.clone()).map_err(|e| {
                    anyhow::Error::from(
                        crate::standards::apply::PlanStructureError::EditPayloadDecode {
                            path: c.path.clone(),
                            message: e.to_string(),
                        },
                    )
                })
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

    Ok(ComposedFile { content, units })
}

/// Like `apply_repair_plan` but with additional context for `create_document`
/// operations (e.g., the `-p` / `--parents` flag) and a telemetry sink + a
/// `change_id -> op span id` map for emitting per-action events.
///
/// `wrote_any` (NRN-150/183) is the runtime write-state fact: it is flipped to
/// `true` the instant the FIRST filesystem write of this apply lands (an
/// `atomic_write`, a `rename`, a `remove`, a `create_dir_all`, or a backlink
/// rewrite). It persists across the `Err` boundary — the caller reads it to
/// decide whether a failure is a byte-identical **refusal** (nothing written
/// yet) or a partial **failure** (disk already mutated). This is the correct
/// gate; the per-variant `ApplyError::is_precondition()` flag structurally
/// cannot be, because the SAME variant (e.g. `stale-document-hash` /
/// `unknown-path`) is raised from both a pre-write site (Phase A1 content CAS)
/// and a post-write-possible site (the Phase B delete pass, after Phase A2 has
/// already written other ops). Pass `None` when the caller does not need the
/// fact (single-op set/new/edit paths, tests).
#[allow(clippy::too_many_arguments)]
pub fn apply_repair_plan_with_context(
    cwd: &Utf8PathBuf,
    index: &GraphIndex,
    plan: &ApplyBatch,
    dry_run: bool,
    ctx: &CreateApplyContext,
    sink: &mut crate::telemetry::EventSink,
    spans: &std::collections::HashMap<String, String>,
    mut wrote_any: Option<&mut bool>,
) -> Result<RepairApplyReport> {
    use crate::telemetry::event;
    use crate::telemetry::Severity;
    // Flip the runtime write-state fact the moment a write lands. Reborrows the
    // `Option<&mut bool>` each call, so it can be marked at every write site.
    macro_rules! mark_wrote {
        () => {
            if let Some(w) = wrote_any.as_deref_mut() {
                *w = true;
            }
        };
    }
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

    // NRN-145 containment gate: every mutation op target must resolve inside the
    // vault root. Refuse absolute paths, `..` traversal, and directories
    // symlinked out of the vault BEFORE any write — the vault is self-contained.
    // The vault root is canonicalized ONCE here (not per op, never on a read
    // path); each op target's parent is then contained against it. Runs on
    // dry-run too, so a preview refuses exactly where the real apply would.
    let canonical_root = cwd
        .as_std_path()
        .canonicalize()
        .with_context(|| format!("cannot canonicalize vault root {cwd}"))?;
    for change in &plan.changes {
        ensure_within_vault(cwd, &canonical_root, &change.path)?;
        if let Some(dest) = &change.destination {
            ensure_within_vault(cwd, &canonical_root, dest)?;
        }
        if let Some(risk) = &change.link_risk {
            for affected in risk
                .stem_links
                .iter()
                .chain(risk.path_qualified_wikilinks.iter())
                .chain(risk.markdown_links.iter())
            {
                ensure_within_vault(cwd, &canonical_root, &affected.source_path)?;
            }
        }
    }

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
            ContentClass::StripBom => bucket.strip_bom.push(change),
            ContentClass::Frontmatter => bucket.frontmatter.push(change),
            ContentClass::RewriteLink => bucket.rewrite_links.push(change),
            ContentClass::ReplaceBody => bucket.replace_bodies.push(change),
            ContentClass::EditOps => bucket.edit_ops.push(change),
        }
    }

    // Per-file transactions (NRN-406 step 3): each file runs its OWN complete
    // transaction — fingerprint (file-bytes CAS) → shadow (compose) → verify →
    // swap — to completion before the next file starts. This replaces the old
    // compute-all-then-write-all split: it shrinks the drift window (a file is
    // read and written back-to-back rather than read early and written late) and
    // makes the per-file unit real. Op ordering within the plan is otherwise
    // unchanged; a transform or CAS failure still aborts the remaining plan (a
    // partial write of earlier files is a truthful partial-apply, gated by
    // `wrote_any`).
    //
    // `phase_a_wrote` records the paths this phase actually rewrote, so the later
    // delete pass can SKIP its file-bytes CAS for a path we ourselves just wrote:
    // its bytes no longer match the plan's pre-apply hash BY DESIGN (a legitimate
    // edit-then-delete of the same path). The old index-based check tolerated
    // this because it compared against the pre-apply index snapshot; the
    // file-bytes CAS must too.
    let mut phase_a_wrote: std::collections::BTreeSet<Utf8PathBuf> =
        std::collections::BTreeSet::new();

    for path in &content_order {
        let ops = content_ops
            .remove(path)
            .expect("content_order paths are keys of content_ops");
        // A plan targeting a path the index never saw is unknown-path, exactly as
        // the old per-op hash check refused before comparing hashes.
        if !current_hashes.contains_key(path) {
            return Err(anyhow::anyhow!(ApplyError::UnknownPath {
                path: path.clone()
            }));
        }
        let plan_hash = content_plan_hash(&ops);
        let declarative_only = is_declarative_only(&ops);
        let absolute_path = cwd.join(path);

        // The shadow: compose all of this file's transforms over the content it is
        // handed. Called once per attempt, so it clones the ref-buckets rather
        // than consuming them (a retry round re-composes against fresh content).
        let compose = |content: &str| -> Result<Composition<ComposedFile>> {
            let file = compose_content_ops(path, clone_ops(&ops), content)?;
            let changed = file.units.iter().any(|u| u.changed);
            Ok(Composition {
                content: file.content.clone(),
                changed,
                payload: file,
            })
        };

        // Dry-run still fingerprints (a preview must refuse exactly where the real
        // apply would) and composes for the forecast, but never swaps/writes.
        // Apply runs the full transaction: the engine does the swap re-read, drift
        // handling, and the atomic write.
        let (file, wrote) = if dry_run {
            let original = transaction::fingerprint_cas(&absolute_path, path, plan_hash)?;
            let composition = compose(&original)?;
            (composition.payload, false)
        } else {
            let policy = if declarative_only {
                DriftPolicy::RetryDeclarative { max_attempts: 3 }
            } else {
                DriftPolicy::RefuseContentAnchored
            };
            let committed = transaction::run_content_transaction(
                &absolute_path,
                path,
                plan_hash,
                policy,
                compose,
            )?;
            (committed.payload, committed.wrote)
        };

        let overall_changed = file.units.iter().any(|u| u.changed);

        // changed_files: present once iff the composed file actually changed.
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
                ContentClass::StripBom | ContentClass::Frontmatter | ContentClass::EditOps => {}
            }
        }

        if wrote {
            phase_a_wrote.insert(path.clone());
            mark_wrote!();
            // One action per contributing change_id: a unit that was a
            // byte-identical no-op emits nothing.
            for unit in &file.units {
                if !unit.changed {
                    continue;
                }
                for change in unit.changes.iter().copied() {
                    emit_op_action(sink, spans, change, Severity::Info, Vec::new());
                }
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
        // Refuse a delete of a path the index never saw, then fingerprint the
        // file itself against the plan's document_hash (NRN-406): the file-bytes
        // CAS catches an external modification the pre-apply index snapshot would
        // miss. Skip the CAS when Phase A already rewrote this path in THIS plan
        // (edit-then-delete): its bytes intentionally no longer match the
        // pre-apply hash. A missing/unreadable file falls through to
        // `apply_delete`'s precise `delete-source-missing` refusal.
        ensure_known_path(&current_hashes, change)?;
        if !phase_a_wrote.contains(&change.path) {
            transaction::fingerprint_delete(
                &cwd.join(&change.path),
                &change.path,
                &change.document_hash,
            )?;
        }

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
                if !outcome.rewritten.is_empty() {
                    mark_wrote!();
                }
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
            mark_wrote!();
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
        // filesystem max+1 (`seq_alloc::resolve_seq_create`, shared with the
        // MigrationPlan applier's pre-resolution barrier). This runs under the
        // mutation lock the caller holds around apply, so two concurrent creates
        // serialize — the second observes the first's file and gets a distinct
        // sequential id. No new lock is introduced: the NRN-87 warm daemon will
        // own this same boundary and can swap the impl behind it untouched.
        let resolved_path = if crate::seq_alloc::has_seq(&change.path) {
            // NRN-265: per-caller reachability of this branch is documented on
            // `CreateApplyContext::creates_preresolved`. Allocation semantics
            // live in the shared `resolve_seq_create`; under the pre-resolved
            // declaration a surviving `{{seq}}` token means the applier's
            // pre-resolution step (and with it the NRN-264 owner-set barrier,
            // which validated the RESOLVED paths) was skipped — fail closed
            // rather than allocate a path that barrier never saw. Earlier
            // passes of a mixed plan may already have written; that surfaces
            // as a truthful partial-apply report, but this create writes
            // nothing.
            if ctx.creates_preresolved {
                return Err(anyhow::anyhow!(
                    "create_document: `{{{{seq}}}}` reached the apply delegate at {} after the caller declared creates pre-resolved — the pre-resolution barrier did not run",
                    change.path
                ));
            }
            let resolved =
                crate::seq_alloc::resolve_seq_create(cwd, &change.path, &allocated_this_plan)?;
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
            return Err(ApplyError::CreateIgnoredPath {
                path: resolved_path.clone(),
            }
            .into());
        }

        let nv = change.new_value.as_ref().ok_or_else(|| {
            anyhow::anyhow!(ApplyError::MissingNewValue {
                path: resolved_path.clone(),
            })
        })?;
        let fm_obj = nv
            .get("frontmatter")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::Error::from(ApplyError::CreateFrontmatterMalformed))?;
        let body = nv.get("body").and_then(|v| v.as_str()).unwrap_or("");

        let full = cwd.join(&resolved_path);

        // Pre-flight (defense in depth — preflight/synth should have caught these).
        if full.as_std_path().exists() && !change.force {
            return Err(ApplyError::CreateDestinationExists {
                path: resolved_path.clone(),
            }
            .into());
        }
        let parent_to_create = match full.parent() {
            Some(parent) if !parent.as_std_path().exists() => {
                if !ctx.parents {
                    return Err(ApplyError::CreateParentMissing {
                        path: resolved_path.clone(),
                    }
                    .into());
                }
                Some(parent.to_path_buf())
            }
            _ => None,
        };

        // Serialize the document.
        let fm_btree: std::collections::BTreeMap<String, serde_json::Value> =
            fm_obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let contents = norn_frontmatter::frontmatter::serialize_new_document(&fm_btree, body)
            .map_err(|e| {
                anyhow::Error::from(ApplyError::CreateSerializeFailed {
                    message: e.to_string(),
                })
            })?;

        // NRN-142 create-path gate: this is the one frontmatter-write path with
        // no post-image verification, so re-parse the fresh document through the
        // same read pipeline before writing. A serializer output that fails to
        // parse (e.g. a field name past libyaml's 1024-byte simple-key limit,
        // where render_key's terminal fallback is unverified) or that is not a
        // top-level mapping is refused unwritten — a refusal is recoverable, a
        // written document whose frontmatter can never be read back is not. An
        // empty frontmatter block reads back as YAML null; that is the empty
        // mapping, not a refusal. Runs on dry-run too, so a plan preview refuses
        // exactly where the real apply would; and it runs BEFORE parent-dir
        // creation, so a refusal leaves no empty directory behind.
        verify_created_document(&resolved_path, &contents)?;

        if dry_run {
            report.created_documents.push(CreateDocumentResult {
                path: resolved_path.clone(),
            });
            if !report.changed_files.contains(&resolved_path) {
                report.changed_files.push(resolved_path.clone());
            }
            continue;
        }

        if let Some(parent) = parent_to_create {
            fs::create_dir_all(parent.as_std_path()).with_context(|| {
                format!("create_document: create parent dirs for {}", resolved_path)
            })?;
            mark_wrote!();
        }

        // Materialize the document (NRN-160): unless `--force`, this must NOT
        // clobber a file that appeared between the exists-precheck above and now
        // (the create TOCTOU window). `create_document_file` uses an
        // atomic-exclusive `hard_link` for the no-force case; an `AlreadyExists`
        // there is the same "destination already exists" refusal the precheck
        // gives for the common no-race case. `--force` keeps overwrite semantics.
        fsops::create_document_file(&full, &contents, change.force).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::Error::from(ApplyError::CreateDestinationExists {
                    path: resolved_path.clone(),
                })
            } else {
                anyhow::Error::new(e).context(format!("create_document: write {resolved_path}"))
            }
        })?;
        mark_wrote!();

        report.created_documents.push(CreateDocumentResult {
            path: resolved_path.clone(),
        });
        if !report.changed_files.contains(&resolved_path) {
            report.changed_files.push(resolved_path.clone());
        }
        // Audit the action against the resolved path, not the `{{seq}}` template
        // (NRN-101). Same change_id, so it still hangs off the op_planned span.
        let resolved_change = ApplyOp {
            path: resolved_path.clone(),
            ..change.clone()
        };
        emit_op_action(sink, spans, &resolved_change, Severity::Info, Vec::new());
    }

    // Collect move_document changes for passes 2 and 3.
    let move_changes: Vec<&ApplyOp> = plan
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
            mark_wrote!();
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
            if !outcome.rewritten.is_empty() {
                mark_wrote!();
            }
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
            // The retried failure carries only the raw link text, not its kind.
            // Recover the kind from the parser (authoritative): a raw the wikilink
            // parser recognizes whole is a `[[…]]` reference (the splice path
            // treats Wikilink/Embed identically); anything else is a Markdown
            // destination that rewrites by literal replace.
            let kind = if norn_frontmatter::wikilink::parse_wikilinks_in_text(&f.from)
                .first()
                .is_some_and(|link| link.raw == f.from)
            {
                crate::domain::LinkKind::Wikilink
            } else {
                crate::domain::LinkKind::Markdown
            };
            crate::standards::apply::rewrite_one_backlink(
                cwd.as_path(),
                &f.file,
                &f.from,
                &f.to,
                &kind,
            )
        });
        if !recovered.is_empty() {
            mark_wrote!();
        }
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
        ApplyBatch, ApplyOp, RepairPlanSummary, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
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

    fn delete_plan(vault_root: &camino::Utf8PathBuf, doc_rel: &str, hash: &str) -> ApplyBatch {
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "delete-foo".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
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
        }
    }

    fn move_plan(
        vault_root: &camino::Utf8PathBuf,
        from_rel: &str,
        to_rel: &str,
        hash: &str,
    ) -> ApplyBatch {
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "move-test".into(),
                path: from_rel.into(),
                document_hash: hash.to_string(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
                operation: "move_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: Some(to_rel.into()),
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            }],
        }
    }

    fn rewrite_link_plan(
        vault_root: &camino::Utf8PathBuf,
        doc_rel: &str,
        hash: &str,
        old_target: &str,
        new_target: &str,
    ) -> ApplyBatch {
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "rewrite-test".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: Some("link-target-missing".into()),
                finding_rule: None,
                repair_rule: None,
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
        }
    }

    fn remove_field_plan(
        vault_root: &camino::Utf8PathBuf,
        doc_rel: &str,
        hash: &str,
        field: &str,
        expected_old: serde_json::Value,
    ) -> ApplyBatch {
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "remove-test".into(),
                path: doc_rel.into(),
                document_hash: hash.to_string(),
                finding_code: Some("operator-mutation".into()),
                finding_rule: None,
                repair_rule: Some("vault-set".into()),
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
        let (fm, _, _, _) =
            norn_frontmatter::frontmatter::extract_frontmatter(&written, &mut diags);
        assert!(diags.is_empty());
        assert_eq!(
            fm.unwrap().get("up"),
            Some(&serde_json::json!("[[parent-two]]"))
        );
    }

    #[test]
    fn rewrite_breaking_same_plan_created_frontmatter_is_refused() {
        // NRN-141 round 3: the degradation baseline was the pre-ALL-ops on-disk
        // original, so frontmatter CREATED by ops in the same plan escaped the
        // check — add_frontmatter `up: '[[Parent]]'` on a frontmatter-less doc,
        // then a structural-byte rewrite_link in the same plan, wrote the broken
        // block (the on-disk original had no frontmatter → early Ok). The
        // baseline is now the content state ENTERING the rewrite stage
        // (post-frontmatter ops), so the same-plan-created mapping is protected
        // too. add_frontmatter single-quotes the wikilink, so the structural
        // byte here is a single quote in the new target.
        let doc = "see [[Parent]]\n";
        let (_tmp, root, index, hash) =
            make_vault_with_doc("norn-orch-rewrite-created-", "doc.md", doc);
        let mut plan = rewrite_link_plan(&root, "doc.md", &hash, "Parent", "Parent's Two");
        plan.changes.insert(
            0,
            ApplyOp {
                change_id: "add-up".into(),
                path: "doc.md".into(),
                document_hash: hash.clone(),
                finding_code: Some("operator-mutation".into()),
                finding_rule: None,
                repair_rule: Some("vault-set".into()),
                operation: "add_frontmatter".into(),
                field: Some("up".into()),
                expected_old_value: None,
                new_value: Some(serde_json::json!("[[Parent]]")),
                destination: None,
                link_risk: None,
                warnings: Vec::new(),
                force: false,
                parents: false,
            },
        );
        plan.summary.planned_changes = 2;

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false)
            .expect_err("breaking same-plan-created frontmatter must be refused");
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

        let plan = ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "delete-b".into(),
                path: "b.md".into(),
                document_hash: b_doc.hash.clone(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
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
    ) -> ApplyBatch {
        let new_value = serde_json::json!({
            "frontmatter": serde_json::Value::Object(fm),
            "body": body,
        });
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 1,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "create-test".into(),
                path: rel_path.into(),
                document_hash: String::new(),
                finding_code: Some("imperative-create".into()),
                finding_rule: None,
                repair_rule: Some("vault-new".into()),
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

    /// (NRN-146) `atomic_write`'s destination-mode preservation only applies
    /// when the destination already exists. A brand-new `create_document`
    /// target has no prior mode to preserve, so it must land with ordinary
    /// umask-based permissions — the same as any other fresh file written in
    /// this process — not some leftover/default mode from the mode-copy path.
    #[test]
    #[cfg(unix)]
    fn apply_create_document_new_file_gets_default_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, root, index) = make_empty_vault("vault-apply-create-mode-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "foo.md", fm, "Hello\n", false);

        apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();

        // A plain fresh write in the same process, for comparison against
        // whatever the ambient umask makes "default" here.
        let reference_path = root.join("reference.md");
        std::fs::write(reference_path.as_std_path(), "x").unwrap();
        let reference_mode = std::fs::metadata(reference_path.as_std_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        let created_mode = std::fs::metadata(root.join("foo.md").as_std_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(
            created_mode, reference_mode,
            "a brand-new create_document target must get ordinary umask-based \
             permissions, not a mode inherited from a nonexistent destination"
        );
    }

    #[test]
    fn apply_create_document_refuses_unparseable_serialized_frontmatter() {
        // NRN-142: render_key's terminal fallback returns an UNVERIFIED double-
        // quoted render when no quoting rank round-trips — reachable for a field
        // name past libyaml's 1024-byte simple-key parse limit, where the parse
        // side rejects every rank. The create path must gate the fresh document
        // through the read pipeline before writing, refusing cleanly instead of
        // writing frontmatter that can never be read back.
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-badkey-");
        let mut fm = serde_json::Map::new();
        fm.insert("k".repeat(1100), serde_json::json!("v"));
        let plan = create_plan(&root, "foo.md", fm, "body\n", false);

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("foo.md") && msg.contains("parse"),
            "refusal must name the doc and the parse failure, got: {msg}"
        );
        assert!(
            !root.join("foo.md").as_std_path().exists(),
            "nothing may be written on a refused create"
        );
    }

    #[test]
    fn apply_create_document_dry_run_refuses_unparseable_frontmatter() {
        // NRN-142: dry-run must run the same serialize + gate as the real apply,
        // so a plan preview refuses exactly where the apply would — no
        // "would create" for a document the real apply then declines.
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-dry-badkey-");
        let mut fm = serde_json::Map::new();
        fm.insert("k".repeat(1100), serde_json::json!("v"));
        let plan = create_plan(&root, "foo.md", fm, "body\n", false);

        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("foo.md") && msg.contains("parse"),
            "dry-run must surface the gate refusal, got: {msg}"
        );
        assert!(
            !root.join("foo.md").as_std_path().exists(),
            "dry-run must not touch disk"
        );
    }

    #[test]
    fn apply_create_document_gate_refusal_leaves_no_parent_dir() {
        // NRN-142: a `-p` create refused by the gate must be side-effect free —
        // parent directories are created only after the gate passes, so a
        // refusal leaves no empty directory behind.
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-nodirs-");
        let mut fm = serde_json::Map::new();
        fm.insert("k".repeat(1100), serde_json::json!("v"));
        let plan = create_plan(&root, "sub/dir/foo.md", fm, "body\n", false);

        let ctx = CreateApplyContext {
            parents: true,
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let err = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
        assert!(
            !root.join("sub").as_std_path().exists(),
            "a refused create must not leave parent directories behind"
        );
    }

    #[test]
    fn apply_create_document_quote_needing_key_round_trips() {
        // NRN-142: a quote-requiring field name creates fine — the key is emitted
        // quoted and the created document's frontmatter reads back byte-exact.
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-hashkey-");
        let mut fm = serde_json::Map::new();
        fm.insert("#foo".to_string(), serde_json::json!("bar"));
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "foo.md", fm, "body\n", false);

        apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();
        let written = std::fs::read_to_string(root.join("foo.md").as_std_path()).unwrap();
        let mut diagnostics = Vec::new();
        let (parsed, _, _, _) =
            norn_frontmatter::frontmatter::extract_frontmatter(&written, &mut diagnostics);
        assert!(diagnostics.is_empty(), "created doc must parse: {written}");
        let map = parsed.unwrap();
        assert_eq!(map["#foo"], serde_json::json!("bar"), "got: {written}");
        assert_eq!(map["type"], serde_json::json!("note"));
    }

    #[test]
    fn apply_create_document_empty_frontmatter_passes_gate() {
        // An empty frontmatter map serializes to `---\n---\n`, which reads back
        // as YAML null over an empty block — the gate must treat that as the
        // empty mapping, not refuse the create.
        let (_tmp, root, index) = make_empty_vault("vault-apply-create-emptyfm-");
        let plan = create_plan(&root, "foo.md", serde_json::Map::new(), "body\n", false);

        apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap();
        let written = std::fs::read_to_string(root.join("foo.md").as_std_path()).unwrap();
        assert_eq!(written, "---\n---\nbody\n");
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
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let err = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
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
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let report = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
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

    #[test]
    fn apply_create_document_fails_closed_on_seq_when_preresolved_declared() {
        // NRN-265: when a caller declares `creates_preresolved` (the MigrationPlan
        // applier's contract — it resolves `{{seq}}` before delegating) but a
        // `{{seq}}` token still reaches Pass 1e, the pre-resolution/owner-set
        // barrier did not run. The delegate must fail closed rather than allocate
        // a path that barrier never validated — before THIS create writes or
        // creates parent dirs. (Earlier passes of a mixed plan may already have
        // written and would surface as a truthful partial-apply report; this
        // plan is create-only, so nothing is written at all.)
        let (_tmp, root, index) = make_empty_vault("vault-apply-seq-preresolved-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "logs/{{seq}}.md", fm, "Hello\n", false);

        let ctx = CreateApplyContext {
            parents: true,
            creates_preresolved: true,
            ..Default::default()
        };
        let mut sink = discard_sink();
        let spans = std::collections::HashMap::new();
        let err = apply_repair_plan_with_context(
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("pre-resolved") && msg.contains("{{seq}}"),
            "expected fail-closed drift error, got: {msg}"
        );
        assert!(
            !root.join("logs/1.md").as_std_path().exists(),
            "fail-closed guard must run before this create writes"
        );
        assert!(
            !root.join("logs").as_std_path().exists(),
            "guard must fire before this create's parent-dir creation"
        );
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
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
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

        // Build the move plan directly (the `move` verb's preflight is a later
        // port): a single move_document change carrying the classified link risk,
        // exactly the shape `move::preflight_and_plan` synthesizes — src hash from
        // the index, destination `b.md`, and the full LinkRisk over the `[[a]]`
        // backlink in `d.md`. This keeps the assertion (cascade record folded from
        // actuals) at the `apply_repair_plan_with_context` boundary intact.
        let src_rel = camino::Utf8PathBuf::from("a.md");
        let dst_rel = camino::Utf8PathBuf::from("b.md");
        let src_hash = index
            .documents
            .iter()
            .find(|d| d.path == src_rel)
            .map(|d| d.hash.clone())
            .unwrap_or_default();
        let link_risk = Some(crate::standards::classify_link_risk(
            &src_rel,
            &dst_rel,
            &index.documents,
            &index.files,
        ));
        let move_change = ApplyOp {
            change_id: "move-a.md".into(),
            path: src_rel,
            document_hash: src_hash,
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some(dst_rel),
            link_risk,
            warnings: Vec::new(),
            force: false,
            parents: false,
        };
        let plan = ApplyBatch {
            schema_version: crate::standards::REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: root.clone(),
            summary: crate::standards::RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: crate::standards::SkippedSummary::default(),
            },
            changes: vec![move_change],
        };

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
            None,
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
            &root, &index, &plan, /*dry_run=*/ false, &ctx, &mut sink, &spans, None,
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
    ) -> ApplyOp {
        ApplyOp {
            change_id: format!("set-{field}"),
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
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

    fn append_to_section_change(path: &str, hash: &str, heading: &str, content: &str) -> ApplyOp {
        let payload = serde_json::json!({
            "op": "append_to_section",
            "heading": heading,
            "content": content,
        });
        ApplyOp {
            change_id: format!("append-{heading}"),
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
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

    fn plan_with(vault_root: &camino::Utf8PathBuf, changes: Vec<ApplyOp>) -> ApplyBatch {
        ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: vault_root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: changes.len(),
                skipped: SkippedSummary::default(),
            },
            changes,
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
            strip_bom: Vec::new(),
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
            strip_bom: Vec::new(),
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
            strip_bom: Vec::new(),
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

    fn op_change(change_id: &str, path: &str, operation: &str) -> ApplyOp {
        ApplyOp {
            change_id: change_id.into(),
            path: path.into(),
            document_hash: String::new(),
            finding_code: None,
            finding_rule: None,
            repair_rule: None,
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

    // ── NRN-145: vault-root containment gate ──────────────────────────────────
    // The mutation stack must refuse any op target that resolves outside the
    // vault root — via relative `../` traversal, an absolute path, or a
    // symlinked directory inside the vault pointing outside. Vaults are
    // self-contained; outbound symlinks are unsupported. Each escape is refused
    // as a preflight, before any byte is written outside the vault.

    /// A vault nested inside an outer temp dir, so a `../` escape target lands in
    /// a directory this test OWNS (and which is cleaned up), rather than polluting
    /// the shared system temp root. Returns (outer tempdir, vault root, index).
    fn make_nested_vault(prefix: &str) -> (tempfile::TempDir, camino::Utf8PathBuf, GraphIndex) {
        let outer = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let root = camino::Utf8Path::from_path(outer.path())
            .unwrap()
            .join("vault");
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "validate: {}\n",
        )
        .unwrap();
        let index = crate::graph::build_index(&root).unwrap();
        (outer, root, index)
    }

    /// Class 1: a relative `../` traversal target is refused before any write.
    #[test]
    fn containment_refuses_relative_traversal_create() {
        let (outer, root, index) = make_nested_vault("vault-containment-traversal-");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "../escape-traversal.md", fm, "x\n", false);
        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vault"),
            "expected containment refusal, got: {msg}"
        );
        let outside = camino::Utf8Path::from_path(outer.path())
            .unwrap()
            .join("escape-traversal.md");
        assert!(
            !outside.as_std_path().exists(),
            "traversal wrote outside the vault: {outside}"
        );
    }

    /// Class 2: an absolute-path target is refused before any write.
    #[test]
    fn containment_refuses_absolute_path_create() {
        let (_tmp, root, index) = make_empty_vault("vault-containment-abs-");
        let outside_dir = tempfile::Builder::new()
            .prefix("vault-containment-abs-target-")
            .tempdir()
            .unwrap();
        let abs_target = camino::Utf8Path::from_path(outside_dir.path())
            .unwrap()
            .join("escape-abs.md");
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, abs_target.as_str(), fm, "x\n", false);
        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vault"), "got: {msg}");
        assert!(
            !abs_target.as_std_path().exists(),
            "absolute path wrote outside the vault: {abs_target}"
        );
    }

    /// Class 3: a symlinked directory inside the vault pointing outside is
    /// refused — the lexical check (no `..`, not absolute) passes it, so this is
    /// the case the canonicalized-parent containment must catch.
    #[cfg(unix)]
    #[test]
    fn containment_refuses_symlinked_dir_escape_create() {
        let (_tmp, root, index) = make_empty_vault("vault-containment-symlink-");
        let outside_dir = tempfile::Builder::new()
            .prefix("vault-containment-symlink-target-")
            .tempdir()
            .unwrap();
        std::os::unix::fs::symlink(outside_dir.path(), root.join("outlink").as_std_path()).unwrap();
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let plan = create_plan(&root, "outlink/escape-symlink.md", fm, "x\n", false);
        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vault") || msg.contains("symlink"),
            "got: {msg}"
        );
        let outside = camino::Utf8Path::from_path(outside_dir.path())
            .unwrap()
            .join("escape-symlink.md");
        assert!(
            !outside.as_std_path().exists(),
            "symlinked-dir escape wrote outside the vault: {outside}"
        );
    }

    /// Class 4 (whole mutation stack): the same containment covers `move`'s
    /// destination target, not just create — a `../` destination is refused and
    /// the source is left untouched.
    #[test]
    fn containment_refuses_move_destination_traversal() {
        let (outer, root, _index) = make_nested_vault("vault-containment-move-");
        std::fs::write(
            root.join("doc.md").as_std_path(),
            "---\ntype: note\n---\n# Doc\n",
        )
        .unwrap();
        let index = crate::graph::build_index(&root).unwrap();
        let hash = index
            .documents
            .iter()
            .find(|d| d.path == "doc.md")
            .unwrap()
            .hash
            .clone();
        let plan = move_plan(&root, "doc.md", "../escape-move.md", &hash);
        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("vault"), "got: {msg}");
        let outside = camino::Utf8Path::from_path(outer.path())
            .unwrap()
            .join("escape-move.md");
        assert!(
            !outside.as_std_path().exists(),
            "move wrote outside the vault: {outside}"
        );
        assert!(
            root.join("doc.md").as_std_path().exists(),
            "move source should be untouched after a refused destination"
        );
    }

    /// Class 5 (F1, NRN-145 follow-up): a backlink-cascade rewrite SOURCE is a
    /// symlink FILE inside the vault whose parent (the vault root) is
    /// legitimately in-vault, but the file itself resolves outside. The
    /// parent-only containment check passes this (the parent is in-vault), so
    /// without the fix `rewrite_one_backlink`'s bare `fs::write` writes THROUGH
    /// the symlink to the outside file. The gate must canonicalize the target
    /// itself (not just its parent) whenever the target already exists —
    /// cascade sources always exist.
    #[cfg(unix)]
    #[test]
    fn containment_refuses_symlink_file_cascade_target() {
        let outer = tempfile::Builder::new()
            .prefix("vault-containment-cascade-")
            .tempdir()
            .unwrap();
        let outside_dir = outer.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("linker.md");
        std::fs::write(&outside_file, "see [[old]] here\n").unwrap();

        let root = camino::Utf8Path::from_path(outer.path())
            .unwrap()
            .join("vault");
        std::fs::create_dir_all(root.join(".norn").as_std_path()).unwrap();
        std::fs::write(
            root.join(".norn/config.yaml").as_std_path(),
            "validate: {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("old.md").as_std_path(),
            "---\ntype: note\n---\n# Old\n",
        )
        .unwrap();
        // A symlink FILE inside the vault whose parent (the vault root) is
        // legitimately in-vault, but which itself resolves outside.
        std::os::unix::fs::symlink(&outside_file, root.join("linker.md").as_std_path()).unwrap();

        let index = crate::graph::build_index(&root).unwrap();
        let hash = index
            .documents
            .iter()
            .find(|d| d.path == "old.md")
            .unwrap()
            .hash
            .clone();

        // Recorded as a backlink-cascade source for the move below.
        let risk = crate::standards::LinkRisk {
            stem_changed: true,
            directory_changed: false,
            stem_links: vec![crate::standards::AffectedLink {
                source_path: "linker.md".into(),
                raw: "[[old]]".into(),
                kind: crate::domain::LinkKind::Wikilink,
                source_span: None,
                rewritten: "[[new]]".into(),
                unrepresentable: false,
            }],
            path_qualified_wikilinks: vec![],
            markdown_links: vec![],
        };

        let plan = ApplyBatch {
            schema_version: REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: root.clone(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![ApplyOp {
                change_id: "move-old".into(),
                path: "old.md".into(),
                document_hash: hash,
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
                operation: "move_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: Some("new.md".into()),
                link_risk: Some(risk),
                warnings: Vec::new(),
                force: false,
                parents: false,
            }],
        };

        let before = std::fs::read_to_string(&outside_file).unwrap();
        let err = apply_repair_plan(&root, &index, &plan, /*dry_run=*/ false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("vault") || msg.contains("symlink"),
            "got: {msg}"
        );
        let after = std::fs::read_to_string(&outside_file).unwrap();
        assert_eq!(
            before, after,
            "symlink-file cascade target must be byte-unchanged after a refused move"
        );
    }

    /// F3 (test gap, no correctness change): the vault ROOT itself reached via
    /// a symlink. A normal in-vault create and move must still succeed —
    /// guards against the root-canonicalize (`canonical_root`, computed once
    /// from `cwd`) and the op-parent/op-target-canonicalize diverging, which
    /// would make every op on a symlinked-root vault wrongly refuse.
    #[cfg(unix)]
    #[test]
    fn containment_allows_normal_ops_when_vault_root_is_symlinked() {
        let real = tempfile::Builder::new()
            .prefix("vault-containment-root-real-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(real.path().join(".norn")).unwrap();
        std::fs::write(real.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(real.path().join("doc.md"), "---\ntype: note\n---\n# Doc\n").unwrap();

        let link_parent = tempfile::Builder::new()
            .prefix("vault-containment-root-link-")
            .tempdir()
            .unwrap();
        let symlinked_root_std = link_parent.path().join("vault-link");
        std::os::unix::fs::symlink(real.path(), &symlinked_root_std).unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(symlinked_root_std).unwrap();

        let index = crate::graph::build_index(&root).unwrap();

        // Create: a normal in-vault create must still succeed.
        let mut fm = serde_json::Map::new();
        fm.insert("type".to_string(), serde_json::json!("note"));
        let create = create_plan(&root, "new.md", fm, "x\n", false);
        apply_repair_plan(&root, &index, &create, /*dry_run=*/ false)
            .expect("in-vault create on a symlinked-root vault must succeed");
        assert!(root.join("new.md").as_std_path().exists());

        // Move: a normal in-vault move must still succeed.
        let index2 = crate::graph::build_index(&root).unwrap();
        let hash = index2
            .documents
            .iter()
            .find(|d| d.path == "doc.md")
            .unwrap()
            .hash
            .clone();
        let mv = move_plan(&root, "doc.md", "moved.md", &hash);
        apply_repair_plan(&root, &index2, &mv, /*dry_run=*/ false)
            .expect("in-vault move on a symlinked-root vault must succeed");
        assert!(root.join("moved.md").as_std_path().exists());
        assert!(!root.join("doc.md").as_std_path().exists());
    }
}
