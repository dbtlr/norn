//! The single render entry (NRN-370): one [`emit`] call turns a command's
//! returned [`Output`] into bytes, and it is the ONLY place the read/registry
//! verbs render. A verb resolves its report and returns an `Output`; `emit`
//! resolves the effective [`Format`] (isatty defaulting), resolves the palette
//! once, composes records through a [`Sink`], routes annotations through a
//! [`Conversation`], and derives the process exit code. A user error returns a
//! [`Diagnostic`] instead, rendered through the one presenter path.
//!
//! The renderers themselves live one per verb under [`super::render`] (NRN-409);
//! they are pinned to the donor CLI's output by the parity suite: the `find` /
//! `get` projections run through the shared `output::projection` ladder;
//! `count` / `describe` / `vault list` reproduce their bespoke text unstyled
//! (they never resolved a palette in the donor, and their output is pinned by
//! the parity cases).

use std::io::{self, Write};

use crate::cli::GlobalArgs;
use crate::output::palette::{self, Palette};

use super::conversation::Conversation;
use super::output::Output;
use super::prompt;
use super::render;
use super::sink::Sink;
use super::{Diagnostic, Presenter, EXIT_OK, EXIT_OPERATIONAL, EXIT_USAGE};

/// Whether the process stdout is a terminal — the one isatty read, consumed by
/// [`FormatSpec::resolve`](super::format::FormatSpec::resolve).
pub(crate) fn is_stdout_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Whether the process stdin is a terminal — the interactive-confirm gate
/// (NRN-389). Deliberately separate from [`is_stdout_tty`]: the confirm
/// prompt READS from stdin, so stdin is the terminal whose ttyness actually
/// matters, independent of whatever stdout is connected to.
fn is_stdin_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

/// The effective terminal width for record wrapping (donor default 80).
pub(crate) fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// The one render IO-error policy (NRN-372), applied by every render path.
///
/// A render closure does its writes with `?` and, on success, returns the exit
/// code its content implies (e.g. `get`'s `has_error` outcome). This resolves
/// that result the same way for every verb:
/// - `BrokenPipe` (the reader end closed early — `norn find | head`) is
///   tolerated silently and treated as success. This is standard CLI
///   behavior; a downstream reader closing the pipe is not the vault's fault.
/// - Every other IO error (full disk, closed fd, …) is a real failure: one
///   `norn: <e>` diagnostic on stderr, and the operational exit.
///
/// No render path swallows an IO error with `let _ =` — every write funnels
/// through this one outcome.
pub(crate) fn render_outcome(result: io::Result<i32>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => EXIT_OK,
        Err(e) => {
            let _ = writeln!(err, "norn: {e}");
            EXIT_OPERATIONAL
        }
    }
}

/// Build a [`Sink`] over the presenter's stdout and a [`Conversation`] over its
/// stderr — the ready styling seam a renderer composes through — and run `f`.
/// The single place per render where the two stream borrows are split and the
/// resolved palette + width are threaded into the sink.
fn render_with<O: Write, E: Write, R>(
    presenter: &mut Presenter<O, E>,
    palette: &Palette,
    width: usize,
    f: impl FnOnce(&mut Sink<'_>, &mut Conversation<'_>) -> R,
) -> R {
    let (out, err) = presenter.streams();
    let mut sink = Sink::new(out, palette, width);
    let mut conv = Conversation::new(err);
    f(&mut sink, &mut conv)
}

