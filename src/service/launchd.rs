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
//!   retry the bootstrap on exactly that code.
//! - **Honest stop** — a `KeepAlive` daemon is resurrected if merely killed, so
//!   stop is a `bootout` (the plist stays on disk).
//! - **restart** is `kickstart -k`; a nonzero `print` means the unit is not
//!   loaded.

use std::time::Duration;

/// A captured `launchctl` invocation result. Never carries the spawn failure
/// itself — that is an [`std::io::Error`] the caller maps — only the exit of a
/// process that did run.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub code: i32,
    #[allow(dead_code)] // Captured for symmetry / future diagnostics; not read today.
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

/// What a `launchctl print` says about the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
}

/// `bootout` is asynchronous: it can return before launchd finishes tearing the
/// old unit down, so an immediate `bootstrap` races and fails with error 5
/// ("Input/output error"), leaving nothing loaded. Retry the bootstrap a few
/// times on exactly that code; any other exit is a genuine failure that
/// surfaces at once.
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
    /// the usual load error.
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

    /// Unload the unit (tolerant of an already-unloaded unit). The plist file is
    /// the caller's to remove.
    pub fn uninstall(&self) -> anyhow::Result<()> {
        self.run(&["bootout", &self.service], "", true)
    }

    /// Bring an installed-but-unloaded unit back. A stop→start sequence hits the
    /// same async-teardown race as a reinstall, so it shares the retry.
    pub fn start(&self, plist_file: &str) -> anyhow::Result<()> {
        self.bootstrap_with_retry(plist_file)
    }

    /// Honest stop: `bootout` (KeepAlive would resurrect a merely-killed pid).
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

    /// Probe load/run state via `launchctl print`. A nonzero exit means the unit
    /// is not loaded.
    pub fn info(&self) -> anyhow::Result<ServiceInfo> {
        let result = self
            .exec
            .run(&["launchctl", "print", &self.service])
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
        if result.code != 0 {
            return Ok(ServiceInfo {
                loaded: false,
                running: false,
                pid: None,
            });
        }
        Ok(parse_print(&result.stdout))
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

/// Parse a `launchctl print gui/<uid>/<label>` dump into load/run state. A `pid
/// = N` line (or an explicit `state = running`) means running; the pid is read
/// when present.
fn parse_print(stdout: &str) -> ServiceInfo {
    let pid = extract_pid(stdout);
    let running = pid.is_some() || stdout.contains("state = running");
    ServiceInfo {
        loaded: true,
        running,
        pid,
    }
}

/// Read the integer after the first `pid = ` in a launchctl print dump.
fn extract_pid(stdout: &str) -> Option<u32> {
    let idx = stdout.find("pid = ")?;
    let rest = &stdout[idx + "pid = ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every argv it is handed and replays a queued sequence of results,
    /// so a test can both assert the exact launchctl calls and drive multi-step
    /// flows (e.g. the bootstrap-race retry).
    struct FakeExec {
        calls: RefCell<Vec<Vec<String>>>,
        results: RefCell<std::collections::VecDeque<ExecResult>>,
    }

    impl FakeExec {
        fn new(results: Vec<ExecResult>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                results: RefCell::new(results.into_iter().collect()),
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
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

    fn ok() -> ExecResult {
        ExecResult {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn sup(exec: FakeExec) -> LaunchdSupervisor<FakeExec> {
        let mut s = LaunchdSupervisor::new(exec, 501, "com.dbtlr.norn.serve");
        s.settle = Duration::ZERO; // no real sleep in the retry path
        s
    }

    #[test]
    fn install_is_bootout_first_then_bootstrap() {
        let s = sup(FakeExec::new(vec![ok(), ok()]));
        s.install("/p/serve.plist").unwrap();
        let calls = s.exec.calls();
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
        let s = sup(FakeExec::new(vec![
            ExecResult {
                code: 3,
                stdout: String::new(),
                stderr: "No such process".into(),
            },
            ok(),
        ]));
        assert!(s.install("/p/serve.plist").is_ok());
    }

    #[test]
    fn bootstrap_retries_only_the_race_code() {
        // First bootstrap loses the async-teardown race (code 5), second wins.
        let s = sup(FakeExec::new(vec![
            ok(), // bootout
            ExecResult {
                code: 5,
                stdout: String::new(),
                stderr: "Input/output error".into(),
            },
            ok(), // retried bootstrap
        ]));
        s.install("/p/serve.plist").unwrap();
        let calls = s.exec.calls();
        assert_eq!(calls.len(), 3, "one bootout + two bootstrap attempts");
    }

    #[test]
    fn a_non_race_bootstrap_failure_surfaces_immediately() {
        let s = sup(FakeExec::new(vec![
            ok(), // bootout
            ExecResult {
                code: 2,
                stdout: String::new(),
                stderr: "bad plist".into(),
            },
        ]));
        let err = s.install("/p/serve.plist").unwrap_err().to_string();
        assert!(err.contains("could not load the service"), "got {err}");
        assert!(err.contains("bad plist"), "stderr is surfaced: {err}");
        assert_eq!(s.exec.calls().len(), 2, "no retry on a non-race failure");
    }

    #[test]
    fn stop_is_a_bootout() {
        let s = sup(FakeExec::new(vec![ok()]));
        s.stop().unwrap();
        assert_eq!(
            s.exec.calls()[0],
            vec!["launchctl", "bootout", "gui/501/com.dbtlr.norn.serve"]
        );
    }

    #[test]
    fn restart_is_kickstart_k() {
        let s = sup(FakeExec::new(vec![ok()]));
        s.restart().unwrap();
        assert_eq!(
            s.exec.calls()[0],
            vec![
                "launchctl",
                "kickstart",
                "-k",
                "gui/501/com.dbtlr.norn.serve"
            ]
        );
    }

    #[test]
    fn info_reports_not_loaded_on_nonzero_print() {
        let s = sup(FakeExec::new(vec![ExecResult {
            code: 113,
            stdout: String::new(),
            stderr: "Could not find service".into(),
        }]));
        assert_eq!(
            s.info().unwrap(),
            ServiceInfo {
                loaded: false,
                running: false,
                pid: None
            }
        );
    }

    #[test]
    fn info_parses_pid_and_running_state() {
        let dump = "com.dbtlr.norn.serve = {\n\tstate = running\n\tpid = 4242\n}";
        let s = sup(FakeExec::new(vec![ExecResult {
            code: 0,
            stdout: dump.into(),
            stderr: String::new(),
        }]));
        assert_eq!(
            s.info().unwrap(),
            ServiceInfo {
                loaded: true,
                running: true,
                pid: Some(4242)
            }
        );
    }

    #[test]
    fn info_loaded_but_not_running_has_no_pid() {
        let dump = "com.dbtlr.norn.serve = {\n\tstate = not running\n}";
        let s = sup(FakeExec::new(vec![ExecResult {
            code: 0,
            stdout: dump.into(),
            stderr: String::new(),
        }]));
        assert_eq!(
            s.info().unwrap(),
            ServiceInfo {
                loaded: true,
                running: false,
                pid: None
            }
        );
    }
}
