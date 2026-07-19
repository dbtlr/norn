//! Parse the read-verb wire filter vocabulary into a [`DocumentQuery`].
//!
//! [`build_document_query`] converts [`norn_wire::FilterParams`] (the raw
//! `field:value` strings the CLI collected) into the typed predicate model,
//! applying ADR 0010 separator forgiveness ([`crate::grammar::split_field_value`])
//! and JSON value coercion. This is the canonical query-build path.
//!
//! # Clock seam (value-in / value-out)
//!
//! The donor resolved `--on today` by reading the process clock
//! (`chrono::Local::now()`) inside the parser. norn-core takes no ambient reads
//! and must stay deterministic (same input, same output), so the caller injects
//! the current date as `today` (a pre-formatted `%Y-%m-%d` string). The external
//! contract is unchanged — `on:today` still resolves to the current date — only
//! the clock source moves out of core to the CLI / verb.
//!
//! # Seam left behind
//!
//! `--links-to TARGET` resolution (the donor `resolve_links_to`) needs the warm
//! cache + target resolution; [`build_document_query`] leaves
//! [`DocumentQuery::links_to`] empty and passes `unresolved_links` through, as
//! the donor's pure builder did. Resolution ports with the read verbs.

use anyhow::{anyhow, Result};
use serde_json::Value;

use norn_wire::FilterParams;

use super::DocumentQuery;
use crate::grammar::split_field_value;

/// Translate the parsed filter vocabulary into a [`DocumentQuery`]. `today` is
/// the injected current date (`%Y-%m-%d`), used only to resolve `--on today`.
pub fn build_document_query(params: &FilterParams, today: &str) -> Result<DocumentQuery> {
    let body_text_contains = params.text.as_ref().filter(|s| !s.is_empty()).cloned();

    let mut frontmatter_eq = Vec::new();
    for spec in &params.eq {
        frontmatter_eq.push(parse_field_value(spec, "--eq")?);
    }
    let mut frontmatter_not_eq = Vec::new();
    for spec in &params.not_eq {
        frontmatter_not_eq.push(parse_field_value(spec, "--not-eq")?);
    }
    let mut frontmatter_in = Vec::new();
    for spec in &params.r#in {
        frontmatter_in.push(parse_field_value_list(spec, "--in")?);
    }
    let mut frontmatter_not_in = Vec::new();
    for spec in &params.not_in {
        frontmatter_not_in.push(parse_field_value_list(spec, "--not-in")?);
    }
    let mut frontmatter_starts_with = Vec::new();
    for spec in &params.starts_with {
        frontmatter_starts_with.push(parse_field_text(spec, "--starts-with")?);
    }
    let mut frontmatter_ends_with = Vec::new();
    for spec in &params.ends_with {
        frontmatter_ends_with.push(parse_field_text(spec, "--ends-with")?);
    }
    let mut frontmatter_contains = Vec::new();
    for spec in &params.contains {
        frontmatter_contains.push(parse_field_text(spec, "--contains")?);
    }
    let mut date_before = Vec::new();
    for spec in &params.before {
        date_before.push(parse_field_date(spec, "--before", today)?);
    }
    let mut date_after = Vec::new();
    for spec in &params.after {
        date_after.push(parse_field_date(spec, "--after", today)?);
    }
    let mut date_on = Vec::new();
    for spec in &params.on {
        date_on.push(parse_field_date(spec, "--on", today)?);
    }

    Ok(DocumentQuery {
        body_text_contains,
        frontmatter_eq,
        frontmatter_not_eq,
        frontmatter_in,
        frontmatter_not_in,
        frontmatter_has: params.has.clone(),
        frontmatter_missing: params.missing.clone(),
        frontmatter_starts_with,
        frontmatter_ends_with,
        frontmatter_contains,
        date_before,
        date_after,
        date_on,
        path_globs: params.path.clone(),
        // `links_to` is resolved at the command layer (needs the cache); the
        // pure builder leaves it empty. `has_unresolved_links` is a pure flag.
        links_to: Vec::new(),
        has_unresolved_links: params.unresolved_links,
    })
}

