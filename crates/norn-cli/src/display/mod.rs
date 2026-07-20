//! The CLI display layer: verbs return values, this layer renders them (NRN-370).
//!
//! A command module resolves its report and returns an [`Output`]; the single
//! [`emit`] call in the dispatch loop turns it into bytes. Rendering lives here
//! and only here: [`emit`] resolves the effective [`Format`] (the isatty default
//! policy in [`FormatSpec`]), resolves the palette once, composes records through
//! a [`Sink`], routes report annotations through a stderr-only [`Conversation`],
//! and derives the exit code. A user error returns a [`Diagnostic`] instead,
//! rendered through the one [`Presenter`] path with the `norn:` prefix.
//!
//! The pieces:
//! - [`Format`] / [`FormatSpec`] — the one semantic output vocabulary and the
//!   tty/piped default policy.
//! - [`Output`] and its views — the value each verb returns.
//! - [`Sink`] (stdout, styled record primitives) and [`Conversation`] (stderr,
//!   the oracle-parity `note:` / `warning:` annotations).
//! - [`Presenter`] / [`Diagnostic`] — the single stderr `norn:` diagnostic path.

mod conversation;
mod diagnostic;
mod emit;
mod fix_hints;
mod format;
mod output;
mod presenter;
mod sink;

pub use conversation::Conversation;
pub use diagnostic::Diagnostic;
pub use emit::emit;
pub use format::{Format, FormatSpec};
pub use output::{
    CountView, DescribeView, FindView, GetView, NewMutationView, Output, SetMutationView,
    ValidateView, VaultListView,
};
pub use presenter::{Presenter, HINT, PROGRAM};
pub use sink::Sink;

/// Clean success: every operation applied (or a dry-run forecast). Produced by
/// clap for `--help` / `--version`; a ported verb returns it on success.
pub const EXIT_OK: i32 = 0;

/// Operational failure — the invocation was well-formed but could not be
/// carried out. The uniform not-yet-ported outcome exits with this code.
pub const EXIT_OPERATIONAL: i32 = 1;

/// Bad invocation — unparseable argv, unknown command, or a bad flag. Produced
/// by clap itself before dispatch (`docs/errors.md`).
pub const EXIT_USAGE: i32 = 2;
