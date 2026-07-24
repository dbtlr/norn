//! The routed-read entry the read verbs share: resolve the target vault,
//! summon-or-connect its owner, and wait for it to be ready.
//!
//! This is the CLI's whole job on the request side of a read (ADR 0016/0017):
//! turn the global flags into a resolved vault, hand it to `norn-client` (the
//! only crate that spawns owners or dials sockets), and return a live
//! [`OwnerSession`] the verb sends its `Params` to.

use std::path::Path;
use std::time::Duration;

use norn_client::{
    open, ClientError, ConfigHome, OwnerSession, Registry, ResolveInput, ResolvedVia, SummonConfig,
};
use norn_config::ConfigError;

use crate::cli::GlobalArgs;
use crate::display::Diagnostic;

/// How long a read waits for a freshly-summoned owner to finish warming up
/// before giving up. Warm-up is size-linear (a one-shot full cache build), so
/// this is a generous ceiling, not an expected latency: agent bursts and small
/// vaults are ready in well under a second, while a very large vault's first
/// summon still completes inside the budget. The owner answers pings promptly
/// throughout warm-up, so a truly hung owner is caught by the per-request stall
/// budget long before this ceiling — this only bounds a legitimately slow build.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

/// Resolve the target vault from the global flags, summon-or-connect its owner,
/// and return a ready session. On failure returns a soft-landing [`Diagnostic`]
/// the caller presents on stderr (NRN-361): the headline plus a next-step hint
/// where a diagnosis path genuinely exists (unknown vault name → `vault list`,
/// a stale registry entry → re-register, and so on).
pub fn open_session(global: &GlobalArgs) -> Result<OwnerSession, Diagnostic> {
    let cwd = std::env::current_dir()
        .map_err(|e| Diagnostic::new(format!("cannot read the current directory: {e}")))?;
    let home = ConfigHome::from_env().map_err(|e| config_error_diagnostic(&e))?;

    let input = ResolveInput {
        explicit_path: global.cwd.clone(),
        explicit_name: global.vault.clone(),
        cwd,
        norn_root_env: std::env::var("NORN_ROOT").ok(),
    };

    let registry = Registry::new(home);
    let resolved = registry
        .resolve(&input)
        .map_err(|e| config_error_diagnostic(&e))?;

    // Vault-root precheck (NRN-414): the direct-path vias (`-C` and NORN_ROOT)
    // name a root that never passes through the registry's live-directory gate
    // (`ensure_live`), so a bad path would otherwise be discovered only when the
    // summoned owner's warm-up fails — after the full connect budget, blaming the
    // socket. Check existence here, instantly, BEFORE summoning: a missing or
    // non-directory root is a clear USER error naming the actual path and how it
    // was supplied, with no owner spawned and no socket dialed. The registry vias
    // (name / binding / reverse-lookup) already fail loud via `ensure_live`
    // (`StaleEntry`, which names the registered vault), so they are not rechecked
    // here. Post-NRN-415 resolution canonicalizes an existing root and falls back
    // to the grounded path for a missing one, so this composes cleanly: a
    // fallback-path root that does not exist IS this user-error case.
    if let Some(diagnostic) = vault_root_precheck(&resolved.root, &resolved.via) {
        return Err(diagnostic);
    }

    // A registered vault may carry a `[vaults.<name>].config` override; the
    // summoned owner warms under it (ADR 0017 resolver-derived config). An
    // unregistered cwd (the common ephemeral case) has no override — the owner
    // loads `<root>/.norn/config.yaml`. `resolve` already looked the entry up
    // for a registry via, so the resolved entry itself carries the override —
    // no second registry lookup here.
    let config_override = resolved.vault.as_ref().and_then(|v| v.config.clone());

    // Registration is what unlocks durable telemetry (NRN-400): a registered
    // vault resolves an events dir (honoring a `logs` override) the owner writes
    // the mutation event stream under and `audit` reads back; an unregistered
    // root resolves `None`, so its owner keeps in-memory (ephemeral) telemetry.
    //
    // Durability is keyed on whether the RESOLVED ROOT is registered, NOT on how
    // this invocation addressed it — an owner is keyed by vault root + build, so
    // a registered root reached by `-C <path>` (resolver name `None`) must
    // resolve the SAME events dir as one reached by `--vault <name>`, or the
    // first summon would decide durability by luck of addressing. A reverse
    // lookup over the canonical root answers "is this root registered?"
    // independent of the addressing.
    //
    // Fail-safe on a registry I/O or parse error: `.ok()` alone would collapse
    // `Err` onto the same `None` as a genuinely unregistered root, silently
    // dropping durability for a vault that IS registered — the operator would
    // lose the audit trail with no signal. Distinguish the two: an `Err` prints
    // one client-side warning through the closed `warning:` vocabulary and
    // proceeds with `events_dir = None`; the mutation itself is never blocked
    // on the registry read.
    let events_dir = match registry.reverse_lookup(&resolved.root) {
        Ok(found) => found.and_then(|vault| {
            norn_config::events_dir_for(
                |key| std::env::var_os(key),
                &resolved.root,
                vault.logs.as_deref(),
            )
        }),
        Err(e) => {
            eprintln!(
                "warning: vault registry unreadable; audit trail disabled for this invocation ({e})"
            );
            None
        }
    };

    let exe = std::env::current_exe()
        .map_err(|e| Diagnostic::new(format!("cannot locate the norn executable: {e}")))?;
    let config = SummonConfig::for_vault(resolved.root, exe)
        .map_err(|e| client_error_diagnostic(&e))?
        .with_config_override(config_override)
        .with_events_dir(events_dir);

    let mut session = open(&config).map_err(|e| client_error_diagnostic(&e))?;
    session
        .wait_until_ready(MAX_WAIT)
        .map_err(|e| client_error_diagnostic(&e))?;
    Ok(session)
}

