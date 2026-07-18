//! Mutation telemetry: an OTEL-shaped event stream for vault mutations.
//!
//! Task 1 provides the data model ([`Event`], [`Severity`]), ID/clock seams
//! ([`IdGen`], [`Clock`]), and an in-memory [`EventSink`]. Later tasks add the
//! file-backed sink and wire emits into the command paths.

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
    /// Durable sink; `None` = in-memory only. Wired in a later task.
    writer: Option<Box<dyn Write + Send>>,
    service_version: String,
}

impl EventSink {
    /// In-memory-only sink (dry-run, tests, degraded). Never touches disk.
    pub fn discard(ids: IdGen, clock: Clock) -> Self {
        Self::with_writer(ids, clock, None)
    }

    /// Shared constructor: mints the trace ID and wires the (optional) writer.
    fn with_writer(mut ids: IdGen, clock: Clock, writer: Option<Box<dyn Write + Send>>) -> Self {
        let trace_id = ids.trace_id();
        Self {
            trace_id,
            ids,
            clock,
            events: Vec::new(),
            writer,
            service_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// File-backed sink. Best-effort: if the dir can't be created or the daily
    /// file can't be opened, emits one stderr warning and returns an in-memory
    /// (`discard`-style) sink instead — NEVER returns `Err` for an IO problem.
    ///
    /// `start_ts` is the invocation start timestamp (RFC-3339 UTC); it selects
    /// the daily file via [`store::daily_file_name`].
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
            return Ok(Self::discard(ids, clock));
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
                Ok(Self::discard(ids, clock))
            }
        }
    }

    /// The trace ID shared by every event in this invocation.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
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
            if writeln!(w, "{line}").and_then(|_| w.flush()).is_err() {
                eprintln!("warning: mutation telemetry write failed; continuing without the log");
                self.writer = None;
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
}
