//! Shared pager subprocess spawn. Used by `find` records output and by `--help`
//! long-form rendering. Honors `$PAGER`; defaults to `less -FRX` (-F quit if
//! fits, -R raw ANSI, -X no init/deinit).

use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

pub fn should_page(buffer_line_count: usize, no_pager: bool, stdout_is_tty: bool) -> bool {
    if no_pager || !stdout_is_tty {
        return false;
    }
    let term_height = terminal_size::terminal_size()
        .map(|(_, h)| h.0 as usize)
        .unwrap_or(24);
    buffer_line_count > term_height.saturating_sub(2)
}

pub fn resolve_pager() -> (String, Vec<String>) {
    match env::var("PAGER") {
        Ok(p) if !p.is_empty() => {
            let mut parts = p.split_whitespace().map(String::from);
            let cmd = parts.next().unwrap_or_else(|| "less".to_string());
            let args: Vec<String> = parts.collect();
            (cmd, args)
        }
        _ => ("less".to_string(), vec!["-FRX".to_string()]),
    }
}

pub fn spawn_pager_or_passthrough(
    buffer: &[u8],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    context: &str,
) -> std::io::Result<()> {
    let (cmd, args) = resolve_pager();
    let mut child = match Command::new(&cmd).args(&args).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            writeln!(
                stderr,
                "{context}: pager '{}' failed: {}; writing directly to terminal",
                cmd, e
            )?;
            stdout.write_all(buffer)?;
            return Ok(());
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(e) = stdin.write_all(buffer) {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
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

    #[test]
    fn no_pager_flag_disables() {
        assert!(!should_page(1000, true, true));
    }

    #[test]
    fn non_tty_disables() {
        assert!(!should_page(1000, false, false));
    }

    #[test]
    fn short_output_skips_pager() {
        assert!(!should_page(5, false, true));
    }
}
