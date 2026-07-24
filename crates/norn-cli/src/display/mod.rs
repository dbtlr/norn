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
//!   the `note:` / `warning:` annotations).
//! - [`Presenter`] / [`Diagnostic`] — the single stderr `norn:` diagnostic path.

mod conversation;
mod diagnostic;
mod emit;
mod fix_hints;
mod format;
mod output;
mod presenter;
mod prompt;
mod render;
mod sink;

pub use conversation::Conversation;
pub use diagnostic::Diagnostic;
pub use emit::{emit, emit_mutation};
pub use format::{Format, FormatChoice, FormatSpec};
pub use output::{
    ApplyMutationView, AuditView, CountView, DeleteMutationView, DescribeView, EditMutationView,
    FindView, GetView, MoveMutationView, NewMutationView, Output, RepairView, RewriteWikilinkView,
    SetMutationView, ValidateView, VaultListView,
};
pub use presenter::{Presenter, HINT, PROGRAM};
pub use sink::Sink;

/// One `norn: <msg>` diagnostic headline, written verbatim to `w`. The one
/// byte layout [`Presenter::diagnostic`](presenter::Presenter::diagnostic) and
/// [`Conversation::diagnostic`](conversation::Conversation::diagnostic) both
/// call, so the headline can never drift between the two entry points.
pub(crate) fn diagnostic_line(w: &mut dyn std::io::Write, msg: &str) -> std::io::Result<()> {
    writeln!(w, "{PROGRAM}: {msg}")
}

/// The user-facing label for a serde-kebab enum value — its serialized name.
///
/// A records renderer that needs the printed word for an [`OpStatus`] /
/// [`PreconditionStatus`] (`applied`, `not-run`, …) asks this rather than
/// `format!("{value:?}")`, whose `Debug` derives the VARIANT identifier
/// (`NotRun`) and only accidentally lowercases to a hyphen-free `notrun`. This
/// returns the enum's own `#[serde(rename_all = "kebab-case")]` name, so the
/// printed label and the wire value are one string by construction. The value
/// must serialize to a bare JSON string (every unit-variant enum does); a
/// non-string serialization yields an empty label.
///
/// [`OpStatus`]: norn_wire::OpStatus
/// [`PreconditionStatus`]: norn_wire::PreconditionStatus
pub(crate) fn serde_label<T: serde::Serialize>(value: &T) -> String {
    let label = serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    debug_assert!(
        !label.is_empty(),
        "serde_label is for a unit-variant enum serializing to a bare string; \
         got a value that did not (empty label)"
    );
    label
}

/// Clean success: every operation applied (or a dry-run forecast). Produced by
/// clap for `--help` / `--version`; a ported verb returns it on success.
pub const EXIT_OK: i32 = 0;

/// Operational failure — the invocation was well-formed but could not be
/// carried out. The uniform not-yet-ported outcome exits with this code.
pub const EXIT_OPERATIONAL: i32 = 1;

/// Bad invocation — unparseable argv, unknown command, or a bad flag. Produced
/// by clap itself before dispatch (`docs/errors.md`).
pub const EXIT_USAGE: i32 = 2;

#[cfg(test)]
mod label_tests {
    use super::serde_label;
    use norn_wire::{OpStatus, PreconditionStatus, Severity};

    #[test]
    fn serde_label_renders_the_kebab_serde_name_not_the_debug_variant() {
        // The load-bearing case: `NotRun` serializes to `not-run`, where the old
        // `Debug`-lowercase path produced the hyphen-free `notrun`.
        assert_eq!(serde_label(&OpStatus::NotRun), "not-run");
        assert_eq!(serde_label(&PreconditionStatus::NotRun), "not-run");
        assert_eq!(serde_label(&OpStatus::Applied), "applied");
        assert_eq!(serde_label(&PreconditionStatus::Passed), "passed");
        assert_eq!(serde_label(&Severity::Warning), "warning");
    }
}
