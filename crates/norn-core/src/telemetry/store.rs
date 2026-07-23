//! File-backed telemetry storage helpers.
//!
//! Names the per-day JSONL file for an invocation and owns the retention
//! policy over the events dir. The file-backed
//! [`EventSink::open`](super::EventSink::open) constructor lives in the parent
//! module; the daily-name policy and the prune / size-cap guards live here so
//! the writer and the retention sweep share one filename contract.
//!
//! norn-core stays root-free: the events dir arrives as a VALUE (the owner
//! resolves it from the vault's state/logs home and hands it in), so nothing
//! here reads XDG or the ambient environment.

/// Daily JSONL filename from an invocation start timestamp (RFC-3339 UTC).
/// Uses the `YYYY-MM-DD` prefix — the file is chosen once per invocation.
pub fn daily_file_name(start_rfc3339: &str) -> String {
    format!("events-{}.jsonl", &start_rfc3339[..10])
}

/// Parse the `YYYY-MM-DD` date from an `events-YYYY-MM-DD.jsonl` file name.
/// Returns `None` for any name that does not match the exact pattern.
fn event_file_date(name: &str) -> Option<chrono::NaiveDate> {
    let date = name.strip_prefix("events-")?.strip_suffix(".jsonl")?;
    // Reject anything not exactly `\d{4}-\d{2}-\d{2}` by parsing strictly.
    if date.len() != 10 {
        return None;
    }
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Default retention for daily event files: 90 days. The store is an audit
/// trail, so the window is generous; the size cap is the hard ceiling.
pub const DEFAULT_RETENTION: std::time::Duration = std::time::Duration::from_secs(90 * 86_400);

/// Unlink event files older than `retention` (by their filename date), relative
/// to `today` (a `YYYY-MM-DD` string). The file for `today` is NEVER removed.
/// Best-effort: per-file IO errors and unparseable names are skipped silently.
pub fn prune_events(dir: &camino::Utf8Path, retention: std::time::Duration, today: &str) {
    let Ok(today_date) = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d") else {
        return;
    };
    let days = (retention.as_secs() / 86_400) as i64;
    let cutoff = today_date - chrono::Duration::days(days);

    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == daily_file_name(today) {
            continue;
        }
        let Some(date) = event_file_date(name) else {
            continue;
        };
        if date < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Internal non-configurable total-size guardrail for the events dir (256 MiB).
pub const EVENTS_SIZE_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Internal guardrail: if total size of event files exceeds `cap_bytes`, unlink
/// oldest files (by filename date) until under the cap. Never unlinks `today`.
/// Emits ONE stderr warning if anything was dropped. Best-effort.
pub fn enforce_size_cap(dir: &camino::Utf8Path, cap_bytes: u64, today: &str) {
    let today_name = daily_file_name(today);

    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return;
    };
    // Collect (date, path, size) for matching event files.
    let mut files: Vec<(chrono::NaiveDate, std::path::PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = event_file_date(name) else {
            continue;
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((date, entry.path(), size));
    }

    let mut total: u64 = files.iter().map(|(_, _, s)| *s).sum();
    if total <= cap_bytes {
        return;
    }

    // Oldest first.
    files.sort_by_key(|(date, _, _)| *date);

    let mut dropped = 0u64;
    for (_, path, size) in &files {
        if total <= cap_bytes {
            break;
        }
        // Never drop today's file.
        if path.file_name().and_then(|n| n.to_str()) == Some(today_name.as_str()) {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*size);
            dropped += 1;
        }
    }

    if dropped > 0 {
        eprintln!(
            "warning: mutation event log exceeded {cap_bytes} bytes; dropped {dropped} old daily file(s)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{daily_file_name, enforce_size_cap, prune_events};
    use crate::telemetry::{Clock, EventSink, IdGen, Severity};
    use std::time::Duration;

    #[test]
    fn prune_unlinks_files_older_than_retention_keeps_recent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        for name in [
            "events-2026-01-01.jsonl",
            "events-2026-05-01.jsonl",
            "events-2026-05-29.jsonl",
        ] {
            std::fs::write(tmp.path().join(name), b"{}\n").unwrap();
        }
        prune_events(dir, Duration::from_secs(30 * 86_400), "2026-05-29");
        assert!(
            !tmp.path().join("events-2026-01-01.jsonl").exists(),
            "Jan file pruned"
        );
        assert!(
            tmp.path().join("events-2026-05-01.jsonl").exists(),
            "May 1 within 30d kept"
        );
        assert!(
            tmp.path().join("events-2026-05-29.jsonl").exists(),
            "today kept"
        );
    }

    #[test]
    fn prune_never_removes_todays_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("events-2026-05-29.jsonl"), b"{}\n").unwrap();
        prune_events(dir, Duration::from_secs(0), "2026-05-29"); // retention 0 = all stale
        assert!(
            tmp.path().join("events-2026-05-29.jsonl").exists(),
            "today never pruned"
        );
    }

    #[test]
    fn prune_ignores_non_event_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("README.txt"), b"x").unwrap();
        std::fs::write(tmp.path().join("events-2026-01-01.jsonl"), b"{}\n").unwrap();
        prune_events(dir, Duration::from_secs(30 * 86_400), "2026-05-29");
        assert!(
            tmp.path().join("README.txt").exists(),
            "non-event files untouched"
        );
        assert!(!tmp.path().join("events-2026-01-01.jsonl").exists());
    }

    #[test]
    fn size_cap_drops_oldest_until_under_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        for name in [
            "events-2026-05-01.jsonl",
            "events-2026-05-02.jsonl",
            "events-2026-05-03.jsonl",
        ] {
            std::fs::write(tmp.path().join(name), b"0123456789").unwrap(); // 10 bytes each = 30 total
        }
        enforce_size_cap(dir, 25, "2026-05-03"); // cap 25 < 30 → drop oldest
        assert!(
            !tmp.path().join("events-2026-05-01.jsonl").exists(),
            "oldest dropped"
        );
        assert!(
            tmp.path().join("events-2026-05-03.jsonl").exists(),
            "newest/today kept"
        );
        // total now 20 <= 25
    }

    #[test]
    fn size_cap_noop_when_under_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("events-2026-05-03.jsonl"), b"0123456789").unwrap();
        enforce_size_cap(dir, 1_000, "2026-05-03");
        assert!(tmp.path().join("events-2026-05-03.jsonl").exists());
    }

    #[test]
    fn daily_file_name_uses_date_prefix() {
        assert_eq!(
            daily_file_name("2026-05-29T23:59:59.999Z"),
            "events-2026-05-29.jsonl"
        );
    }

    #[test]
    fn open_creates_daily_file_and_appends_flushed_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let mut sink = EventSink::open(
            dir,
            "2026-05-29T10:00:00.000Z".to_string(),
            IdGen::with_seed(3),
            Clock::fixed("2026-05-29T10:00:00.000Z"),
        )
        .unwrap();
        sink.lifecycle(
            crate::telemetry::event::EVENT_INVOCATION_STARTED,
            Severity::Info,
            "started",
            vec![],
        );
        drop(sink);
        let f = tmp.path().join("events-2026-05-29.jsonl");
        let body = std::fs::read_to_string(f).unwrap();
        assert_eq!(body.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(v["EventName"], "norn.invocation.started");
    }

    #[test]
    fn open_on_unwritable_dir_falls_back_to_in_memory() {
        // A regular file where a directory is expected → create_dir_all fails.
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, b"x").unwrap();
        let bogus = camino::Utf8Path::from_path(&file_path).unwrap();
        let sink = EventSink::open(
            bogus,
            "2026-05-29T10:00:00.000Z".to_string(),
            IdGen::with_seed(3),
            Clock::fixed("2026-05-29T10:00:00.000Z"),
        );
        assert!(
            sink.is_ok(),
            "best-effort: open never returns Err for an IO problem"
        );
        let mut s = sink.unwrap();
        // Degraded sink writes nothing to disk but still records in memory.
        s.lifecycle(
            crate::telemetry::event::EVENT_INVOCATION_STARTED,
            Severity::Info,
            "x",
            vec![],
        );
        assert_eq!(s.events().len(), 1);
    }
}
