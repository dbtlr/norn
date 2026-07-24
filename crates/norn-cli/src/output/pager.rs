//! The pager: on a real stdout TTY, a long `records`/`markdown` render pages
//! through `$PAGER` instead of scrolling past — `find` / `get` / `--help`
//! (NRN-454). Honors `--no-pager` and degrades to a direct write whenever
//! paging isn't possible or useful: non-TTY stdout, a render short enough to
//! fit the screen, or a `$PAGER` that fails to spawn.
//!
//! [`should_page`] is the pure selection function (env/flag/TTY matrix,
//! unit-tested directly); [`page_or_write_direct`] is the one side-effecting
//! call, invoked only from [`crate::display::emit`]'s buffered render path and
//! the `--help` interceptor — the two real call sites that hold an actual
//! process stdout.
//!
//! `$PAGER` is split on whitespace into argv ([`resolve_pager`]) — no shell
//! interpretation. `PAGER="less -R"` works; a value that relies on shell
//! syntax, e.g. quoted args or a pipeline (`PAGER='sh -c "col -b | less"'`),
//! does not — it is split into literal words and passed to `Command::new` as
//! one program plus argv, so a construct like that spawns a program named
//! `sh` with the literal arguments `-c`, `"col`, `-b`, `|`, `less"` rather than
//! running a shell. Command lookup then fails and [`page_or_write_direct`]
//! degrades to a direct write, with a warning on stderr.
//!
//! A renderer's own `Conversation` warnings (an unknown column, an unknown
//! sort field, …) reach the real stderr during buffering — before the paging
//! decision runs and before any pager spawns. With the default `less -FRX`,
//! `-X` keeps the primary screen, so that pre-spawn text stays on screen
//! alongside the pager's own output rather than being cleared on exit. A
//! `$PAGER` that switches to the alternate screen itself (plain `less`
//! without `-X`, `most`, …) can still overdraw it for the duration of the
//! pager session, even though it was written first.

use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use crate::display::Conversation;

/// The line count of a buffered render, for [`should_page`]'s threshold
/// check: one per `\n`, plus one more for a final unterminated line (content
/// after the last newline, or the whole buffer when it contains none) — that
/// trailing line still occupies a screen row even without its own newline.
/// Byte-faithful markdown (a source file with no trailing newline) is the
/// case this covers; counting `\n` bytes alone would undercount it by one
/// right at the paging threshold.
pub fn count_lines(buffer: &[u8]) -> usize {
    let newlines = buffer.iter().filter(|&&b| b == b'\n').count();
    if buffer.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}

/// Whether a buffered render should page: never with `--no-pager`, never off
/// a real terminal, and — even on a TTY — only when the buffer's line count
/// exceeds the terminal height. A render that already fits the screen has
/// nothing to gain from a pager (and `less -F` would quit-if-fits anyway;
/// skipping the spawn entirely avoids paying for a subprocess on the common
/// short-output case).
pub fn should_page(buffer_line_count: usize, no_pager: bool, stdout_is_tty: bool) -> bool {
    if no_pager || !stdout_is_tty {
        return false;
    }
    let term_height = terminal_size::terminal_size()
        .map(|(_, h)| h.0 as usize)
        .unwrap_or(24);
    buffer_line_count > term_height.saturating_sub(2)
}

/// Resolve the pager command and its arguments: `$PAGER` split on
/// whitespace, or `less -FRX` (quit-if-fits, raw ANSI passthrough, no
/// init/deinit) when `$PAGER` is unset or empty.
pub fn resolve_pager() -> (String, Vec<String>) {
    match env::var("PAGER") {
        Ok(p) if !p.is_empty() => {
            let mut parts = p.split_whitespace().map(String::from);
            let cmd = parts.next().unwrap_or_else(|| "less".to_string());
            (cmd, parts.collect())
        }
        _ => ("less".to_string(), vec!["-FRX".to_string()]),
    }
}

