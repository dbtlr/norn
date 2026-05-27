//! `vault config show` — render the effective config: discovery paths plus
//! per-section counts.
//!
//! Supported formats via `ConfigFormat`: `records` (default TTY), `json`
//! (pretty-printed single object), and `jsonl` (single NDJSON line).
//! Records output uses `output::primitives::record_block`: file path as header,
//! remaining keys as 2-indent field rows. Color follows the global `--color`
//! flag via `Palette::resolve`.

use std::fs;
use std::io::{IsTerminal, Write};

use crate::standards::{parse_config, VaultConfig};
use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{json, Value};

use crate::cli::{ConfigFormat, ConfigShowArgs};
use crate::config::{discover, Discovery};
use crate::output::palette::{self, Palette};
use crate::output::primitives::{record_block, Field};

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
    alias_field: Option<String>,
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
        let mut pairs = vec![
            ("file", self.file.as_str().to_string()),
            ("vault_root", self.vault_root.as_str().to_string()),
            ("cache", self.cache.as_str().to_string()),
            ("version", self.version.to_string()),
            ("ignore", format!("{} patterns", self.ignore_count)),
            ("required", format!("{} fields", self.required_count)),
            ("rules", format!("{} rules", self.rule_count)),
            ("repair_rules", format!("{} rules", self.repair_rule_count)),
        ];
        // links.alias_field is opt-in; surface it only when configured so
        // the row doesn't show a noisy "(unset)" for the common default.
        if let Some(field) = &self.alias_field {
            pairs.push(("alias_field", field.clone()));
        }
        pairs
    }
}

/// Run `vault config show`. Returns the process exit code.
pub fn run(
    cwd: &Utf8Path,
    config_override: Option<&Utf8PathBuf>,
    args: &ConfigShowArgs,
    color: crate::cli::ColorWhen,
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
    let palette = palette::resolve(color);

    let format = resolve_format(args.format);
    let stdout_is_tty = std::io::stdout().is_terminal();

    let mut buffer: Vec<u8> = Vec::new();
    match format {
        ConfigFormat::Json => render_json(&snapshot, &mut buffer)?,
        ConfigFormat::Jsonl => render_jsonl(&snapshot, &mut buffer)?,
        ConfigFormat::Records => render_records(&snapshot, &palette, &mut buffer)?,
    }

    let buffer_lines = buffer.iter().filter(|&&b| b == b'\n').count();
    let should_page = matches!(format, ConfigFormat::Records)
        && crate::output::pager::should_page(buffer_lines, args.no_pager, stdout_is_tty);

    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if should_page {
        let stderr = std::io::stderr();
        let mut stderr_lock = stderr.lock();
        crate::output::pager::spawn_pager_or_passthrough(
            &buffer,
            &mut stdout_lock,
            &mut stderr_lock,
            "vault config show",
        )?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    Ok(0)
}

/// Default to records — `vault config show` always describes a single config,
/// so path-style listing has no meaning, and JSON is only chosen explicitly.
/// TTY/pipe doesn't change the shape here.
fn resolve_format(explicit: Option<ConfigFormat>) -> ConfigFormat {
    explicit.unwrap_or(ConfigFormat::Records)
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
        alias_field: cfg.links.alias_field.clone(),
    }
}

/// Records: file path as header, then 2-indent field rows via `record_block`.
/// The `file` pair is promoted to the header and excluded from the field list.
fn render_records(
    snapshot: &ShowSnapshot,
    palette: &Palette,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    // Leading blank gives breathing room from the user's shell prompt.
    writeln!(out)?;
    let pairs = snapshot.pairs();
    // Drop the "file" pair — it becomes the header.
    let header = snapshot.file.as_str();
    let fields: Vec<Field<'_>> = pairs
        .iter()
        .filter(|(k, _)| *k != "file")
        .map(|(k, v)| Field {
            label: k,
            value: v.as_str(),
            highlight: false,
        })
        .collect();
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    record_block(out, palette, Some(header), &fields, term_width)
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
        "links": { "alias_field": snapshot.alias_field },
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
            file: Utf8PathBuf::from("/v/.norn/config.yaml"),
            vault_root: Utf8PathBuf::from("/v"),
            cache: Utf8PathBuf::from("/c/cache.db"),
            version: 1,
            ignore_count: 2,
            required_count: 3,
            rule_count: 0,
            repair_rule_count: 0,
            alias_field: None,
        }
    }

    #[test]
    fn records_format_emits_2_indent_field_rows() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        let palette = Palette::off();
        render_records(&snapshot, &palette, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // lines[0] = leading blank; lines[1] = header (config file path).
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "/v/.norn/config.yaml");
        // Subsequent lines are 2-indent fields.
        assert!(
            lines[2].starts_with("  "),
            "expected 2-indent, got: {:?}",
            lines[2]
        );
        // Longest remaining label is "repair_rules" (12); column width = 14.
        // Field rows include vault_root, cache, version, ignore, required, rules, repair_rules.
        assert!(text.contains("  repair_rules"));
        assert!(text.contains("  version"));
        assert!(text.contains("  vault_root"));
        // No "file" field row (file is now the header).
        assert!(
            !text.contains("\n  file "),
            "file should be header, not field"
        );
    }

    #[test]
    fn json_format_emits_flat_object_with_nested_counts() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        render_json(&snapshot, &mut buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["file"], "/v/.norn/config.yaml");
        assert_eq!(parsed["vault_root"], "/v");
        assert_eq!(parsed["cache"], "/c/cache.db");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["files"]["ignore_count"], 2);
        assert_eq!(parsed["validate"]["required_count"], 3);
        assert_eq!(parsed["validate"]["rule_count"], 0);
        assert_eq!(parsed["repair"]["rule_count"], 0);
        // links.alias_field surfaces nullably; None renders as JSON null so
        // agents can distinguish "unset" from any literal field name.
        assert!(parsed["links"]["alias_field"].is_null());
    }

    #[test]
    fn records_format_omits_alias_field_row_when_unset() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        let palette = Palette::off();
        render_records(&snapshot, &palette, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // alias_field is None — its row should be absent.
        assert!(
            !text.contains("alias_field"),
            "expected no alias_field row when unset, got: {text}"
        );
    }

    #[test]
    fn records_format_emits_alias_field_row_when_configured() {
        let mut snapshot = sample_snapshot();
        snapshot.alias_field = Some("aliases".to_string());
        let mut buf = Vec::new();
        let palette = Palette::off();
        render_records(&snapshot, &palette, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("  alias_field"),
            "expected alias_field row when configured, got: {text}"
        );
        assert!(
            text.contains("aliases"),
            "expected alias_field value 'aliases' in output, got: {text}"
        );
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
        assert_eq!(parsed["file"], "/v/.norn/config.yaml");
        assert_eq!(parsed["vault_root"], "/v");
        assert_eq!(parsed["cache"], "/c/cache.db");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["files"]["ignore_count"], 2);
        assert_eq!(parsed["validate"]["required_count"], 3);
        assert_eq!(parsed["validate"]["rule_count"], 0);
        assert_eq!(parsed["repair"]["rule_count"], 0);
        assert!(parsed["links"]["alias_field"].is_null());
    }
}
