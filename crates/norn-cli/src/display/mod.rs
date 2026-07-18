//! The CLI display layer: how presented Reports reach stdout and how
//! diagnostics reach stderr.
//!
//! This phase establishes the LAYER, not its content. The command modules
//! present through a [`Presenter`]; today the only presentation is the uniform
//! not-yet-ported diagnostic. What lands here as the read/mutation verbs port
//! (the `src/output/` row of the porting burn-down in `retired/CLAUDE.md`):
//!
//! - glyphs / palette — color and symbol vocabulary,
//! - pager — the TTY pager,
//! - primitives — record-block rendering,
//! - projection — column selection.
//!
//! Kept deliberately thin: the [`Format`] vocabulary, the stderr `norn:`
//! convention, and the process exit-code constants live here now so a porting
//! PR fills the renderers in rather than reshaping the seam.

mod format;
mod presenter;

pub use format::Format;
pub use presenter::{Presenter, PROGRAM};

/// Clean success: every operation applied (or a dry-run forecast). Produced by
/// clap for `--help` / `--version`; a ported verb returns it on success.
pub const EXIT_OK: i32 = 0;

/// Operational failure — the invocation was well-formed but could not be
/// carried out. The uniform not-yet-ported outcome exits with this code.
pub const EXIT_OPERATIONAL: i32 = 1;

/// Bad invocation — unparseable argv, unknown command, or a bad flag. Produced
/// by clap itself before dispatch (`docs/errors.md`).
pub const EXIT_USAGE: i32 = 2;
