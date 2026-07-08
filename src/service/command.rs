//! `norn service` verb dispatch (NRN-115).
//!
//! Wires the real edges — `launchctl` via [`RealExec`], the live control-ping,
//! the current-binary path, and the on-disk plist — to the pure layers
//! ([`plist`], [`launchd`], [`status`]). The verb cores are generic over the
//! [`Exec`] seam and return typed outcomes, so the lifecycle policy (act only
//! on an installed unit; structured no-ops; probe failures surface, never
//! default) is regression-tested with a fake without touching launchctl.
//!
//! **Format contract:** [`dispatch`] is the ONE error boundary. Every failure
//! anywhere in a verb — including the path-derivation preamble — flows through
//! [`report_failure`], so `--format json` always emits a machine-readable
//! `{ok:false}` object (never empty stdout), and text renders canonically.
//!
//! macOS-only: [`require_macos`] gates every verb, printing Mimir's friendly
//! fallback on any other host. norn has exactly ONE unit (serve), so there is
//! no unit selector — a verb acts on the single serve daemon.

use crate::cli::{ServiceCommand, ServiceFormat, ServiceSubcommand};

#[cfg(unix)]
use super::launchd::{Exec, LaunchdSupervisor, LoadState, RealExec};
#[cfg(unix)]
use super::{plist, status};

/// The non-Verb action names, single-sourced here and shared by [`verb_meta`]
/// and each verb's success emitter (start/stop/restart come from
/// [`Verb::name`]) — so a rename cannot ship disagreeing `action` fields.
const ACTION_INSTALL: &str = "install";
const ACTION_UNINSTALL: &str = "uninstall";
const ACTION_STATUS: &str = "status";

/// Refuse on any non-macOS host with Mimir's fallback shape. Non-macOS (which
/// includes every non-Unix host) has no launchd, so the verbs cannot act; the
/// message points at the portable path (`norn serve` under the user's own
/// supervisor). Runs on all platforms, so the crate builds — and reports
/// honestly — everywhere. Called INSIDE [`dispatch_verb`], so the refusal
/// flows through the format boundary like every other failure (`--format
/// json` gets the structured `{ok:false}` object, not bare stderr text).
fn require_macos() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        anyhow::bail!(
            "norn service requires macOS (launchd)\n  \
             run `norn serve` under your supervisor of choice; systemd support is planned"
        );
    }
    Ok(())
}

/// Dispatch a `norn service` verb. Returns the process exit code. Lifecycle
/// contract: exit 0 iff the unit ended in the requested state through this
/// call (acted), or start found it already running; a no-op on a missing unit
/// is nonzero, so a deploy chain does not proceed on it.
pub fn run(cmd: &ServiceCommand) -> anyhow::Result<i32> {
    dispatch(cmd)
}

/// Marker for a failure writing an already-rendered report to stdout. The
/// dispatch boundary must NOT route this into another stdout emission — the
/// stream is broken, and a second `println!` would panic — so it propagates
/// raw: the top level maps a broken pipe to exit 0 and anything else to
/// stderr + exit 1, like every other output-write error in the CLI. The
/// source chain keeps the underlying [`std::io::Error`] so the top level's
/// broken-pipe detection still sees it.
#[derive(Debug)]
struct StdoutWriteError(std::io::Error);

impl std::fmt::Display for StdoutWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to write the report to stdout")
    }
}

impl std::error::Error for StdoutWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The single format-aware error boundary (see the module docs): resolve the
/// verb's name + format FIRST, then route EVERY failure — the non-macOS
/// refusal and the path-derivation preamble included — through
/// [`report_failure`]. The one exception is [`StdoutWriteError`]: stdout is
/// already broken, so emitting a second object there would panic — it
/// propagates raw instead.
fn dispatch(cmd: &ServiceCommand) -> anyhow::Result<i32> {
    let (action, format) = verb_meta(&cmd.command);
    match dispatch_verb(&cmd.command) {
        Ok(code) => Ok(code),
        Err(error) if error.is::<StdoutWriteError>() => Err(error),
        Err(error) => report_failure(action, error, format),
    }
}

