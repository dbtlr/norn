//! `norn service status` assembly + rendering (NRN-115).
//!
//! Pure: [`assemble_status`] folds the probed inputs (the launchd probe — or
//! its failure — the live control-ping pong, the on-disk build version, and
//! the resolved paths) into a [`ServiceStatus`], and the renderers turn that
//! into human text or JSON. No platform calls here, so the whole layer is
//! unit-testable on any host; the command layer supplies the probed inputs.
//!
//! Status reports what it knows: unlike the acting verbs (which propagate a
//! launchd probe failure — they make irreversible calls on the answer), a
//! failed probe here becomes [`LaunchdState::Unavailable`] and the report
//! still renders, carrying whatever the live socket pong said.
//!
//! **Exit-code rule:** degradation must not silence health gates. A report
//! whose launchd state is unknown ([`ServiceStatus::launchd_error`] present)
//! exits NONZERO ([`exit_code`]) even though it rendered — a
//! `norn service status || alert` gate fires when supervision state cannot be
//! determined. Every KNOWN state (running, stopped, not installed) exits 0.

use std::io::Write;

/// What the launchd probe said — or that it couldn't say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchdState {
    Loaded {
        running: bool,
        pid: Option<u32>,
    },
    NotLoaded,
    /// The probe failed (neither "loaded" nor the not-found signal); `error`
    /// is the probe's failure text, surfaced in the report.
    Unavailable {
        error: String,
    },
}

/// The probed inputs feeding [`assemble_status`]: the launchd verdict plus
/// whatever the live control-ping returned (`running_version` / `running_build`
/// / `uptime_secs` / `pong_pid` are `None` when nothing answered the socket).
#[derive(Debug, Clone)]
pub struct ProbedState {
    pub launchd: LaunchdState,
    pub running_version: Option<String>,
    /// The build fingerprint the pong self-reported (NRN-247). `None` when
    /// nothing answered OR when the answering daemon's pong predates the field
    /// (an old daemon) — either way it can't match the on-disk build, so a
    /// running daemon with `None` here reads as restart-pending.
    pub running_build: Option<String>,
    pub uptime_secs: Option<u64>,
    /// The pid the pong self-reported — used when launchd can't supply one.
    pub pong_pid: Option<u32>,
}

/// The resolved on-disk paths `status` reports.
#[derive(Debug, Clone)]
pub struct ServicePaths {
    pub plist: String,
    pub log: String,
    pub socket: String,
}

/// The assembled `service status` report. `loaded`/`running` are `None` when
/// the launchd probe failed (`launchd_error` then carries why); the pong
/// fields are absent when nothing answered the control socket;
/// `restart_pending` is set when a running daemon's version OR build
/// fingerprint (NRN-247) differs from the on-disk binary the plist would
/// launch — so a same-version rebuild surfaces as restart-pending too.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceStatus {
    pub loaded: Option<bool>,
    pub running: Option<bool>,
    /// Why the launchd state is unknown, when it is.
    pub launchd_error: Option<String>,
    pub pid: Option<u32>,
    pub running_version: Option<String>,
    pub on_disk_version: String,
    /// The build fingerprint the running daemon reported (NRN-247); `None` when
    /// nothing answered or the daemon predates the field.
    pub running_build: Option<String>,
    /// The on-disk binary's build fingerprint (this process's `NORN_BUILD_ID`).
    pub on_disk_build: String,
    pub restart_pending: bool,
    pub uptime_secs: Option<u64>,
    pub plist: String,
    pub log: String,
    pub socket: String,
}

