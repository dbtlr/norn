//! Mutation telemetry: an OTEL-shaped event stream for vault mutations.
//!
//! This module provides the data model ([`Event`], [`Severity`]), the ID/clock
//! seams ([`IdGen`], [`Clock`]), and the in-memory [`EventSink`] the mutation
//! executor emits through and reads back to fold an [`norn_wire::ApplyReport`].
//!
//! # Two halves (ADR 0018)
//!
//! The executor needs the event STREAM in memory: it emits `op_planned` /
//! per-action / retry events and then folds them back into cascade summaries and
//! per-op statuses — the in-memory stream, always retained in
//! [`EventSink::events`] regardless of the durable writer state.
//!
//! The DURABLE side (NRN-400) sits behind the value-in `writer` seam: for a
//! REGISTERED vault the owner attaches a daily-file JSONL sink via
//! [`EventSink::open`] (the `store` submodule names the file and owns
//! retention), so every CONFIRMED mutation appends one OTEL Logs object per
//! line; an ephemeral (unregistered) tier keeps the in-memory
//! [`EventSink::discard`]. The `read` submodule is the mirror image — the
//! `norn audit` read verb parses those lines back into
//! [`norn_wire::AuditEvent`] rows. norn-core stays root-free: the events dir
//! arrives as a value.

pub mod event;
pub mod ids;
pub mod read;
pub mod store;

pub use event::{Event, Severity};
pub use ids::{Clock, IdGen};

use std::io::Write;

/// Records telemetry events for one invocation under a shared trace ID.
///
/// The optional `writer` is the durable JSONL sink; when `None` the sink is
/// purely in-memory (dry-run, tests, or after a degraded write failure). All
/// events are always retained in `events` regardless of the writer state, so
/// callers can fold an `ApplyReport` from the same stream.
pub struct EventSink {
    trace_id: String,
    ids: IdGen,
    clock: Clock,
    events: Vec<Event>,
    /// Durable sink; `None` = in-memory only. The owner injects one when it wants
    /// the mutation stream persisted; norn-core never opens a file itself.
    writer: Option<Box<dyn Write + Send>>,
    service_version: String,
    /// Set when a durable writer was ATTEMPTED (a registered vault) but failed —
    /// an `open()` dir-create/file-open failure, or a later `emit()` write
    /// failure — never for a by-design in-memory sink ([`Self::discard`] called
    /// directly for a forecast or an unregistered vault, where no durable
    /// attempt was ever made). See [`degraded`](Self::degraded).
    degraded: bool,
}

impl EventSink {
    /// In-memory-only sink (dry-run, tests, an unregistered vault). Never
    /// touches disk, and never counts as [`degraded`](Self::degraded) — there
    /// was no durable attempt to fail.
    pub fn discard(ids: IdGen, clock: Clock) -> Self {
        Self::with_writer(ids, clock, None)
    }

    /// Shared constructor: mints the trace ID and wires the (optional) durable
    /// writer. The owner passes `Some(writer)` to persist the stream; norn-core
    /// stays I/O-free and never opens the file itself (value-in seam).
    pub fn with_writer(
        mut ids: IdGen,
        clock: Clock,
        writer: Option<Box<dyn Write + Send>>,
    ) -> Self {
        let trace_id = ids.trace_id();
        Self {
            trace_id,
            ids,
            clock,
            events: Vec::new(),
            writer,
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            degraded: false,
        }
    }

