//! `vault config validate` — validate the config file itself (distinct
//! from `vault validate`, which validates vault content against the rules
//! in the config).
//!
//! Findings share the shape `{code, severity, path, message}` with
//! `vault validate` so agents can handle both with one parser. Exit codes:
//!
//! - `0` — clean (no findings).
//! - `1` — warnings only.
//! - `2` — at least one error finding (parse error, unknown schema
//!   version, deprecated key, etc.).
//! - `3` — config file missing or unreadable. Distinct from `2` so callers
//!   can branch on "no config to validate" vs "config exists but is
//!   broken."

use std::io::Write;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use serde_json::{json, Value};
use vault_standards::{parse_config, CURRENT_SCHEMA_VERSION};

use crate::cli::{ConfigValidateArgs, OutputFormat};
use crate::config::discover;

/// One validation finding. Shape mirrors `vault validate` so agents can
/// parse output from both commands with the same code path.
#[derive(Debug, Serialize)]
struct Finding {
    code: &'static str,
    severity: &'static str,
    path: String,
    message: String,
}

/// Severity ranks used to compute the process exit code from a list of
/// findings. `max_severity` over all findings drives the exit: 0 → clean,
/// 1 → warnings, 2 → errors.
const SEVERITY_CLEAN: u8 = 0;
const SEVERITY_WARNING: u8 = 1;
const SEVERITY_ERROR: u8 = 2;

/// Run `vault config validate`. Returns the process exit code.
pub fn run(
    cwd: &Utf8Path,
    config_override: Option<&Utf8PathBuf>,
    args: &ConfigValidateArgs,
) -> Result<i32> {
    // Missing/unreadable config → exit 3 (distinct from error findings).
    // We deliberately swallow the discover error here; `vault config show`
    // surfaces the same condition as exit 1 via the standard error path,
    // but validate's job is to *report* on the config, so "no config" is a
    // first-class outcome with its own exit code.
    let discovery = match discover(cwd, config_override) {
        Ok(d) => d,
        Err(_) => return Ok(3),
    };

    let yaml = match std::fs::read_to_string(&discovery.config_file) {
        Ok(y) => y,
        Err(_) => return Ok(3),
    };

    let (findings, max_severity) = collect_findings(&yaml, &discovery.config_file);

    let format = args.format.unwrap_or(OutputFormat::Table);
    let mut stdout = std::io::stdout().lock();
    render(&findings, format, &mut stdout)?;

    Ok(match max_severity {
        SEVERITY_CLEAN => 0,
        SEVERITY_WARNING => 1,
        _ => 2,
    })
}

/// Parse the YAML and accumulate findings. Returns the findings plus the
/// max severity observed so the caller can map to an exit code without
/// rescanning. Separated from `run` so the parser logic can grow (more
/// finding codes, warnings) without touching IO.
fn collect_findings(yaml: &str, config_path: &Utf8Path) -> (Vec<Finding>, u8) {
    let mut findings: Vec<Finding> = Vec::new();
    let mut max_severity: u8 = SEVERITY_CLEAN;

    match parse_config(yaml, config_path) {
        Err(e) => {
            findings.push(Finding {
                code: "config-parse-error",
                severity: "error",
                path: config_path.to_string(),
                message: format!("{e}"),
            });
            max_severity = max_severity.max(SEVERITY_ERROR);
        }
        Ok(cfg) => {
            if cfg.version != CURRENT_SCHEMA_VERSION {
                findings.push(Finding {
                    code: "unknown-schema-version",
                    severity: "error",
                    path: config_path.to_string(),
                    message: format!(
                        "config has version {} but this build only recognizes {}",
                        cfg.version, CURRENT_SCHEMA_VERSION
                    ),
                });
                max_severity = max_severity.max(SEVERITY_ERROR);
            }
        }
    }

    (findings, max_severity)
}

