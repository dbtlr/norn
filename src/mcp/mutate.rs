//! Shared mutation plumbing for the MCP mutation tools.
//!
//! The MCP mutation contract (`vault.set` today; `vault.new` / `vault.move` /
//! `vault.delete` / `vault.apply` next) must produce the SAME append-only
//! event-stream records a CLI mutation does — that audit trail is how an
//! off-filesystem MCP client gets "audited for free." This module owns the one
//! seam that guarantees it: [`open_mutation_event_sink`], the MCP analogue of
//! `main.rs::open_event_sink`'s real-apply branch.
//!
//! Keeping it here (not duplicated per tool) means every later mutation tool
//! inherits correct auditing by calling the same helper with the same semantics:
//! honor `telemetry.location`, prune/size-cap before opening, and fall back to a
//! `discard` sink if anything about opening the file fails — telemetry must
//! never block or fail a mutation.
//!
//! The post-apply cache-increment commit is NOT wrapped here: every warm
//! mutation tool calls [`VaultEnv::commit_apply_increments`] directly with
//! the apply report's touched-path set (NRN-252 / NRN-158), so there is no
//! one-line passthrough to keep in sync.

use crate::env::{RequestScope, VaultEnv};
use crate::mutation_lock::MutationLock;
use crate::telemetry::{Clock, EventSink, IdGen};
use camino::Utf8Path;

/// Acquire the per-vault mutation lock on an MCP mutation's CONFIRM/apply path.
///
/// **THE ordering invariant (NRN-99 / NRN-106), canonical statement:** on
/// `confirm`, every MCP mutation tool acquires this lock BEFORE any read that
/// feeds the write — the graph-index load, the query-cache open, preflight,
/// and plan synthesis all run lock-held — so a concurrent norn writer cannot
/// drift a file in the read→apply window and slip past both the plan-time
/// hash checks and the applier's index-snapshot check. Dry-run
/// (`confirm: false`) NEVER locks: it is read-only by contract. Each handler
/// guards with `if p.confirm { Some(acquire_mutation_lock(..)?) } else { None }`
/// at the top of its confirm path, mirroring the CLI arms in `main.rs`, which
/// lock before their cache load. Cheap, read-free argument validation (plan
/// parsing, schema-version checks, param-shape refusals) stays BEFORE the
/// lock so malformed input never contends.
///
/// Sweeps stale pending markers, then acquires the lock with `is_apply = true`.
/// Returns the RAII guard the caller must hold for the duration of the apply.
///
/// On contention the TYPED [`CacheError::MutationLockTimeout`](crate::cache::CacheError::MutationLockTimeout)
/// propagates (NRN-229) — not a bail'd string — so the tool's `handle_output`
/// recovers it via [`refusal_from_error`] and returns a coded, structured
/// `mutation-lock-timeout` refusal RESULT. This is the MCP analogue of the CLI's
/// lock block in `main.rs`, which maps a timeout to exit code 2 + a stderr line;
/// here it becomes the same-coded structured refusal every mutation tool shares.
pub(crate) fn acquire_mutation_lock(cwd: &Utf8Path) -> anyhow::Result<Option<MutationLock>> {
    let (_, state_dir) = crate::cache::state_dir_for(cwd)
        .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    crate::mutation_lock::pending::sweep_pending(&state_dir);
    match MutationLock::acquire_if_mutating(&state_dir, /*is_apply=*/ true) {
        Ok(guard) => Ok(guard),
        // NRN-229: propagate the TYPED `MutationLockTimeout` (not a bail'd string)
        // so it survives `downcast_ref` — each mutation tool's `handle_output`
        // recovers it via `refusal_from_error` and returns a coded, structured
        // `mutation-lock-timeout` refusal RESULT (isError + report) instead of a
        // JSON-RPC protocol error that a committing routed apply can't tell apart
        // from an unknown-state transport failure.
        Err(e @ crate::cache::CacheError::MutationLockTimeout) => Err(e.into()),
        Err(e) => anyhow::bail!("mutation lock error: {e}"),
    }
}

