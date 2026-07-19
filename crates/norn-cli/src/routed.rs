//! The routed-read entry the read verbs share: resolve the target vault,
//! summon-or-connect its owner, and wait for it to be ready.
//!
//! This is the CLI's whole job on the request side of a read (ADR 0016/0017):
//! turn the global flags into a resolved vault, hand it to `norn-client` (the
//! only crate that spawns owners or dials sockets), and return a live
//! [`OwnerSession`] the verb sends its `Params` to.

use std::time::Duration;

use norn_client::{open, ConfigHome, OwnerSession, Registry, ResolveInput, SummonConfig};

use crate::cli::GlobalArgs;

/// How long a read waits for a freshly-summoned owner to finish warming up
/// before giving up. Warm-up is size-linear (a one-shot full cache build), so
/// this is a generous ceiling, not an expected latency: agent bursts and small
/// vaults are ready in well under a second, while a very large vault's first
/// summon still completes inside the budget. The owner answers pings promptly
/// throughout warm-up, so a truly hung owner is caught by the per-request stall
/// budget long before this ceiling — this only bounds a legitimately slow build.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

/// Resolve the target vault from the global flags, summon-or-connect its owner,
/// and return a ready session. On failure returns an operator-facing message the
/// caller prints as a `norn:` diagnostic.
pub fn open_session(global: &GlobalArgs) -> Result<OwnerSession, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?;
    let home = ConfigHome::from_env().map_err(|e| e.to_string())?;

    let input = ResolveInput {
        explicit_path: global.cwd.clone(),
        explicit_name: global.vault.clone(),
        cwd,
        norn_root_env: std::env::var("NORN_ROOT").ok(),
    };

    let registry = Registry::new(home);
    let resolved = registry.resolve(&input).map_err(|e| e.to_string())?;

    // A registered vault may carry a `[vaults.<name>].config` override; the
    // summoned owner warms under it (ADR 0017 resolver-derived config). An
    // unregistered cwd (the common ephemeral case) has no override — the owner
    // loads `<root>/.norn/config.yaml`.
    let config_override = match &resolved.name {
        Some(name) => registry.lookup(name).ok().flatten().and_then(|v| v.config),
        None => None,
    };

    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate the norn executable: {e}"))?;
    let config = SummonConfig::for_vault(resolved.root, exe)
        .map_err(|e| e.to_string())?
        .with_config_override(config_override);

    let mut session = open(&config).map_err(|e| e.to_string())?;
    session
        .wait_until_ready(MAX_WAIT)
        .map_err(|e| e.to_string())?;
    Ok(session)
}
