//! Integration tests for the Slice 3 mutation event stream.
//!
//! Verifies that an applier-routed mutating command (here: `norn move`) writes
//! `invocation_started` + `op_planned` + `invocation_finished` lines to a
//! per-vault JSONL event stream on a REAL apply, and writes nothing on a
//! dry-run. The stream lands under `$XDG_STATE_HOME/norn/<hash>/events/`, so we
//! isolate each test with a fresh tempdir as `XDG_STATE_HOME`. `XDG_CACHE_HOME`
//! is isolated too (to a hidden subdir of the vault tempdir) so the binary
//! never reads or sweeps the developer's real cache tree.

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// A vault with `a.md` and `c.md` containing a `[[a]]` backlink. The backlink
/// guarantees a cascade op is planned alongside the move so multiple
/// `op_planned` events can appear; here we only assert on the lifecycle/planned
/// event names.
fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-telemetry-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n[[a]]\n").unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Recursively collect all `events-*.jsonl` files under `dir`.
fn find_event_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_event_files(&path));
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                found.push(path);
            }
        }
    }
    found
}

/// Find every event file under the state dir and parse every JSON line.
fn read_all_event_lines(state_root: &Path) -> Vec<serde_json::Value> {
    let files = find_event_files(state_root);
    let mut events = Vec::new();
    for f in files {
        let body = std::fs::read_to_string(&f).unwrap();
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            events.push(serde_json::from_str(line).expect("each event line must parse as JSON"));
        }
    }
    events
}

fn no_event_files(state_root: &Path) -> bool {
    find_event_files(state_root).is_empty()
}

#[test]
fn move_apply_writes_invocation_and_planned_events_to_stream() {
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--yes", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = read_all_event_lines(state.path());
    assert!(!events.is_empty(), "events stream should not be empty");

    let names: Vec<&str> = events
        .iter()
        .map(|e| e["EventName"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"norn.invocation.started"),
        "missing started; names: {names:?}"
    );
    assert!(
        names.contains(&"norn.op.planned"),
        "missing op.planned; names: {names:?}"
    );
    assert!(
        names.contains(&"norn.invocation.finished"),
        "missing finished; names: {names:?}"
    );

    let trace = events[0]["TraceId"].as_str().unwrap();
    assert!(
        events.iter().all(|e| e["TraceId"] == trace),
        "all events share one trace id"
    );
}

#[test]
fn dry_run_writes_no_events_to_disk() {
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--dry-run", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        no_event_files(state.path()),
        "dry-run must persist nothing to disk"
    );
}

#[test]
fn move_emits_applied_action_events() {
    // vault: a.md ; c.md contains [[a]] → move a.md b.md rewrites c.md's backlink.
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--yes", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = read_all_event_lines(state.path());

    // The move op itself emits an applied action sharing its op span.
    let mv = events
        .iter()
        .find(|e| e["EventName"] == "norn.action.move_document")
        .expect("move action");
    assert_eq!(mv["Attributes"]["norn.status"], "applied");
    assert_eq!(mv["SeverityNumber"], 9);
    assert!(mv["SpanId"].is_string());
    assert_eq!(mv["Attributes"]["norn.target.to"], "b.md");

    // The cascade backlink rewrite emits an applied rewrite_link action.
    let rw = events
        .iter()
        .find(|e| {
            e["EventName"] == "norn.action.rewrite_link"
                && e["Attributes"]["norn.status"] == "applied"
        })
        .expect("cascade rewrite action");
    assert_eq!(rw["SeverityNumber"], 9);
    assert_eq!(rw["Attributes"]["norn.target"], "c.md");

    // The op action shares its span with the `op_planned` for that op.
    let planned_spans: Vec<_> = events
        .iter()
        .filter(|e| e["EventName"] == "norn.op.planned")
        .map(|e| e["SpanId"].clone())
        .collect();
    assert!(
        planned_spans.contains(&mv["SpanId"]),
        "move action span must match an op.planned span"
    );
}

#[test]
fn dry_run_emits_no_action_events() {
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--dry-run", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // dry-run persists nothing at all (no file), so also no action events on disk.
    assert!(no_event_files(state.path()));
}