/// The instant, pre-summon vault-root check for the direct-path resolution vias
/// (NRN-414). Returns `Some(diagnostic)` when a `-C` or NORN_ROOT root is
/// missing or is not a directory — naming the actual path and how it was
/// supplied — so the CLI refuses immediately instead of summoning an owner that
/// would burn the connect budget and then blame the socket. Returns `None` for
/// every registry via (their roots are gated by the resolver's `ensure_live`)
/// and for any direct-path root that exists as a directory (the summon proceeds).
///
/// Fail-safe (NRN-414): ambiguity resolves toward the clear refusal — a root the
/// process cannot confirm is a directory is treated as the user error, which is
/// strictly better than failing open into the summon timeout.
fn vault_root_precheck(root: &Path, via: &ResolvedVia) -> Option<Diagnostic> {
    let source = match via {
        ResolvedVia::ExplicitPath => "-C",
        ResolvedVia::NornRootEnv => "NORN_ROOT",
        // Registry vias are gated by the resolver's `ensure_live`; the
        // unregistered-cwd root is the process cwd, which necessarily exists.
        _ => return None,
    };
    let shown = root.display();
    if !root.exists() {
        Some(Diagnostic::new(format!(
            "vault root does not exist: {shown} (from {source})"
        )))
    } else if !root.is_dir() {
        Some(Diagnostic::new(format!(
            "vault root is not a directory: {shown} (from {source})"
        )))
    } else {
        None
    }
}

