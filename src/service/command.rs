//! `norn service` verb dispatch (NRN-115).
//!
//! Wires the real edges — `launchctl` via [`RealExec`], the live control-ping,
//! the current-binary path, and the on-disk plist — to the pure layers
//! ([`plist`], [`launchd`], [`status`]). The verb cores are generic over the
//! [`Exec`] seam and return typed outcomes, so the lifecycle policy (act only
//! on an installed unit; structured no-ops; exit 0 iff acted-or-already-there)
//! is regression-tested with a fake without touching launchctl.
//!
//! macOS-only: [`require_macos`] gates every verb, printing Mimir's friendly
//! fallback on any other host. norn has exactly ONE unit (serve), so there is
//! no unit selector — a verb acts on the single serve daemon.

use crate::cli::ServiceCommand;
#[cfg(unix)]
use crate::cli::ServiceFormat;

#[cfg(unix)]
use super::launchd::{Exec, LaunchdSupervisor, RealExec};
#[cfg(unix)]
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
/// verbs exit 0 iff the unit ended in the requested state through this call
/// (acted, or start on an already-running unit); a no-op on a missing unit is
/// nonzero, so a deploy chain does not proceed on it.
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
        Sub::Uninstall(a) => match uninstall_outcome(&supervisor, &plist_path) {
            Ok(outcome) => {
                emit_uninstall(&outcome, a.format);
                Ok(0)
            }
            Err(error) => report_failure("uninstall", error, a.format),
        },
        Sub::Start(a) => lifecycle(&supervisor, &plist_path, Verb::Start, a.format),
        Sub::Stop(a) => lifecycle(&supervisor, &plist_path, Verb::Stop, a.format),
        Sub::Restart(a) => lifecycle(&supervisor, &plist_path, Verb::Restart, a.format),
        Sub::Status(a) => status_cmd(&supervisor, &plist_path, &log_path, &socket_path, a.format),
    }
}

#[cfg(not(unix))]
fn dispatch(_cmd: &ServiceCommand) -> anyhow::Result<i32> {
    // Unreachable: `require_macos` already errored on any non-macOS host (which
    // is every non-Unix host). Present so the crate compiles on Windows.
    unreachable!("service is macOS-only; require_macos gates non-macOS hosts")
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// The typed result of a lifecycle verb — computed by [`lifecycle_outcome`],
/// rendered by [`emit_lifecycle`]. Keeping it a value (not prints) is what lets
/// the no-op/exit-code policy be asserted in tests.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The verb ran its launchctl action.
    Acted(Verb),
    /// `start` on a unit that launchd already has loaded: the desired state
    /// holds, nothing to do. Success (exit 0) — an idempotent start must not
    /// fail a deploy chain whose daemon is already up. (Without the pre-check,
    /// launchctl reports this case with the same exit 5 as the bootstrap race
    /// and the retry loop would spin, then fail.)
    AlreadyRunning,
    /// `stop`/`restart` on an installed unit that is not loaded: a reported
    /// no-op (exit 1) — the unit was not acted on.
    NotLoaded(Verb),
    /// No plist on disk AND nothing loaded: a reported no-op (exit 1).
    NotInstalled(Verb),
}

#[cfg(unix)]
impl Outcome {
    fn exit_code(&self) -> i32 {
        match self {
            Outcome::Acted(_) | Outcome::AlreadyRunning => 0,
            Outcome::NotLoaded(_) | Outcome::NotInstalled(_) => 1,
        }
    }

    fn json(&self) -> serde_json::Value {
        match self {
            Outcome::Acted(verb) => serde_json::json!({
                "action": verb.name(),
                "ok": true,
            }),
            Outcome::AlreadyRunning => serde_json::json!({
                "action": "start",
                "ok": true,
                "reason": "already running",
            }),
            Outcome::NotLoaded(verb) => serde_json::json!({
                "action": verb.name(),
                "ok": false,
                "reason": "not running",
            }),
            Outcome::NotInstalled(verb) => serde_json::json!({
                "action": verb.name(),
                "ok": false,
                "reason": "not installed",
            }),
        }
    }

    /// The human line, and whether it belongs on stderr (warnings do; the
    /// output contract keeps stdout for results).
    fn text(&self) -> (String, bool) {
        match self {
            Outcome::Acted(verb) => (format!("serve {}", verb.past()), false),
            Outcome::AlreadyRunning => ("serve already running".to_string(), false),
            Outcome::NotLoaded(verb) => (
                format!("serve: not running — nothing to {}", verb.name()),
                true,
            ),
            Outcome::NotInstalled(verb) => (
                format!("serve: not installed — nothing to {}", verb.name()),
                true,
            ),
        }
    }
}

