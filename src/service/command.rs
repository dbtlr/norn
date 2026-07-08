//! `norn service` verb dispatch (NRN-115).
//!
//! Wires the real edges — `launchctl` via [`RealExec`], the live control-ping,
//! the current-binary path, and the on-disk plist — to the pure layers
//! ([`plist`], [`launchd`], [`status`]). macOS-only: [`require_macos`] gates
//! every verb, printing Mimir's friendly fallback on any other host. norn has
//! exactly ONE unit (serve), so there is no unit selector — a verb acts on the
//! single serve daemon.

use crate::cli::{ServiceCommand, ServiceFormat};

#[cfg(unix)]
use super::launchd::{LaunchdSupervisor, RealExec};
use super::{plist, status};

/// Refuse on any non-macOS host with Mimir's fallback shape. Non-macOS (which
/// includes every non-Unix host) has no launchd, so the verbs cannot act; the
/// message points at the portable path (`norn serve` under the user's own
/// supervisor). Runs on all platforms, so the crate builds — and reports
/// honestly — everywhere.
fn require_macos() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        anyhow::bail!(
            "norn service requires macOS (launchd)\n  \
             run `norn serve` under your supervisor of choice; systemd support is planned"
        );
    }
    Ok(())
}

/// Dispatch a `norn service` verb. Returns the process exit code: lifecycle
/// verbs exit 0 iff they acted on the installed unit (nonzero no-op, so a deploy
/// chain does not proceed); install/uninstall/status are 0 on success.
pub fn run(cmd: &ServiceCommand) -> anyhow::Result<i32> {
    require_macos()?;
    dispatch(cmd)
}

#[cfg(unix)]
fn dispatch(cmd: &ServiceCommand) -> anyhow::Result<i32> {
    use crate::cli::ServiceSubcommand as Sub;

    // SAFETY: `getuid(2)` is always successful and takes no arguments.
    let uid = unsafe { libc::getuid() };
    let supervisor = LaunchdSupervisor::new(RealExec, uid, plist::SERVE_LABEL);
    let plist_path = plist::plist_path()?;
    let log_path = plist::log_path()?;
    let socket_path = crate::service::host_socket_path()?;

    match &cmd.command {
        Sub::Install(a) => install(&supervisor, &plist_path, &log_path, &socket_path, a.format),
        Sub::Uninstall(a) => uninstall(&supervisor, &plist_path, a.format),
        Sub::Start(a) => lifecycle(&supervisor, &plist_path, Verb::Start, a.format),
        Sub::Stop(a) => lifecycle(&supervisor, &plist_path, Verb::Stop, a.format),
        Sub::Restart(a) => lifecycle(&supervisor, &plist_path, Verb::Restart, a.format),
        Sub::Status(a) => status(&supervisor, &plist_path, &log_path, &socket_path, a.format),
    }
}

#[cfg(not(unix))]
fn dispatch(_cmd: &ServiceCommand) -> anyhow::Result<i32> {
    // Unreachable: `require_macos` already errored on any non-macOS host (which
    // is every non-Unix host). Present so the crate compiles on Windows.
    unreachable!("service is macOS-only; require_macos gates non-macOS hosts")
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum Verb {
    Start,
    Stop,
    Restart,
}

#[cfg(unix)]
impl Verb {
    fn name(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }
    fn past(self) -> &'static str {
        match self {
            Verb::Start => "started",
            Verb::Stop => "stopped",
            Verb::Restart => "restarted",
        }
    }
}

/// Resolve the binary the plist launches: the running executable, canonicalized
/// to an absolute, symlink-resolved path (launchd gives no `PATH` and does no
/// expansion). Falls back to the raw `current_exe` if canonicalization fails.
#[cfg(unix)]
fn resolve_bin() -> anyhow::Result<camino::Utf8PathBuf> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("resolve current_exe: {e}"))?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    camino::Utf8PathBuf::from_path_buf(resolved)
        .map_err(|p| anyhow::anyhow!("binary path is not UTF-8: {}", p.display()))
}

#[cfg(unix)]
fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("json serializes")
    );
}