/// The verb's wire name and requested format — resolved before any fallible
/// work so a preamble failure can still honor the format contract. Lifecycle
/// names come from [`Verb::name`] (the same source `Outcome::json` renders),
/// the rest from the shared `ACTION_*` constants.
fn verb_meta(sub: &ServiceSubcommand) -> (&'static str, ServiceFormat) {
    match sub {
        ServiceSubcommand::Install(a) => (ACTION_INSTALL, a.format),
        ServiceSubcommand::Uninstall(a) => (ACTION_UNINSTALL, a.format),
        ServiceSubcommand::Start(a) => (Verb::Start.name(), a.format),
        ServiceSubcommand::Stop(a) => (Verb::Stop.name(), a.format),
        ServiceSubcommand::Restart(a) => (Verb::Restart.name(), a.format),
        ServiceSubcommand::Status(a) => (ACTION_STATUS, a.format),
    }
}

#[cfg(not(unix))]
fn dispatch_verb(_sub: &ServiceSubcommand) -> anyhow::Result<i32> {
    require_macos()?;
    // Unreachable: `require_macos` refuses every non-macOS host, and every
    // non-Unix host is non-macOS. Present so the crate compiles on Windows.
    unreachable!("service is macOS-only; require_macos gates non-macOS hosts")
}

#[cfg(unix)]
fn dispatch_verb(sub: &ServiceSubcommand) -> anyhow::Result<i32> {
    require_macos()?;
    // SAFETY: `getuid(2)` is always successful and takes no arguments.
    let uid = unsafe { libc::getuid() };
    let supervisor = LaunchdSupervisor::new(RealExec, uid, plist::SERVE_LABEL);
    let plist_path = plist::plist_path()?;
    let log_path = plist::log_path()?;
    let socket_path = crate::service::host_socket_path()?;

    match sub {
        ServiceSubcommand::Install(a) => {
            install(&supervisor, &plist_path, &log_path, &socket_path, a.format)
        }
        ServiceSubcommand::Uninstall(a) => {
            let outcome = uninstall_outcome(&supervisor, &plist_path)?;
            emit_uninstall(&outcome, a.format);
            Ok(0)
        }
        ServiceSubcommand::Start(a) => lifecycle(&supervisor, &plist_path, Verb::Start, a.format),
        ServiceSubcommand::Stop(a) => lifecycle(&supervisor, &plist_path, Verb::Stop, a.format),
        ServiceSubcommand::Restart(a) => {
            lifecycle(&supervisor, &plist_path, Verb::Restart, a.format)
        }
        ServiceSubcommand::Status(a) => {
            status_cmd(&supervisor, &plist_path, &log_path, &socket_path, a.format)
        }
    }
}

/// The three lifecycle verbs. Cross-platform (its [`Verb::name`] is the ONE
/// source of the lifecycle action strings, consumed by the cross-platform
/// [`verb_meta`] as well as the unix `Outcome` renderers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Start,
    Stop,
    Restart,
}