/// The lifecycle policy, over the seam (testable with a fake):
///
/// - **Installedness** = plist on disk OR unit still loaded — a loaded unit
///   whose plist vanished must remain stoppable/restartable, and `status`
///   already reports it as running.
/// - `start` on a loaded unit is [`Outcome::AlreadyRunning`] (never the
///   bootstrap-retry, whose race code launchctl shares with "already loaded").
/// - `stop`/`restart` on an installed-but-unloaded unit is the structured
///   [`Outcome::NotLoaded`] no-op, never a raw launchctl error.
#[cfg(unix)]
fn lifecycle_outcome<E: Exec>(
    supervisor: &LaunchdSupervisor<E>,
    plist_path: &camino::Utf8Path,
    verb: Verb,
) -> anyhow::Result<Outcome> {
    let loaded = supervisor.info()?.loaded;
    if !plist_path.exists() && !loaded {
        return Ok(Outcome::NotInstalled(verb));
    }
    match verb {
        Verb::Start => {
            if loaded {
                return Ok(Outcome::AlreadyRunning);
            }
            supervisor.start(plist_path.as_str())?;
        }
        Verb::Stop => {
            if !loaded {
                return Ok(Outcome::NotLoaded(verb));
            }
            supervisor.stop()?;
        }
        Verb::Restart => {
            if !loaded {
                return Ok(Outcome::NotLoaded(verb));
            }
            supervisor.restart()?;
        }
    }
    Ok(Outcome::Acted(verb))
}