/// Extract a structured refusal envelope (NRN-220) from a single-op mutation
/// error, IF it is a recognized precondition/refusal carrying a stable machine
/// `code`.
///
/// Returns `Some` for the coded refusal types the mutation tools can raise — the
/// rich apply-time [`ApplyError`](crate::standards::apply::ApplyError) (CAS /
/// precondition), a [`ContainmentError`](crate::standards::apply::ContainmentError),
/// the `edit` anchor/CAS family ([`EditError`](crate::edit::transform::EditError)),
/// `vault.new`'s [`PreflightError`](crate::new::validate::PreflightError),
/// its target-resolution family ([`NewResolveError`](crate::new::NewResolveError)),
/// its body-scaffold render refusal
/// ([`BodyScaffoldRenderError`](crate::new::BodyScaffoldRenderError))
/// and its plan-synthesis family ([`SynthError`](crate::new::synth::SynthError),
/// NRN-230), `vault.set`'s schema/argument-refusal family
/// ([`SetError`](crate::set::error::SetError), NRN-221), the `move` / `delete` /
/// `rewrite_wikilink` typed preflight refusals and the per-vault
/// mutation-lock timeout (NRN-229). Returns `None` for everything else — IO,
/// cache corruption, and other genuinely internal failures — so those still
/// propagate as a bare MCP `Err` rather than being laundered into a misleading
/// `internal-error` structured refusal.
///
/// This is the deliberate counterpart to
/// [`ApplyError::from_anyhow`](crate::apply_report::ApplyError::from_anyhow),
/// which ALWAYS produces an envelope (falling back to `internal-error`): here a
/// non-refusal must stay a non-refusal.
// Adding a refusal code: register it in BOTH this ladder and
// `apply_report::ApplyError::from_anyhow`, and add the row to docs/errors.md.
// See the checklist there. Superseded when the CodedError trait lands (NRN-236).
pub(crate) fn refusal_from_error(e: &anyhow::Error) -> Option<crate::apply_report::ApplyError> {
    use crate::apply_report::ApplyError as Envelope;
    use crate::standards::apply::{ApplyError as RichApplyError, ContainmentError};

    if let Some(rich) = e.downcast_ref::<RichApplyError>() {
        return Some(Envelope::from_rich(rich));
    }
    if let Some(c) = e.downcast_ref::<ContainmentError>() {
        return Some(Envelope::from_containment(c));
    }
    if let Some(ed) = e.downcast_ref::<crate::edit::transform::EditError>() {
        return Some(Envelope {
            code: ed.code().to_string(),
            message: ed.to_string(),
            path: ed.path().map(str::to_string),
        });
    }
    if let Some(pf) = e.downcast_ref::<crate::new::validate::PreflightError>() {
        return Some(Envelope {
            code: pf.code().to_string(),
            message: pf.to_string(),
            path: pf.path(),
        });
    }
    // NRN-230 (PR A, F3): `vault.new`'s three-mode target-resolution refusals
    // (both path+rule given, unknown/non-creatable rule, `generate_path`'s
    // missing-var/missing-title/render/seq family, no inbox configured) —
    // previously bare `anyhow!()` strings that laundered to `internal-error`.
    if let Some(re) = e.downcast_ref::<crate::new::NewResolveError>() {
        return Some(Envelope {
            code: re.code().to_string(),
            message: re.to_string(),
            path: None,
        });
    }
    // NRN-230 (PR A): a rule `body` scaffold template that fails to render —
    // previously stringified via `anyhow!("body scaffold render error: {e}")`
    // at both call sites, destroying the type. Reuses `template-render-failed`
    // (same semantic as a path-template render failure; one-semantic-one-code);
    // the message carries the site-distinguishing prose.
    if let Some(bs) = e.downcast_ref::<crate::new::BodyScaffoldRenderError>() {
        return Some(Envelope {
            code: bs.code().to_string(),
            message: bs.to_string(),
            path: None,
        });
    }
    // NRN-230 (PR A, F4): `vault.new`'s plan-synthesis refusal family
    // (`SynthError`) — previously stringified via `anyhow!("{e}")` before it
    // reached this seam, destroying the type.
    if let Some(se) = e.downcast_ref::<crate::new::synth::SynthError>() {
        return Some(Envelope {
            code: se.code().to_string(),
            message: se.to_string(),
            path: se.path(),
        });
    }
    if let Some(se) = e.downcast_ref::<crate::set::error::SetError>() {
        // No SetError variant resolves a vault-relative document path — the
        // offending identifier is a field name or a raw (possibly unresolved)
        // target token, not a path the envelope can name.
        return Some(Envelope {
            code: se.code().to_string(),
            message: se.to_string(),
            path: None,
        });
    }
    // NRN-229: the `move` / `delete` / `rewrite_wikilink` typed PREFLIGHT
    // refusals. Previously these `anyhow::bail!("{e}")`-ed in the tool handlers,
    // discarding the type — so the code laundered to `internal-error` (or, on a
    // committing routed apply, `post-send-uncertain`). Recovered here, each
    // becomes a coded, structured `refused` report.
    if let Some(mv) = e.downcast_ref::<crate::r#move::MovePreflightError>() {
        return Some(Envelope {
            code: mv.code().to_string(),
            message: mv.to_string(),
            path: None,
        });
    }
    if let Some(del) = e.downcast_ref::<crate::delete::DeletePreflightError>() {
        return Some(Envelope {
            code: del.code().to_string(),
            message: del.to_string(),
            path: None,
        });
    }
    if let Some(rw) =
        e.downcast_ref::<crate::planner::intent::rewrite_wikilink::RewriteWikilinkError>()
    {
        return Some(Envelope {
            code: rw.code().to_string(),
            message: rw.to_string(),
            path: None,
        });
    }
    // NRN-229: the per-vault mutation-lock TIMEOUT. `acquire_mutation_lock` now
    // propagates the typed `CacheError::MutationLockTimeout`; recover it here so a
    // lock-contention refusal is a coded, structured `mutation-lock-timeout`
    // RESULT across every mutation tool, not a bare protocol error. `from_anyhow`
    // already recognizes the same code for the CLI `--format json` envelope.
    if let Some(cache) = e.downcast_ref::<crate::cache::CacheError>() {
        if matches!(cache, crate::cache::CacheError::MutationLockTimeout) {
            return Some(Envelope {
                code: "mutation-lock-timeout".to_string(),
                message: cache.to_string(),
                path: None,
            });
        }
    }
    None
}