/// Render findings in the requested format. Records / paths print one
/// line per finding (`severity [code] path: message`) or `config is clean`
/// when there are none; JSON / JSONL emit machine-readable shapes.
fn render(findings: &[Finding], format: OutputFormat, out: &mut dyn Write) -> Result<()> {
    match format {
        OutputFormat::Json => {
            let payload = json_payload(findings);
            writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)?;
        }
        OutputFormat::Jsonl => {
            // NDJSON: one finding per line. When there are zero findings,
            // jsonl emits zero lines — the absence of output IS the signal,
            // mirroring how `vault validate --format jsonl` behaves on a
            // clean vault.
            for f in findings {
                writeln!(out, "{}", serde_json::to_string(f)?)?;
            }
        }
        OutputFormat::Table | OutputFormat::Paths => {
            if findings.is_empty() {
                writeln!(out, "config is clean")?;
            } else {
                for f in findings {
                    writeln!(out, "{} [{}] {}: {}", f.severity, f.code, f.path, f.message)?;
                }
            }
        }
    }
    Ok(())
}

/// Build the JSON payload (an object with `findings: [...]`). Wrapping
/// the array in an object leaves room to add summary fields (counts,
/// schema version probed) without breaking existing parsers.
fn json_payload(findings: &[Finding]) -> Value {
    json!({ "findings": findings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_findings_clean_config_is_empty() {
        let yaml = "version: 1\nfiles:\n  ignore: []\n";
        let (findings, max) = collect_findings(yaml, Utf8Path::new("/v/.vault/config.yaml"));
        assert!(findings.is_empty());
        assert_eq!(max, SEVERITY_CLEAN);
    }

    #[test]
    fn collect_findings_unknown_version_emits_unknown_schema_version() {
        let yaml = "version: 99\nfiles:\n  ignore: []\n";
        let (findings, max) = collect_findings(yaml, Utf8Path::new("/v/.vault/config.yaml"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "unknown-schema-version");
        assert_eq!(findings[0].severity, "error");
        assert_eq!(max, SEVERITY_ERROR);
    }

    #[test]
    fn collect_findings_unknown_field_emits_config_parse_error() {
        let yaml = "version: 1\nbogus: true\n";
        let (findings, max) = collect_findings(yaml, Utf8Path::new("/v/.vault/config.yaml"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "config-parse-error");
        assert_eq!(findings[0].severity, "error");
        assert_eq!(max, SEVERITY_ERROR);
    }

    #[test]
    fn render_json_emits_findings_array() {
        let findings = vec![Finding {
            code: "unknown-schema-version",
            severity: "error",
            path: "/v/.vault/config.yaml".into(),
            message: "msg".into(),
        }];
        let mut buf = Vec::new();
        render(&findings, OutputFormat::Json, &mut buf).unwrap();
        let parsed: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["findings"][0]["code"], "unknown-schema-version");
        assert_eq!(parsed["findings"][0]["severity"], "error");
        assert_eq!(parsed["findings"][0]["path"], "/v/.vault/config.yaml");
    }

    #[test]
    fn render_jsonl_emits_one_line_per_finding() {
        let findings = vec![
            Finding {
                code: "a",
                severity: "error",
                path: "/x".into(),
                message: "m1".into(),
            },
            Finding {
                code: "b",
                severity: "warning",
                path: "/x".into(),
                message: "m2".into(),
            },
        ];
        let mut buf = Vec::new();
        render(&findings, OutputFormat::Jsonl, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["code"], "a");
    }

    #[test]
    fn render_table_clean_prints_clean_message() {
        let findings: Vec<Finding> = Vec::new();
        let mut buf = Vec::new();
        render(&findings, OutputFormat::Table, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("config is clean"));
    }

    #[test]
    fn render_table_with_findings_prints_one_per_line() {
        let findings = vec![Finding {
            code: "unknown-schema-version",
            severity: "error",
            path: "/v/.vault/config.yaml".into(),
            message: "version 99 unknown".into(),
        }];
        let mut buf = Vec::new();
        render(&findings, OutputFormat::Table, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text
            .contains("error [unknown-schema-version] /v/.vault/config.yaml: version 99 unknown"));
    }
}