/// Fold the probed inputs into a [`ServiceStatus`]. `restart_pending` is true
/// iff a daemon answered (a running version is known) and its version OR its
/// build fingerprint differs from the on-disk binary — the operator's cue that
/// the supervised process predates the installed binary and a
/// `norn service restart` would pick up the new build. Gating on the build too
/// (NRN-247) makes a same-version rebuild — where the version alone matches —
/// surface as restart-pending; a daemon whose pong predates the field
/// (`running_build` == `None`) can't match and reads as pending as well.
pub fn assemble_status(
    probed: ProbedState,
    on_disk_version: &str,
    on_disk_build: &str,
    paths: ServicePaths,
) -> ServiceStatus {
    let restart_pending = probed.running_version.is_some()
        && (probed.running_version.as_deref() != Some(on_disk_version)
            || probed.running_build.as_deref() != Some(on_disk_build));
    let (loaded, running, pid, launchd_error) = match probed.launchd {
        LaunchdState::Loaded { running, pid } => {
            (Some(true), Some(running), pid.or(probed.pong_pid), None)
        }
        LaunchdState::NotLoaded => (Some(false), Some(false), probed.pong_pid, None),
        LaunchdState::Unavailable { error } => (None, None, probed.pong_pid, Some(error)),
    };
    ServiceStatus {
        loaded,
        running,
        launchd_error,
        pid,
        running_version: probed.running_version,
        on_disk_version: on_disk_version.to_string(),
        running_build: probed.running_build,
        on_disk_build: on_disk_build.to_string(),
        restart_pending,
        uptime_secs: probed.uptime_secs,
        plist: paths.plist,
        log: paths.log,
        socket: paths.socket,
    }
}

/// The `norn service status` process exit code for a report (the module-doc
/// exit-code rule): 0 for every KNOWN launchd state, 1 when the launchd probe
/// failed — the report rendered, but a health gate must still fire on unknown
/// supervision state.
pub fn exit_code(status: &ServiceStatus) -> i32 {
    if status.launchd_error.is_some() {
        1
    } else {
        0
    }
}