/// Open a real, file-backed event sink for an MCP mutation's CONFIRM/apply path,
/// mirroring `main.rs::open_event_sink`'s real-apply branch exactly:
///
/// - resolves the events dir from `telemetry.location` if configured, else
///   `cache::events_dir_for(vault_root)`;
/// - prunes by retention and enforces the size cap before opening (same order as
///   the CLI);
/// - opens the daily file via [`EventSink::open`], falling back to
///   [`EventSink::discard`] if the dir can't be resolved or opened.
///
/// **Best-effort by contract:** every failure mode degrades to an in-memory
/// `discard` sink rather than erroring, so telemetry can never block, fail, or
/// roll back the mutation. The returned sink still mints a real trace id (used as
/// the report's `trace_id`) even in the degraded case.
///
/// Use this ONLY on the confirm/apply path. Dry-run must keep `EventSink::discard`
/// directly (the CLI's dry-run branch does the same) so a plan-only call persists
/// nothing.
pub(crate) fn open_mutation_event_sink(ctx: &VaultEnv, scope: &RequestScope) -> EventSink {
    let ids = IdGen::new();
    let clock = Clock::System;
    let start_ts = clock.now_rfc3339();

    // Read the request's bound config (NRN-253), not the live stored one.
    let config = scope.config();
    let telemetry = config.vault_config.telemetry.as_ref();
    let dir = telemetry
        .and_then(|t| t.location.clone())
        .map(camino::Utf8PathBuf::from)
        .or_else(|| {
            crate::cache::events_dir_for(&ctx.vault_root)
                .ok()
                .map(|(_, d)| d)
        });
    let retention = telemetry
        .and_then(|t| t.retention)
        .unwrap_or(crate::standards::DEFAULT_RETENTION);

    if let Some(dir) = dir.as_ref() {
        let today = &start_ts[..10];
        crate::telemetry::store::prune_events(dir, retention, today);
        crate::telemetry::store::enforce_size_cap(
            dir,
            crate::telemetry::store::EVENTS_SIZE_CAP_BYTES,
            today,
        );
        EventSink::open(dir, start_ts, ids, clock)
            .unwrap_or_else(|_| EventSink::discard(IdGen::new(), Clock::System))
    } else {
        EventSink::discard(ids, clock)
    }
}

#[cfg(test)]
mod lock_ordering_tests {
    use crate::env::VaultEnv;
    use camino::Utf8PathBuf;
    use fs2::FileExt;
    use tempfile::TempDir;