/// Spawn the resolved pager, feed it `buffer` on its stdin, and wait for it to
/// exit. The pager's own stdout/stderr inherit the process's (its display goes
/// straight to the terminal); only its stdin is piped.
///
/// A spawn failure (missing binary, no permissions, …) degrades to a direct
/// write on `stdout` plus a `warning:` line on `conv` — never an `Err`, since
/// the render itself already succeeded and an unusable `$PAGER` is not the
/// vault's fault. Quitting the pager before it has read the whole buffer
/// (`q` mid-stream) closes its stdin early; that ordinary broken pipe is
/// swallowed here rather than propagated, so the caller sees a clean `Ok(())`
/// exactly like a full read would.
pub fn page_or_write_direct(
    buffer: &[u8],
    stdout: &mut dyn Write,
    conv: &mut Conversation<'_>,
) -> io::Result<()> {
    let (cmd, args) = resolve_pager();
    let mut child = match Command::new(&cmd).args(&args).stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = conv.warning(&format!(
                "pager '{cmd}' failed to start ({e}); writing directly to stdout"
            ));
            return stdout.write_all(buffer);
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(e) = stdin.write_all(buffer) {
            if e.kind() != io::ErrorKind::BrokenPipe {
                return Err(e);
            }
        }
    }
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;

    // ── count_lines: the trailing-unterminated-line boundary ───────────────

    #[test]
    fn empty_buffer_counts_zero_lines() {
        assert_eq!(count_lines(b""), 0);
    }

    #[test]
    fn a_trailing_newline_counts_only_the_terminated_lines() {
        assert_eq!(count_lines(b"one\ntwo\n"), 2);
    }

    #[test]
    fn a_missing_trailing_newline_still_counts_the_final_line() {
        // Byte-faithful markdown without a trailing newline: the `\n` count
        // alone (1) would undercount the real two-line buffer.
        assert_eq!(count_lines(b"one\ntwo"), 2);
    }

    #[test]
    fn a_single_unterminated_line_counts_as_one() {
        assert_eq!(count_lines(b"no newline at all"), 1);
    }

    // ── should_page: the env/flag/TTY selection matrix ──────────────────────

    #[test]
    fn no_pager_flag_disables_even_on_a_long_tty_render() {
        assert!(!should_page(1000, true, true));
    }

    #[test]
    fn non_tty_stdout_disables_even_when_not_suppressed() {
        assert!(!should_page(1000, false, false));
    }

    #[test]
    fn short_output_on_a_tty_skips_the_pager() {
        assert!(!should_page(5, false, true));
    }

    #[test]
    fn long_output_on_an_unsuppressed_tty_pages() {
        assert!(should_page(1000, false, true));
    }

    // ── resolve_pager: $PAGER vs the less -FRX default ──────────────────────

    #[test]
    fn empty_pager_env_falls_back_to_the_default() {
        let _env = EnvGuard::new(&[("PAGER", Some(""))]);
        let (cmd, args) = resolve_pager();
        assert_eq!(cmd, "less");
        assert_eq!(args, vec!["-FRX".to_string()]);
    }

    #[test]
    fn unset_pager_env_falls_back_to_the_default() {
        let _env = EnvGuard::new(&[("PAGER", None)]);
        let (cmd, args) = resolve_pager();
        assert_eq!(cmd, "less");
        assert_eq!(args, vec!["-FRX".to_string()]);
    }

    #[test]
    fn pager_env_with_args_splits_on_whitespace() {
        let _env = EnvGuard::new(&[("PAGER", Some("less -R"))]);
        let (cmd, args) = resolve_pager();
        assert_eq!(cmd, "less");
        assert_eq!(args, vec!["-R".to_string()]);
    }

    #[test]
    fn bare_pager_env_carries_no_args() {
        let _env = EnvGuard::new(&[("PAGER", Some("more"))]);
        let (cmd, args) = resolve_pager();
        assert_eq!(cmd, "more");
        assert!(args.is_empty());
    }

    // ── page_or_write_direct: spawn-failure degrade ─────────────────────────

    #[test]
    fn a_pager_that_cannot_spawn_degrades_to_a_direct_write_and_warns() {
        let _env = EnvGuard::new(&[("PAGER", Some("norn-nonexistent-pager-binary"))]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        let result = page_or_write_direct(b"hello\n", &mut out, &mut conv);
        assert!(result.is_ok(), "a spawn failure must never be an Err");
        assert_eq!(out, b"hello\n");
        let warning = String::from_utf8(err).unwrap();
        assert!(
            warning.contains("warning: pager 'norn-nonexistent-pager-binary' failed to start"),
            "got: {warning}"
        );
    }

    #[test]
    fn a_real_pager_receives_the_whole_buffer() {
        // `cat` as a stand-in pager: reads stdin to EOF, writes it back out —
        // proves the buffer reaches the child's stdin uncorrupted.
        let _env = EnvGuard::new(&[("PAGER", Some("cat"))]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut conv = Conversation::new(&mut err);
        let result = page_or_write_direct(b"one\ntwo\nthree\n", &mut out, &mut conv);
        assert!(result.is_ok());
        assert!(err.is_empty(), "a spawnable pager must not warn");
        // `cat`'s own stdout is inherited (goes to the test process's real
        // stdout, not `out`) — `out` staying empty here proves the buffer went
        // to the CHILD's stdin, not a fallback direct write.
        assert!(out.is_empty());
    }
}
