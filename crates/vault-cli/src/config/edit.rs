use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;

use crate::cli::{ConfigEditArgs, ConfigValidateArgs};
use crate::config::{discover, validate};

pub fn run(
    cwd: &Utf8Path,
    config_override: Option<&Utf8PathBuf>,
    args: &ConfigEditArgs,
) -> Result<i32> {
    let discovery = discover(cwd, config_override)?;
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .ok_or_else(|| {
            anyhow!("set EDITOR or VISUAL to edit (e.g., EDITOR=nano vault config edit)")
        })?;

    let (program, leading_args) = parse_editor(&editor);
    let mut cmd = Command::new(program);
    for a in leading_args {
        cmd.arg(a);
    }
    cmd.arg(discovery.config_file.as_str());

    let status = cmd.status()?;
    let editor_code = status.code().unwrap_or(1);

    if args.no_validate {
        return Ok(editor_code);
    }

    // Editor failure: surface its exit code without running validate.
    if editor_code != 0 {
        return Ok(editor_code);
    }

    // Post-validate. Validate's exit code takes precedence over the
    // editor's 0 (errors in the saved config matter more than a
    // successful editor exit).
    let v_args = ConfigValidateArgs { format: None };
    validate::run(cwd, config_override, &v_args)
}

fn parse_editor(input: &str) -> (&str, Vec<&str>) {
    // Treat space-separated tokens as program + args. Good enough for
    // common values like "vim -p" or "code --wait".
    let mut parts = input.split_whitespace();
    let program = parts.next().unwrap_or("");
    let rest = parts.collect();
    (program, rest)
}
