//! The semantic output vocabulary the display layer renders a report in, and the
//! per-Output-kind isatty default policy (NRN-370).
//!
//! [`Format`] is the ONE display-layer format enum: every verb's per-command clap
//! value-enum (`FindFormat`, `GetFormat`, `CountFormat`, …) stays as the truthful
//! `--help` declaration of that verb's supported subset and maps into `Format` via
//! a `From` impl. The renderers switch on `Format`, so a new verb reuses the same
//! rendering vocabulary rather than growing another bespoke enum.
//!
//! [`FormatSpec`] carries each Output kind's `{ tty, piped }` default pair. When
//! `--format` is absent, [`FormatSpec::resolve`] picks by whether stdout is a
//! terminal — so isatty defaulting lives in the layer, once, instead of in each
//! command module.

/// How the display layer renders a report to stdout. The union of every verb's
/// supported formats; a given verb accepts only a subset (declared by its clap
/// value-enum) and never resolves to a variant outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Vertical key-value record block(s) — the human-legible terminal shape.
    /// Also the target of the text/human variants of `count` / `describe` /
    /// `vault list`, whose renderers produce their own bespoke text.
    Records,
    /// One document path per line.
    Paths,
    /// A single JSON document (object or array).
    Json,
    /// One JSON object per line (streaming).
    Jsonl,
    /// Byte-faithful source passthrough (`get --format markdown`).
    Markdown,
}

/// One Output kind's default format pair: which [`Format`] to use on a terminal
/// versus a pipe when `--format` is absent. Most verbs use the same format for
/// both (their clap value-enum carries a hard default); only `find` differs
/// (`records` on a tty, `paths` when piped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatSpec {
    /// The default when stdout is a terminal.
    pub tty: Format,
    /// The default when stdout is a pipe / file.
    pub piped: Format,
}

impl FormatSpec {
    /// Resolve the effective format: an explicit `--format` always wins;
    /// otherwise pick `tty` or `piped` by whether stdout is a terminal.
    pub fn resolve(&self, explicit: Option<Format>, is_tty: bool) -> Format {
        explicit.unwrap_or(if is_tty { self.tty } else { self.piped })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIND: FormatSpec = FormatSpec {
        tty: Format::Records,
        piped: Format::Paths,
    };
    const UNIFORM: FormatSpec = FormatSpec {
        tty: Format::Records,
        piped: Format::Records,
    };

    #[test]
    fn explicit_format_always_wins() {
        assert_eq!(FIND.resolve(Some(Format::Json), true), Format::Json);
        assert_eq!(FIND.resolve(Some(Format::Json), false), Format::Json);
    }

    #[test]
    fn absent_format_picks_by_tty() {
        // find's one tty-sensitive default pair.
        assert_eq!(FIND.resolve(None, true), Format::Records);
        assert_eq!(FIND.resolve(None, false), Format::Paths);
    }

    #[test]
    fn uniform_spec_ignores_tty() {
        // Every other verb: the same format on a terminal and a pipe.
        assert_eq!(UNIFORM.resolve(None, true), Format::Records);
        assert_eq!(UNIFORM.resolve(None, false), Format::Records);
    }
}