#[cfg(unix)]
fn install(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    log_path: &camino::Utf8Path,
    socket_path: &camino::Utf8Path,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    let bin = resolve_bin()?;
    let rendered = plist::render_serve_plist(bin.as_str(), log_path.as_str());
    // launchd creates NEITHER the LaunchAgents dir nor the log's parent, and a
    // missing StandardOutPath parent makes the unit fail to launch — so ensure
    // both exist before bootstrapping.
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(plist_path, rendered)?;
    supervisor.install(plist_path.as_str())?;

    match format {
        ServiceFormat::Json => print_json(&serde_json::json!({
            "action": "install",
            "ok": true,
            "binary": bin.as_str(),
            "plist": plist_path.as_str(),
            "log": log_path.as_str(),
            "socket": socket_path.as_str(),
        })),
        ServiceFormat::Text => {
            println!("serve installed");
            println!("  binary {bin}");
            println!("  socket {socket_path}");
            println!("  plist  {plist_path}");
            println!("  log    {log_path}");
        }
    }
    Ok(0)
}

#[cfg(unix)]
fn uninstall(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    let on_disk = plist_path.exists();
    // "Present" = on disk OR still loaded (a plist can vanish while the unit
    // runs). The bootout is tolerant either way; report a teardown exactly when
    // there was something to tear down.
    let present = on_disk || supervisor.info()?.loaded;
    supervisor.uninstall()?;
    if on_disk {
        std::fs::remove_file(plist_path)?;
    }

    match format {
        ServiceFormat::Json => print_json(&serde_json::json!({
            "action": "uninstall",
            "ok": true,
            "was_present": present,
            "removed_plist": on_disk,
        })),
        ServiceFormat::Text => {
            if present {
                println!("serve uninstalled (config and logs kept)");
            } else {
                println!("serve: not installed (nothing to remove)");
            }
        }
    }
    Ok(0)
}

#[cfg(unix)]
fn lifecycle(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    verb: Verb,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    // A lifecycle verb acts only on an INSTALLED unit. Naming a not-installed
    // unit is a reported no-op (nonzero), never a launchctl throw — so a
    // `norn service restart && …` chain does not proceed on a no-op.
    if !plist_path.exists() {
        match format {
            ServiceFormat::Json => print_json(&serde_json::json!({
                "action": verb.name(),
                "ok": false,
                "reason": "not installed",
            })),
            ServiceFormat::Text => {
                eprintln!("serve: not installed — nothing to {}", verb.name());
            }
        }
        return Ok(1);
    }

    match verb {
        Verb::Start => supervisor.start(plist_path.as_str())?,
        Verb::Stop => supervisor.stop()?,
        Verb::Restart => supervisor.restart()?,
    }

    match format {
        ServiceFormat::Json => print_json(&serde_json::json!({
            "action": verb.name(),
            "ok": true,
        })),
        ServiceFormat::Text => println!("serve {}", verb.past()),
    }
    Ok(0)
}

#[cfg(unix)]
fn status(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    log_path: &camino::Utf8Path,
    socket_path: &camino::Utf8Path,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    let info = supervisor.info()?;
    // Probe the live daemon regardless of launchd load state: a `norn serve`
    // running outside launchd still answers and should surface (running version
    // / uptime), rather than reading as dead.
    let pong = crate::service::probe_status(crate::service::handshake_timeout());
    let (running_version, uptime_secs, pong_pid) = match pong {
        Some(p) => (Some(p.version), p.uptime_secs, p.pid),
        None => (None, None, None),
    };

    let report = status::assemble_status(
        info.loaded,
        info.running,
        info.pid.or(pong_pid),
        running_version,
        uptime_secs,
        env!("CARGO_PKG_VERSION"),
        plist_path.to_string(),
        log_path.to_string(),
        socket_path.to_string(),
    );

    let mut out = std::io::stdout().lock();
    match format {
        ServiceFormat::Json => status::render_json(&report, &mut out)?,
        ServiceFormat::Text => status::render_text(&report, &mut out)?,
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_macos_reports_the_portable_fallback() {
        // On the Linux CI host this is the live path; on macOS it is vacuously
        // true. Assert the message shape only where the guard actually fires.
        if std::env::consts::OS != "macos" {
            let err = require_macos().unwrap_err().to_string();
            assert!(err.contains("requires macOS"), "got {err}");
            assert!(err.contains("systemd support is planned"), "got {err}");
        }
    }
}
