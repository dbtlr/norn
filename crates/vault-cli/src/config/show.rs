//! `vault config show` — render the effective config: discovery paths plus
//! per-section counts.
//!
//! Output format mirrors `vault find`'s display contract: TTY default
//! renders as records (key/value blocks), `--format json` emits a single
//! flat object (pretty-printed), `--format jsonl` emits the same object
//! on a single line (NDJSON: one record per line — `vault config show`
//! has one record), and `--format paths` is rejected — `paths` only
//! makes sense for multi-document queries.

use std::fs;
use std::io::{IsTerminal, Write};

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{json, Value};
use vault_standards::{parse_config, VaultConfig};

use crate::cli::{ConfigShowArgs, OutputFormat};
use crate::config::{discover, Discovery};

/// Snapshot of everything `vault config show` reports, decoupled from the
/// renderer choice. Building this once means records / json / jsonl all
/// pull from the same source-of-truth instead of re-deriving counts.
struct ShowSnapshot {
    file: Utf8PathBuf,
    vault_root: Utf8PathBuf,
    cache: Utf8PathBuf,
    version: u32,
    ignore_count: usize,
    required_count: usize,
    rule_count: usize,
    repair_rule_count: usize,
}

impl ShowSnapshot {
    /// (display_key, display_value) pairs in records order.
    ///
    /// The records shape is intentionally different from the JSON shape:
    /// records is operator-facing (flat key/value rows with embedded units
    /// like "2 patterns" / "3 fields") while JSON is agent-facing (nested
    /// by config section, raw integer counts under `files.ignore_count`,
    /// `validate.required_count`, etc., so machines can read the structure
    /// without parsing unit strings). Update the JSON builder in
    /// `render_json` separately from this list when the shape needs to
    /// change; they will diverge by design.
    fn pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("file", self.file.as_str().to_string()),
            ("vault_root", self.vault_root.as_str().to_string()),
            ("cache", self.cache.as_str().to_string()),
            ("version", self.version.to_string()),
            ("ignore", format!("{} patterns", self.ignore_count)),
            ("required", format!("{} fields", self.required_count)),
            ("rules", format!("{} rules", self.rule_count)),
            ("repair_rules", format!("{} rules", self.repair_rule_count)),
        ]
    }
}

/// Run `vault config show`. Returns the process exit code.
pub fn run(
    cwd: &Utf8Path,
    config_override: Option<&Utf8PathBuf>,
    args: &ConfigShowArgs,
) -> Result<i32> {
    let Discovery {
        config_file,
        vault_root,
        cache,
    } = discover(cwd, config_override)?;

    let yaml = fs::read_to_string(&config_file)
        .map_err(|e| anyhow!("failed to read config {config_file}: {e}"))?;
    let cfg = parse_config(&yaml, &config_file)?;
    let snapshot = build_snapshot(config_file, vault_root, cache, &cfg);

    let format = resolve_format(args.format);
    let stdout_is_tty = std::io::stdout().is_terminal();

    let mut buffer: Vec<u8> = Vec::new();
    match format {
        OutputFormat::Paths => {
            return Err(anyhow!(
                "paths format is not supported for `vault config show`"
            ));
        }
        OutputFormat::Json => render_json(&snapshot, &mut buffer)?,
        OutputFormat::Jsonl => render_jsonl(&snapshot, &mut buffer)?,
        OutputFormat::Table => render_records(&snapshot, &mut buffer)?,
    }

    let buffer_lines = buffer.iter().filter(|&&b| b == b'\n').count();
    let should_page = matches!(format, OutputFormat::Table)
        && crate::find::pager::should_page(buffer_lines, args.no_pager, stdout_is_tty);

    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if should_page {
        let stderr = std::io::stderr();
        let mut stderr_lock = stderr.lock();
        crate::find::pager::spawn_pager_or_passthrough(
            &buffer,
            &mut stdout_lock,
            &mut stderr_lock,
        )?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    Ok(0)
}

/// Default to records (`Table`) — `vault config show` always describes a
/// single config, so the path-style listing has no meaning, and JSON is
/// only chosen explicitly. TTY/pipe doesn't change the shape here.
fn resolve_format(explicit: Option<OutputFormat>) -> OutputFormat {
    explicit.unwrap_or(OutputFormat::Table)
}

fn build_snapshot(
    file: Utf8PathBuf,
    vault_root: Utf8PathBuf,
    cache: Utf8PathBuf,
    cfg: &VaultConfig,
) -> ShowSnapshot {
    ShowSnapshot {
        file,
        vault_root,
        cache,
        version: cfg.version,
        ignore_count: cfg.files.ignore.len(),
        required_count: cfg.validate.required_frontmatter.len(),
        rule_count: cfg.validate.rules.len(),
        repair_rule_count: cfg.repair.rules.len(),
    }
}

/// Records: key column right-padded to the longest key width, two spaces,
/// then value. Mirrors find's records contract for single-row output.
fn render_records(snapshot: &ShowSnapshot, out: &mut dyn Write) -> std::io::Result<()> {
    let pairs = snapshot.pairs();
    let key_width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in pairs {
        writeln!(out, "{:<width$}  {}", key, value, width = key_width)?;
    }
    Ok(())
}

/// Build the agent-facing JSON payload (nested by config section). Shared
/// between `--format json` (pretty) and `--format jsonl` (single line) so
/// the shapes stay identical.
fn json_payload(snapshot: &ShowSnapshot) -> Value {
    json!({
        "file": snapshot.file.as_str(),
        "vault_root": snapshot.vault_root.as_str(),
        "cache": snapshot.cache.as_str(),
        "version": snapshot.version,
        "files": { "ignore_count": snapshot.ignore_count },
        "validate": {
            "required_count": snapshot.required_count,
            "rule_count": snapshot.rule_count,
        },
        "repair": { "rule_count": snapshot.repair_rule_count },
    })
}

/// JSON: a flat object with discovery paths + nested per-section counts,
/// pretty-printed for human inspection.
fn render_json(snapshot: &ShowSnapshot, out: &mut dyn Write) -> std::io::Result<()> {
    let payload = json_payload(snapshot);
    writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)
}