/// A read-only backlinker forces every cascade rewrite attempt to fail (EACCES)
/// through all retry rounds, so the settled cascade still has a failure — which
/// must surface as a `norn.action.rewrite_link` (status=failed, ERROR) plus a
/// `norn.retry` event.
#[test]
#[cfg(unix)]
fn move_with_unwritable_backlinker_emits_failed_action_and_retry() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::Builder::new()
        .prefix("norn-telemetry-fail-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("target.md"), "---\ntype: note\n---\n# Target\n").unwrap();
    // linker.md lives in its own subdirectory so its containing directory's
    // permissions can be locked down without also blocking the primary move
    // (target.md -> renamed.md), which needs to create/remove entries in
    // `root` itself.
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(
        sub.join("linker.md"),
        "---\ntype: note\n---\n# Linker\n[[target]]\n",
    )
    .unwrap();

    // (NRN-146) The cascade write now goes through `atomic_write` (temp file +
    // rename), so a read-only linker.md file no longer fails the write —
    // `rename(2)` doesn't consult the replaced file's permission bits, only
    // the containing directory's. Lock down `sub` itself instead, which still
    // blocks creation of the sibling temp file.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Skip when unix perms are not enforced (e.g. running as root).
    let probe_path = sub.join(".rowb-perm-probe");
    let probe_writable = std::fs::write(&probe_path, "x").is_ok();
    let _ = std::fs::remove_file(&probe_path);
    if probe_writable {
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args([
            "move",
            "target.md",
            "renamed.md",
            "--yes",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();

    // Restore perms before assertions so tempdir cleanup always works.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Primary move succeeded → exit 0 (best-effort cascade semantics).
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = read_all_event_lines(state.path());

    let failed = events
        .iter()
        .find(|e| {
            e["EventName"] == "norn.action.rewrite_link"
                && e["Attributes"]["norn.status"] == "failed"
        })
        .expect("failed rewrite_link action");
    assert_eq!(failed["SeverityNumber"], 17);
    assert_eq!(failed["Attributes"]["norn.target"], "sub/linker.md");
    assert_eq!(failed["Attributes"]["norn.reason.code"], "write_failed");
    assert!(
        failed["Attributes"]["norn.reason.message"].is_string(),
        "failed action must carry a reason message"
    );

    let retry = events
        .iter()
        .find(|e| e["EventName"] == "norn.retry")
        .expect("retry event");
    assert_eq!(retry["SeverityNumber"], 13);

    // Keystone equivalence on the FAILED path: the report's move-op
    // cascade.failed must equal the count of failed rewrite_link events under
    // the move op's span (summary == log, not just for the applied path).
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mv_span = events
        .iter()
        .find(|e| e["EventName"] == "norn.action.move_document")
        .unwrap()["SpanId"]
        .as_str()
        .unwrap()
        .to_string();
    let folded_failed = events
        .iter()
        .filter(|e| {
            e["EventName"] == "norn.action.rewrite_link"
                && e["SpanId"].as_str() == Some(mv_span.as_str())
                && e["Attributes"]["norn.status"] == "failed"
        })
        .count();
    assert!(folded_failed >= 1, "fixture must produce a failed rewrite");
    let op = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == "move_document")
        .unwrap();
    assert_eq!(
        op["cascade"]["failed"].as_u64().unwrap() as usize,
        folded_failed,
        "report move cascade.failed must equal failed rewrite_link events under its span"
    );
}

/// Keystone equivalence check (Task 7): the `ApplyReport` produced for a real
/// apply must be a projection (fold) of the on-disk event stream. We re-derive
/// the op tallies and the move op's cascade tally independently from the JSONL
/// stream and assert they equal the report's numbers.
#[test]
fn apply_report_op_tallies_equal_fold_of_on_disk_stream() {
    // vault: a.md ; c.md contains [[a]] → move a.md b.md rewrites c.md's backlink.
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--yes", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let events = read_all_event_lines(state.path());

    // Independently fold applied-op count from the stream: an op is applied iff
    // its op-action event (norn.action.<kind> under the op's span) has
    // status "applied".
    let mut applied_ops = 0usize;
    for e in events
        .iter()
        .filter(|e| e["EventName"] == "norn.op.planned")
    {
        let span = e["SpanId"].as_str().unwrap();
        let kind = e["Attributes"]["norn.op.kind"].as_str().unwrap();
        let op_action = format!("norn.action.{kind}");
        let applied = events.iter().any(|a| {
            a["SpanId"].as_str() == Some(span)
                && a["EventName"].as_str() == Some(op_action.as_str())
                && a["Attributes"]["norn.status"] == "applied"
        });
        if applied {
            applied_ops += 1;
        }
    }
    assert_eq!(
        report["applied"].as_u64().unwrap() as usize,
        applied_ops,
        "report.applied must equal the fold of the on-disk stream"
    );

    // The move op's cascade.applied must equal the count of applied
    // rewrite_link events under the move op's span.
    let mv_span = events
        .iter()
        .find(|e| e["EventName"] == "norn.action.move_document")
        .unwrap()["SpanId"]
        .as_str()
        .unwrap()
        .to_string();
    let cascade_applied = events
        .iter()
        .filter(|e| {
            e["EventName"] == "norn.action.rewrite_link"
                && e["SpanId"].as_str() == Some(mv_span.as_str())
                && e["Attributes"]["norn.status"] == "applied"
        })
        .count();
    let op = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == "move_document")
        .unwrap();
    assert_eq!(
        op["cascade"]["applied"].as_u64().unwrap() as usize,
        cascade_applied,
        "report move cascade.applied must equal applied rewrite_link events under its span"
    );
    assert!(
        cascade_applied >= 1,
        "fixture must produce a real cascade rewrite"
    );
}

// ── Task 8: set/new emit the event stream + carry trace_id in their report ────

/// A minimal single-doc vault for `set`/`new` tests.
fn synth_single() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-telemetry-setnew-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("foo.md"), "---\ntype: note\n---\n# Foo\n").unwrap();
    tmp
}