    /// File-backed sink for a registered vault's durable telemetry. Best-effort:
    /// if the events dir can't be created or the daily file can't be opened, one
    /// stderr warning is emitted and an in-memory ([`Self::discard`]-style) sink
    /// is returned instead — this NEVER returns `Err` for an IO problem, so a
    /// telemetry hiccup can never fail a mutation. The returned sink's
    /// [`degraded`](Self::degraded) flag is set in that fallback case, so the
    /// caller can surface the loss to the operator instead of silently minting
    /// a trace id that correlates to nothing.
    ///
    /// `start_ts` is the invocation start timestamp (RFC-3339 UTC); it selects
    /// the daily file via [`store::daily_file_name`]. The `events_dir` arrives as
    /// a value (the owner resolved it from the vault's state/logs home).
    pub fn open(
        events_dir: &camino::Utf8Path,
        start_ts: String,
        ids: IdGen,
        clock: Clock,
    ) -> std::io::Result<Self> {
        if let Err(e) = std::fs::create_dir_all(events_dir.as_std_path()) {
            eprintln!(
                "warning: could not create telemetry events dir {events_dir}: {e}; \
                 continuing without the log"
            );
            let mut sink = Self::discard(ids, clock);
            sink.degraded = true;
            return Ok(sink);
        }
        let file_path = events_dir.join(store::daily_file_name(&start_ts));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path.as_std_path())
        {
            Ok(file) => Ok(Self::with_writer(ids, clock, Some(Box::new(file)))),
            Err(e) => {
                eprintln!(
                    "warning: could not open telemetry events file {file_path}: {e}; \
                     continuing without the log"
                );
                let mut sink = Self::discard(ids, clock);
                sink.degraded = true;
                Ok(sink)
            }
        }
    }

    /// The trace ID shared by every event in this invocation.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Whether the durable writer degraded: it was attempted (a registered
    /// vault) but either failed to open ([`open`](Self::open)'s fallback) or
    /// failed mid-write (`emit`'s fallback). `false` for a by-design in-memory
    /// sink (forecast, or an unregistered vault) — there, no durable write was
    /// ever attempted, so there is nothing to have degraded FROM.
    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// All events recorded so far, in emit order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Emit a lifecycle (span-less) event.
    pub fn lifecycle(
        &mut self,
        name: &str,
        sev: Severity,
        body: impl Into<String>,
        attrs: Vec<(&'static str, String)>,
    ) {
        self.emit(None, name, sev, body, attrs)
    }

    /// Begin an op: emit `op_planned` and return the new SpanId for actions to
    /// reference.
    pub fn start_op(&mut self, kind: &str, target: &str, from: Option<usize>) -> String {
        let span = self.ids.span_id();
        let mut attrs = vec![
            (event::ATTR_OP_KIND, kind.to_string()),
            (event::ATTR_TARGET, target.to_string()),
        ];
        if let Some(f) = from {
            attrs.push((event::ATTR_OP_FROM, f.to_string()));
        }
        self.emit(
            Some(span.clone()),
            event::EVENT_OP_PLANNED,
            Severity::Info,
            format!("planned {kind} on {target}"),
            attrs,
        );
        span
    }

    /// Emit an action event under an existing op span.
    pub fn action(
        &mut self,
        span: &str,
        sev: Severity,
        name: &str,
        body: impl Into<String>,
        attrs: Vec<(&'static str, String)>,
    ) {
        self.emit(Some(span.to_string()), name, sev, body, attrs)
    }

    fn emit(
        &mut self,
        span: Option<String>,
        name: &str,
        sev: Severity,
        body: impl Into<String>,
        attrs: Vec<(&'static str, String)>,
    ) {
        let ev = Event {
            trace_id: self.trace_id.clone(),
            span_id: span,
            severity: sev,
            name: name.to_string(),
            body: body.into(),
            attributes: attrs,
            timestamp: self.clock.now_rfc3339(),
        };
        if let Some(w) = self.writer.as_mut() {
            let line = ev.to_json(&self.service_version).to_string();
            // Two owners on DIFFERENT builds (a mid-upgrade window) can append to
            // the same daily file concurrently; a line longer than PIPE_BUF is not
            // guaranteed atomic and the two writers' bytes can interleave into one
            // torn line — the reader (`telemetry::read`) skips any line it cannot
            // parse rather than failing the whole read. Tracked: NRN-464.
            if writeln!(w, "{line}").and_then(|_| w.flush()).is_err() {
                eprintln!("warning: mutation telemetry write failed; continuing without the log");
                self.writer = None;
                self.degraded = true;
            }
        }
        self.events.push(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_records_events_in_memory() {
        let mut sink = EventSink::discard(
            IdGen::with_seed(7),
            Clock::fixed("2026-05-29T00:00:00.000Z"),
        );
        let span = sink.start_op("move_document", "a.md", None);
        sink.action(
            &span,
            Severity::Info,
            "norn.action.move_document",
            "moved a.md → b.md",
            vec![("norn.status", "applied".into())],
        );
        assert_eq!(sink.events().len(), 2);
        assert_eq!(sink.events()[1].name, "norn.action.move_document");
        assert_eq!(sink.events()[0].name, "norn.op.planned");
        assert_eq!(sink.trace_id().len(), 32);
    }

    #[test]
    fn with_writer_persists_and_retains_in_memory() {
        // The durable-writer seam: events flow to the injected sink AND stay in
        // memory for the report fold.
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = EventSink::with_writer(
            IdGen::with_seed(3),
            Clock::fixed("2026-05-29T00:00:00.000Z"),
            Some(Box::new(SharedBuf(std::sync::Arc::clone(&buffer)))),
        );
        sink.lifecycle("norn.retry", Severity::Warn, "retried", vec![]);
        assert_eq!(sink.events().len(), 1);
        let written = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(written.contains("norn.retry"));
        assert!(written.contains("\"SeverityText\":\"WARN\""));
    }
}