/// Render a command's returned [`Output`] (or its [`Diagnostic`]) and return the
/// process exit code. The single render seam: every read/registry verb reaches
/// stdout through here and nowhere else.
///
/// This is the ONE place presentation is resolved (ADR 0021): the effective
/// [`Format`](super::format::Format) per view, the [`Palette`] once (from
/// `--color` + isatty), and the terminal width — a renderer receives the
/// resolved `Format` plus a ready [`Sink`] and never re-resolves any of them.
///
/// The unstyled contract is STRUCTURAL here: `describe` / `count` / `vault list`
/// (and the mutation verbs that never colorized in the donor) are handed a
/// no-op ([`Palette::off`]) sink, so their pinned, unstyled bytes cannot acquire
/// styling even if a future edit routes them through a record primitive. Only
/// the styled verbs (`find` / `get` / `validate` / `repair` / `set`) receive the
/// resolved palette.
pub fn emit<O: Write, E: Write>(
    result: Result<Output, Diagnostic>,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let output = match result {
        Ok(output) => output,
        Err(diag) => {
            presenter.present_diagnostic(&diag);
            return EXIT_OPERATIONAL;
        }
    };
    // The three isatty / palette / width reads, once, at the dispatch boundary.
    let is_tty = is_stdout_tty();
    let styled = palette::resolve(global.color);
    let plain = Palette::off();
    let width = term_width();

    match output {
        Output::Find(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &styled, width, |sink, conv| {
                render::find::render_find(view, format, sink, conv)
            })
        }
        Output::Get(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &styled, width, |sink, conv| {
                render::get::render_get(view, format, sink, conv)
            })
        }
        Output::Count(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &plain, width, |sink, conv| {
                render::count::render_count(view, format, sink, conv)
            })
        }
        Output::Describe(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &plain, width, |sink, conv| {
                render::describe::render_describe(view, format, sink, conv)
            })
        }
        Output::Validate(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &styled, width, |sink, conv| {
                render::validate::render_validate(view, format, sink, conv)
            })
        }
        Output::Repair(view) => render_with(presenter, &styled, width, |sink, conv| {
            render::repair::render_repair(view, is_tty, sink, conv)
        }),
        Output::VaultList(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &plain, width, |sink, conv| {
                render::vault::render_vault_list(view, format, sink, conv)
            })
        }
        Output::Set(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &styled, width, |sink, conv| {
                render::set::render_set(view, format, sink, conv)
            })
        }
        Output::New(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &plain, width, |sink, conv| {
                render::new::render_new(view, format, sink, conv)
            })
        }
        Output::Edit(view) => {
            let format = view.spec.resolve(view.explicit, is_tty);
            render_with(presenter, &plain, width, |sink, conv| {
                render::edit::render_edit(view, format, sink, conv)
            })
        }
        Output::Move(view) => render_with(presenter, &plain, width, |sink, conv| {
            render::move_doc::render_move(view, sink, conv)
        }),
        Output::Delete(view) => render_with(presenter, &plain, width, |sink, conv| {
            render::delete::render_delete(view, sink, conv)
        }),
        Output::RewriteWikilink(view) => render_with(presenter, &plain, width, |sink, conv| {
            render::rewrite_wikilink::render_rewrite_wikilink(view, sink, conv)
        }),
        Output::Apply(view) => render_with(presenter, &plain, width, |sink, conv| {
            render::apply::render_apply(view, sink, conv)
        }),
        Output::Line(line) => {
            let (out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                writeln!(out, "{line}")?;
                Ok(EXIT_OK)
            })();
            render_outcome(result, err)
        }
        Output::Usage(bytes) => {
            let (_out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                err.write_all(&bytes)?;
                Ok(EXIT_USAGE)
            })();
            render_outcome(result, err)
        }
    }
}

