//! The records-composition sink (NRN-370).
//!
//! A [`Sink`] owns the stdout writer, the resolved [`Palette`], and the terminal
//! width for one render, and exposes the record-block primitive vocabulary
//! (`output/primitives`) as methods. Records-mode rendering composes ONLY these
//! primitives, so it structurally cannot emit an unstyled record block — the
//! palette is threaded through the sink, never re-resolved at the call site.
//!
//! The non-record formats (`paths` / `json` / `jsonl` / `markdown`, and the
//! bespoke `count` / `describe` text) are raw payload; they write through
//! [`Sink::writer`] rather than the record primitives.

use std::io::{self, Write};

use crate::output::palette::Palette;
use crate::output::primitives::{self, Field};

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