pub(crate) fn parse_field_value(spec: &str, flag: &str) -> Result<(String, Value)> {
    let (field, raw) = split_field_value(spec)
        .ok_or_else(|| anyhow!("invalid {} value, expected field:value: {}", flag, spec))?;
    let field = field.trim().to_string();
    let raw = raw.trim();
    if field.is_empty() || raw.is_empty() {
        return Err(anyhow!(
            "invalid {} value, expected non-empty field and value: {}",
            flag,
            spec
        ));
    }
    Ok((field, coerce_value(raw)))
}

fn parse_field_value_list(spec: &str, flag: &str) -> Result<(String, Vec<Value>)> {
    let (field, raw) = split_field_value(spec)
        .ok_or_else(|| anyhow!("invalid {} value, expected field:v1,v2,...: {}", flag, spec))?;
    let field = field.trim().to_string();
    if field.is_empty() {
        return Err(anyhow!(
            "invalid {} value, expected non-empty field: {}",
            flag,
            spec
        ));
    }
    let values: Vec<Value> = raw
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(coerce_value)
        .collect();
    if values.is_empty() {
        return Err(anyhow!(
            "invalid {} value, expected at least one value: {}",
            flag,
            spec
        ));
    }
    Ok((field, values))
}

/// Parse a `field:VALUE` token for the anchored string operators. The value
/// stays a literal string — no bool/number coercion (a `--contains prio:1`
/// needle is the text "1") and no whitespace trimming: for anchored operators
/// the boundary characters are exactly what the user asserts, so
/// `--ends-with 'title:done '` keeps its trailing space.
fn parse_field_text(spec: &str, flag: &str) -> Result<(String, String)> {
    let (field, raw) = split_field_value(spec)
        .ok_or_else(|| anyhow!("invalid {} value, expected field:value: {}", flag, spec))?;
    let field = field.trim().to_string();
    if field.is_empty() || raw.is_empty() {
        return Err(anyhow!(
            "invalid {} value, expected non-empty field and value: {}",
            flag,
            spec
        ));
    }
    Ok((field, raw.to_string()))
}

/// Parse a `field:DATE` token. `today` (an injected `%Y-%m-%d` string) is
/// substituted for the literal `today`; every other value passes through
/// verbatim (trimmed).
fn parse_field_date(spec: &str, flag: &str, today: &str) -> Result<(String, String)> {
    let (field, raw) = split_field_value(spec)
        .ok_or_else(|| anyhow!("invalid {} value, expected field:DATE: {}", flag, spec))?;
    let field = field.trim().to_string();
    let raw = raw.trim();
    if field.is_empty() || raw.is_empty() {
        return Err(anyhow!(
            "invalid {} value, expected non-empty field and date: {}",
            flag,
            spec
        ));
    }
    let date = if raw == "today" {
        today.to_string()
    } else {
        raw.to_string()
    };
    Ok((field, date))
}

