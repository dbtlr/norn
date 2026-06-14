//! `vault.validate` — validate vault graph facts and configured frontmatter/link rules.
//!
//! The pure handler drives the same pipeline as `norn validate`:
//!
//! 1. Reconstruct the `GraphIndex` via `cache_cmd::load_graph_index`, applying
//!    `files.ignore` patterns and an implicit incremental cache refresh.
//! 2. Call `validate_with_compiled` with the warm server-lifetime config.
//! 3. Filter findings via `filter_findings` (triage filters from params).
//! 4. Serialize each `Finding` as `serde_json::Value` into the output envelope.
//!
//! **No seam extraction needed:** the validate logic is already expressed through
//! the public functions `validate_with_compiled` (in `src/standards/engine.rs`) and
//! `filter_findings` (in `src/validate_filter.rs`). `main.rs` calls exactly these
//! two functions in sequence; we replicate that call chain here without touching
//! `main.rs` or the validate render path.
//!
//! **Finding serialization:** `Finding` carries `Utf8PathBuf`, which has no
//! `schemars::JsonSchema` impl. We avoid deriving `JsonSchema` on `Finding` and
//! instead serialize each finding to `serde_json::Value` before placing it in the
//! typed `ValidateOutput` envelope. The envelope root is `type: object`, satisfying
//! rmcp 1.7.0's `outputSchema` constraint.
//!
//! **`trim_diagnostics`:** the CLI applies `trim_diagnostics` (drops verbose
//! parse-level diagnostics unless `--verbose`) before validating. The MCP tool
//! skips this — diagnostics round-trip through cache rows as pre-parsed, and the
//! verbose/concise distinction is a CLI presentation concern, not a semantic one.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cache_cmd::load_graph_index;
use crate::mcp::context::VaultContext;
use crate::standards::engine::validate_with_compiled;
use crate::validate_filter::{filter_findings, ValidateFilterOptions};

/// Parameters for `vault.validate`.
///
/// Mirrors the agent-relevant triage filters from `norn validate`'s
/// `ValidateTriageArgs`: path scope and code filter are the most common
/// agent use-cases. All filters are optional; omitted → no filter applied
/// (return all findings).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct ValidateParams {
    /// Filter findings by code. Comma-separated values match any listed code.
    /// Supports glob patterns (e.g. `link-*` matches all link findings).
    #[serde(default)]
    pub code: Vec<String>,

    /// Filter findings by severity (`warning` or `error`). Comma-separated.
    #[serde(default)]
    pub severity: Vec<String>,

    /// Filter findings by frontmatter field name. Comma-separated.
    #[serde(default)]
    pub field: Vec<String>,

    /// Filter findings by validate rule name. Comma-separated.
    #[serde(default)]
    pub rule: Vec<String>,

    /// Filter findings by vault-relative path glob. Comma-separated.
    #[serde(default)]
    pub path: Vec<String>,

    /// Filter link findings by link target. Comma-separated.
    #[serde(default)]
    pub target: Vec<String>,

    /// Filter link findings by unresolved reason. Comma-separated.
    #[serde(default)]
    pub reason: Vec<String>,
}

/// Structured output for `vault.validate`.
///
/// rmcp requires a root `type: object` schema; this typed envelope provides it
/// while keeping the per-finding payload as generic `serde_json::Value`
/// (because `Finding` carries `Utf8PathBuf` which has no `JsonSchema` impl —
/// see module docs).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ValidateOutput {
    /// Validation findings, filtered by any supplied triage predicates.
    /// Each entry is the JSON form of a `Finding`: `{code, severity, path,
    /// message, …}` with the finding-specific body flattened in. Empty array
    /// means the vault is clean (or all findings were filtered out).
    pub findings: Vec<serde_json::Value>,
}