#[test]
fn set_apply_emits_events_and_report_carries_trace_id() {
    let tmp = synth_single();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "set",
            "foo.md",
            "--field",
            "status=done",
            "--yes",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let trace = report["trace_id"].as_str().unwrap();
    assert_eq!(trace.len(), 32, "trace_id must be a 32-hex trace id");

    let events = read_all_event_lines(state.path());
    assert!(
        events
            .iter()
            .any(|e| e["EventName"] == "norn.invocation.started"),
        "missing started event"
    );
    assert!(
        events
            .iter()
            .any(|e| e["EventName"].as_str().unwrap().starts_with("norn.action.")),
        "missing an action event"
    );
    assert!(
        events.iter().all(|e| e["TraceId"] == trace),
        "report trace_id must match every event in the stream"
    );
}

#[test]
fn set_dry_run_writes_no_events() {
    let tmp = synth_single();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "set",
            "foo.md",
            "--field",
            "status=done",
            "--dry-run",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        no_event_files(state.path()),
        "set dry-run must persist nothing"
    );
}

#[test]
fn new_apply_emits_events_and_report_carries_trace_id() {
    let tmp = synth_single();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "new",
            "notes/x.md",
            "--field",
            "type=note",
            "--parents",
            "--yes",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let trace = report["trace_id"].as_str().unwrap();
    assert_eq!(trace.len(), 32, "trace_id must be a 32-hex trace id");

    let events = read_all_event_lines(state.path());
    assert!(
        events
            .iter()
            .any(|e| e["EventName"] == "norn.invocation.started"),
        "missing started event"
    );
    assert!(
        events
            .iter()
            .any(|e| e["EventName"].as_str().unwrap().starts_with("norn.action.")),
        "missing an action event"
    );
    assert!(
        events.iter().all(|e| e["TraceId"] == trace),
        "report trace_id must match every event in the stream"
    );
}

#[test]
fn new_records_apply_prints_trace_footer() {
    let tmp = synth_single();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["new", "notes/y.md", "--parents", "--yes"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("trace: "),
        "records-format apply must print a `trace:` footer; got: {stdout}"
    );
}

#[test]
fn set_report_schema_version_is_2() {
    let tmp = synth_single();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "set",
            "foo.md",
            "--field",
            "status=done",
            "--yes",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
}

/// Dry-run preservation (Task 7): dry-run has no event stream, so its cascade
/// forecast still comes from the planner/RepairApplyReport path and op status
/// is `not_run`. Locks the dry-run behavior the event-fold must NOT touch.
#[test]
fn dry_run_move_report_keeps_forecast_cascade_and_not_run_status() {
    let tmp = synth();
    let state = TempDir::new().unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--dry-run", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);

    let op = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == "move_document")
        .unwrap();
    assert_eq!(op["status"], "not_run", "dry-run op status must be not_run");
    assert!(
        op["cascade"]["planned"].as_u64().unwrap() >= 1,
        "dry-run must still forecast the backlink cascade"
    );
}
