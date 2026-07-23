//! The records-composition sink (NRN-370).
//!
//! A [`Sink`] owns the stdout writer, the resolved [`Palette`], and the terminal
//! width for one render, and exposes the full styled line vocabulary the
//! renderers compose — the record-block primitives (`count_line`,
//! `record_block`, `separator`), the report primitives (`status_headline`,
//! `severity_tally`, `tally_group`, `change_line`), and the shared mutation
//! shapes (`trace_footer`, the two warnings blocks) — as methods over
//! `output/primitives`. Styled rendering composes ONLY these methods, so it
//! structurally cannot emit an unstyled block — the palette is threaded through
//! the sink, never re-resolved at the call site (ADR 0021).
//!
//! The non-record formats (`paths` / `json` / `jsonl` / `markdown`, and the
//! bespoke `count` / `describe` text) are raw payload; they write through
//! [`Sink::writer`] rather than the record primitives.

use std::io::{self, Write};

use crate::output::palette::Palette;
use crate::output::primitives;

/// The record-block field row, re-exported so renderers compose blocks without
/// importing the `output::primitives` module directly (the render stream guard
/// forbids that path).
pub use crate::output::primitives::Field;

/// The stdout sink a renderer composes a report into, carrying the styling and
/// width the record primitives need. Built once per render inside the emit path.
pub struct Sink<'a> {
    out: &'a mut dyn Write,
    palette: &'a Palette,
    width: usize,
}

impl<'a> Sink<'a> {
    /// Build a sink over a stdout writer, the resolved palette, and the terminal
    /// width.
    pub fn new(out: &'a mut dyn Write, palette: &'a Palette, width: usize) -> Self {
        Self {
            out,
            palette,
            width,
        }
    }

    /// The resolved palette — for the few dim edge lines (a records placeholder)
    /// that are not themselves a primitive.
    pub fn palette(&self) -> &Palette {
        self.palette
    }

    /// The raw stdout writer, for the non-record payload formats.
    pub fn writer(&mut self) -> &mut dyn Write {
        self.out
    }

    /// The terminal width this sink wraps to — for the few renderers that need
    /// the resolved width for their own layout math.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The status-headline primitive: `{text}…` (or `...` in ASCII mode), the
    /// whole line dimmed. The `validate` / `repair` report headers.
    pub fn status_headline(&mut self, text: &str, ascii: bool) -> io::Result<()> {
        primitives::status_headline(self.out, self.palette, text, ascii)
    }

    /// The severity-tally primitive: the pass / warn / err block, zero rows
    /// elided. The `validate --summary` and clean-run tallies.
    pub fn severity_tally(
        &mut self,
        pass: usize,
        warn: usize,
        err: usize,
        noun: &str,
    ) -> io::Result<()> {
        primitives::severity_tally(self.out, self.palette, pass, warn, err, noun)
    }

    /// The tally-group primitive: an optional section header then aligned
    /// `label ···· count` rows, wrapped to the sink's width. The `validate`
    /// by-code group and the `repair` report tallies.
    pub fn tally_group(
        &mut self,
        header: &str,
        rows: &[(&str, usize)],
        ascii: bool,
    ) -> io::Result<()> {
        primitives::tally_group(self.out, self.palette, header, rows, self.width, ascii)
    }

    /// The change-line primitive: `  {label}: {before}` plus ` → {after}` when
    /// an `after` value is given. The `set` mutation report's per-field lines.
    pub fn change_line(
        &mut self,
        label: &str,
        before: &str,
        after: Option<&str>,
        ascii: bool,
    ) -> io::Result<()> {
        primitives::change_line(self.out, self.palette, label, before, after, ascii)
    }

    /// The applied-mutation `trace:` footer, shared by every mutation verb
    /// (`set` / `new` / `edit` / `move` / `delete` / `rewrite-wikilink` /
    /// `apply`). One line, unstyled, so the seven verbs cannot drift.
    pub fn trace_footer(&mut self, trace_id: &str) -> io::Result<()> {
        writeln!(self.out, "trace: {trace_id}")
    }

    /// The `set`-style warnings block on stdout: `  warnings: N`, then `    - <s>`
    /// for the first three, then `    … (K more)`. `shorts` are the already
    /// per-code-shortened messages; empty writes nothing.
    pub fn mutation_warnings_block(&mut self, shorts: &[String]) -> io::Result<()> {
        if shorts.is_empty() {
            return Ok(());
        }
        writeln!(self.out, "  warnings: {}", shorts.len())?;
        for s in shorts.iter().take(3) {
            writeln!(self.out, "    - {s}")?;
        }
        if shorts.len() > 3 {
            writeln!(self.out, "    … ({} more)", shorts.len() - 3)?;
        }
        Ok(())
    }

    /// The `new`-style aligned warnings block on stdout: the `warnings` cell in
    /// the 9-wide label column carries the first message (or `none`), with the
    /// rest indented under the value column. `shorts` are the per-code-shortened
    /// messages.
    pub fn mutation_warnings_aligned(&mut self, shorts: &[String]) -> io::Result<()> {
        if shorts.is_empty() {
            return writeln!(self.out, "{:<9}  none", "warnings");
        }
        for (i, s) in shorts.iter().enumerate() {
            if i == 0 {
                writeln!(self.out, "{:<9}  {s}", "warnings")?;
            } else {
                writeln!(self.out, "           {s}")?;
            }
        }
        Ok(())
    }

    /// The count line primitive: `"{total} {noun}"` plus the shown-window suffix.
    pub fn count_line(
        &mut self,
        total: usize,
        returned: usize,
        starts_at: usize,
        noun: &str,
    ) -> io::Result<()> {
        primitives::count_line(self.out, self.palette, total, returned, starts_at, noun)
    }

    /// The record block primitive: an optional header then indented `label value`
    /// rows, wrapped to the sink's width.
    pub fn record_block(&mut self, header: Option<&str>, fields: &[Field<'_>]) -> io::Result<()> {
        primitives::record_block(self.out, self.palette, header, fields, self.width)
    }

    /// The separator bar primitive between records.
    pub fn separator(&mut self) -> io::Result<()> {
        primitives::separator(self.out, self.palette, self.width)
    }

    /// A bare newline (between the count line and the first record).
    pub fn blank_line(&mut self) -> io::Result<()> {
        writeln!(self.out)
    }
}
