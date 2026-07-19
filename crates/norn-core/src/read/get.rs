//! The `get` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! Ported from the donor `src/get/`: resolve each target (path / stem /
//! wikilink-shaped / alias), load its full connection set, resolve any
//! `--section` spans, then apply in-memory sort/paging over the resolved
//! records. Every resolved document carries its whole facet set (frontmatter,
//! headings, the three link sets, body, hash, stem); the CLI projects `--col` /
//! `--all-cols` / `--format` over that, so routing never narrows.
//!
//! # Error model
//!
//! Unlike `find`/`count`, target resolution failures are NOT request-rejecting
//! user errors — a missing or ambiguous target becomes a NOTE and the report
//! still returns (the CLI derives its exit-1 from an `error:`-prefixed note).
//! So [`execute`] returns `Ok(Ok(report))` on every well-formed request; only a
//! cache read failure is `Err(_)` (exit-to-heal, ADR 0017).
//!
//! # `--format markdown`
//!
//! ADR 0014: markdown is the exact source file, straight from disk, and does NOT
//! participate in the relational snapshot. [`execute`] still resolves the
//! target(s) (so the caller can count them), but the file read itself is the
//! owner's job (it holds the vault root and is co-located with the vault — a
//! future off-filesystem client could not read it). `markdown_content` is left
//! `None` here and filled by the owner.

use anyhow::Result;

use camino::{Utf8Path, Utf8PathBuf};
use norn_frontmatter::section::{resolve_section, SectionError};
use norn_wire::{GetParams, GetRecord, GetReport};

use crate::cache::Cache;
use crate::query::DocumentQuery;
use crate::read::connection_values;

/// Run a `get` request against the warm cache. See the module docs for the
/// note-not-reject error model.
pub fn execute(
    cache: &Cache,
    params: &GetParams,
    _today: &str,
) -> Result<Result<GetReport, String>> {
    let mut records: Vec<GetRecord> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // `--section` headings, deduped preserving first-occurrence order — a user
    // can repeat `--section X`, and without dedup the keyed JSON object collapses
    // the duplicate while records would emit two blocks (a cardinality mismatch).
    // The CLI already sends an empty list for formats that ignore `--section`
    // (`paths` / `markdown`), so this stays empty for them.
    let requested_sections = dedup_preserve_order(&params.sections);

    // Body is LOADED when it will be displayed OR when `--section` needs it to
    // resolve spans; it is RETURNED (on `GetRecord.body`) only when display was
    // asked for, so a body loaded solely for `--section` never leaks into output.
    let wants_body_display = params.with_body;
    let wants_body_load = wants_body_display || !requested_sections.is_empty();

    for raw in &params.targets {
        let resolved = resolve_target(cache, raw)?;
        if resolved.is_empty() {
            notes.push(format!("error: '{raw}' did not resolve to any doc"));
            continue;
        }
        if resolved.len() > 1 {
            notes.push(format!("note: '{raw}' resolved to {} docs", resolved.len()));
        }
        for path in &resolved {
            let Some(deep) = cache.document_with_connections(path.as_path(), wants_body_load)?
            else {
                notes.push(format!(
                    "error: '{path}' missing from cache after resolution"
                ));
                continue;
            };

            let sections = if requested_sections.is_empty() {
                None
            } else {
                Some(resolve_requested_sections(
                    deep.body.as_deref().unwrap_or(""),
                    &requested_sections,
                    &deep.path,
                    &mut notes,
                ))
            };

            let conns = connection_values(&deep)?;
            let body = if wants_body_display { deep.body } else { None };
            records.push(GetRecord {
                path: deep.path.to_string(),
                stem: deep.stem,
                hash: deep.hash,
                frontmatter: deep.frontmatter,
                headings: conns.headings,
                outgoing_links: conns.outgoing_links,
                unresolved_links: conns.unresolved_links,
                incoming_links: conns.incoming_links,
                body,
                sections,
            });
        }
    }

    // Sort / limit / paging are applied in-memory, post-resolution. Skipped for
    // markdown (a single byte-faithful doc that errors on >1 selected) so limit
    // can't mask a multi-select.
    if !params.markdown {
        apply_sort(
            &mut records,
            params.paging.sort.as_deref(),
            params.paging.desc,
        );
        apply_paging(&mut records, params.paging.starts_at, params.paging.limit);
    }

    Ok(Ok(GetReport {
        records,
        notes,
        markdown_content: None,
    }))
}

// ── Target resolution (donor `get/target.rs`) ────────────────────────────────

