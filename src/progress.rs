//! Work-evidenced progress ticks for long-running writer-queue liveness ops
//! (NRN-465).
//!
//! A routed CLI mutation waits on the warm daemon's per-vault writer thread while
//! it runs one indivisible liveness op — a freshness refresh (which on a changed
//! vault reparses every document) or a generation open. The client-side stall
//! watchdog ([`crate::service`]) declares the daemon wedged when a BUSY writer's
//! opaque progress sequence stops advancing for the stall budget. Op boundaries
//! alone advance that sequence, so a multi-second whole-vault reparse froze it and
//! tripped a FALSE stall — the client gave up post-send-uncertain even though the
//! daemon was healthy and applied the write moments later.
//!
//! A [`ProgressReporter`] is the cheap hook threaded from the writer-queue op
//! context into that long work. Each [`ProgressReporter::tick`] advances the
//! sequence exactly as a bulk-chunk boundary does. Every tick is EVIDENCE OF REAL
//! WORK — a batch of files parsed / hashed / staged, a lock-acquire retry
//! attempted — never timer-driven: a genuinely wedged thread stops ticking, so the
//! watchdog still catches the exact failure it exists for. Direct (non-daemon)
//! paths pass [`ProgressReporter::none`], whose `tick` is a no-op, so their
//! behavior is unchanged.

/// How many units of work (files parsed / hashed / staged) between ticks. A tick
/// is one mutex lock + increment on the shared writer-progress state, so batching
/// keeps the per-file cost sub-microsecond over a multi-thousand-file reparse while
/// still advancing the sequence many times a second.
pub(crate) const PROGRESS_TICK_FILES: usize = 64;

/// A cheap, optional progress hook handed to a long-running liveness op. `Copy`,
/// so it threads by value through the cache write paths without ceremony.
#[derive(Clone, Copy)]
pub(crate) struct ProgressReporter<'a> {
    tick: Option<&'a dyn Fn()>,
}

impl<'a> ProgressReporter<'a> {
    /// The no-op reporter used on every direct (non-daemon) path — its [`tick`]
    /// does nothing, so instrumented loops behave identically to before.
    ///
    /// [`tick`]: ProgressReporter::tick
    pub(crate) fn none() -> Self {
        Self { tick: None }
    }

    /// Wrap a tick callback — in production the warm daemon's writer-queue
    /// sequence advance (see [`crate::mcp::writer_queue`]).
    pub(crate) fn new(tick: &'a dyn Fn()) -> Self {
        Self { tick: Some(tick) }
    }

    /// Advance the progress sequence once, iff a callback is wired.
    #[inline]
    pub(crate) fn tick(&self) {
        if let Some(tick) = self.tick {
            tick();
        }
    }
}

/// Batches [`ProgressReporter`] ticks over a hot per-file loop: advances the
/// sequence once per [`PROGRESS_TICK_FILES`] units of work rather than on every
/// iteration. The FIRST recorded unit ticks immediately, so a short stage (fewer
/// than [`PROGRESS_TICK_FILES`] files) still emits early evidence that the op has
/// begun real work rather than staying silent until a full batch accrues.
pub(crate) struct BatchProgress<'a> {
    reporter: ProgressReporter<'a>,
    seen: usize,
}

impl<'a> BatchProgress<'a> {
    pub(crate) fn new(reporter: ProgressReporter<'a>) -> Self {
        Self { reporter, seen: 0 }
    }

    /// Record one unit of completed work. Ticks on the first unit and then once
    /// per [`PROGRESS_TICK_FILES`] units thereafter.
    #[inline]
    pub(crate) fn record(&mut self) {
        // Tick when `seen` is 0, PROGRESS_TICK_FILES, 2*…, etc. — i.e. the first
        // record and every Nth after it.
        if self.seen.is_multiple_of(PROGRESS_TICK_FILES) {
            self.reporter.tick();
        }
        self.seen += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn count_ticks(records: usize) -> usize {
        let ticks = Cell::new(0usize);
        let tick = || ticks.set(ticks.get() + 1);
        let reporter = ProgressReporter::new(&tick);
        let mut batch = BatchProgress::new(reporter);
        for _ in 0..records {
            batch.record();
        }
        ticks.get()
    }

    /// A short stage (fewer than one batch) still emits ONE tick — the first
    /// record — so a small op is not silent until a full batch accrues (NRN-465
    /// review F2).
    #[test]
    fn first_record_ticks_immediately_for_a_short_stage() {
        assert_eq!(count_ticks(1), 1);
        assert_eq!(count_ticks(PROGRESS_TICK_FILES - 1), 1);
    }

    /// Zero recorded work emits zero ticks.
    #[test]
    fn no_records_no_ticks() {
        assert_eq!(count_ticks(0), 0);
    }

    /// Ticks fire on the first record and then every `PROGRESS_TICK_FILES` after:
    /// records at index 0, N, 2N, … With `K` records that is `1 + (K-1)/N` ticks.
    #[test]
    fn ticks_on_first_then_every_batch() {
        let n = PROGRESS_TICK_FILES;
        assert_eq!(count_ticks(n), 1, "records 0..N-1 tick only at index 0");
        assert_eq!(
            count_ticks(n + 1),
            2,
            "the N-th record (index N) ticks again"
        );
        assert_eq!(count_ticks(2 * n), 2, "indices 0 and N tick");
        assert_eq!(count_ticks(2 * n + 1), 3, "indices 0, N, 2N tick");
    }

    /// The no-op reporter never panics and never counts.
    #[test]
    fn none_reporter_is_inert() {
        let mut batch = BatchProgress::new(ProgressReporter::none());
        for _ in 0..(PROGRESS_TICK_FILES * 2) {
            batch.record();
        }
        // Nothing to assert beyond "did not panic"; the no-op path is a no-op.
    }
}
