//! `norn service status` assembly + rendering (NRN-115).
//!
//! Pure: [`assemble_status`] folds the probed inputs (launchctl load/run state,
//! the live control-ping pong, the on-disk build version, and the resolved
//! paths) into a [`ServiceStatus`], and the renderers turn that into human text
//! or JSON. No platform calls here, so the whole layer is unit-testable on any
//! host; the command layer supplies the probed inputs.

use std::io::Write;

/// The probed daemon state feeding [`assemble_status`]: launchctl's load/run
/// verdict plus whatever the live control-ping returned (`running_version` /
/// `uptime_secs` are `None` when nothing answered the socket).
#[derive(Debug, Clone, Default)]
pub struct ProbedState {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub running_version: Option<String>,
    pub uptime_secs: Option<u64>,
}

/// The resolved on-disk paths `status` reports.
#[derive(Debug, Clone)]
pub struct ServicePaths {
    pub plist: String,
    pub log: String,
    pub socket: String,
}

/// The assembled `service status` report. `running_version`/`uptime_secs` come
/// from the live daemon's pong (absent when nothing answered the control
/// socket); `restart_pending` is set when a running version is known and differs
/// from the on-disk build the plist would launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceStatus {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub running_version: Option<String>,
    pub on_disk_version: String,
    pub restart_pending: bool,
    pub uptime_secs: Option<u64>,
    pub plist: String,
    pub log: String,
    pub socket: String,
}

/// Fold the probed inputs into a [`ServiceStatus`]. `restart_pending` is true
/// iff a running version is known and differs from `on_disk_version` — the
/// operator's cue that the supervised process predates the installed binary and
/// a `norn service restart` would pick up the new build.
pub fn assemble_status(
    probed: ProbedState,
    on_disk_version: &str,
    paths: ServicePaths,
) -> ServiceStatus {
    let restart_pending = probed
        .running_version
        .as_deref()
        .is_some_and(|v| v != on_disk_version);
    ServiceStatus {
        loaded: probed.loaded,
        running: probed.running,
        pid: probed.pid,
        running_version: probed.running_version,
        on_disk_version: on_disk_version.to_string(),
        restart_pending,
        uptime_secs: probed.uptime_secs,
        plist: paths.plist,
        log: paths.log,
        socket: paths.socket,
    }
}

/// Human status block. First line is load/run state; the second reconciles the
/// running vs on-disk build (with a restart-pending cue); the rest are the paths.
pub fn render_text(status: &ServiceStatus, out: &mut impl Write) -> std::io::Result<()> {
    let state = if !status.loaded {
        "not loaded".to_string()
    } else if status.running {
        match status.pid {
            Some(pid) => format!("loaded, running (pid {pid})"),
            None => "loaded, running".to_string(),
        }
    } else {
        "loaded, not running".to_string()
    };
    writeln!(out, "serve: {state}")?;

    match &status.running_version {
        Some(running) => {
            let pending = if status.restart_pending {
                " — restart pending"
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

    fn base(running_version: Option<&str>) -> ServiceStatus {
        assemble_status(
            ProbedState {
                loaded: true,
                running: true,
                pid: Some(4242),
                running_version: running_version.map(str::to_string),
                uptime_secs: Some(3725),
            },
            "0.45.1",
            paths(),
        )
    }

    #[test]
    fn matching_versions_are_not_restart_pending() {
        assert!(!base(Some("0.45.1")).restart_pending);
    }

    #[test]
    fn a_stale_running_version_is_restart_pending() {
        let s = base(Some("0.44.0"));
        assert!(s.restart_pending);
        let mut buf = Vec::new();
        render_text(&s, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("running v0.44.0 · on-disk v0.45.1 — restart pending"),
            "{text}"
        );
    }

    #[test]
    fn no_pong_reports_no_answer_and_is_never_restart_pending() {
        let s = assemble_status(
            ProbedState {
                loaded: true,
                ..ProbedState::default()
            },
            "0.45.1",
            paths(),
        );
        assert!(!s.restart_pending);
        let mut buf = Vec::new();
        render_text(&s, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("serve: loaded, not running"), "{text}");
        assert!(text.contains("no answer on the control socket"), "{text}");
    }

    #[test]
    fn not_loaded_renders_cleanly() {
        let s = assemble_status(ProbedState::default(), "0.45.1", paths());
        let mut buf = Vec::new();
        render_text(&s, &mut buf).unwrap();
        assert!(String::from_utf8(buf)
            .unwrap()
            .starts_with("serve: not loaded"));
    }

    #[test]
    fn json_carries_every_field() {
        let s = base(Some("0.45.1"));
        let mut buf = Vec::new();
        render_json(&s, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["running_version"], "0.45.1");
        assert_eq!(v["restart_pending"], false);
        assert_eq!(v["socket"], "/s/norn.sock");
    }

    #[test]
    fn uptime_formats_drop_leading_zero_units() {
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(723), "12m03s");
        assert_eq!(format_uptime(3725), "1h02m");
    }
}