/// Pure handler for `vault.validate`.
///
/// Reconstructs the graph index via the same entry point the CLI validate uses
/// (`cache_cmd::load_graph_index`), which applies `files.ignore` patterns at
/// read time and handles lock-timeout-tolerant cache refresh. This keeps the
/// MCP tool consistent with `norn validate` on vaults that configure
/// `files.ignore`.
pub fn handle(ctx: &VaultContext, p: ValidateParams) -> Result<ValidateOutput> {
    // Route through cache_cmd::load_graph_index — the same entry point the CLI
    // validate uses — so that `files.ignore` patterns are applied via
    // `apply_ignore_filter`. The old `ctx.query_cache()? + cache.load_graph_index()`
    // path hardcoded `ignored_files: Vec::new()` and silently skipped the filter.
    let index = load_graph_index(&ctx.vault_root, &ctx.config.index_options, false)?;

    // Run validation using the warm server-lifetime config — same config path
    // as `norn validate` (load_config is called once at server start, held in ctx).
    let findings = validate_with_compiled(
        &index,
        &ctx.config.validate,
        &ctx.config.compiled,
        ctx.config.index_options.alias_field.as_deref(),
    );

    // Apply triage filters (code, severity, field, rule, path, target, reason).
    let filter_opts = ValidateFilterOptions {
        codes: &p.code,
        severities: &p.severity,
        fields: &p.field,
        rules: &p.rule,
        paths: &p.path,
        targets: &p.target,
        reasons: &p.reason,
    };
    let filtered = filter_findings(findings, &filter_opts)?;

    // Serialize each Finding to a serde_json::Value.
    let findings = filtered
        .into_iter()
        .map(|f| serde_json::to_value(&f))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ValidateOutput { findings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Vault with a broken wikilink: `source.md` links to `[[MissingTarget]]`
    /// which does not exist. This must produce at least one `link-target-missing`
    /// finding.
    fn vault_with_broken_link() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-validate-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("source.md"),
            "---\ntype: note\ntitle: Source\n---\n\nSee [[MissingTarget]] for details.\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn handle_broken_link_returns_link_target_missing_finding() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(&ctx, ValidateParams::default()).expect("handle should succeed");

        assert!(
            !out.findings.is_empty(),
            "expected at least one finding for a broken wikilink, got 0"
        );

        let link_finding = out
            .findings
            .iter()
            .find(|f| {
                f["code"]
                    .as_str()
                    .map(|c| c.starts_with("link-"))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected a link-* finding, got: {:?}",
                    out.findings
                        .iter()
                        .map(|f| f["code"].as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                )
            });

        assert_eq!(
            link_finding["code"], "link-target-missing",
            "broken wikilink to nonexistent target should produce link-target-missing, got: {link_finding}"
        );

        // Verify the finding carries the expected shape.
        assert!(
            link_finding.get("path").is_some(),
            "finding must carry a path: {link_finding}"
        );
        assert!(
            link_finding.get("message").is_some(),
            "finding must carry a message: {link_finding}"
        );
        assert!(
            link_finding.get("severity").is_some(),
            "finding must carry a severity: {link_finding}"
        );
    }

    #[test]
    fn handle_clean_vault_returns_no_findings() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-validate-clean-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("clean.md"),
            "---\ntype: note\ntitle: Clean Note\n---\n\nNo links here.\n",
        )
        .unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle(&ctx, ValidateParams::default()).expect("handle should succeed");

        assert_eq!(
            out.findings.len(),
            0,
            "clean vault with no broken links should yield 0 findings, got: {:?}",
            out.findings
        );
    }

    #[test]
    fn handle_code_filter_narrows_findings() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Filter to only link-target-missing codes.
        let out = handle(
            &ctx,
            ValidateParams {
                code: vec!["link-target-missing".into()],
                ..ValidateParams::default()
            },
        )
        .expect("handle with code filter should succeed");

        assert!(
            !out.findings.is_empty(),
            "code filter link-target-missing should still return findings for a broken link"
        );
        for f in &out.findings {
            assert_eq!(
                f["code"], "link-target-missing",
                "code filter should only return link-target-missing findings, got: {f}"
            );
        }
    }

    #[test]
    fn handle_non_matching_code_filter_returns_empty() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Filter to a code that never appears in this vault.
        let out = handle(
            &ctx,
            ValidateParams {
                code: vec!["frontmatter-required-field-missing".into()],
                ..ValidateParams::default()
            },
        )
        .expect("handle with non-matching filter should succeed");

        assert_eq!(
            out.findings.len(),
            0,
            "filter for frontmatter-required-field-missing should return 0 findings in a link-only vault"
        );
    }

    /// Regression test: `files.ignore` must be applied by the handler so that
    /// broken wikilinks inside ignored directories do NOT appear in findings.
    ///
    /// Before the fix, `handle` loaded the graph index via
    /// `cache.load_graph_index()` (the cache reader), which hardcodes
    /// `ignored_files: Vec::new()` and never calls `apply_ignore_filter`.
    /// Result: a broken link under `templates/` would surface as a
    /// `link-target-missing` finding even when the vault config declares
    /// `files.ignore: ["templates/**"]`.
    ///
    /// After the fix the handler routes through
    /// `cache_cmd::load_graph_index(…)` — the same entry point as
    /// `norn validate` — which calls `apply_ignore_filter` at read time.
    #[test]
    fn handle_files_ignore_suppresses_findings_from_ignored_directory() {
        // Build a vault with:
        //   - .norn/config.yaml  → files.ignore: ["templates/**"]
        //   - templates/tpl.md   → broken wikilink [[MissingTarget]]
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-validate-ignore-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        // Config: ignore the templates directory at index load time.
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            "files:\n  ignore:\n    - \"templates/**\"\n",
        )
        .unwrap();

        // A doc inside the ignored directory with a broken wikilink.
        let templates_dir = root.join("templates");
        std::fs::create_dir_all(&templates_dir).unwrap();
        std::fs::write(
            templates_dir.join("tpl.md"),
            "---\ntype: note\ntitle: Template\n---\n\nSee [[MissingTarget]] for details.\n",
        )
        .unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle(&ctx, ValidateParams::default())
            .expect("handle should succeed on ignored vault");

        assert_eq!(
            out.findings.len(),
            0,
            "broken wikilink in files.ignore-d directory must produce 0 findings; \
             got {} finding(s): {:?}",
            out.findings.len(),
            out.findings
                .iter()
                .map(|f| f["code"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );
    }

    /// Companion to the `files.ignore` regression: a broken link in a NON-ignored
    /// doc must still surface a finding even when `files.ignore` is configured.
    ///
    /// This guards against an overly-broad fix that drops ALL findings when an
    /// ignore pattern is present.
    #[test]
    fn handle_files_ignore_does_not_suppress_findings_outside_ignored_directory() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-validate-ignore-nonmatching-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        // Config: ignore the templates directory.
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            "files:\n  ignore:\n    - \"templates/**\"\n",
        )
        .unwrap();

        // A doc OUTSIDE the ignored directory with a broken wikilink — must still
        // produce a finding.
        std::fs::write(
            root.join("source.md"),
            "---\ntype: note\ntitle: Source\n---\n\nSee [[MissingTarget]] for details.\n",
        )
        .unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle(&ctx, ValidateParams::default())
            .expect("handle should succeed on vault with non-ignored broken link");

        assert!(
            !out.findings.is_empty(),
            "broken wikilink in non-ignored doc must still produce findings when files.ignore \
             is configured; got 0 findings"
        );

        let has_link_finding = out.findings.iter().any(|f| {
            f["code"]
                .as_str()
                .map(|c| c.starts_with("link-"))
                .unwrap_or(false)
        });
        assert!(
            has_link_finding,
            "expected a link-* finding for the broken wikilink outside the ignored directory, \
             got: {:?}",
            out.findings
                .iter()
                .map(|f| f["code"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );
    }
}