/// Map a summoner [`ClientError`] onto a soft-landing [`Diagnostic`] (NRN-361).
///
/// The single CLI-side conversion for every read verb's `session.<verb>()`
/// failure AND every [`open_session`] summon failure — so the hint attached to
/// (say) an owner that went away is written once and rendered identically
/// everywhere. Hints are attached only where a genuinely useful next step
/// exists; a bare headline is a complete diagnostic otherwise.
pub fn client_error_diagnostic(e: &ClientError) -> Diagnostic {
    match e {
        // A config-resolution failure carries the richest diagnosis paths.
        ClientError::Resolve(cfg) => config_error_diagnostic(cfg),
        // The owner rejected a well-formed request (bad predicate, unresolvable
        // `--links-to`, a bad-config warm-up): the message is user-facing, and
        // any structured hints the owner attached ride straight through.
        ClientError::Rejected { message, hints } => {
            Diagnostic::new(message).with_hints(hints.iter().cloned())
        }
        // A self-healable transient that surfaced anyway (the owner exited at the
        // connection level, before or after the request was written): re-running
        // summons a fresh owner.
        ClientError::OwnerGone(_) | ClientError::OwnerGonePreSend(_) => {
            Diagnostic::new(e.to_string())
                .with_hint("the vault owner exited before replying — re-run the command")
        }
        // Reachable but hung: re-running is the honest next step (it re-attempts
        // the summon against a fresh owner). The `norn service` supervision verbs
        // that would add a status-check hint here are not yet ported, so no hint
        // may point at them.
        ClientError::OwnerHealth(_) => Diagnostic::new(e.to_string())
            .with_hint("the vault owner is not responding — re-run the command, or retry"),
        // The remaining variants (runtime-dir security, foreign owner, spawn,
        // protocol, raw IO) are self-explanatory headlines with no honest next
        // step to add — a hint here would be noise.
        _ => Diagnostic::new(e.to_string()),
    }
}