    /// RAII cleanup for the per-vault state dir under the REAL XDG state home.
    ///
    /// We deliberately do NOT set `XDG_STATE_HOME` in-process:
    /// `std::env::set_var` is process-global and races other in-binary tests
    /// that resolve state/cache dirs mid-flight (the same reason
    /// `server.rs::cold_seeded_vault` documents for the cache dir). Vault
    /// identity is a hash of the unique tempdir root, so the state dir is
    /// already test-private; this guard removes it on drop — panic included —
    /// so the held `.mutation.lock` never outlives the test run.
    struct StateDirCleanup(Utf8PathBuf);
    impl Drop for StateDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.as_std_path());
        }
    }

    /// Seed a temp vault with one real document, hold its `.mutation.lock`
    /// exclusively (the same fs2 mechanism `tests/mutation_lock.rs` uses for
    /// the CLI arms), and return everything the ordering assertions need.
    fn contended_vault() -> (TempDir, Utf8PathBuf, std::fs::File, StateDirCleanup) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-lock-order-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("doc.md"), "---\ntype: note\n---\nHello\n").unwrap();

        let (_, state_dir) =
            crate::cache::state_dir_for(&root).expect("resolve state dir for lock path");
        std::fs::create_dir_all(state_dir.as_std_path()).unwrap();
        let cleanup = StateDirCleanup(state_dir.clone());
        let lock_path = state_dir.join(".mutation.lock");
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path.as_std_path())
            .unwrap();
        held.try_lock_exclusive()
            .expect("test setup: could not hold the mutation lock");
        (tmp, root, held, cleanup)
    }

    /// NRN-106: on the confirm path, EVERY mutation tool acquires the mutation
    /// lock BEFORE any read that feeds the write. Proven by ORDER, not just
    /// presence, wherever the harness allows: each case targets a document
    /// that does not exist, so a preflight-first ordering would fail fast with
    /// that tool's not-found/preflight error (naming the bogus target) and
    /// never reach the lock. With the lock held by a simulated concurrent
    /// writer, every confirm call must instead surface the lock-contention
    /// refusal — and never a message naming the bogus target.
    ///
    /// For `vault.rewrite_wikilink` and `vault.apply` the pre-lock steps
    /// (index load / plan parse) cannot observably fail on a bogus target, so
    /// their cases pin contention-refusal-under-held-lock rather than strict
    /// error precedence.
    ///
    /// Runs all seven tools in one test against one contended vault, with the
    /// debug-only `NORN_MUTATION_LOCK_TIMEOUT_MS` override keeping each
    /// contended acquire at ~150ms instead of the real 5s.
    #[test]
    fn confirm_locks_before_any_preflight_read_across_all_mutation_tools() {
        // Debug-build-only knob (compiled out of release); other tests never
        // contend on a mutation lock, so a process-visible short timeout is
        // inert outside this test.
        std::env::set_var("NORN_MUTATION_LOCK_TIMEOUT_MS", "150");

        let (_tmp, root, held, _cleanup) = contended_vault();
        let ctx = VaultEnv::open(&root, None).expect("open ctx");

        type Case = (&'static str, Box<dyn Fn(&VaultEnv) -> anyhow::Error>);
        let cases: Vec<Case> = vec![
            (
                "set",
                Box::new(|ctx| {
                    crate::mcp::tools::set::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::set::SetParams {
                            target: "bogus-target".into(),
                            field_json: vec![r#"status="active""#.into()],
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect_err("held lock must refuse vault.set confirm")
                }),
            ),
            (
                "edit",
                Box::new(|ctx| {
                    crate::mcp::tools::edit::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::edit::EditParams {
                            target: "bogus-target".into(),
                            edits: vec![crate::edit::ops::EditOp::StrReplace {
                                old: "Hello".into(),
                                new: "Goodbye".into(),
                                replace_all: false,
                            }],
                            expected_hash: None,
                            confirm: true,
                        },
                    )
                    .expect_err("held lock must refuse vault.edit confirm")
                }),
            ),
            (
                "delete",
                Box::new(|ctx| {
                    crate::mcp::tools::delete::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::delete::DeleteParams {
                            target: "bogus-target.md".into(),
                            rewrite_to: None,
                            allow_broken_links: true,
                            confirm: true,
                        },
                    )
                    .expect_err("held lock must refuse vault.delete confirm")
                }),
            ),
            (
                "move",
                Box::new(|ctx| {
                    crate::mcp::tools::move_doc::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::move_doc::MoveParams {
                            from: "bogus-target.md".into(),
                            to: "dst.md".into(),
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect_err("held lock must refuse vault.move confirm")
                }),
            ),
            (
                "new",
                Box::new(|ctx| {
                    crate::mcp::tools::new::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::new::NewParams {
                            // Missing parent dir without `parents` — a
                            // preflight-first ordering refuses on this path.
                            path: Some("bogus-target/nested.md".to_string()),
                            parents: false,
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect_err("held lock must refuse vault.new confirm")
                }),
            ),
            (
                "rewrite_wikilink",
                Box::new(|ctx| {
                    crate::mcp::tools::rewrite_wikilink::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::rewrite_wikilink::RewriteWikilinkParams {
                            from: "bogus-target".into(),
                            to: "doc".into(),
                            confirm: true,
                        },
                    )
                    .expect_err("held lock must refuse vault.rewrite_wikilink confirm")
                }),
            ),
            (
                "apply",
                Box::new(|ctx| {
                    let plan = serde_json::json!({
                    "schema_version": 2,
                                    "vault_root": ctx.vault_root.to_string(),
                                    "operations": [{
                                        "kind": "delete_document",
                                        "fields": { "path": "bogus-target.md" }
                                    }]
                                });
                    crate::mcp::tools::apply::handle(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::apply::ApplyParams {
                            plan,
                            confirm: true,
                            parents: false,
                        },
                    )
                    .expect_err("held lock must refuse vault.apply confirm")
                }),
            ),
        ];

        for (tool, call) in cases {
            let msg = call(&ctx).to_string();
            assert!(
                msg.contains("another norn mutation is in progress"),
                "[{tool}] expected the lock-contention refusal (proving the \
                 lock is acquired before the preflight read), got: {msg}"
            );
            assert!(
                !msg.contains("bogus-target"),
                "[{tool}] no preflight read may run while the lock is held — \
                 the error must not name the bogus target: {msg}"
            );
        }

        drop(held);
    }

    /// NRN-229: under lock contention, every mutation tool's `handle_output`
    /// returns a STRUCTURED, coded refusal RESULT (`isError:true` + the
    /// `mutation-lock-timeout` code in the structured content) — not a bare
    /// JSON-RPC protocol error. This is what makes a routed apply's daemon-side
    /// lock contention reconcilable with Direct's clean refusal (the seam the next
    /// checkpoint routes over). Covers all seven mutation tools in one contended
    /// vault. Normalizes each tool's typed output to `(isError, structured JSON)`.
    #[test]
    fn confirm_lock_timeout_is_a_structured_coded_refusal_across_all_tools() {
        std::env::set_var("NORN_MUTATION_LOCK_TIMEOUT_MS", "150");

        let (_tmp, root, held, _cleanup) = contended_vault();
        let ctx = VaultEnv::open(&root, None).expect("open ctx");

        type Case = (
            &'static str,
            Box<dyn Fn(&VaultEnv) -> (bool, serde_json::Value)>,
        );
        let cases: Vec<Case> = vec![
            (
                "set",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::set::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::set::SetParams {
                            target: "doc.md".into(),
                            field_json: vec![r#"status="active""#.into()],
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "edit",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::edit::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::edit::EditParams {
                            target: "doc.md".into(),
                            edits: vec![crate::edit::ops::EditOp::StrReplace {
                                old: "Hello".into(),
                                new: "Goodbye".into(),
                                replace_all: false,
                            }],
                            expected_hash: None,
                            confirm: true,
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "new",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::new::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::new::NewParams {
                            path: Some("fresh.md".to_string()),
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "move",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::move_doc::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::move_doc::MoveParams {
                            from: "doc.md".into(),
                            to: "dst.md".into(),
                            confirm: true,
                            ..Default::default()
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "delete",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::delete::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::delete::DeleteParams {
                            target: "doc.md".into(),
                            rewrite_to: None,
                            allow_broken_links: true,
                            confirm: true,
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "rewrite_wikilink",
                Box::new(|ctx| {
                    let r = crate::mcp::tools::rewrite_wikilink::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::rewrite_wikilink::RewriteWikilinkParams {
                            from: "doc".into(),
                            to: "renamed".into(),
                            confirm: true,
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
            (
                "apply",
                Box::new(|ctx| {
                    let plan = serde_json::json!({
                    "schema_version": 2,
                                    "vault_root": ctx.vault_root.to_string(),
                                    "operations": [{
                                        "kind": "delete_document",
                                        "fields": { "path": "doc.md" }
                                    }]
                                });
                    let r = crate::mcp::tools::apply::handle_output(
                        ctx,
                        &ctx.begin_request().unwrap(),
                        crate::mcp::tools::apply::ApplyParams {
                            plan,
                            confirm: true,
                            parents: false,
                        },
                    )
                    .expect("lock timeout must be a structured refusal, not Err");
                    (r.is_error(), serde_json::to_value(r.value()).unwrap())
                }),
            ),
        ];

        for (tool, call) in cases {
            let (is_error, value) = call(&ctx);
            assert!(
                is_error,
                "[{tool}] a confirmed lock-timeout refusal must map to isError:true"
            );
            let json = value.to_string();
            assert!(
                json.contains("mutation-lock-timeout"),
                "[{tool}] structured content must carry the mutation-lock-timeout \
                 code, got: {json}"
            );
        }

        drop(held);
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::refusal_from_error;

    /// A rich apply-time `ApplyError` (the TOCTOU CAS path) is recognized and
    /// keeps its stable kebab code — the machine-branchable value survives.
    #[test]
    fn rich_apply_error_yields_its_code() {
        let e = anyhow::anyhow!(crate::standards::apply::ApplyError::StaleDocumentHash {
            path: "a.md".into(),
            expected: "aaaa".into(),
            actual: "bbbb".into(),
        });
        let env = refusal_from_error(&e).expect("a rich ApplyError is a recognized refusal");
        assert_eq!(env.code, "stale-document-hash");
        assert_eq!(env.path.as_deref(), Some("a.md"));
    }

    /// The `edit` anchor family (`EditError`) is recognized with its code.
    #[test]
    fn edit_error_yields_its_code() {
        let e: anyhow::Error = crate::edit::transform::EditError::StrNotFound {
            index: 0,
            kind: "str_replace",
            anchor: "nope".into(),
        }
        .into();
        assert_eq!(
            refusal_from_error(&e).expect("recognized").code,
            "anchor-not-found"
        );
    }

    /// `vault.new`'s `PreflightError` is recognized with its code + path.
    #[test]
    fn preflight_error_yields_its_code() {
        let e: anyhow::Error =
            crate::new::validate::PreflightError::DestinationExists("x.md".into()).into();
        let env = refusal_from_error(&e).expect("recognized");
        assert_eq!(env.code, "destination-exists");
        assert_eq!(env.path.as_deref(), Some("x.md"));
    }

    /// NRN-230 (F3): `vault.new`'s `NewResolveError` family is recognized with
    /// its code. `GeneratePath`'s transparent delegation carries the inner
    /// `GeneratePathError`'s own code through unchanged.
    #[test]
    fn new_resolve_error_yields_its_code() {
        let e: anyhow::Error = crate::new::NewResolveError::UnknownRule("bogus".into()).into();
        let env = refusal_from_error(&e).expect("a NewResolveError is a recognized refusal");
        assert_eq!(env.code, "unknown-rule");
        assert_eq!(env.message, "unknown rule `bogus`");

        let e: anyhow::Error = crate::new::NewResolveError::GeneratePath(
            crate::new::generate::GeneratePathError::MissingTitle,
        )
        .into();
        let env = refusal_from_error(&e).expect("recognized");
        assert_eq!(
            env.code, "missing-title",
            "GeneratePath must delegate to the inner GeneratePathError's code"
        );
    }

    /// NRN-230: a rule `body` scaffold render failure is recognized with the
    /// REUSED `template-render-failed` code (same semantic as a path-template
    /// render failure), keeping the exact "body scaffold render error: …"
    /// Display as the message.
    #[test]
    fn body_scaffold_render_error_yields_its_code() {
        let e: anyhow::Error = crate::new::BodyScaffoldRenderError(
            crate::standards::substitution::RenderError::UnknownVariable {
                name: "bogus".into(),
                hint: String::new(),
            },
        )
        .into();
        let env =
            refusal_from_error(&e).expect("a BodyScaffoldRenderError is a recognized refusal");
        assert_eq!(env.code, "template-render-failed");
        assert_eq!(
            env.message,
            "body scaffold render error: unknown variable `bogus`"
        );
        assert_eq!(env.path, None);
    }

    /// NRN-230 (F4): `vault.new`'s plan-synthesis refusal family (`SynthError`)
    /// is recognized with its code — previously stringified via
    /// `anyhow!("{e}")` before reaching this seam. `PathIgnored` also carries
    /// the offending path.
    #[test]
    fn synth_error_yields_its_code() {
        let e: anyhow::Error = crate::new::synth::SynthError::PathIgnored {
            path: "scratch/x.md".into(),
        }
        .into();
        let env = refusal_from_error(&e).expect("a SynthError is a recognized refusal");
        assert_eq!(env.code, "path-ignored");
        assert_eq!(env.path.as_deref(), Some("scratch/x.md"));

        let e: anyhow::Error = crate::new::synth::SynthError::InvalidField("bogus".into()).into();
        assert_eq!(
            refusal_from_error(&e).expect("recognized").code,
            "assignment-malformed"
        );
    }

    /// `vault.set`'s schema/argument-refusal family (`SetError`, NRN-221) is
    /// recognized with its code. This is the behavior change NRN-221 makes: a
    /// `set` schema refusal used to fall into the `None` bucket (see the git
    /// history of this test) and is now a structured, coded refusal like
    /// `edit`/`new`.
    #[test]
    fn set_error_yields_its_code() {
        let e: anyhow::Error = crate::set::error::SetError::ValueNotAllowed {
            field: "status".into(),
            value: "bogus".into(),
            allowed: "backlog, done".into(),
        }
        .into();
        let env = refusal_from_error(&e).expect("a SetError is a recognized refusal");
        assert_eq!(env.code, "value-not-allowed");
        assert_eq!(env.path, None);
    }

    /// NRN-229: the `move` typed preflight family is recognized with its code.
    #[test]
    fn move_preflight_error_yields_its_code() {
        let e: anyhow::Error =
            crate::r#move::MovePreflightError::SourceMissing("a.md".into()).into();
        assert_eq!(
            refusal_from_error(&e)
                .expect("a MovePreflightError is a recognized refusal")
                .code,
            "target-not-found"
        );
        let e: anyhow::Error = crate::r#move::MovePreflightError::SamePath("a.md".into()).into();
        assert_eq!(
            refusal_from_error(&e).expect("recognized").code,
            "source-destination-same"
        );
    }

    /// NRN-229: the `delete` typed preflight family is recognized with its code.
    #[test]
    fn delete_preflight_error_yields_its_code() {
        let e: anyhow::Error =
            crate::delete::DeletePreflightError::IncomingLinksRefused { count: 2 }.into();
        assert_eq!(
            refusal_from_error(&e)
                .expect("a DeletePreflightError is a recognized refusal")
                .code,
            "backlinks-present"
        );
    }

    /// NRN-229: `rewrite_wikilink`'s typed OLD-unresolvable refusal is recognized
    /// — previously a bare `anyhow!` string that laundered to `internal-error`.
    #[test]
    fn rewrite_wikilink_error_yields_its_code() {
        let e: anyhow::Error =
            crate::planner::intent::rewrite_wikilink::RewriteWikilinkError::OldUnresolved(
                "old".into(),
            )
            .into();
        assert_eq!(
            refusal_from_error(&e)
                .expect("a RewriteWikilinkError is a recognized refusal")
                .code,
            "target-not-found"
        );
    }

    /// NRN-229: the per-vault mutation-lock TIMEOUT is a recognized refusal
    /// (`mutation-lock-timeout`), so a routed apply hitting daemon-side lock
    /// contention becomes a clean coded refusal, not `post-send-uncertain`.
    #[test]
    fn mutation_lock_timeout_yields_its_code() {
        let e: anyhow::Error = crate::cache::CacheError::MutationLockTimeout.into();
        let env = refusal_from_error(&e).expect("a lock-timeout is a recognized refusal");
        assert_eq!(env.code, "mutation-lock-timeout");
        assert!(
            env.message.contains("another norn mutation is in progress"),
            "message keeps the contention prose; got: {}",
            env.message
        );
    }

    /// THE load-bearing invariant (NRN-220/221): an UNRECOGNIZED error — an
    /// opaque anyhow-wrapped string that matches none of the typed refusal
    /// families above — returns `None`, so it stays a bare MCP `Err` and is NOT
    /// laundered into a misleading `internal-error` structured refusal.
    #[test]
    fn unrecognized_prose_error_is_not_a_refusal() {
        let e = anyhow::anyhow!("some genuinely internal failure with no typed home");
        assert!(
            refusal_from_error(&e).is_none(),
            "uncoded prose must propagate as a bare Err, not a laundered refusal"
        );
    }
}