/// Render a mutation verb's first report and, when the invocation is
/// interactive-eligible, carry the donor's preview → prompt → apply
/// conversation (NRN-389).
///
/// The first render is BYTE-IDENTICAL to today's non-interactive path in
/// every case — forecast (with its `Apply with --yes` hint), applied, or
/// refused — because this wraps [`emit`] rather than changing it. Only when
/// ALL of the following hold does anything additional happen:
///
/// - `prompt_eligible` is true (the caller's `--dry-run`/`--yes`/
///   `--format json` ladder decided this run was a plain, unconfirmed
///   forecast attempt — the same ladder that decides `confirm` itself);
/// - the first render's exit code is [`EXIT_OK`] (a clean forecast, not a
///   refusal or an operational error — nothing to confirm on those); and
/// - stdin is a real terminal ([`is_stdin_tty`]).
///
/// Then: prompt on stderr (blank line + `Proceed? [y/N] `, donor text). On a
/// "y"/"yes" answer, call `rerun` — a SECOND routed request with `confirm`
/// forced true, re-planned and applied fresh under the owner's lock, exactly
/// as a direct `--yes` invocation would — and render its report as the FINAL
/// outcome. On anything else (a declined answer, EOF, or an I/O error
/// reading the prompt), decline: no second request, exit
/// [`EXIT_OPERATIONAL`] (1) — the donor's `process::exit(1)` on a declined
/// confirm, carried here as a return rather than a hard process exit.
///
/// Piped / non-TTY invocations never reach the prompt at all (`is_stdin_tty`
/// is false), so today's forecast-plus-hint contract for scripts and the
/// parity harness is unchanged byte-for-byte.
///
/// `--body-from-stdin` (`set`/`new`) at a TTY auto-declines rather than
/// prompting: the body read already consumed stdin to EOF before the ladder
/// runs, so a subsequent `confirm()` read sees EOF and returns `Ok(false)`.
/// This is safe (no write happens on a decline) but means the human never
/// actually gets asked — an accepted, documented gap rather than a bug: the
/// same stdin cannot serve both a piped body and an interactive prompt.
pub fn emit_mutation<O: Write, E: Write>(
    first: Result<Output, Diagnostic>,
    prompt_eligible: bool,
    rerun: impl FnOnce() -> Result<Output, Diagnostic>,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    confirm_and_finish(
        first,
        prompt_eligible,
        is_stdin_tty(),
        prompt::confirm_interactive,
        rerun,
        global,
        presenter,
    )
}