#[cfg(unix)]
fn emit_lifecycle(outcome: &Outcome, format: ServiceFormat) {
    match format {
        ServiceFormat::Json => print_json(&outcome.json()),
        ServiceFormat::Text => {
            let (line, warn) = outcome.text();
            if warn {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
}

#[cfg(unix)]
fn lifecycle(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    verb: Verb,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    match lifecycle_outcome(supervisor, plist_path, verb) {
        Ok(outcome) => {
            emit_lifecycle(&outcome, format);
            Ok(outcome.exit_code())
        }
        Err(error) => report_failure(verb.name(), error, format),
    }
}

/// Render a genuine verb failure through the format contract: under `json`,
/// emit a machine-readable failure object (exit 1) — a JSON consumer must
/// never get a bare stderr string in place of the promised object; under
/// `text`, propagate so the top level renders it canonically (stderr, exit 1).
#[cfg(unix)]
fn report_failure(
    action: &'static str,
    error: anyhow::Error,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    match format {
        ServiceFormat::Json => {
            print_json(&serde_json::json!({
                "action": action,
                "ok": false,
                "error": format!("{error:#}"),
            }));
            Ok(1)
        }
        ServiceFormat::Text => Err(error),
    }
}

/// Resolve the binary the plist launches: the running executable, made absolute
/// WITHOUT resolving symlinks. A Homebrew (or `~/.local/bin`) install is a
/// symlink into a versioned real path (`…/Cellar/norn/<ver>/bin/norn`) that
/// dies on the next upgrade; the symlink is the stable launcher path launchd
/// should exec. (Restarting the service after `self-update` is NRN-226 and
/// complements this.)
#[cfg(unix)]
fn resolve_bin() -> anyhow::Result<camino::Utf8PathBuf> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("resolve current_exe: {e}"))?;
    stable_bin_path(exe)
}

/// Absolute-ize `exe` while deliberately preserving symlinks (see
/// [`resolve_bin`]). Split out so the no-canonicalize contract is testable.
#[cfg(unix)]
fn stable_bin_path(exe: std::path::PathBuf) -> anyhow::Result<camino::Utf8PathBuf> {
    let absolute = std::path::absolute(&exe)
        .map_err(|e| anyhow::anyhow!("absolutize {}: {e}", exe.display()))?;
    camino::Utf8PathBuf::from_path_buf(absolute)
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
    match install_inner(supervisor, plist_path, log_path, socket_path, format) {
        Ok(code) => Ok(code),
        Err(error) => report_failure("install", error, format),
    }
}

#[cfg(unix)]
fn install_inner(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    log_path: &camino::Utf8Path,
    socket_path: &camino::Utf8Path,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    let bin = resolve_bin()?;
    let xdg = plist::install_env_xdg_cache_home();
    let rendered = plist::render_serve_plist(bin.as_str(), log_path.as_str(), xdg.as_deref());
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

/// What `uninstall` found and did — rendered by [`emit_uninstall`].
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct UninstallOutcome {
    /// There was something to tear down (a plist on disk or a loaded unit).
    was_present: bool,
    removed_plist: bool,
}

/// Tear down the unit. The bootout runs ONLY when launchd reports the unit
/// loaded, and it is NOT tolerated: a genuine unload failure propagates BEFORE
/// the plist is removed, so a still-loaded KeepAlive daemon is never orphaned
/// from its on-disk unit (the failure mode of blanket-tolerating bootout).
/// A not-loaded unit skips the bootout entirely — that is the one case the old
/// tolerance existed for.
#[cfg(unix)]
fn uninstall_outcome<E: Exec>(
    supervisor: &LaunchdSupervisor<E>,
    plist_path: &camino::Utf8Path,
) -> anyhow::Result<UninstallOutcome> {
    let on_disk = plist_path.exists();
    let loaded = supervisor.info()?.loaded;
    if loaded {
        supervisor.stop()?;
    }
    if on_disk {
        std::fs::remove_file(plist_path)?;
    }
    Ok(UninstallOutcome {
        was_present: on_disk || loaded,
        removed_plist: on_disk,
    })
}

#[cfg(unix)]
fn emit_uninstall(outcome: &UninstallOutcome, format: ServiceFormat) {
    match format {
        ServiceFormat::Json => print_json(&serde_json::json!({
            "action": "uninstall",
            "ok": true,
            "was_present": outcome.was_present,
            "removed_plist": outcome.removed_plist,
        })),
        ServiceFormat::Text => {
            if outcome.was_present {
                println!("serve uninstalled (config and logs kept)");
            } else {
                println!("serve: not installed (nothing to remove)");
            }
        }
    }
}

#[cfg(unix)]
fn status_cmd(
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
        status::ProbedState {
            loaded: info.loaded,
            running: info.running,
            pid: info.pid.or(pong_pid),
            running_version,
            uptime_secs,
        },
        env!("CARGO_PKG_VERSION"),
        status::ServicePaths {
            plist: plist_path.to_string(),
            log: log_path.to_string(),
            socket: socket_path.to_string(),
        },
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

    #[cfg(unix)]
    mod unix {
        use super::super::*;
        use crate::service::launchd::testing::{
            exec_fail, exec_ok, print_not_loaded, print_running, supervisor, FakeExec,
        };
        use camino::Utf8PathBuf;

        /// A tempdir with (or without) a plist file, standing in for the
        /// LaunchAgents install state.
        fn plist_fixture(installed: bool) -> (tempfile::TempDir, Utf8PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let path =
                Utf8PathBuf::from_path_buf(dir.path().join("com.dbtlr.norn.serve.plist")).unwrap();
            if installed {
                std::fs::write(&path, "<plist/>").unwrap();
            }
            (dir, path)
        }

        /// stop on an installed-but-unloaded unit (a second `stop`) is the
        /// structured no-op — ok:false / "not running" / exit 1 — in BOTH
        /// formats, never a raw launchctl error (which under --format json
        /// would emit no JSON at all).
        #[test]
        fn stop_twice_is_a_structured_not_running_no_op() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Stop).unwrap();
            assert_eq!(outcome, Outcome::NotLoaded(Verb::Stop));
            assert_eq!(outcome.exit_code(), 1);
            let json = outcome.json();
            assert_eq!(json["ok"], false);
            assert_eq!(json["reason"], "not running");
            let (line, warn) = outcome.text();
            assert!(line.contains("nothing to stop"), "{line}");
            assert!(warn, "the no-op line is a warning (stderr)");
            // Only the info() probe ran — no bootout was attempted.
            assert_eq!(s.exec().calls().len(), 1);
        }

        /// restart on an installed-but-stopped unit: same structured no-op.
        #[test]
        fn restart_on_stopped_is_a_structured_not_running_no_op() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Restart).unwrap();
            assert_eq!(outcome, Outcome::NotLoaded(Verb::Restart));
            assert_eq!(outcome.exit_code(), 1);
            assert_eq!(outcome.json()["reason"], "not running");
            assert!(outcome.text().0.contains("nothing to restart"));
            assert_eq!(s.exec().calls().len(), 1, "no kickstart was attempted");
        }

        /// start on an already-loaded unit reports "already running" and exits
        /// 0 (idempotent start) — and never enters the bootstrap retry, whose
        /// race code launchctl shares with "already loaded".
        #[test]
        fn start_on_running_unit_is_already_running_exit_zero() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_running(4242)]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Start).unwrap();
            assert_eq!(outcome, Outcome::AlreadyRunning);
            assert_eq!(outcome.exit_code(), 0);
            let json = outcome.json();
            assert_eq!(json["ok"], true);
            assert_eq!(json["reason"], "already running");
            let calls = s.exec().calls();
            assert_eq!(calls.len(), 1, "info only");
            assert!(
                calls.iter().all(|c| c[1] != "bootstrap"),
                "no bootstrap on an already-loaded unit"
            );
        }

        /// start on an installed, not-loaded unit bootstraps and reports Acted.
        #[test]
        fn start_on_stopped_unit_bootstraps() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_not_loaded(), exec_ok()]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Start).unwrap();
            assert_eq!(outcome, Outcome::Acted(Verb::Start));
            assert_eq!(outcome.exit_code(), 0);
            let calls = s.exec().calls();
            assert_eq!(calls[1][1], "bootstrap");
        }

        /// Installedness is plist-exists OR loaded: a loaded unit whose plist
        /// vanished must still be stoppable (previously the plist-only gate
        /// reported "not installed" while status said running).
        #[test]
        fn stop_acts_on_a_loaded_unit_whose_plist_vanished() {
            let (_dir, plist_path) = plist_fixture(false); // no plist on disk
            let s = supervisor(FakeExec::new(vec![print_running(7), exec_ok()]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Stop).unwrap();
            assert_eq!(outcome, Outcome::Acted(Verb::Stop));
            let calls = s.exec().calls();
            assert_eq!(calls[1][1], "bootout", "the stop actually ran");
        }

        /// Nothing on disk and nothing loaded: the structured not-installed
        /// no-op, exit 1.
        #[test]
        fn lifecycle_with_nothing_installed_is_not_installed_exit_one() {
            let (_dir, plist_path) = plist_fixture(false);
            for verb in [Verb::Start, Verb::Stop, Verb::Restart] {
                let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
                let outcome = lifecycle_outcome(&s, &plist_path, verb).unwrap();
                assert_eq!(outcome, Outcome::NotInstalled(verb));
                assert_eq!(outcome.exit_code(), 1);
                assert_eq!(outcome.json()["reason"], "not installed");
            }
        }

        /// A genuine bootout failure during uninstall must PROPAGATE and leave
        /// the plist on disk — deleting it would orphan a still-loaded
        /// KeepAlive daemon with no unit file left to manage it by.
        #[test]
        fn uninstall_keeps_the_plist_when_bootout_fails() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![
                print_running(7),                          // info: loaded
                exec_fail(1, "Operation now in progress"), // bootout fails
            ]));
            let err = uninstall_outcome(&s, &plist_path).unwrap_err();
            assert!(
                err.to_string().contains("could not unload"),
                "the bootout failure surfaces: {err}"
            );
            assert!(
                plist_path.exists(),
                "the plist must survive a failed unload"
            );
        }

        /// uninstall of a not-loaded unit skips the bootout entirely (the one
        /// case the old blanket tolerance existed for) and removes the plist.
        #[test]
        fn uninstall_not_loaded_skips_bootout_and_removes_plist() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
            let outcome = uninstall_outcome(&s, &plist_path).unwrap();
            assert_eq!(
                outcome,
                UninstallOutcome {
                    was_present: true,
                    removed_plist: true
                }
            );
            assert!(!plist_path.exists());
            let calls = s.exec().calls();
            assert_eq!(
                calls.len(),
                1,
                "info only — no bootout for an unloaded unit"
            );
        }

        /// uninstall with nothing present is a tolerated no-op (exit 0 at the
        /// dispatch layer), reported as not-present.
        #[test]
        fn uninstall_nothing_present_reports_not_present() {
            let (_dir, plist_path) = plist_fixture(false);
            let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
            let outcome = uninstall_outcome(&s, &plist_path).unwrap();
            assert_eq!(
                outcome,
                UninstallOutcome {
                    was_present: false,
                    removed_plist: false
                }
            );
        }

        /// uninstall of a loaded unit boots it out (non-tolerant) then removes
        /// the plist.
        #[test]
        fn uninstall_loaded_unit_bootouts_then_removes_plist() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_running(7), exec_ok()]));
            let outcome = uninstall_outcome(&s, &plist_path).unwrap();
            assert_eq!(
                outcome,
                UninstallOutcome {
                    was_present: true,
                    removed_plist: true
                }
            );
            assert!(!plist_path.exists());
            assert_eq!(s.exec().calls()[1][1], "bootout");
        }

        /// The plist's binary path preserves symlinks: absolute-ized, NOT
        /// canonicalized (a Homebrew symlink must stay the stable launcher
        /// path, not the Cellar-versioned target that dies on upgrade).
        #[test]
        fn stable_bin_path_preserves_symlinks() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("norn-0.45.1-real");
            std::fs::write(&target, b"#!ELF").unwrap();
            let link = dir.path().join("norn");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let resolved = stable_bin_path(link.clone()).unwrap();
            assert_eq!(
                resolved.as_std_path(),
                link.as_path(),
                "the symlink itself is the launcher path"
            );
            assert!(
                !resolved.as_str().ends_with("norn-0.45.1-real"),
                "must not resolve to the versioned target"
            );
        }
    }
}
