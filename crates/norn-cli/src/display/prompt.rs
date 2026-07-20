//! The interactive TTY confirm prompt (NRN-389): preview → prompt → apply.
//!
//! Ported from the donor `prompt::confirm` (`retired/src/prompt.rs`): read one
//! line, true on a case-insensitive "y"/"yes", false on anything else —
//! including EOF and garbage. The donor spread this across several
//! verb-specific call sites with drifted details: `set`/`edit`/`move`/`delete`
//! prompted `"Proceed? [y/N] "`, `new` prompted `"Apply? [y/N] "`, `apply`
//! prompted `"Apply migration plan? [y/N] "`, `rewrite-wikilink` prompted
//! `"Apply wikilink rewrite? [y/N] "` — and `new` alone gated on
//! `stdout.is_terminal()` where every other verb gated on `stdin.is_terminal()`
//! (a prompt READS from stdin, so stdin is the correct terminal to test; the
//! donor's `new` gate reads as an oversight, not a deliberate choice).
//!
//! This port standardizes on ONE prompt string and ONE gate (`stdin`) across
//! all seven mutation verbs that write (`set`/`new`/`edit`/`move`/`delete`/
//! `rewrite-wikilink`/`apply` — `repair` stays read-only, and `init` is not
//! yet ported) (NRN-389 uniformity) — a deliberate redesign call the parity
//! harness cannot observe either way, since this text is TTY-only and never
//! appears in piped output.

use std::io::{self, BufRead, Write};

/// The one confirm prompt text, shared by every mutation verb.
pub(crate) const CONFIRM_PROMPT: &str = "Proceed? [y/N] ";

/// Read one line from `reader`, having first written `prompt` to `writer`.
/// Returns `true` only for a trimmed, case-insensitive "y" or "yes" — EOF (a
/// zero-byte read) and any other input (including "n", a blank line, or
/// garbage) decline.
pub(crate) fn confirm<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
) -> io::Result<bool> {
    write!(writer, "{prompt}")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Wire [`confirm`] to the real process stdin/stderr, with the donor's
/// leading blank line separating the already-rendered forecast from the
/// prompt itself.
pub(crate) fn confirm_interactive() -> io::Result<bool> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut out = io::stderr();
    writeln!(out)?;
    confirm(&mut reader, &mut out, CONFIRM_PROMPT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn confirm_returns_true_on_y() {
        let mut reader = Cursor::new(b"y\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_true_on_uppercase_y() {
        let mut reader = Cursor::new(b"Y\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_true_on_yes() {
        let mut reader = Cursor::new(b"yes\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_true_on_mixed_case_yes() {
        let mut reader = Cursor::new(b"YeS\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_false_on_n() {
        let mut reader = Cursor::new(b"n\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_false_on_blank_line() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_returns_false_on_garbage() {
        let mut reader = Cursor::new(b"maybe\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    /// True EOF (a zero-byte reader, no trailing newline at all) — the
    /// piped-stdin-closed-early case. `read_line` returns `Ok(0)`, the line
    /// stays empty, and an empty answer is a decline like any other non-"y".
    #[test]
    fn confirm_returns_false_on_eof() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap());
    }

    #[test]
    fn confirm_writes_the_prompt_text_before_reading() {
        let mut reader = Cursor::new(b"y\n".to_vec());
        let mut writer = Vec::new();
        confirm(&mut reader, &mut writer, CONFIRM_PROMPT).unwrap();
        assert_eq!(String::from_utf8(writer).unwrap(), CONFIRM_PROMPT);
    }
}
