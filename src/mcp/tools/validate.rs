//! `vault.validate` — validate vault graph facts and configured frontmatter/link rules.
//!
//! The pure handler drives the same pipeline as `norn validate`:
//!
//! 1. Reconstruct the `GraphIndex` via `VaultContext::load_graph_index` (warm-
//!    connection reuse under the daemon, fresh open in cold mode) with an
//!    implicit incremental cache refresh. `files.ignore` is enforced at
//!    cache-build time, so the index arrives already filtered.
//! 2. Call `validate_with_compiled` with the context's current config
//!    (`ctx.config()`; hot-swapped in warm mode).
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

use crate::env::{RequestScope, VaultContext};
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

    /// Return the grouped finding-count rollup (by code / severity / rule /
    /// field / path-prefix) instead of the raw findings list — the structured
    /// analogue of `norn validate --summary`. When set, `findings` is empty and
    /// `summary` carries the rollup. Triage filters still apply first.
    #[serde(default)]
    pub summary: bool,
}

/// Structured output for `vault.validate`.
///
/// rmcp requires a root `type: object` schema; this typed envelope provides it
/// while keeping the per-finding payload as generic `serde_json::Value`
/// (because `Finding` carries `Utf8PathBuf` which has no `JsonSchema` impl —
/// see module docs).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ValidateOutput {
    /// Validation findings, filtered by any supplied triage predicates. Each
    /// entry is the JSON form of a `Finding`: `{code, severity, path, message,
    /// …}` with the finding-specific body flattened in. An **empty array** means
    /// the vault is clean (or all findings were filtered out).
    ///
    /// **Absent** (F3) in `summary` mode: the findings are rolled up into
    /// `summary` instead, and this field is omitted from the JSON entirely — so
    /// `findings: []` unambiguously means CLEAN, never "a dirty vault under
    /// summary". Present (possibly empty) in the default projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<serde_json::Value>>,

    /// The grouped finding-count rollup, present only when `summary: true` was
    /// requested. Byte-for-byte the same shape `norn validate --summary --format
    /// json` emits: `{findings, codes, severities, rules, fields,
    /// disallowed_values, invalid_types, path_prefixes}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<serde_json::Value>,
}

