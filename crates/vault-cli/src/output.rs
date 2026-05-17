use std::io::{self, Write};

use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;

pub fn write_output<T: Serialize>(items: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(items)?)?;
        }
        OutputFormat::Jsonl => {
            for item in items {
                write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
            }
        }
    }
    Ok(())
}

pub fn write_item_output<T: Serialize>(item: &T, format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(item)?)?;
        }
        OutputFormat::Jsonl => {
            write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
        }
    }
    Ok(())
}

pub fn write_json_line(stdout: &mut impl Write, json: &str) -> Result<()> {
    stdout.write_all(json.as_bytes())?;
    stdout.write_all(b"\n")?;
    Ok(())
}

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}