/// Map a central-config [`ConfigError`] onto a soft-landing [`Diagnostic`]
/// (NRN-361). The headline is the error's own message; a hint is added for the
/// variants with a concrete recovery path.
///
/// Public because the `vault` registry verbs fold their `ConfigError`s through
/// this same constructor (NRN-370), so a variant like `UnknownName` carries its
/// `norn vault list` hint identically whether it surfaced from a read summon or
/// from `vault set <unknown>`.
pub fn config_error_diagnostic(e: &ConfigError) -> Diagnostic {
    let base = Diagnostic::new(e.to_string());
    match e {
        ConfigError::UnknownName { .. } => {
            base.with_hint("run `norn vault list` to see registered vault names")
        }
        ConfigError::StaleEntry { .. } => base.with_hint(
            "the registered path no longer exists — re-register with `norn vault register`",
        ),
        ConfigError::BindingUnregistered { .. } => {
            base.with_hint("register the vault with `norn vault register`, or fix the binding file")
        }
        ConfigError::ConfigParse { .. } => base.with_hint(
            "fix the YAML syntax, then re-run — `norn config validate` reports the details",
        ),
        // `#[non_exhaustive]`: every other variant is a clear headline on its own.
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unknown_vault_name_gets_a_vault_list_hint() {
        let e = ConfigError::UnknownName {
            name: "atlas".into(),
        };
        let diag = config_error_diagnostic(&e);
        assert!(diag.message().contains("atlas"));
        assert_eq!(
            diag.hints(),
            ["run `norn vault list` to see registered vault names".to_string()]
        );
    }

    #[test]
    fn stale_entry_gets_a_reregister_hint() {
        let e = ConfigError::StaleEntry {
            name: "atlas".into(),
            path: PathBuf::from("/gone"),
        };
        let diag = config_error_diagnostic(&e);
        assert_eq!(diag.hints().len(), 1);
        assert!(diag.hints()[0].contains("re-register"));
    }

    #[test]
    fn rejected_rides_wire_hints_through_verbatim() {
        let e = ClientError::Rejected {
            message: "bad predicate `type`".into(),
            hints: vec!["did you mean `--eq type:note`?".into()],
        };
        let diag = client_error_diagnostic(&e);
        assert_eq!(diag.message(), "bad predicate `type`");
        assert_eq!(diag.hints(), ["did you mean `--eq type:note`?".to_string()]);
    }

    #[test]
    fn precheck_flags_a_missing_explicit_path_naming_the_via() {
        // NRN-414: a `-C` root that does not exist is refused instantly, naming
        // the path and the `-C` source — no owner summon, no socket dial.
        let missing = PathBuf::from("/nonexistent/nrn414-vault-root");
        let diag = vault_root_precheck(&missing, &ResolvedVia::ExplicitPath)
            .expect("a missing -C root must be flagged before summoning");
        assert!(
            diag.message().contains("does not exist"),
            "got {:?}",
            diag.message()
        );
        assert!(diag.message().contains("/nonexistent/nrn414-vault-root"));
        assert!(diag.message().contains("(from -C)"));
    }

    #[test]
    fn precheck_flags_a_non_directory_norn_root_naming_the_via() {
        // A NORN_ROOT that points at a FILE (not a directory) is the other
        // user-error case; the diagnostic names the NORN_ROOT source. The test
        // binary itself is a convenient always-present regular file.
        let a_file = std::env::current_exe().unwrap();
        let diag = vault_root_precheck(&a_file, &ResolvedVia::NornRootEnv)
            .expect("a file-as-root must be flagged");
        assert!(
            diag.message().contains("is not a directory"),
            "got {:?}",
            diag.message()
        );
        assert!(diag.message().contains("(from NORN_ROOT)"));
    }

    #[test]
    fn precheck_passes_an_existing_directory_and_skips_registry_vias() {
        // An existing directory (this crate's manifest dir) summons normally.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(vault_root_precheck(&dir, &ResolvedVia::ExplicitPath).is_none());
        // A registry via is gated by the resolver's `ensure_live`, so the
        // precheck never re-flags it — even for a missing path.
        let missing = PathBuf::from("/nonexistent/nrn414-registered");
        assert!(vault_root_precheck(&missing, &ResolvedVia::ExplicitName).is_none());
        assert!(vault_root_precheck(&missing, &ResolvedVia::ReverseLookup).is_none());
    }

    #[test]
    fn owner_gone_gets_a_rerun_hint_but_raw_io_stays_bare() {
        let gone = client_error_diagnostic(&ClientError::OwnerGone("eof".into()));
        assert_eq!(gone.hints().len(), 1, "owner-gone earns a re-run hint");

        let io = client_error_diagnostic(&ClientError::Io(std::io::Error::other("boom")));
        assert!(io.hints().is_empty(), "a raw IO error adds no filler hint");
    }

    #[test]
    fn precheck_messages_stay_prefix_aligned_with_index_error_display() {
        // Drift-seam guard: `vault_root_precheck` hardcodes its own message
        // strings rather than reusing `norn_core::graph::IndexError`'s Display
        // (the two diverge for a good reason — the precheck appends the `(from
        // <via>)` suffix `IndexError` has no business knowing about). If a
        // future edit to either side's wording drifts the shared headline
        // out of sync, this test catches it: the owner-side fail-safe classifies
        // the very same vanished-root fault via `IndexError`, so an operator
        // reading both a client-side precheck refusal and an owner-side warm-up
        // failure for the same root sees one consistent headline.
        let missing = PathBuf::from("/nonexistent/nrn414-vault-root");
        let precheck_missing = vault_root_precheck(&missing, &ResolvedVia::ExplicitPath)
            .expect("a missing -C root must be flagged before summoning");
        let index_missing = norn_core::graph::IndexError::MissingRoot(
            camino::Utf8PathBuf::from_path_buf(missing.clone()).expect("test path is valid UTF-8"),
        );
        assert!(
            precheck_missing
                .message()
                .starts_with(&index_missing.to_string()),
            "precheck message {:?} must stay prefixed with IndexError::MissingRoot's Display {:?}",
            precheck_missing.message(),
            index_missing.to_string()
        );

        let a_file = std::env::current_exe().unwrap();
        let precheck_not_dir = vault_root_precheck(&a_file, &ResolvedVia::NornRootEnv)
            .expect("a file-as-root must be flagged");
        let index_not_dir = norn_core::graph::IndexError::RootNotDirectory(
            camino::Utf8PathBuf::from_path_buf(a_file.clone()).expect("test path is valid UTF-8"),
        );
        assert!(
            precheck_not_dir
                .message()
                .starts_with(&index_not_dir.to_string()),
            "precheck message {:?} must stay prefixed with IndexError::RootNotDirectory's Display {:?}",
            precheck_not_dir.message(),
            index_not_dir.to_string()
        );
    }
}