/// Pure handler for `vault.validate`.
///
/// Reconstructs the graph index via `VaultContext::load_graph_index` — warm-
/// connection reuse under the daemon, a fresh open in cold mode, with the same
/// lock-timeout-tolerant cache refresh either way. `files.ignore` is enforced
/// at cache-build time (`Cache::open_with_index`, NRN-117), so the loaded index
/// is already filtered — consistent with `norn validate` on vaults that
/// configure `files.ignore`.
pub fn handle(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: ValidateParams,
) -> Result<ValidateOutput> {
    // Route through VaultContext::load_graph_index — the graph-index entry point
    // for daemon-served tools. It reuses the warm connection when served by the
    // daemon (verify-once) and opens fresh in cold mode, but applies `files.ignore`
    // via `Cache::open_with_index` in both, so the index is filtered identically
    // to `norn validate` (NRN-130).
    let config = scope.config();
    let index = ctx.load_graph_index(scope)?;

    // Run validation using the request's bound config (`scope.config()`;
    // hot-swapped in warm mode, bound at the request boundary — NRN-253) — same
    // config path as `norn validate`.
    let findings = validate_with_compiled(
        &index,
        &config.validate,
        &config.compiled,
        config.index_options.alias_field.as_deref(),
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

    // `--summary` projection: roll the (post-filter) findings up into grouped
    // counts and return that instead of the raw list. Reuses the CLI's
    // `standards::summarize` — the same function `norn validate --summary`'s JSON
    // renderer calls — so the two rollups cannot drift.
    if p.summary {
        let rollup = crate::standards::summarize(&filtered);
        return Ok(ValidateOutput {
            // F3: omit `findings` entirely in summary mode. Emitting `[]` here
            // would be indistinguishable from a CLEAN vault; a dirty vault under
            // `--summary` must not present an empty findings array.
            findings: None,
            summary: Some(serde_json::to_value(&rollup)?),
        });
    }

    // Serialize each Finding to a serde_json::Value.
    let findings = filtered
        .into_iter()
        .map(|f| serde_json::to_value(&f))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ValidateOutput {
        findings: Some(findings),
        summary: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::mcp::tools::scoped_shim! {
        fn handle(ValidateParams) -> ValidateOutput;
    }
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert!(
            !findings.is_empty(),
            "expected at least one finding for a broken wikilink, got 0"
        );

        let link_finding = findings
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
                    findings
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert_eq!(
            findings.len(),
            0,
            "clean vault with no broken links should yield 0 findings, got: {findings:?}"
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert!(
            !findings.is_empty(),
            "code filter link-target-missing should still return findings for a broken link"
        );
        for f in findings {
            assert_eq!(
                f["code"], "link-target-missing",
                "code filter should only return link-target-missing findings, got: {f}"
            );
        }
    }

    /// NRN-182: `summary: true` returns the grouped rollup (not the raw list).
    /// `findings` is empty and `summary` carries per-code / per-severity counts
    /// for the same post-filter finding set.
    #[test]
    fn handle_summary_returns_grouped_rollup() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            ValidateParams {
                summary: true,
                ..ValidateParams::default()
            },
        )
        .expect("handle with summary should succeed");

        // F3: summary mode OMITS the findings field entirely (None), so the raw
        // list is never present as an empty `[]` that would read as CLEAN.
        assert!(
            out.findings.is_none(),
            "summary mode must omit the findings field, got: {:?}",
            out.findings
        );
        let summary = out
            .summary
            .as_ref()
            .expect("summary mode must populate the summary rollup");

        // F6: the fixture has exactly ONE broken link and no schema rules, so the
        // rollup totals are exact — not merely `>= 1`.
        assert_eq!(
            summary["findings"].as_u64().unwrap_or(0),
            1,
            "summary.findings must count exactly the one broken link: {summary}"
        );
        assert_eq!(
            summary["codes"]["link-target-missing"]
                .as_u64()
                .unwrap_or(0),
            1,
            "summary.codes must tally exactly one link-target-missing: {summary}"
        );
    }

    /// F3 serialization contract: the `findings` key is ABSENT from the serialized
    /// output in summary mode, and PRESENT (as an array) in the default mode — so
    /// a client cannot confuse a summarized dirty vault with a clean one.
    #[test]
    fn summary_mode_omits_findings_key_in_serialized_output() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let summary_out = handle(
            &ctx,
            ValidateParams {
                summary: true,
                ..ValidateParams::default()
            },
        )
        .expect("summary handle should succeed");
        let summary_json = serde_json::to_value(&summary_out).expect("serialize summary output");
        assert!(
            summary_json.get("findings").is_none(),
            "summary mode must omit the `findings` key from serialized output, got: {summary_json}"
        );
        assert!(
            summary_json.get("summary").is_some(),
            "summary mode must carry the `summary` key: {summary_json}"
        );

        let default_out =
            handle(&ctx, ValidateParams::default()).expect("default handle should succeed");
        let default_json = serde_json::to_value(&default_out).expect("serialize default output");
        assert!(
            default_json.get("findings").is_some(),
            "default mode must carry the `findings` key: {default_json}"
        );
        assert!(
            default_json.get("summary").is_none(),
            "default mode must omit the `summary` key: {default_json}"
        );
    }

    /// Companion: without `summary`, the raw findings list is returned and the
    /// summary rollup is absent — the default projection is unchanged.
    #[test]
    fn handle_without_summary_omits_rollup() {
        let (_tmp, root) = vault_with_broken_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(&ctx, ValidateParams::default()).expect("handle should succeed");
        assert!(
            out.findings.as_ref().is_some_and(|f| !f.is_empty()),
            "default mode must return the raw findings list"
        );
        assert!(
            out.summary.is_none(),
            "default mode must not populate the summary rollup"
        );
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert_eq!(
            findings.len(),
            0,
            "filter for frontmatter-required-field-missing should return 0 findings in a link-only vault"
        );
    }

    /// Regression test: `files.ignore` must be applied by the handler so that
    /// broken wikilinks inside ignored directories do NOT appear in findings.
    ///
    /// Historically, `handle` opened the cache without threading the config's
    /// ignore patterns into `Cache::open_with_index`, so a broken link under
    /// `templates/` surfaced as a `link-target-missing` finding even when the
    /// vault config declared `files.ignore: ["templates/**"]`.
    ///
    /// Today the handler routes through `VaultContext::load_graph_index`, which
    /// opens the cache with the config's ignore patterns; `files.ignore` is
    /// enforced at cache-BUILD time (the scan gate, NRN-117), so ignored docs
    /// never enter the rows the index is reconstructed from — same behavior as
    /// `norn validate`.
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert_eq!(
            findings.len(),
            0,
            "broken wikilink in files.ignore-d directory must produce 0 findings; \
             got {} finding(s): {:?}",
            findings.len(),
            findings
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

        let findings = out
            .findings
            .as_ref()
            .expect("default mode must return a findings list");
        assert!(
            !findings.is_empty(),
            "broken wikilink in non-ignored doc must still produce findings when files.ignore \
             is configured; got 0 findings"
        );

        let has_link_finding = findings.iter().any(|f| {
            f["code"]
                .as_str()
                .map(|c| c.starts_with("link-"))
                .unwrap_or(false)
        });
        assert!(
            has_link_finding,
            "expected a link-* finding for the broken wikilink outside the ignored directory, \
             got: {:?}",
            findings
                .iter()
                .map(|f| f["code"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );
    }
}
