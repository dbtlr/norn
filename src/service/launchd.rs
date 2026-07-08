//! The launchd supervisor seam (NRN-115).
//!
//! `norn service` verbs speak through [`LaunchdSupervisor`]; every `launchctl`
//! invocation flows through the [`Exec`] trait so the argv construction and
//! exit-code interpretation are pure and unit-testable with a fake, while CI
//! never shells out. Modern launchctl subcommands only. Quirks the shape
//! encodes (mirroring Mimir's contract):
//!
//! - **Idempotent install** — `bootout` any loaded copy first (a no-op bootout
//!   is tolerated), then `bootstrap`, so a re-install refreshes cleanly.
//! - **Async-teardown race** — `bootout` returns before launchd has fully torn
//!   the unit down, so an immediate `bootstrap` can lose the race with error 5;
//!   retry the bootstrap on exactly that code. launchctl reports an
//!   already-loaded unit with the SAME code 5, so the retry must only ever run
//!   on a post-bootout path — `install` (after its own bootout) and `start`
//!   (which the command layer gates to a not-loaded unit, so any code 5 there
//!   is a just-finished `stop` still settling, never "already loaded").
//! - **Honest stop** — a `KeepAlive` daemon is resurrected if merely killed, so
//!   stop is a `bootout` (the plist stays on disk).
//! - **restart** is `kickstart -k`.
//! - **Three-outcome probe** — `launchctl print` distinguishes loaded (exit 0),
//!   definitively not loaded (its not-found signal), and a genuine probe
//!   failure (anything else). The gates built on the probe make irreversible
//!   calls (delete a plist, report "not running"), so a transient print
//!   failure must surface as an error, never read as "not loaded".

use std::time::Duration;

/// A captured `launchctl` invocation result. Never carries the spawn failure
/// itself — that is an [`std::io::Error`] the caller maps — only the exit of a
/// process that did run.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The one seam between the supervisor and the OS: run an argv, capture the
/// result. Faked in tests; [`RealExec`] shells out for real.
pub trait Exec {
    fn run(&self, argv: &[&str]) -> std::io::Result<ExecResult>;
}

/// Runs the argv as a real subprocess, capturing its exit code and streams.
pub struct RealExec;

