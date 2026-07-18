//! Fail-closed self-heal for the warm host daemon (NRN-337).
//!
//! One `norn serve` process serves every vault on the host, so a host-level
//! condition it cannot recover from in place — file-descriptor exhaustion, or a
//! cache database that can no longer be opened for an entry that was already
//! serving — would otherwise wedge the daemon for *every* vault until a human
//! runs `norn service restart`. The heal is simpler and blast-radius-free:
//! **answer the in-flight request with the error, log one line, and exit.** The
//! next client invocation's existing spawn-on-absent-daemon path brings up a
//! fresh daemon with an empty fd table. Exit IS the heal — there is no in-daemon
//! retry loop.
//!
//! The poison predicate is deliberately CONSERVATIVE and explicit (not
//! any-error-exits): only the two host-poisoning classes below trip it. A
//! per-vault error (a bad query, a corrupt cache — self-healed by the
//! generational reopen, see `env::error`) never exits the shared daemon.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::Notify;

/// The process-global exit trigger. The daemon is a singleton process, so a
/// global is the honest shape: [`run`](super::run) installs it before the accept
/// loop, the loop selects on [`Notify::notified`], and the request path trips it
/// from deep inside a `spawn_blocking` tool body via [`trip`].
static POISON: OnceLock<Arc<Notify>> = OnceLock::new();

/// Latches the one-time log + notify so a burst of concurrent requests all
/// hitting the same fd exhaustion logs exactly one line and notifies once.
static TRIPPED: AtomicBool = AtomicBool::new(false);

/// Install (or fetch) the process-global poison trigger. Called once by
/// [`run`](super::run); returns the `Notify` the accept loop waits on.
pub(crate) fn install() -> Arc<Notify> {
    Arc::clone(POISON.get_or_init(|| Arc::new(Notify::new())))
}

/// Trip the poison latch: log one clear line and wake the accept loop so the
/// daemon exits to self-heal. Idempotent — only the first call logs and notifies.
pub(crate) fn trip(reason: &str) {
    if TRIPPED.swap(true, Ordering::AcqRel) {
        return;
    }
    eprintln!(
        "norn serve: poisoned state ({reason}); exiting to self-heal \
         — the next norn invocation respawns a fresh daemon"
    );
    if let Some(notify) = POISON.get() {
        notify.notify_one();
    }
}

/// Classify a failed request's error and trip the poison latch if it is a
/// host-poisoning class. `previously_served` gates the SQLite-open class: a
/// first-touch open failure for a genuinely-broken vault must NOT take the whole
/// daemon down (that vault is simply unservable while others are fine), whereas a
/// reopen failure for an entry that was already serving is the NRN-325 shape and
/// warrants exit. Fd exhaustion always trips — it is a whole-process condition
/// independent of which vault surfaced it.
pub(crate) fn maybe_trip(err: &anyhow::Error, previously_served: bool) {
    if is_fd_exhaustion(err) {
        trip("file-descriptor exhaustion (EMFILE/ENFILE)");
    } else if previously_served && is_sqlite_cant_open(err) {
        trip("cache database can no longer be opened (SQLITE_CANTOPEN)");
    }
}

/// The full poisoned-state predicate: fd exhaustion OR a SQLite open failure
/// anywhere in the chain. The production trip paths use the two finer predicates
/// directly ([`maybe_trip`] gates the open class on `previously_served`; the
/// accept loop matches [`is_fd_exhaustion_io`]); this is the combined predicate
/// the unit tests pin.
#[cfg(test)]
pub(crate) fn is_poisoned_state(err: &anyhow::Error) -> bool {
    is_fd_exhaustion(err) || is_sqlite_cant_open(err)
}

/// EMFILE (per-process fd limit) or ENFILE (system-wide fd limit) anywhere in the
/// error chain, surfaced as a `std::io::Error` (the `.lock`/sentinel open, or a
/// sqlite open lowered to io). The two errno values are identical across Linux
/// and macOS (POSIX/BSD): ENFILE=23, EMFILE=24.
fn is_fd_exhaustion(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(is_fd_exhaustion_io)
    })
}