/// Normalize a user-supplied target string. Accepts a path, a stem, or a
/// wikilink-shaped string (`[[foo]]`, `[[foo#anchor]]`, `[[foo^block]]`,
/// `[[foo|alias]]`, brackets optional). Anchor / block-ref / pipe-alias suffixes
/// identify a position inside a doc, not which doc, so they are stripped.
pub fn normalize_target(raw: &str) -> &str {
    let trimmed = raw.trim();
    let core = if let Some(inner) = trimmed
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        inner.trim()
    } else {
        trimmed
    };
    let core = core.split('|').next().unwrap_or(core);
    let core = core.split(&['#', '^'][..]).next().unwrap_or(core);
    core.trim()
}

/// Resolve a target string to one-or-more document paths: exact-path probe, then
/// case-insensitive stem scan, then alias scan (only when the vault configures an
/// alias field). Empty result means "no match" (the caller notes the error);
/// more than one means an ambiguous stem (one record per candidate).
fn resolve_target(cache: &Cache, raw: &str) -> Result<Vec<Utf8PathBuf>> {
    let normalized = normalize_target(raw).to_string();
    if normalized.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Exact path — O(1) index lookup for the common "full vault-relative path".
    if cache
        .document_by_path(Utf8Path::new(&normalized))?
        .is_some()
    {
        return Ok(vec![Utf8PathBuf::from(normalized)]);
    }

    // 2. Stem fallback — one SELECT, case-insensitive stem match.
    let all = cache.documents_matching(&DocumentQuery::default())?;
    let stem_matches: Vec<Utf8PathBuf> = all
        .iter()
        .filter(|d| d.stem.eq_ignore_ascii_case(&normalized))
        .map(|d| d.path.clone())
        .collect();
    if !stem_matches.is_empty() {
        return Ok(stem_matches);
    }

    // 3. Alias fallback — only when the stem found nothing AND an alias field is
    //    configured. `parse_aliases` lowercases, so compare against the lowered
    //    target. Reuses the `all` set (still one SELECT total).
    if let Some(field) = cache.alias_field() {
        let target_lower = normalized.to_lowercase();
        let alias_matches: Vec<Utf8PathBuf> = all
            .iter()
            .filter(|d| {
                let (aliases, _) = crate::graph::parse_aliases(d.frontmatter.as_ref(), field);
                aliases.iter().any(|a| a == &target_lower)
            })
            .map(|d| d.path.clone())
            .collect();
        return Ok(alias_matches);
    }

    Ok(Vec::new())
}

// ── `--section` resolution (donor `get/mod.rs`) ──────────────────────────────

/// Resolve every requested `--section` heading against one document's body,
/// reusing the shared `resolve_section` primitive so a section READ mirrors a
/// section WRITE (identical spans + missing/ambiguous failure modes). A heading
/// that fails to resolve is warned and omitted; if NONE resolve, one `error:`
/// note is pushed (so this target counts toward the nonzero-exit contract) but
/// the record is still returned with an empty `sections`.
fn resolve_requested_sections(
    body: &str,
    headings: &[String],
    path: &Utf8Path,
    notes: &mut Vec<String>,
) -> Vec<(String, String)> {
    let mut resolved = Vec::with_capacity(headings.len());
    for heading in headings {
        match resolve_section(body, heading) {
            Ok(span) => {
                resolved.push((
                    heading.clone(),
                    body[span.heading_start..span.end].to_string(),
                ));
            }
            Err(SectionError::HeadingNotFound { .. }) => {
                notes.push(format!(
                    "warning: --section heading '{heading}' not found in '{path}'"
                ));
            }
            Err(SectionError::HeadingAmbiguous { count, .. }) => {
                notes.push(format!(
                    "warning: --section heading '{heading}' is ambiguous ({count} matches) in '{path}'"
                ));
            }
        }
    }
    if resolved.is_empty() {
        notes.push(format!(
            "error: none of the requested --section headings resolved for '{path}'"
        ));
    }
    resolved
}

/// Deduplicate preserving first-occurrence order (collapse repeated `--section H`).
fn dedup_preserve_order(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert(item.as_str()))
        .cloned()
        .collect()
}

// ── In-memory sort / paging (donor `get/mod.rs`) ─────────────────────────────

/// Stably sort by `field` (a frontmatter key, or the identity `path` / `stem`).
/// Records missing the field sort last in both directions; `desc` reverses only
/// the present/present comparison.
fn apply_sort(records: &mut [GetRecord], field: Option<&str>, desc: bool) {
    let Some(field) = field else { return };

    let key = |r: &GetRecord| -> Option<String> {
        if field == "path" {
            return Some(r.path.clone());
        }
        if field == "stem" {
            return Some(r.stem.clone());
        }
        r.frontmatter
            .as_ref()
            .and_then(|fm| fm.as_object())
            .and_then(|obj| obj.get(field))
            .map(sort_key_string)
    };

    records.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        let ord = match (&ka, &kb) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        match (&ka, &kb) {
            (Some(_), Some(_)) if desc => ord.reverse(),
            _ => ord,
        }
    });
}

