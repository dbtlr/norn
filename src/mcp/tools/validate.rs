//! `vault.validate` — validate vault graph facts and configured frontmatter/link rules.
//!
//! The pure handler drives the same pipeline as `norn validate`:
//!
//! 1. Open a fresh query cache (per-call freshness, same as the CLI).
//! 2. Reconstruct the `GraphIndex` from cache rows via `cache.load_graph_index()`.
//! 3. Call `validate_with_compiled` with the warm server-lifetime config.
//! 4. Filter findings via `filter_findings` (triage filters from params).
//! 5. Serialize each `Finding` as `serde_json::Value` into the output envelope.
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
/// Opens a fresh query cache (per-call freshness), reconstructs the graph index,
/// runs validation with the server-lifetime compiled config, applies triage
/// filters, and returns the findings as serialized JSON values in the envelope.
pub fn handle(ctx: &VaultContext, p: ValidateParams) -> Result<ValidateOutput> {
    // Open cache (per-call freshness) and reconstruct the graph index.
    let cache = ctx.query_cache()?;
    let index = cache.load_graph_index()?;

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
}
