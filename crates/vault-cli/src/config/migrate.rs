//! `vault config migrate` — migrate the config file to the schema version
//! this build understands. In v1 the only known schema is `1`, so migrate
//! is a deliberate no-op that reserves the verb for future schema bumps:
//! a v2 schema can land without users having to learn a new command.
//!
//! Exit codes follow the rest of the config family but compressed (no
//! finding rendering, single decision):
//!
//! - `0` — config already on the current schema version. Prints a
//!   `Nothing to migrate` line so agents can confirm the no-op happened.
//! - `1` — discovery / read / parse failure, or an unknown schema version
//!   this build has no migration path for. Surfaced via the standard
//!   `anyhow` error path (main maps `Err` to exit 1).
//!
//! When a future schema lands, the unknown-version branch grows into a
//! real migration: read v1 → transform → write v2 → succeed.

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};

use crate::config::discover;
use vault_standards::{parse_config, CURRENT_SCHEMA_VERSION};

fn write_noop(out: &mut dyn std::io::Write, version: u32) -> std::io::Result<()> {
    writeln!(out, "config is on schema v{version} — nothing to migrate")
}

/// Run `vault config migrate`. Returns the process exit code.
pub fn run(cwd: &Utf8Path, config_override: Option<&Utf8PathBuf>) -> Result<i32> {
    let discovery = discover(cwd, config_override)?;
    let yaml = std::fs::read_to_string(&discovery.config_file)?;
    let cfg = parse_config(&yaml, &discovery.config_file)?;
    if cfg.version == CURRENT_SCHEMA_VERSION {
        let stdout = std::io::stdout();
        write_noop(&mut stdout.lock(), cfg.version)?;
        Ok(0)
    } else {
        Err(anyhow!(
            "config schema v{} has no migration path in this build",
            cfg.version
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_message_is_lowercase_with_em_dash() {
        let expected = "config is on schema v1 — nothing to migrate\n";
        let mut buf = Vec::new();
        write_noop(&mut buf, 1).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }
}