fn coerce_value(s: &str) -> Value {
    if s == "true" {
        Value::Bool(true)
    } else if s == "false" {
        Value::Bool(false)
    } else if let Ok(n) = s.parse::<i64>() {
        Value::Number(n.into())
    } else if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            Value::Number(num)
        } else {
            Value::String(s.to_string())
        }
    } else {
        Value::String(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TODAY: &str = "2026-07-18";

    fn empty() -> FilterParams {
        FilterParams::default()
    }

    fn build(params: &FilterParams) -> DocumentQuery {
        build_document_query(params, TODAY).unwrap()
    }

    #[test]
    fn unresolved_links_flag_passes_through_and_links_to_stays_empty() {
        let mut a = empty();
        a.unresolved_links = true;
        a.links_to = vec!["hub".to_string()];
        let q = build(&a);
        assert!(q.has_unresolved_links);
        // links_to is resolved at the command layer, not by the pure builder.
        assert!(q.links_to.is_empty());
    }

    #[test]
    fn empty_text_is_no_predicate() {
        let mut a = empty();
        a.text = Some(String::new());
        let q = build(&a);
        assert!(q.body_text_contains.is_none());
    }

    #[test]
    fn eq_string_value_coerces() {
        let mut a = empty();
        a.eq = vec!["type:note".into()];
        let q = build(&a);
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }

    #[test]
    fn eq_accepts_equals_separator() {
        // ADR 0010 separator forgiveness: a predicate token spelled with `=`
        // parses identically to the canonical `:` form.
        let mut a = empty();
        a.eq = vec!["type=note".into()];
        let q = build(&a);
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }

    #[test]
    fn eq_date_value_embedded_colon_uses_first_separator() {
        // `=` comes first, so the value keeps its embedded `:` verbatim.
        let mut a = empty();
        a.on = vec!["modified=2026-07-01".into()];
        let q = build(&a);
        assert_eq!(
            q.date_on,
            vec![("modified".to_string(), "2026-07-01".to_string())]
        );
    }

    #[test]
    fn on_today_resolves_to_injected_date() {
        let mut a = empty();
        a.on = vec!["created:today".into()];
        let q = build(&a);
        assert_eq!(q.date_on, vec![("created".to_string(), TODAY.to_string())]);
    }

    #[test]
    fn invalid_eq_format_errors() {
        let mut a = empty();
        a.eq = vec!["nocolon".into()];
        assert!(build_document_query(&a, TODAY).is_err());
    }

    #[test]
    fn eq_bool_coercion() {
        let mut a = empty();
        a.eq = vec!["published:true".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_eq,
            vec![("published".to_string(), json!(true))]
        );

        let mut a = empty();
        a.eq = vec!["draft:false".into()];
        let q = build(&a);
        assert_eq!(q.frontmatter_eq, vec![("draft".to_string(), json!(false))]);
    }

    #[test]
    fn eq_integer_coercion() {
        let mut a = empty();
        a.eq = vec!["priority:5".into()];
        let q = build(&a);
        assert_eq!(q.frontmatter_eq, vec![("priority".to_string(), json!(5))]);
    }

    #[test]
    fn in_set_value_list() {
        let mut a = empty();
        a.r#in = vec!["status:backlog,active".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_in,
            vec![(
                "status".to_string(),
                vec![json!("backlog"), json!("active")]
            )]
        );
    }

    #[test]
    fn string_operator_values_stay_literal_text() {
        let mut a = empty();
        a.starts_with = vec!["tags:release:".into()];
        a.ends_with = vec!["status:_progress".into()];
        a.contains = vec!["priority:1".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_starts_with,
            vec![("tags".to_string(), "release:".to_string())]
        );
        assert_eq!(
            q.frontmatter_ends_with,
            vec![("status".to_string(), "_progress".to_string())]
        );
        // No numeric coercion — the needle is the literal text "1".
        assert_eq!(
            q.frontmatter_contains,
            vec![("priority".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn string_operator_empty_value_errors() {
        for spec in ["tags:", ":release", "nocolon"] {
            let mut a = empty();
            a.starts_with = vec![spec.into()];
            assert!(
                build_document_query(&a, TODAY).is_err(),
                "spec {spec:?} should be rejected"
            );
        }
    }

    #[test]
    fn string_operator_preserves_needle_whitespace() {
        // Anchored operators assert boundary characters — trimming would
        // silently change the predicate.
        let mut a = empty();
        a.ends_with = vec!["title:done ".into()];
        a.contains = vec!["title:  ".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_ends_with,
            vec![("title".to_string(), "done ".to_string())]
        );
        assert_eq!(
            q.frontmatter_contains,
            vec![("title".to_string(), "  ".to_string())]
        );
    }

    #[test]
    fn before_iso_date_passes_through() {
        let mut a = empty();
        a.before = vec!["created:2026-05-01".into()];
        let q = build(&a);
        assert_eq!(
            q.date_before,
            vec![("created".to_string(), "2026-05-01".to_string())]
        );
    }
}