/// Collapse a possibly multi-line error (launchctl's usage dumps span several
/// lines) into one `"; "`-joined line, so the headline it is embedded in stays
/// a single line. The JSON report carries the raw text untouched.
fn one_line(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Human status block. First line is the launchd state (or why it is
/// unknown); the second reconciles the running vs on-disk build (with a
/// restart-pending cue); the rest are the paths.
pub fn render_text(status: &ServiceStatus, out: &mut impl Write) -> std::io::Result<()> {
    let state = match (&status.launchd_error, status.loaded, status.running) {
        (Some(error), _, _) => format!("launchd state unavailable — {}", one_line(error)),
        (None, Some(false), _) => "not loaded".to_string(),
        (None, Some(true), Some(true)) => match status.pid {
            Some(pid) => format!("loaded, running (pid {pid})"),
            None => "loaded, running".to_string(),
        },
        (None, Some(true), _) => "loaded, not running".to_string(),
        // Unreachable via assemble_status (no-error always sets `loaded`),
        // but render every value honestly.
        (None, None, _) => "launchd state unknown".to_string(),
    };
    writeln!(out, "serve: {state}")?;

    match &status.running_version {
        Some(running) => {
            // A same-version rebuild (NRN-247) is restart-pending while the two
            // version strings match, which reads oddly on its own — call out
            // that the BUILD changed so the operator isn't left comparing two
            // identical version numbers.
            let pending = if status.restart_pending {
                if *running == status.on_disk_version {
                    " — restart pending (rebuilt)"
                } else {
                    " — restart pending"
                }
            } else {
                ""
            };
            writeln!(
                out,
                "  running v{running} · on-disk v{}{pending}",
                status.on_disk_version
            )?;
            if let Some(uptime) = status.uptime_secs {
                writeln!(out, "  uptime {}", format_uptime(uptime))?;
            }
        }
        None => writeln!(
            out,
            "  on-disk v{} · no answer on the control socket",
            status.on_disk_version
        )?,
    }

    writeln!(out, "  socket {}", status.socket)?;
    writeln!(out, "  plist  {}", status.plist)?;
    writeln!(out, "  log    {}", status.log)?;
    Ok(())
}

/// Pretty JSON of the full [`ServiceStatus`].
pub fn render_json(status: &ServiceStatus, out: &mut impl Write) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(status).expect("ServiceStatus serializes");
    writeln!(out, "{json}")
}

/// Compact `h/m/s` uptime (`45s`, `12m03s`, `1h05m`), dropping leading zero units.
fn format_uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ServicePaths {
        ServicePaths {
            plist: "/p/serve.plist".into(),
            log: "/l/serve.log".into(),
            socket: "/s/norn.sock".into(),
        }
    }

    /// A running daemon whose build fingerprint MATCHES the on-disk build, so
    /// only the version drives `restart_pending`. Build-skew cases below pass
    /// their own mismatched build explicitly.
    fn base(running_version: Option<&str>) -> ServiceStatus {
        assemble_status(
            ProbedState {
                launchd: LaunchdState::Loaded {
                    running: true,
                    pid: Some(4242),
                },
                running_version: running_version.map(str::to_string),
                running_build: Some("build-match".into()),
                uptime_secs: Some(3725),
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        )
    }

    fn text_of(status: &ServiceStatus) -> String {
        let mut buf = Vec::new();
        render_text(status, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn matching_versions_are_not_restart_pending() {
        assert!(!base(Some("0.45.1")).restart_pending);
    }

    #[test]
    fn a_stale_running_version_is_restart_pending() {
        let s = base(Some("0.44.0"));
        assert!(s.restart_pending);
        let text = text_of(&s);
        assert!(
            text.contains("running v0.44.0 · on-disk v0.45.1 — restart pending"),
            "{text}"
        );
    }

    /// NRN-247: a same-version rebuild — identical version strings but a
    /// different build fingerprint — is restart-pending, and the text calls out
    /// that the build (not the version) changed so two identical version
    /// numbers don't read as a spurious "restart pending".
    #[test]
    fn a_same_version_rebuild_is_restart_pending() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Loaded {
                    running: true,
                    pid: Some(4242),
                },
                running_version: Some("0.45.1".into()),
                running_build: Some("build-old".into()),
                uptime_secs: Some(10),
                pong_pid: None,
            },
            "0.45.1",
            "build-new",
            paths(),
        );
        assert!(s.restart_pending, "same version, different build must pend");
        let text = text_of(&s);
        assert!(
            text.contains("running v0.45.1 · on-disk v0.45.1 — restart pending (rebuilt)"),
            "{text}"
        );
    }

    /// A daemon whose pong predates the build fingerprint (`running_build` ==
    /// `None`) can't match the on-disk build, so a same-version old daemon
    /// still reads as restart-pending (NRN-247).
    #[test]
    fn a_running_daemon_without_a_build_field_is_restart_pending() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Loaded {
                    running: true,
                    pid: Some(4242),
                },
                running_version: Some("0.45.1".into()),
                running_build: None,
                uptime_secs: Some(10),
                pong_pid: None,
            },
            "0.45.1",
            "build-new",
            paths(),
        );
        assert!(
            s.restart_pending,
            "an unidentifiable build can't match on-disk; must pend"
        );
    }

    #[test]
    fn no_pong_reports_no_answer_and_is_never_restart_pending() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Loaded {
                    running: false,
                    pid: None,
                },
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        assert!(!s.restart_pending);
        let text = text_of(&s);
        assert!(text.contains("serve: loaded, not running"), "{text}");
        assert!(text.contains("no answer on the control socket"), "{text}");
    }

    #[test]
    fn not_loaded_renders_cleanly() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::NotLoaded,
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        assert!(text_of(&s).starts_with("serve: not loaded"));
    }

    /// Status reports what it knows: a failed launchd probe renders as
    /// "unavailable" (carrying the probe error) while the live pong's
    /// version / uptime / pid STILL surface — a daemon that answers the
    /// socket must not read as dead because launchctl hiccuped.
    #[test]
    fn launchd_unavailable_with_live_pong_still_reports_the_daemon() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Unavailable {
                    error: "launchctl print failed (64): could not determine".into(),
                },
                running_version: Some("0.44.0".into()),
                running_build: Some("build-stale".into()),
                uptime_secs: Some(42),
                pong_pid: Some(777),
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        assert_eq!(s.loaded, None);
        assert_eq!(s.running, None);
        assert_eq!(s.pid, Some(777), "the pong's pid fills in");
        assert!(s.restart_pending, "skew still computed from the pong");
        // Text: the unavailability AND the live daemon both render.
        let text = text_of(&s);
        assert!(
            text.contains("launchd state unavailable — launchctl print failed (64)"),
            "{text}"
        );
        assert!(
            text.contains("running v0.44.0 · on-disk v0.45.1 — restart pending"),
            "{text}"
        );
        assert!(text.contains("uptime 42s"), "{text}");
        // JSON: loaded/running null, launchd_error carried, pong fields present.
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["loaded"], serde_json::Value::Null);
        assert_eq!(v["running"], serde_json::Value::Null);
        assert!(v["launchd_error"]
            .as_str()
            .unwrap()
            .contains("launchctl print failed"));
        assert_eq!(v["running_version"], "0.44.0");
        assert_eq!(v["pid"], 777);
    }

    /// The exit-code rule: a degraded report still renders, but exits 1 so a
    /// `status || alert` health gate fires on unknown supervision state; every
    /// KNOWN state (running / stopped / not loaded) exits 0. The rule is
    /// format-independent — the same report drives both renderers.
    #[test]
    fn probe_failed_report_renders_but_exits_nonzero() {
        let degraded = assemble_status(
            ProbedState {
                launchd: LaunchdState::Unavailable {
                    error: "could not determine the service's state".into(),
                },
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        assert_eq!(exit_code(&degraded), 1, "unknown supervision state gates");
        // Both formats still render the degraded report.
        assert!(text_of(&degraded).contains("launchd state unavailable"));
        let mut buf = Vec::new();
        render_json(&degraded, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(v["launchd_error"].is_string());

        // Known states — healthy, stopped, and not-installed — exit 0.
        assert_eq!(exit_code(&base(Some("0.45.1"))), 0, "running is healthy");
        let not_loaded = assemble_status(
            ProbedState {
                launchd: LaunchdState::NotLoaded,
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        assert_eq!(
            exit_code(&not_loaded),
            0,
            "a known stopped state is not a gate"
        );
    }

    /// A multi-line launchctl stderr (the exit-64 usage dump) must not garble
    /// the one-line headline: interior newlines collapse to '; ' in text,
    /// while the JSON report keeps the raw text.
    #[test]
    fn multi_line_probe_error_stays_one_headline_line() {
        let usage_dump = "launchctl print failed (64): could not determine the service's state — \
                          Unrecognized target specifier.\nUsage: launchctl print <domain-target> | <service-target>\n\
                          Please refer to `man launchctl`.";
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Unavailable {
                    error: usage_dump.into(),
                },
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        let text = text_of(&s);
        let headline = text.lines().next().unwrap();
        assert!(
            headline.contains("Unrecognized target specifier.; Usage: launchctl print"),
            "newlines collapse to '; ': {headline}"
        );
        assert!(
            headline.contains("man launchctl"),
            "no content dropped: {headline}"
        );
        // The second line of the report is still the version line, untouched.
        assert!(
            text.lines().nth(1).unwrap().contains("on-disk v0.45.1"),
            "{text}"
        );
        // JSON keeps the raw, multi-line error.
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["launchd_error"].as_str().unwrap().contains('\n'));
    }

    /// A failed launchd probe with NO socket answer still renders a report —
    /// every live field unknown, the probe error and the paths still shown.
    #[test]
    fn launchd_unavailable_with_no_pong_renders_all_unknown() {
        let s = assemble_status(
            ProbedState {
                launchd: LaunchdState::Unavailable {
                    error: "could not determine the service's state".into(),
                },
                running_version: None,
                running_build: None,
                uptime_secs: None,
                pong_pid: None,
            },
            "0.45.1",
            "build-match",
            paths(),
        );
        let text = text_of(&s);
        assert!(text.contains("launchd state unavailable"), "{text}");
        assert!(text.contains("no answer on the control socket"), "{text}");
        assert!(text.contains("plist  /p/serve.plist"), "{text}");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["loaded"], serde_json::Value::Null);
        assert_eq!(v["running_version"], serde_json::Value::Null);
    }

    #[test]
    fn json_carries_every_field() {
        let s = base(Some("0.45.1"));
        let mut buf = Vec::new();
        render_json(&s, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["loaded"], true);
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["running_version"], "0.45.1");
        assert_eq!(v["restart_pending"], false);
        assert_eq!(v["launchd_error"], serde_json::Value::Null);
        assert_eq!(v["socket"], "/s/norn.sock");
    }

    #[test]
    fn uptime_formats_drop_leading_zero_units() {
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(723), "12m03s");
        assert_eq!(format_uptime(3725), "1h02m");
    }
}
