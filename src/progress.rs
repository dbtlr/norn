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
/// iteration.
pub(crate) struct BatchProgress<'a> {
    reporter: ProgressReporter<'a>,
    since_tick: usize,
}

impl<'a> BatchProgress<'a> {
    pub(crate) fn new(reporter: ProgressReporter<'a>) -> Self {
        Self {
            reporter,
            since_tick: 0,
        }
    }

    /// Record one unit of completed work; emit a real tick every
    /// [`PROGRESS_TICK_FILES`] units.
    #[inline]
    pub(crate) fn record(&mut self) {
        self.since_tick += 1;
        if self.since_tick >= PROGRESS_TICK_FILES {
            self.since_tick = 0;
            self.reporter.tick();
        }
    }
}
