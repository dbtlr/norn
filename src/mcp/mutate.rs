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

use crate::mcp::context::VaultContext;
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
/// Returns the RAII guard the caller must hold for the duration of the apply;
/// a timeout or lock error becomes an `anyhow` error.
///
/// This is the MCP analogue of the CLI's lock block in `main.rs`, which differs
/// deliberately: the CLI maps a timeout to exit code 2 + a stderr line, whereas
/// an MCP tool surfaces it as a tool error.
pub(crate) fn acquire_mutation_lock(cwd: &Utf8Path) -> anyhow::Result<Option<MutationLock>> {
    let (_, state_dir) = crate::cache::state_dir_for(cwd)
        .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    crate::mutation_lock::pending::sweep_pending(&state_dir);
    match MutationLock::acquire_if_mutating(&state_dir, /*is_apply=*/ true) {
        Ok(guard) => Ok(guard),
        Err(crate::cache::CacheError::MutationLockTimeout) => anyhow::bail!(
            "another norn mutation is in progress against this vault (timed out after 5 s)"
        ),
        Err(e) => anyhow::bail!("mutation lock error: {e}"),
    }
}

/// Extract a structured refusal envelope (NRN-220) from a single-op mutation
/// error, IF it is a recognized precondition/refusal carrying a stable machine
/// `code`.
///
/// Returns `Some` for the coded refusal types the single-op mutators
/// (`set`/`edit`/`new`) can raise — the rich apply-time
/// [`ApplyError`](crate::standards::apply::ApplyError) (CAS / precondition), a
/// [`ContainmentError`](crate::standards::apply::ContainmentError), the `edit`
/// anchor/CAS family ([`EditError`](crate::edit::transform::EditError)),
/// `vault.new`'s [`PreflightError`](crate::new::validate::PreflightError), and
/// `vault.set`'s schema/argument-refusal family
/// ([`SetError`](crate::set::error::SetError), NRN-221). Returns `None` for
/// everything else — IO, cache corruption, and other genuinely internal
/// failures — so those still propagate as a bare MCP `Err` rather than being
/// laundered into a misleading `internal-error` structured refusal.
///
/// This is the deliberate counterpart to
/// [`ApplyError::from_anyhow`](crate::apply_report::ApplyError::from_anyhow),
/// which ALWAYS produces an envelope (falling back to `internal-error`): here a
/// non-refusal must stay a non-refusal.
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
pub(crate) fn open_mutation_event_sink(ctx: &VaultContext) -> EventSink {
    let ids = IdGen::new();
    let clock = Clock::System;
    let start_ts = clock.now_rfc3339();

    let config = ctx.config();
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
    use crate::mcp::context::VaultContext;
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
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        type Case = (&'static str, Box<dyn Fn(&VaultContext) -> anyhow::Error>);
        let cases: Vec<Case> = vec![
            (
                "set",
                Box::new(|ctx| {
                    let mut set = std::collections::BTreeMap::new();
                    set.insert("status".to_string(), serde_json::json!("active"));
                    crate::mcp::tools::set::handle(
                        ctx,
                        crate::mcp::tools::set::SetParams {
                            target: "bogus-target".into(),
                            set,
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
                        "schema_version": 1,
                        "vault_root": ctx.vault_root.to_string(),
                        "operations": [{
                            "kind": "delete_document",
                            "fields": { "path": "bogus-target.md" }
                        }]
                    });
                    crate::mcp::tools::apply::handle(
                        ctx,
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