impl Exec for RealExec {
    fn run(&self, argv: &[&str]) -> std::io::Result<ExecResult> {
        let output = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .output()?;
        Ok(ExecResult {
            // A process killed by a signal has no exit code; -1 is a sentinel
            // "did not exit cleanly" that no launchctl success path returns.
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// What a successful probe says about the unit. The third outcome — a probe
/// that could not determine the state at all — is the `Err` of
/// [`LaunchdSupervisor::load_state`], deliberately NOT a variant here: every
/// gate consuming this type makes an irreversible call (delete a plist, report
/// "not running"), so "unknown" must not be representable as an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// launchd has the unit (`print` exit 0). `running`/`pid` parsed from the
    /// dump; a loaded unit can be not-running (crash-throttled, or between
    /// KeepAlive respawns).
    Loaded { running: bool, pid: Option<u32> },
    /// launchd definitively does not have the unit — `print`'s not-found
    /// signal, and only that signal.
    NotLoaded,
}

impl LoadState {
    pub fn loaded(&self) -> bool {
        matches!(self, LoadState::Loaded { .. })
    }
    pub fn running(&self) -> bool {
        matches!(self, LoadState::Loaded { running: true, .. })
    }
    pub fn pid(&self) -> Option<u32> {
        match self {
            LoadState::Loaded { pid, .. } => *pid,
            LoadState::NotLoaded => None,
        }
    }
}

/// `launchctl print`'s not-found signal, verified empirically (macOS, Darwin
/// 25.5.0): `launchctl print gui/501/<absent-label>` exits **113** with `Bad
/// request.` / `Could not find service "<label>" in domain for user gui: 501`
/// on stderr, while a malformed target (a genuine usage failure) exits 64 with
/// a usage message. "Definitively not loaded" requires BOTH the code and the
/// stderr marker — exactly the observed signal — so any deviation (a code
/// shuffle, a reworded message, some other 113) degrades to a loud probe
/// error, never to a wrong "not loaded" that a gate acts on irreversibly.
const PRINT_NOT_FOUND_CODE: i32 = 113;
const PRINT_NOT_FOUND_MARKER: &str = "Could not find service";

/// `bootout` is asynchronous: it can return before launchd finishes tearing the
/// old unit down, so an immediate `bootstrap` races and fails with error 5
/// ("Input/output error"), leaving nothing loaded. Retry the bootstrap a few
/// times on exactly that code; any other exit is a genuine failure that
/// surfaces at once. (Code 5 also means "already loaded" — see the module docs
/// for why the retry only ever runs on post-bootout paths.)
const BOOTSTRAP_ATTEMPTS: u32 = 5;
const BOOTSTRAP_SETTLE: Duration = Duration::from_millis(250);
const BOOTSTRAP_RACE_CODE: i32 = 5;

/// A label-bound launchd supervisor. `target` is `gui/<uid>`; `service` is
/// `gui/<uid>/<label>` — the two address forms modern launchctl wants.
pub struct LaunchdSupervisor<E: Exec> {
    exec: E,
    target: String,
    service: String,
    /// Sleep between bootstrap-race retries; 250ms in production, 0 in tests so
    /// the retry path is exercised without a real delay.
    settle: Duration,
}

impl<E: Exec> LaunchdSupervisor<E> {
    /// Test-only view of the exec seam, so command-layer tests can assert the
    /// exact launchctl argv a policy decision produced (or suppressed).
    #[cfg(test)]
    pub(crate) fn exec(&self) -> &E {
        &self.exec
    }

    pub fn new(exec: E, uid: u32, label: &str) -> Self {
        Self {
            target: format!("gui/{uid}"),
            service: format!("gui/{uid}/{label}"),
            exec,
            settle: BOOTSTRAP_SETTLE,
        }
    }

    /// One `launchctl <argv>` call. On a nonzero exit (unless `tolerate`), an
    /// error naming the verb, code, and the caller's `failure` hint plus any
    /// stderr.
    fn run(&self, argv: &[&str], failure: &str, tolerate: bool) -> anyhow::Result<()> {
        let mut full = Vec::with_capacity(argv.len() + 1);
        full.push("launchctl");
        full.extend_from_slice(argv);
        let result = self
            .exec
            .run(&full)
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
        if result.code != 0 && !tolerate {
            anyhow::bail!(
                "{}",
                launchctl_error(
                    argv.first().copied().unwrap_or(""),
                    result.code,
                    failure,
                    &result.stderr
                )
            );
        }
        Ok(())
    }

    /// Idempotent install: clear any loaded copy (a no-op bootout is expected and
    /// tolerated), then bootstrap the plist with the async-teardown retry.
    pub fn install(&self, plist_file: &str) -> anyhow::Result<()> {
        self.run(&["bootout", &self.service], "", true)?;
        self.bootstrap_with_retry(plist_file)
    }

    /// Bootstrap, retrying ONLY the async-teardown race so a genuine failure
    /// still surfaces immediately. A race that outlives the budget surfaces as
    /// the usual load error. Callers must guarantee the unit is NOT loaded when
    /// this runs (install bootouts first; the command layer gates `start`),
    /// because launchctl reports "already loaded" with the same code 5 the
    /// retry treats as the race.
    fn bootstrap_with_retry(&self, plist_file: &str) -> anyhow::Result<()> {
        for attempt in 1..=BOOTSTRAP_ATTEMPTS {
            let full = ["launchctl", "bootstrap", &self.target, plist_file];
            let result = self
                .exec
                .run(&full)
                .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
            if result.code == 0 {
                return Ok(());
            }
            if result.code != BOOTSTRAP_RACE_CODE || attempt >= BOOTSTRAP_ATTEMPTS {
                anyhow::bail!(
                    "{}",
                    launchctl_error(
                        "bootstrap",
                        result.code,
                        "could not load the service",
                        &result.stderr
                    )
                );
            }
            std::thread::sleep(self.settle);
        }
        // Unreachable: the loop returns or bails on the final attempt.
        Ok(())
    }

    /// Bring an installed-but-unloaded unit back. A stop→start sequence hits the
    /// same async-teardown race as a reinstall (the prior stop's bootout may
    /// still be settling), so it shares the retry. The command layer never
    /// calls this on a loaded unit (an already-loaded bootstrap fails with the
    /// race's code 5 and would spin the retry pointlessly).
    pub fn start(&self, plist_file: &str) -> anyhow::Result<()> {
        self.bootstrap_with_retry(plist_file)
    }

    /// Honest stop: `bootout` (KeepAlive would resurrect a merely-killed pid).
    /// NOT tolerant — the caller gates on loaded state, so a failure here is a
    /// genuine "could not unload" that must surface (uninstall relies on this
    /// to avoid deleting the plist out from under a still-loaded daemon).
    pub fn stop(&self) -> anyhow::Result<()> {
        self.run(
            &["bootout", &self.service],
            "could not unload the service",
            false,
        )
    }

    /// Restart in place: `kickstart -k` kills and reruns the loaded unit.
    pub fn restart(&self) -> anyhow::Result<()> {
        self.run(
            &["kickstart", "-k", &self.service],
            "is the service installed?",
            false,
        )
    }

    /// Spawn a loaded-but-not-running unit now (plain `kickstart`, no `-k` —
    /// there is nothing to kill). Used by `start` on a loaded unit that is not
    /// running (e.g. crash-throttled between KeepAlive respawns).
    pub fn kickstart(&self) -> anyhow::Result<()> {
        self.run(
            &["kickstart", &self.service],
            "could not start the loaded unit",
            false,
        )
    }

    /// The three-outcome probe over `launchctl print`:
    ///
    /// - exit 0 → `Ok(LoadState::Loaded { .. })` (running/pid parsed from the dump)
    /// - the verified not-found signal ([`PRINT_NOT_FOUND_CODE`] +
    ///   [`PRINT_NOT_FOUND_MARKER`]) → `Ok(LoadState::NotLoaded)`
    /// - anything else (spawn failure, any other nonzero, 113 without the
    ///   marker) → `Err` — the state is UNKNOWN, and the callers' gates make
    ///   irreversible calls, so unknown must surface, never default.
    pub fn load_state(&self) -> anyhow::Result<LoadState> {
        let result = self
            .exec
            .run(&["launchctl", "print", &self.service])
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
        if result.code == 0 {
            return Ok(parse_print(&result.stdout));
        }
        if result.code == PRINT_NOT_FOUND_CODE && result.stderr.contains(PRINT_NOT_FOUND_MARKER) {
            return Ok(LoadState::NotLoaded);
        }
        anyhow::bail!(
            "{}",
            launchctl_error(
                "print",
                result.code,
                "could not determine the service's state",
                &result.stderr
            )
        );
    }
}

/// Build the uniform launchctl-failure message (naming verb, code, hint, and any
/// stderr) so `run` and `bootstrap_with_retry` cannot drift.
fn launchctl_error(verb: &str, code: i32, failure: &str, stderr: &str) -> String {
    let trimmed = stderr.trim();
    let tail = if trimmed.is_empty() {
        String::new()
    } else {
        format!(" — {trimmed}")
    };
    if failure.is_empty() {
        format!("launchctl {verb} failed ({code}){tail}")
    } else {
        format!("launchctl {verb} failed ({code}): {failure}{tail}")
    }
}

/// Parse a successful `launchctl print gui/<uid>/<label>` dump. A `pid = N`
/// line (or an explicit `state = running`) means running; the pid is read when
/// present.
fn parse_print(stdout: &str) -> LoadState {
    let pid = extract_pid(stdout);
    let running = pid.is_some() || stdout.contains("state = running");
    LoadState::Loaded { running, pid }
}

/// Read the integer after the first `pid = ` in a launchctl print dump.
fn extract_pid(stdout: &str) -> Option<u32> {
    let idx = stdout.find("pid = ")?;
    let rest = &stdout[idx + "pid = ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Test doubles shared by this module's tests and the command layer's
/// (`super::command`) regression tests, so both drive the SAME seam.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::RefCell;

    /// Records every argv it is handed and replays a queued sequence of results,
    /// so a test can both assert the exact launchctl calls and drive multi-step
    /// flows (e.g. the bootstrap-race retry).
    pub(crate) struct FakeExec {
        calls: RefCell<Vec<Vec<String>>>,
        results: RefCell<std::collections::VecDeque<ExecResult>>,
    }

    impl FakeExec {
        pub(crate) fn new(results: Vec<ExecResult>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                results: RefCell::new(results.into_iter().collect()),
            }
        }
        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl Exec for FakeExec {
        fn run(&self, argv: &[&str]) -> std::io::Result<ExecResult> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|s| s.to_string()).collect());
            Ok(self.results.borrow_mut().pop_front().unwrap_or(ExecResult {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }))
        }
    }

    pub(crate) fn exec_ok() -> ExecResult {
        ExecResult {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub(crate) fn exec_fail(code: i32, stderr: &str) -> ExecResult {
        ExecResult {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// A `launchctl print` result for a loaded, running unit with `pid`.
    pub(crate) fn print_running(pid: u32) -> ExecResult {
        ExecResult {
            code: 0,
            stdout: format!("com.dbtlr.norn.serve = {{\n\tstate = running\n\tpid = {pid}\n}}"),
            stderr: String::new(),
        }
    }

    /// A `launchctl print` result for a loaded unit that is NOT running
    /// (crash-throttled / between KeepAlive respawns): exit 0, no pid.
    pub(crate) fn print_loaded_not_running() -> ExecResult {
        ExecResult {
            code: 0,
            stdout: "com.dbtlr.norn.serve = {\n\tstate = not running\n}".into(),
            stderr: String::new(),
        }
    }

    /// The verified not-found signal: exit 113 + the "Could not find service"
    /// stderr marker (see `PRINT_NOT_FOUND_*`).
    pub(crate) fn print_not_loaded() -> ExecResult {
        exec_fail(
            113,
            "Bad request.\nCould not find service \"com.dbtlr.norn.serve\" in domain for user gui: 501",
        )
    }

    /// A genuine `launchctl print` failure — nonzero WITHOUT the not-found
    /// signal (here launchctl's usage error, exit 64). The probe must surface
    /// this as an error, never read it as "not loaded".
    pub(crate) fn print_probe_failed() -> ExecResult {
        exec_fail(64, "Unrecognized target specifier.")
    }

    /// A supervisor over a [`FakeExec`] with a zero retry settle (so the
    /// bootstrap-race path runs without real delay).
    pub(crate) fn supervisor(exec: FakeExec) -> LaunchdSupervisor<FakeExec> {
        let mut s = LaunchdSupervisor::new(exec, 501, "com.dbtlr.norn.serve");
        s.settle = Duration::ZERO;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{
        exec_fail, exec_ok, print_loaded_not_running, print_not_loaded, print_probe_failed,
        print_running, supervisor, FakeExec,
    };
    use super::*;

    #[test]
    fn install_is_bootout_first_then_bootstrap() {
        let s = supervisor(FakeExec::new(vec![exec_ok(), exec_ok()]));
        s.install("/p/serve.plist").unwrap();
        let calls = s.exec().calls();
        assert_eq!(
            calls[0],
            vec!["launchctl", "bootout", "gui/501/com.dbtlr.norn.serve"]
        );
        assert_eq!(
            calls[1],
            vec!["launchctl", "bootstrap", "gui/501", "/p/serve.plist"]
        );
    }

    #[test]
    fn install_tolerates_a_bootout_no_op() {
        // bootout of an unloaded unit fails; install must still bootstrap and succeed.
        let s = supervisor(FakeExec::new(vec![
            exec_fail(3, "No such process"),
            exec_ok(),
        ]));
        assert!(s.install("/p/serve.plist").is_ok());
    }

    #[test]
    fn bootstrap_retries_only_the_race_code() {
        // First bootstrap loses the async-teardown race (code 5), second wins.
        let s = supervisor(FakeExec::new(vec![
            exec_ok(), // bootout
            exec_fail(5, "Input/output error"),
            exec_ok(), // retried bootstrap
        ]));
        s.install("/p/serve.plist").unwrap();
        let calls = s.exec().calls();
        assert_eq!(calls.len(), 3, "one bootout + two bootstrap attempts");
    }

    #[test]
    fn a_non_race_bootstrap_failure_surfaces_immediately() {
        let s = supervisor(FakeExec::new(vec![
            exec_ok(), // bootout
            exec_fail(2, "bad plist"),
        ]));
        let err = s.install("/p/serve.plist").unwrap_err().to_string();
        assert!(err.contains("could not load the service"), "got {err}");
        assert!(err.contains("bad plist"), "stderr is surfaced: {err}");
        assert_eq!(s.exec().calls().len(), 2, "no retry on a non-race failure");
    }

    #[test]
    fn stop_is_a_bootout_and_surfaces_failure() {
        let s = supervisor(FakeExec::new(vec![exec_ok()]));
        s.stop().unwrap();
        assert_eq!(
            s.exec().calls()[0],
            vec!["launchctl", "bootout", "gui/501/com.dbtlr.norn.serve"]
        );
        // A genuine bootout failure is NOT tolerated (uninstall relies on this).
        let s = supervisor(FakeExec::new(vec![exec_fail(1, "boom")]));
        let err = s.stop().unwrap_err().to_string();
        assert!(err.contains("could not unload the service"), "got {err}");
    }

    #[test]
    fn restart_is_kickstart_k() {
        let s = supervisor(FakeExec::new(vec![exec_ok()]));
        s.restart().unwrap();
        assert_eq!(
            s.exec().calls()[0],
            vec![
                "launchctl",
                "kickstart",
                "-k",
                "gui/501/com.dbtlr.norn.serve"
            ]
        );
    }

    /// The verified not-found signal (113 + marker) is the ONLY nonzero print
    /// that reads as definitively-not-loaded.
    #[test]
    fn load_state_not_found_signal_is_not_loaded() {
        let s = supervisor(FakeExec::new(vec![print_not_loaded()]));
        assert_eq!(s.load_state().unwrap(), LoadState::NotLoaded);
    }

    #[test]
    fn load_state_parses_pid_and_running_state() {
        let s = supervisor(FakeExec::new(vec![print_running(4242)]));
        assert_eq!(
            s.load_state().unwrap(),
            LoadState::Loaded {
                running: true,
                pid: Some(4242)
            }
        );
    }

    #[test]
    fn load_state_loaded_but_not_running_has_no_pid() {
        let s = supervisor(FakeExec::new(vec![print_loaded_not_running()]));
        assert_eq!(
            s.load_state().unwrap(),
            LoadState::Loaded {
                running: false,
                pid: None
            }
        );
    }

    /// A genuine print failure (nonzero without the not-found signal) is an
    /// ERROR — the pre-fix probe read every nonzero as "not loaded", so a
    /// transient failure fed the gates a wrong, irreversible answer.
    #[test]
    fn load_state_probe_failure_is_an_error_not_not_loaded() {
        let s = supervisor(FakeExec::new(vec![print_probe_failed()]));
        let err = s.load_state().unwrap_err().to_string();
        assert!(
            err.contains("could not determine the service's state"),
            "got {err}"
        );
        assert!(
            err.contains("Unrecognized target"),
            "stderr surfaced: {err}"
        );
    }

    /// The not-found signal requires BOTH the code and the marker — exit 113
    /// without the marker is a probe failure, and the marker at another code
    /// is too (encode exactly what was verified; deviations degrade loudly).
    #[test]
    fn load_state_partial_not_found_signals_are_probe_failures() {
        let s = supervisor(FakeExec::new(vec![exec_fail(
            113,
            "something else entirely",
        )]));
        assert!(s.load_state().is_err(), "113 without the marker is unknown");
        let s = supervisor(FakeExec::new(vec![exec_fail(
            1,
            "Could not find service x",
        )]));
        assert!(
            s.load_state().is_err(),
            "the marker at another code is unknown"
        );
    }

    #[test]
    fn kickstart_is_plain_kickstart_without_kill() {
        let s = supervisor(FakeExec::new(vec![exec_ok()]));
        s.kickstart().unwrap();
        assert_eq!(
            s.exec().calls()[0],
            vec!["launchctl", "kickstart", "gui/501/com.dbtlr.norn.serve"]
        );
    }

    /// The captured stdout/stderr are load-bearing, not symmetry: stdout feeds
    /// `parse_print` (info) and stderr feeds the failure message.
    #[test]
    fn exec_result_streams_are_consumed() {
        let s = supervisor(FakeExec::new(vec![print_running(7)]));
        assert_eq!(
            s.load_state().unwrap().pid(),
            Some(7),
            "stdout drives load_state()"
        );
        let s = supervisor(FakeExec::new(vec![exec_fail(1, "the reason")]));
        let err = s.stop().unwrap_err().to_string();
        assert!(err.contains("the reason"), "stderr drives the error: {err}");
    }
}