/// The injectable core of [`emit_mutation`] (NRN-389 F2). Identical decision
/// logic, but with the stdin-tty read and the confirm reader taken as
/// parameters instead of wired to the real process — the donor's own
/// `prompt::confirm<R, W>` factoring is the precedent for pulling the I/O
/// dependency out from behind the policy. Without this seam the prompt
/// branch is unreachable under `cargo test`: the test process's stdin is
/// never a real terminal, so `is_stdin_tty()` always reads false there.
/// [`emit_mutation`] is the one production caller and always supplies the
/// real [`is_stdin_tty`] result and [`prompt::confirm_interactive`]; tests
/// call this directly with a fixed `stdin_is_tty` and a stub `confirm`.
fn confirm_and_finish<O: Write, E: Write>(
    first: Result<Output, Diagnostic>,
    prompt_eligible: bool,
    stdin_is_tty: bool,
    confirm: impl FnOnce() -> io::Result<bool>,
    rerun: impl FnOnce() -> Result<Output, Diagnostic>,
    global: &GlobalArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let exit = emit(first, global, presenter);
    if !prompt_eligible || exit != EXIT_OK || !stdin_is_tty {
        return exit;
    }
    match confirm() {
        Ok(true) => emit(rerun(), global, presenter),
        Ok(false) | Err(_) => EXIT_OPERATIONAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ColorWhen, GlobalArgs};
    use crate::display::format::{Format, FormatSpec};
    use crate::display::output::SetMutationView;
    use norn_wire::{CodedError, MutationOutcome, SetReport};

    /// A `Write` that fails every write with a fixed [`io::ErrorKind`].
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn global_args() -> GlobalArgs {
        GlobalArgs {
            cwd: None,
            verbose: false,
            no_cache_refresh: false,
            color: ColorWhen::Never,
            vault: None,
            help_short: false,
            help_long: false,
            dynamic_fields: Vec::new(),
        }
    }

    // ── emit_mutation / confirm_and_finish (NRN-389 F2) ─────────────────────
    //
    // `emit_mutation` itself just wires `confirm_and_finish` to the real
    // `is_stdin_tty()` and `prompt::confirm_interactive`; these tests pin
    // `confirm_and_finish`'s decision logic directly with an injected
    // `stdin_is_tty` and `confirm` closure, since real stdin is never a
    // terminal under `cargo test`.

    /// A `set` report: `outcome: Refused` renders at exit 2; anything else
    /// (the donor's `MutationOutcome::Applied`, which really means "a valid
    /// plan" — `applied` is the separate bool distinguishing a forecast from
    /// a real write) renders at exit 0 regardless of `applied`.
    fn set_report(applied: bool, outcome: MutationOutcome) -> SetReport {
        SetReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "set".into(),
            target: "a.md".into(),
            frontmatter_changes: vec![],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied,
            outcome,
            error: match outcome {
                MutationOutcome::Refused => Some(CodedError {
                    code: "set-refused".into(),
                    message: "refused".into(),
                    path: None,
                }),
                MutationOutcome::Applied => None,
            },
            warnings: vec![],
        }
    }

    fn set_output(applied: bool, outcome: MutationOutcome) -> Result<Output, Diagnostic> {
        Ok(Output::Set(SetMutationView {
            report: set_report(applied, outcome),
            explicit: Some(Format::Records),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }))
    }

    /// A clean, unrefused forecast: `applied: false`, `outcome: Applied` — the
    /// shape every mutation verb's first render produces when the ladder
    /// decided `confirm: false` and nothing was refused. Renders at exit 0.
    fn forecast_output() -> Result<Output, Diagnostic> {
        set_output(false, MutationOutcome::Applied)
    }

    /// A refused report — renders at exit 2 (`EXIT_USAGE`), so a test that
    /// sees this exit code proves the render came from THIS report and not
    /// the forecast, without needing to inspect any state beyond the return
    /// value.
    fn refused_output() -> Result<Output, Diagnostic> {
        set_output(false, MutationOutcome::Refused)
    }

    #[test]
    fn eligible_confirm_yes_invokes_rerun_exactly_once_and_returns_its_exit() {
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let rerun_calls = std::cell::Cell::new(0u32);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                forecast_output(),
                /* prompt_eligible */ true,
                /* stdin_is_tty */ true,
                /* confirm */ || Ok(true),
                /* rerun */
                || {
                    rerun_calls.set(rerun_calls.get() + 1);
                    // A REFUSED second report (exit 2) so the returned exit
                    // is unambiguously the rerun's, not the forecast's (which
                    // renders at exit 0).
                    refused_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert_eq!(rerun_calls.get(), 1, "rerun must be invoked exactly once");
        assert_eq!(code, EXIT_USAGE, "the SECOND report's exit is returned");
    }

    #[test]
    fn eligible_confirm_no_declines_without_ever_calling_rerun() {
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let rerun_called = std::cell::Cell::new(false);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                forecast_output(),
                true,
                true,
                || Ok(false),
                || {
                    rerun_called.set(true);
                    forecast_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert!(!rerun_called.get(), "a decline must never re-run");
        assert_eq!(code, EXIT_OPERATIONAL);
    }

    /// A read error while prompting (e.g. a closed/broken stdin) — treated
    /// the same as a decline: no re-run, `EXIT_OPERATIONAL`. Genuine EOF
    /// (an empty read) is already covered by `prompt::confirm` itself, which
    /// resolves EOF to `Ok(false)` — the "confirm-no" case above — before it
    /// ever reaches this seam; `Err` here models a hard I/O failure instead.
    #[test]
    fn eligible_confirm_io_error_declines_without_ever_calling_rerun() {
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let rerun_called = std::cell::Cell::new(false);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                forecast_output(),
                true,
                true,
                || Err(io::Error::other("stdin closed")),
                || {
                    rerun_called.set(true);
                    forecast_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert!(!rerun_called.get(), "an I/O error must never re-run");
        assert_eq!(code, EXIT_OPERATIONAL);
    }

    #[test]
    fn not_prompt_eligible_never_prompts_and_returns_the_first_exit() {
        // Represents `--yes` / `--dry-run` / `--format json`: the caller's
        // ladder already decided `prompt_eligible: false`, regardless of
        // stdin.
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let confirm_called = std::cell::Cell::new(false);
        let rerun_called = std::cell::Cell::new(false);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                forecast_output(),
                /* prompt_eligible */ false,
                /* stdin_is_tty */ true,
                || {
                    confirm_called.set(true);
                    Ok(true)
                },
                || {
                    rerun_called.set(true);
                    forecast_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert!(!confirm_called.get(), "not eligible must never prompt");
        assert!(!rerun_called.get(), "not eligible must never re-run");
        assert_eq!(code, EXIT_OK, "the first (forecast) exit is returned");
    }

    #[test]
    fn a_refused_first_render_never_prompts_even_when_otherwise_eligible() {
        // A non-EXIT_OK first render (here: refused, exit 2) has nothing to
        // confirm — there is no valid plan to apply.
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let confirm_called = std::cell::Cell::new(false);
        let rerun_called = std::cell::Cell::new(false);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                refused_output(),
                true,
                true,
                || {
                    confirm_called.set(true);
                    Ok(true)
                },
                || {
                    rerun_called.set(true);
                    forecast_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert!(!confirm_called.get(), "a refusal must never prompt");
        assert!(!rerun_called.get(), "a refusal must never re-run");
        assert_eq!(code, EXIT_USAGE, "the refusal's own exit is returned");
    }

    #[test]
    fn non_tty_stdin_never_prompts_even_when_otherwise_eligible() {
        // The piped/non-interactive contract: today's forecast-plus-hint
        // behavior, untouched.
        let global = global_args();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let confirm_called = std::cell::Cell::new(false);
        let rerun_called = std::cell::Cell::new(false);
        let code = {
            let mut presenter = Presenter::new(&mut out, &mut err);
            confirm_and_finish(
                forecast_output(),
                true,
                /* stdin_is_tty */ false,
                || {
                    confirm_called.set(true);
                    Ok(true)
                },
                || {
                    rerun_called.set(true);
                    forecast_output()
                },
                &global,
                &mut presenter,
            )
        };
        assert!(!confirm_called.get(), "non-tty stdin must never prompt");
        assert!(!rerun_called.get(), "non-tty stdin must never re-run");
        assert_eq!(code, EXIT_OK);
    }

    // ── render IO-error policy (NRN-372) ───────────────────────────────────
    //
    // One policy, every render path: BrokenPipe is a silent success (the
    // standard `norn find | head` shape); every other IO error is a `norn:
    // <e>` diagnostic plus the operational exit. `FailingWriter` stands in for
    // a stdout/stderr that can't accept another byte (closed pipe, full disk,
    // …) — these prove the policy at the shared helper and at `emit` itself;
    // each verb's own render path is pinned alongside its other tests in its
    // own module under `render`.

    #[test]
    fn render_outcome_tolerates_broken_pipe_as_success() {
        let mut err = Vec::new();
        let result: io::Result<i32> = Err(io::Error::from(io::ErrorKind::BrokenPipe));
        assert_eq!(render_outcome(result, &mut err), EXIT_OK);
        assert!(err.is_empty(), "broken pipe must not print a diagnostic");
    }

    #[test]
    fn render_outcome_reports_other_io_errors_operationally() {
        let mut err = Vec::new();
        let result: io::Result<i32> = Err(io::Error::other("disk full"));
        assert_eq!(render_outcome(result, &mut err), EXIT_OPERATIONAL);
        assert_eq!(String::from_utf8(err).unwrap(), "norn: disk full\n");
    }

    #[test]
    fn render_outcome_passes_through_the_success_code() {
        let mut err = Vec::new();
        assert_eq!(render_outcome(Ok(EXIT_USAGE), &mut err), EXIT_USAGE);
        assert!(err.is_empty());
    }

    #[test]
    fn emit_line_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            emit(Ok(Output::Line("ok".into())), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn emit_line_reports_other_io_errors() {
        let mut err = Vec::new();
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            emit(Ok(Output::Line("ok".into())), &global, &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn emit_usage_tolerates_broken_pipe_on_stderr() {
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(Vec::new(), FailingWriter(io::ErrorKind::BrokenPipe));
            emit(
                Ok(Output::Usage(b"usage text".to_vec())),
                &global,
                &mut presenter,
            )
        };
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn emit_usage_reports_other_io_errors_on_stderr() {
        // Usage text writes to stderr; a genuine IO failure there still needs
        // the norn: diagnostic and the operational exit — sharing stderr with
        // the diagnostic path doesn't exempt Usage from the policy.
        let global = global_args();
        let code = {
            let mut presenter =
                Presenter::new(Vec::new(), FailingWriter(io::ErrorKind::PermissionDenied));
            emit(
                Ok(Output::Usage(b"usage text".to_vec())),
                &global,
                &mut presenter,
            )
        };
        assert_eq!(code, EXIT_OPERATIONAL);
    }
}
