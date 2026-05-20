//! CLI args → vault_cache::FindQuery translation.

use anyhow::{anyhow, Result};
use chrono::Local;
use serde_json::Value;
use vault_cache::{DocumentQuery, FindQuery, SortClause, SortDirection};

use crate::cli::FindArgs;

/// Convert clap-parsed FindArgs into the cache-layer FindQuery.
pub fn build_find_query(args: &FindArgs) -> Result<FindQuery> {
    // text (empty → no predicate, per spec)
    let body_text_contains = args.text.as_ref().filter(|s| !s.is_empty()).cloned();

    // eq
    let mut frontmatter_eq = Vec::new();
    for spec in &args.eq {
        frontmatter_eq.push(parse_field_value(spec, "--eq")?);
    }

    // in
    let mut frontmatter_in = Vec::new();
    for spec in &args.r#in {
        frontmatter_in.push(parse_field_value_list(spec, "--in")?);
    }

    // not_in
    let mut frontmatter_not_in = Vec::new();
    for spec in &args.not_in {
        frontmatter_not_in.push(parse_field_value_list(spec, "--not-in")?);
    }

    // dates
    let mut date_before = Vec::new();
    for spec in &args.before {
        date_before.push(parse_field_date(spec, "--before")?);
    }
    let mut date_after = Vec::new();
    for spec in &args.after {
        date_after.push(parse_field_date(spec, "--after")?);
    }
    let mut date_on = Vec::new();
    for spec in &args.on {
        date_on.push(parse_field_date(spec, "--on")?);
    }

    let predicates = DocumentQuery {
        body_text_contains,
        frontmatter_eq,
        frontmatter_in,
        frontmatter_not_in,
        frontmatter_has: args.has.clone(),
        frontmatter_missing: args.missing.clone(),
        date_before,
        date_after,
        date_on,
        path_globs: args.path.clone(),
    };

    // sort
    let sort = args.sort.as_ref().map(|field| SortClause {
        field: field.clone(),
        direction: if args.desc {
            SortDirection::Desc
        } else {
            SortDirection::Asc
        },
    });

    // limit (none if --no-limit)
    let limit = if args.no_limit {
        None
    } else {
        Some(args.limit)
    };

    Ok(FindQuery {
        predicates,
        sort,
        limit,
        starts_at: args.starts_at.max(1), // floor at 1 to defend against bad input
    })
}

/// Parse `field:value` into `(field, JSON-coerced value)`.
fn parse_field_value(spec: &str, flag: &str) -> Result<(String, Value)> {
    let (field, raw) = spec
        .split_once(':')
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

/// Parse `field:v1,v2,v3` into `(field, Vec<JSON-coerced values>)`.
fn parse_field_value_list(spec: &str, flag: &str) -> Result<(String, Vec<Value>)> {
    let (field, raw) = spec
        .split_once(':')
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

/// Parse `field:DATE`. Resolves `today` to the current local date in ISO 8601.
fn parse_field_date(spec: &str, flag: &str) -> Result<(String, String)> {
    let (field, raw) = spec
        .split_once(':')
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
        Local::now().date_naive().format("%Y-%m-%d").to_string()
    } else {
        raw.to_string()
    };
    Ok((field, date))
}

/// Coerce a string to its most natural JSON type.
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

    fn empty_args() -> FindArgs {
        FindArgs {
            text: None,
            eq: vec![],
            r#in: vec![],
            not_in: vec![],
            has: vec![],
            missing: vec![],
            before: vec![],
            after: vec![],
            on: vec![],
            path: vec![],
            sort: None,
            desc: false,
            limit: 10,
            no_limit: false,
            starts_at: 1,
            format: None,
            col: vec![],
            no_pager: false,
        }
    }

    #[test]
    fn empty_text_is_no_predicate() {
        let mut args = empty_args();
        args.text = Some(String::new());
        let q = build_find_query(&args).unwrap();
        assert!(q.predicates.body_text_contains.is_none());
    }

    #[test]
    fn text_substring_passes_through() {
        let mut args = empty_args();
        args.text = Some("SQLite".to_string());
        let q = build_find_query(&args).unwrap();
        assert_eq!(q.predicates.body_text_contains.as_deref(), Some("SQLite"));
    }

    #[test]
    fn eq_string_value() {
        let mut args = empty_args();
        args.eq = vec!["type:note".to_string()];
        let q = build_find_query(&args).unwrap();
        assert_eq!(
            q.predicates.frontmatter_eq,
            vec![("type".to_string(), json!("note"))]
        );
    }

    #[test]
    fn eq_bool_coercion() {
        let mut args = empty_args();
        args.eq = vec!["published:true".to_string()];
        let q = build_find_query(&args).unwrap();
        assert_eq!(
            q.predicates.frontmatter_eq,
            vec![("published".to_string(), json!(true))]
        );
    }

    #[test]
    fn eq_integer_coercion() {
        let mut args = empty_args();
        args.eq = vec!["priority:5".to_string()];
        let q = build_find_query(&args).unwrap();
        assert_eq!(
            q.predicates.frontmatter_eq,
            vec![("priority".to_string(), json!(5))]
        );
    }

    #[test]
    fn in_set_value_list() {
        let mut args = empty_args();
        args.r#in = vec!["status:backlog,active".to_string()];
        let q = build_find_query(&args).unwrap();
        assert_eq!(
            q.predicates.frontmatter_in,
            vec![(
                "status".to_string(),
                vec![json!("backlog"), json!("active")]
            )]
        );
    }

    #[test]
    fn on_today_resolves_to_current_date() {
        let mut args = empty_args();
        args.on = vec!["created:today".to_string()];
        let q = build_find_query(&args).unwrap();
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(q.predicates.date_on, vec![("created".to_string(), today)]);
    }

    #[test]
    fn before_iso_date_passes_through() {
        let mut args = empty_args();
        args.before = vec!["created:2026-05-01".to_string()];
        let q = build_find_query(&args).unwrap();
        assert_eq!(
            q.predicates.date_before,
            vec![("created".to_string(), "2026-05-01".to_string())]
        );
    }

    #[test]
    fn no_limit_overrides_limit() {
        let mut args = empty_args();
        args.no_limit = true;
        args.limit = 42;
        let q = build_find_query(&args).unwrap();
        assert!(q.limit.is_none());
    }

    #[test]
    fn sort_desc_flag() {
        let mut args = empty_args();
        args.sort = Some("created".to_string());
        args.desc = true;
        let q = build_find_query(&args).unwrap();
        let sort = q.sort.unwrap();
        assert_eq!(sort.field, "created");
        assert_eq!(sort.direction, SortDirection::Desc);
    }

    #[test]
    fn starts_at_floors_at_one() {
        let mut args = empty_args();
        args.starts_at = 0;
        let q = build_find_query(&args).unwrap();
        assert_eq!(q.starts_at, 1);
    }

    #[test]
    fn invalid_eq_format_errors() {
        let mut args = empty_args();
        args.eq = vec!["nocolon".to_string()];
        let result = build_find_query(&args);
        assert!(result.is_err());
    }
}