/// Render a frontmatter value to the string the sort compares on (donor
/// `json_value_inline`): scalars verbatim, arrays comma-joined.
fn sort_key_string(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(arr) => arr
            .iter()
            .map(sort_key_string)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => v.to_string(),
    }
}

/// Apply the 1-indexed `starts_at` offset, then an optional `limit`.
fn apply_paging(records: &mut Vec<GetRecord>, starts_at: usize, limit: Option<usize>) {
    let offset = starts_at.saturating_sub(1);
    if offset > 0 {
        records.drain(..offset.min(records.len()));
    }
    if let Some(n) = limit {
        records.truncate(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-18";

    fn synth() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\ntitle: A\n---\n# A heading\n[[b]]\n## One\nfirst\n## Two\nsecond\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntype: note\ntitle: B\n---\n# B heading\n[[a]]\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn params(targets: &[&str]) -> GetParams {
        GetParams {
            targets: targets.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn normalize_strips_wikilink_and_suffixes() {
        assert_eq!(normalize_target("[[foo#section|Alias]]"), "foo");
        assert_eq!(normalize_target("foo^abc-123"), "foo");
        assert_eq!(normalize_target("  Notes/a.md  "), "Notes/a.md");
    }

    #[test]
    fn single_target_carries_full_facets() {
        let (_t, root) = synth();
        let cache = built(&root);
        let report = execute(&cache, &params(&["a.md"]), TODAY).unwrap().unwrap();
        assert_eq!(report.records.len(), 1);
        let r = &report.records[0];
        assert_eq!(r.path, "a.md");
        assert!(r.headings.iter().any(|h| h["text"] == "A heading"));
        assert!(r.outgoing_links.iter().any(|l| l["target"] == "b"));
        assert!(r.incoming_links.iter().any(|l| l["source_path"] == "b.md"));
        // Body is not returned unless display was asked for.
        assert!(r.body.is_none());
        assert!(report.notes.is_empty());

        // With display requested, the body rides along.
        let p = GetParams {
            targets: vec!["a.md".into()],
            with_body: true,
            ..Default::default()
        };
        let with_body = execute(&cache, &p, TODAY).unwrap().unwrap();
        assert!(with_body.records[0].body.is_some());
    }

    #[test]
    fn wikilink_target_resolves() {
        let (_t, root) = synth();
        let cache = built(&root);
        let report = execute(&cache, &params(&["[[a]]"]), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].path, "a.md");
    }

    #[test]
    fn missing_target_is_an_error_note_and_continues() {
        let (_t, root) = synth();
        let cache = built(&root);
        let report = execute(&cache, &params(&["a.md", "nope", "b.md"]), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.records.len(), 2);
        assert!(report
            .notes
            .iter()
            .any(|n| n.starts_with("error:") && n.contains("nope")));
    }

    #[test]
    fn section_resolves_requested_span_only() {
        let (_t, root) = synth();
        let cache = built(&root);
        let p = GetParams {
            targets: vec!["a.md".into()],
            sections: vec!["One".into()],
            ..Default::default()
        };
        let report = execute(&cache, &p, TODAY).unwrap().unwrap();
        let sections = report.records[0].sections.as_ref().unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "One");
        assert_eq!(sections[0].1, "## One\nfirst\n");
    }

    #[test]
    fn section_missing_warns_and_zero_resolved_errors() {
        let (_t, root) = synth();
        let cache = built(&root);
        let p = GetParams {
            targets: vec!["a.md".into()],
            sections: vec!["Nope".into()],
            ..Default::default()
        };
        let report = execute(&cache, &p, TODAY).unwrap().unwrap();
        assert!(report.records[0].sections.as_ref().unwrap().is_empty());
        assert!(report
            .notes
            .iter()
            .any(|n| n.starts_with("error:") && n.contains("none of the requested")));
    }

    #[test]
    fn sort_and_limit_apply_over_records() {
        let (_t, root) = synth();
        let cache = built(&root);
        let mut p = params(&["a.md", "b.md"]);
        p.paging.sort = Some("title".into());
        p.paging.desc = true;
        let report = execute(&cache, &p, TODAY).unwrap().unwrap();
        let paths: Vec<&str> = report.records.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["b.md", "a.md"]);
    }
}