impl Verb {
    fn name(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }
    #[cfg(unix)]
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
    /// `start` on a unit that is already RUNNING: the desired state holds,
    /// nothing to do. Success (exit 0) — an idempotent start must not fail a
    /// deploy chain whose daemon is already up. Keyed on running, not loaded:
    /// a loaded-but-not-running unit (crash-throttled) is NOT "already
    /// running" — start kickstarts it instead (that IS the requested state
    /// transition).
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
/// - **The probe is trusted only when it answers.** [`super::launchd::LoadState`] carries
///   loaded/not-loaded definitively; a probe FAILURE propagates as `Err` — a
///   "not running" no-op or "not installed" verdict must mean the daemon
///   really was in that state, never that a `launchctl print` hiccuped.
/// - **Installedness** = plist on disk OR unit still loaded — a loaded unit
///   whose plist vanished must remain stoppable/restartable, and `status`
///   already reports it as running.
/// - `start` is idempotent on RUNNING ([`Outcome::AlreadyRunning`]); a
///   loaded-but-NOT-running unit is kickstarted (plain `kickstart`, nothing to
///   kill) so exit 0 means "the unit ended running through this call or
///   already was". A not-loaded unit bootstraps (never the retry's race code —
///   see `launchd`).
/// - `stop`/`restart` on an installed-but-unloaded unit is the structured
///   [`Outcome::NotLoaded`] no-op, never a raw launchctl error.
#[cfg(unix)]
fn lifecycle_outcome<E: Exec>(
    supervisor: &LaunchdSupervisor<E>,
    plist_path: &camino::Utf8Path,
    verb: Verb,
) -> anyhow::Result<Outcome> {
    let state = supervisor.load_state()?;
    if !plist_path.exists() && !state.loaded() {
        return Ok(Outcome::NotInstalled(verb));
    }
    match verb {
        Verb::Start => {
            if state.running() {
                return Ok(Outcome::AlreadyRunning);
            }
            if state.loaded() {
                // Loaded but not running (e.g. crash-throttled between
                // KeepAlive respawns): spawn it now.
                supervisor.kickstart()?;
            } else {
                supervisor.start(plist_path.as_str())?;
            }
        }
        Verb::Stop => {
            if !state.loaded() {
                return Ok(Outcome::NotLoaded(verb));
            }
            supervisor.stop()?;
        }
        Verb::Restart => {
            if !state.loaded() {
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
    let outcome = lifecycle_outcome(supervisor, plist_path, verb)?;
    emit_lifecycle(&outcome, format);
    Ok(outcome.exit_code())
}

/// The machine-readable failure object [`report_failure`] emits under `json`.
/// Pure so the shape is testable.
fn failure_json(action: &str, error: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({
        "action": action,
        "ok": false,
        "error": format!("{error:#}"),
    })
}

/// Render a genuine verb failure through the format contract: under `json`,
/// emit [`failure_json`] (exit 1) — a JSON consumer must never get empty
/// stdout or a bare stderr string in place of the promised object; under
/// `text`, propagate so the top level renders it canonically (stderr, exit 1).
fn report_failure(
    action: &'static str,
    error: anyhow::Error,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    match format {
        ServiceFormat::Json => {
            print_json(&failure_json(action, &error));
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
            "action": ACTION_INSTALL,
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

/// Tear down the unit. The load-state probe must ANSWER before anything is
/// touched — a probe failure propagates with the plist intact, so the
/// orphaned-daemon state (plist deleted under a possibly-still-loaded
/// KeepAlive unit) is impossible by construction, not by probe luck. The
/// bootout runs ONLY when the unit is definitively loaded, and it is NOT
/// tolerated: a genuine unload failure also propagates BEFORE the plist is
/// removed. A definitively-not-loaded unit skips the bootout entirely.
#[cfg(unix)]
fn uninstall_outcome<E: Exec>(
    supervisor: &LaunchdSupervisor<E>,
    plist_path: &camino::Utf8Path,
) -> anyhow::Result<UninstallOutcome> {
    let on_disk = plist_path.exists();
    let loaded = supervisor.load_state()?.loaded();
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
            "action": ACTION_UNINSTALL,
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

/// Render the complete status report to ONE string. Pure (a `Vec` write
/// cannot fail), so the emission below is a single stdout write — a mid-render
/// stream failure can never leave a half-written report followed by a second
/// JSON object from the failure boundary.
#[cfg(unix)]
fn render_status(report: &status::ServiceStatus, format: ServiceFormat) -> String {
    let mut buf = Vec::new();
    match format {
        ServiceFormat::Json => status::render_json(report, &mut buf),
        ServiceFormat::Text => status::render_text(report, &mut buf),
    }
    .expect("rendering to a Vec cannot fail");
    String::from_utf8(buf).expect("the renderers emit UTF-8")
}

#[cfg(unix)]
fn status_cmd(
    supervisor: &LaunchdSupervisor<RealExec>,
    plist_path: &camino::Utf8Path,
    log_path: &camino::Utf8Path,
    socket_path: &camino::Utf8Path,
    format: ServiceFormat,
) -> anyhow::Result<i32> {
    // Status mutates nothing, so it reports what it knows: a launchd probe
    // failure becomes "unavailable" in the report (carrying the error text)
    // instead of aborting — the severity split from the acting verbs, whose
    // gates propagate the same failure because they act on the answer.
    let launchd = match supervisor.load_state() {
        Ok(LoadState::Loaded { running, pid }) => status::LaunchdState::Loaded { running, pid },
        Ok(LoadState::NotLoaded) => status::LaunchdState::NotLoaded,
        Err(error) => status::LaunchdState::Unavailable {
            error: format!("{error:#}"),
        },
    };
    // Probe the live daemon AT THE SOCKET THE REPORT PRINTS (one derivation,
    // no re-derive drift), regardless of the launchd verdict: a `norn serve`
    // running outside launchd — or behind a failed launchctl probe — still
    // answers and should surface (running version / uptime), rather than
    // reading as dead.
    let pong =
        crate::service::probe_status_socket(socket_path, crate::service::handshake_timeout());
    let (running_version, uptime_secs, pong_pid) = match pong {
        Some(p) => (Some(p.version), p.uptime_secs, p.pid),
        None => (None, None, None),
    };

    let report = status::assemble_status(
        status::ProbedState {
            launchd,
            running_version,
            uptime_secs,
            pong_pid,
        },
        env!("CARGO_PKG_VERSION"),
        status::ServicePaths {
            plist: plist_path.to_string(),
            log: log_path.to_string(),
            socket: socket_path.to_string(),
        },
    );

    // ONE write of the complete report. A failure here is a broken output
    // stream — wrapped so the dispatch boundary propagates it instead of
    // emitting a second object into the same broken stdout.
    let rendered = render_status(&report, format);
    let emit = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        out.write_all(rendered.as_bytes())?;
        out.flush()
    };
    emit().map_err(|e| anyhow::Error::new(StdoutWriteError(e)))?;
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

    /// The non-macOS refusal honors the format contract: it flows through the
    /// dispatch boundary, so `--format json` gets the structured `{ok:false}`
    /// object + exit 1 (never bare stderr text with empty stdout), while
    /// `--format text` propagates the friendly message. Live on Linux CI;
    /// vacuous on macOS.
    #[test]
    fn non_macos_refusal_honors_the_format_contract() {
        use crate::cli::{ServiceActionArgs, ServiceCommand, ServiceFormat, ServiceSubcommand};

        if std::env::consts::OS == "macos" {
            return;
        }
        let json_cmd = ServiceCommand {
            command: ServiceSubcommand::Start(ServiceActionArgs {
                format: ServiceFormat::Json,
            }),
        };
        assert_eq!(
            run(&json_cmd).unwrap(),
            1,
            "json refusal is a reported failure (object on stdout), exit 1"
        );
        // The object shape itself: what report_failure emits for this refusal.
        let value = failure_json("start", &require_macos().unwrap_err());
        assert_eq!(value["ok"], false);
        assert_eq!(value["action"], "start");
        assert!(
            value["error"].as_str().unwrap().contains("requires macOS"),
            "{value}"
        );

        let text_cmd = ServiceCommand {
            command: ServiceSubcommand::Start(ServiceActionArgs {
                format: ServiceFormat::Text,
            }),
        };
        let err = run(&text_cmd).unwrap_err().to_string();
        assert!(err.contains("requires macOS"), "got {err}");
    }

    #[cfg(unix)]
    mod unix {
        use super::super::*;
        use crate::service::launchd::testing::{
            exec_fail, exec_ok, print_loaded_not_running, print_not_loaded, print_probe_failed,
            print_running, supervisor, FakeExec,
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
            // Only the load-state probe ran — no bootout was attempted.
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

        /// A PROBE FAILURE (launchctl print neither succeeded nor gave the
        /// not-found signal) must propagate for every lifecycle verb — the
        /// operator must be able to trust that "not running"/"not installed"
        /// means the daemon actually was in that state, not that the probe
        /// hiccuped. (Pre-fix: any nonzero print read as not-loaded, so stop
        /// reported a false "not running" no-op.)
        #[test]
        fn lifecycle_probe_failure_propagates_never_a_no_op() {
            let (_dir, plist_path) = plist_fixture(true);
            for verb in [Verb::Start, Verb::Stop, Verb::Restart] {
                let s = supervisor(FakeExec::new(vec![print_probe_failed()]));
                let err = lifecycle_outcome(&s, &plist_path, verb).unwrap_err();
                assert!(
                    err.to_string().contains("could not determine"),
                    "{verb:?}: the probe failure surfaces, got {err}"
                );
                assert_eq!(
                    s.exec().calls().len(),
                    1,
                    "{verb:?}: no action after a failed probe"
                );
            }
        }

        /// start on an already-RUNNING unit reports "already running" and exits
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
            assert_eq!(calls.len(), 1, "load-state probe only");
        }

        /// start on a LOADED-but-NOT-running unit (crash-throttled) must NOT
        /// report "already running" — it kickstarts the loaded unit (plain
        /// kickstart, nothing to kill) and reports Acted, so exit 0 means the
        /// unit ended running through this call. (Pre-fix: the gate keyed on
        /// loaded and reported a false "already running".)
        #[test]
        fn start_on_loaded_not_running_kickstarts() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_loaded_not_running(), exec_ok()]));
            let outcome = lifecycle_outcome(&s, &plist_path, Verb::Start).unwrap();
            assert_eq!(outcome, Outcome::Acted(Verb::Start), "not AlreadyRunning");
            assert_eq!(outcome.exit_code(), 0);
            let calls = s.exec().calls();
            assert_eq!(
                calls[1],
                vec!["launchctl", "kickstart", "gui/501/com.dbtlr.norn.serve"],
                "a plain kickstart brings the loaded unit up"
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

        /// Nothing on disk and DEFINITIVELY nothing loaded: the structured
        /// not-installed no-op, exit 1.
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

        /// A PROBE FAILURE during uninstall must propagate WITHOUT touching the
        /// plist — the orphaned-daemon state (plist gone, unit possibly still
        /// loaded) is impossible by construction. (Pre-fix: a failed probe read
        /// as not-loaded, skipped the bootout, and deleted the plist anyway.)
        #[test]
        fn uninstall_probe_failure_propagates_and_keeps_the_plist() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![print_probe_failed()]));
            let err = uninstall_outcome(&s, &plist_path).unwrap_err();
            assert!(
                err.to_string().contains("could not determine"),
                "the probe failure surfaces: {err}"
            );
            assert!(
                plist_path.exists(),
                "the plist must survive an unanswerable probe"
            );
            assert_eq!(s.exec().calls().len(), 1, "no bootout after a failed probe");
        }

        /// A genuine bootout failure during uninstall must PROPAGATE and leave
        /// the plist on disk — deleting it would orphan a still-loaded
        /// KeepAlive daemon with no unit file left to manage it by.
        #[test]
        fn uninstall_keeps_the_plist_when_bootout_fails() {
            let (_dir, plist_path) = plist_fixture(true);
            let s = supervisor(FakeExec::new(vec![
                print_running(7),                          // probe: loaded
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

        /// uninstall of a definitively-not-loaded unit skips the bootout
        /// entirely and removes the plist.
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
                "probe only — no bootout for an unloaded unit"
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

        /// The JSON failure object every verb (preamble included) emits through
        /// the [`report_failure`] boundary: parseable, ok:false, named action,
        /// error text carried.
        #[test]
        fn failure_json_is_a_parseable_ok_false_object() {
            let error = anyhow::anyhow!("HOME is not set");
            let value = failure_json("status", &error);
            // Round-trip through a string to prove it is parseable JSON.
            let parsed: serde_json::Value =
                serde_json::from_str(&value.to_string()).expect("parseable");
            assert_eq!(parsed["ok"], false);
            assert_eq!(parsed["action"], "status");
            assert!(
                parsed["error"]
                    .as_str()
                    .unwrap()
                    .contains("HOME is not set"),
                "{parsed}"
            );
        }

        /// The status report renders to ONE complete string per format — the
        /// single-write emission that makes a mid-render double-emission
        /// (half a report + a second failure object on the same broken
        /// stdout) structurally impossible. The JSON string must be exactly
        /// one parseable document.
        #[test]
        fn render_status_produces_one_complete_document() {
            let report = status::assemble_status(
                status::ProbedState {
                    launchd: status::LaunchdState::Loaded {
                        running: true,
                        pid: Some(42),
                    },
                    running_version: Some("0.45.1".into()),
                    uptime_secs: Some(9),
                    pong_pid: None,
                },
                "0.45.1",
                status::ServicePaths {
                    plist: "/p".into(),
                    log: "/l".into(),
                    socket: "/s".into(),
                },
            );
            let json = render_status(&report, ServiceFormat::Json);
            let v: serde_json::Value =
                serde_json::from_str(&json).expect("exactly one parseable JSON document");
            assert_eq!(v["pid"], 42);
            let text = render_status(&report, ServiceFormat::Text);
            assert!(
                text.starts_with("serve: loaded, running (pid 42)"),
                "{text}"
            );
            assert!(
                text.ends_with("log    /l\n"),
                "complete to the last line: {text}"
            );
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