/// JSONL: the same JSON payload emitted as a single line (no indentation).
/// Standard NDJSON contract: one record per line. `vault config show` has
/// one record, so JSONL output is exactly one line.
fn render_jsonl(snapshot: &ShowSnapshot, out: &mut dyn Write) -> std::io::Result<()> {
    let payload = json_payload(snapshot);
    writeln!(out, "{}", serde_json::to_string(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> ShowSnapshot {
        ShowSnapshot {
            file: Utf8PathBuf::from("/v/.vault/config.yaml"),
            vault_root: Utf8PathBuf::from("/v"),
            cache: Utf8PathBuf::from("/c/cache.db"),
            version: 1,
            ignore_count: 2,
            required_count: 3,
            rule_count: 0,
            repair_rule_count: 0,
        }
    }

    #[test]
    fn records_format_aligns_keys_to_longest() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        render_records(&snapshot, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // longest key is "repair_rules" (12 chars); other keys padded to 12.
        assert!(text.contains("file          /v/.vault/config.yaml"));
        assert!(text.contains("repair_rules  0 rules"));
    }

    #[test]
    fn json_format_emits_flat_object_with_nested_counts() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        render_json(&snapshot, &mut buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["file"], "/v/.vault/config.yaml");
        assert_eq!(parsed["vault_root"], "/v");
        assert_eq!(parsed["cache"], "/c/cache.db");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["files"]["ignore_count"], 2);
        assert_eq!(parsed["validate"]["required_count"], 3);
        assert_eq!(parsed["validate"]["rule_count"], 0);
        assert_eq!(parsed["repair"]["rule_count"], 0);
    }

    #[test]
    fn jsonl_format_emits_single_line_flat_object_matching_json_shape() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        render_jsonl(&snapshot, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // NDJSON: one record per line; `vault config show` has one record.
        assert_eq!(lines.len(), 1, "jsonl should emit exactly one line");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        // Shape matches `render_json` output exactly.
        assert_eq!(parsed["file"], "/v/.vault/config.yaml");
        assert_eq!(parsed["vault_root"], "/v");
        assert_eq!(parsed["cache"], "/c/cache.db");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["files"]["ignore_count"], 2);
        assert_eq!(parsed["validate"]["required_count"], 3);
        assert_eq!(parsed["validate"]["rule_count"], 0);
        assert_eq!(parsed["repair"]["rule_count"], 0);
    }
}