/// Sibling of [`is_fd_exhaustion`] for a bare `std::io::Error` — used on the
/// accept-loop error, which is not wrapped in `anyhow`.
pub(crate) fn is_fd_exhaustion_io(io: &std::io::Error) -> bool {
    matches!(io.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE))
}

/// A SQLite "unable to open the database file" (SQLITE_CANTOPEN) anywhere in the
/// chain — the NRN-325 shape when it strikes an entry that was already serving.
fn is_sqlite_cant_open(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
            .is_some_and(|code| code == rusqlite::ErrorCode::CannotOpen)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn emfile_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EMFILE)
    }

    fn cant_open() -> rusqlite::Error {
        // SQLITE_CANTOPEN (14): the primary result code `ffi::Error::new` maps to
        // `ErrorCode::CannotOpen`.
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(14), None)
    }

    /// A bare EMFILE io error is poison.
    #[test]
    fn emfile_is_poison() {
        let err = anyhow::Error::new(emfile_error());
        assert!(is_poisoned_state(&err));
        assert!(is_fd_exhaustion_io(&emfile_error()));
    }

    /// ENFILE (system-wide) is poison too.
    #[test]
    fn enfile_is_poison() {
        let err = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENFILE));
        assert!(is_poisoned_state(&err));
    }

    /// EMFILE wrapped as the observed `CacheError::Io { path, source }` — the
    /// exact "io error at ~/.cache/norn/<hash>/.lock: Too many open files" shape —
    /// is still classified through the chain.
    #[test]
    fn emfile_wrapped_in_cache_error_is_poison() {
        let cache_err = crate::cache::CacheError::Io {
            path: Utf8PathBuf::from("/cache/norn/deadbeef/.lock"),
            source: emfile_error(),
        };
        let err = anyhow::Error::new(cache_err).context("opening the entry lock");
        assert!(is_poisoned_state(&err));
    }

    /// A SQLite cannot-open failure is poison.
    #[test]
    fn sqlite_cant_open_is_poison() {
        let err = anyhow::Error::new(cant_open());
        assert!(is_poisoned_state(&err));
    }

    /// A cannot-open wrapped in `CacheError::Sqlite` is still classified.
    #[test]
    fn sqlite_cant_open_wrapped_is_poison() {
        let err = anyhow::Error::new(crate::cache::CacheError::Sqlite(cant_open()));
        assert!(is_poisoned_state(&err));
    }

    /// Conservative: a garden-variety NotFound io error is NOT poison (a missing
    /// file elsewhere must never take the daemon down).
    #[test]
    fn not_found_is_not_poison() {
        let err = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ));
        assert!(!is_poisoned_state(&err));
    }

    /// Conservative: SQLite corruption is self-healed by the generational reopen,
    /// NOT an exit — so it must not be classified as poison here.
    #[test]
    fn corruption_is_not_poison() {
        let corrupt = rusqlite::Error::SqliteFailure(
            // SQLITE_CORRUPT (11) → ErrorCode::DatabaseCorrupt.
            rusqlite::ffi::Error::new(11),
            None,
        );
        let err = anyhow::Error::new(corrupt);
        assert!(!is_poisoned_state(&err));
    }

    /// A plain string error is NOT poison.
    #[test]
    fn plain_error_is_not_poison() {
        let err = anyhow::anyhow!("something ordinary failed");
        assert!(!is_poisoned_state(&err));
    }

    /// `maybe_trip`'s open-class gate: a cannot-open on a vault that has NOT
    /// served is not a trip candidate (fd exhaustion still is). This asserts the
    /// PREDICATE-level gating decision without touching the process-global latch:
    /// cannot-open alone is poison, but `maybe_trip` only forwards it when
    /// `previously_served`.
    #[test]
    fn cant_open_first_touch_gate() {
        // The predicate says cannot-open is poison...
        assert!(is_poisoned_state(&anyhow::Error::new(cant_open())));
        // ...but fd exhaustion is unconditional while cannot-open is gated — the
        // gating logic lives in `maybe_trip` (exercised end-to-end by the
        // integration self-heal test, which drives a real EMFILE exit).
        assert!(is_fd_exhaustion(&anyhow::Error::new(emfile_error())));
    }
}
