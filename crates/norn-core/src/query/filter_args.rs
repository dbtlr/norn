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

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};
use serde_json::Value;

use norn_wire::FilterParams;

use super::DocumentQuery;
use crate::grammar::split_field_value;
use crate::standards::path_match::PathPattern;
use crate::standards::{ValidateConfig, VaultConfig};

/// Translate the parsed filter vocabulary into a [`DocumentQuery`]. `today` is
/// the injected current date (`%Y-%m-%d`), used only to resolve `--on today`.
///
/// `types` carries the vault-wide resolved predicate types (NRN-426): where the
/// schema declares a field's type unambiguously, a value-comparison predicate
/// (`--eq` / `--not-eq` / `--in` / `--not-in`) compiles as that type (and refuses
/// a value that cannot be that type — a declared date/datetime rejects a non-ISO
/// value). Where no rule declares the field, or declaring rules disagree, a
/// numeric-/bool-looking token dual-types: it matches EITHER the string OR the
/// parsed-scalar representation, curing the silent zero-match (and the inverted
/// `--not-eq`) on quoted zips / phones / zero-padded ids. See ADR 0023.
pub fn build_document_query(
    params: &FilterParams,
    today: &str,
    types: &PredicateFieldTypes,
) -> Result<DocumentQuery> {
    let body_text_contains = params.text.as_ref().filter(|s| !s.is_empty()).cloned();

    // Value-comparison operators route by resolved type: a single typed value
    // stays an equality/membership predicate; a fallback dual value fans out to
    // the membership predicate (`--eq x:07030` ⇒ `x IN ["07030", 7030]`, whose
    // De Morgan for `--not-eq` is `x NOT IN ["07030", 7030]`), so the existing
    // array-aware EAV membership compile is reused verbatim — no new SQL shape.
    let mut frontmatter_eq = Vec::new();
    let mut frontmatter_in = Vec::new();
    for spec in &params.eq {
        let (field, typed) = parse_field_value(spec, "--eq", types)?;
        match typed {
            TypedValue::Single(v) => frontmatter_eq.push((field, v)),
            TypedValue::Dual(s, scalar) => frontmatter_in.push((field, vec![s, scalar])),
        }
    }
    for spec in &params.r#in {
        frontmatter_in.push(parse_field_value_list(spec, "--in", types)?);
    }
    let mut frontmatter_not_eq = Vec::new();
    let mut frontmatter_not_in = Vec::new();
    for spec in &params.not_eq {
        let (field, typed) = parse_field_value(spec, "--not-eq", types)?;
        match typed {
            TypedValue::Single(v) => frontmatter_not_eq.push((field, v)),
            TypedValue::Dual(s, scalar) => frontmatter_not_in.push((field, vec![s, scalar])),
        }
    }
    for spec in &params.not_in {
        frontmatter_not_in.push(parse_field_value_list(spec, "--not-in", types)?);
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

    // NRN-428: an unparseable `--path` glob is refused up front — a single seam
    // covering find (`find_documents`) and count/describe (`documents_matching`).
    // Left unvalidated, the downstream post-pass `.ok()`-discards the parse error
    // and silently filters out every document (empty result, exit 0 — a real
    // no-match is indistinguishable from a typo'd glob). See ADR 0023.
    for pattern in &params.path {
        if let Err(e) = PathPattern::parse(pattern) {
            return Err(anyhow!("invalid --path glob '{pattern}': {e}"));
        }
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

fn parse_field_value(
    spec: &str,
    flag: &str,
    types: &PredicateFieldTypes,
) -> Result<(String, TypedValue)> {
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
    let typed = type_value(&field, raw, flag, types)?;
    Ok((field, typed))
}

fn parse_field_value_list(
    spec: &str,
    flag: &str,
    types: &PredicateFieldTypes,
) -> Result<(String, Vec<Value>)> {
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
    // Each element is typed independently (NRN-426 rule 4): the declared type
    // applies to each; a fallback dual element fans into both representations,
    // which membership OR-semantics (and De Morgan for `--not-in`) fold into the
    // one value set correctly.
    let mut values: Vec<Value> = Vec::new();
    for element in raw.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        match type_value(&field, element, flag, types)? {
            TypedValue::Single(v) => values.push(v),
            TypedValue::Dual(s, scalar) => {
                values.push(s);
                values.push(scalar);
            }
        }
    }
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
/// substituted for the literal `today`; every other value must be a valid ISO
/// 8601 date or datetime (NRN-427, ADR 0023) — an unrecognized value refuses
/// rather than passing verbatim into a TEXT lexical compare (where e.g.
/// `--before due:yesterday` would match essentially every stored ISO date, and
/// `--on created:2026-13-45` would compare as a literal string). This validates
/// the predicate VALUE the user typed; it does not touch how valid ISO values
/// compare against stored frontmatter (that lexical compare is unchanged).
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
    } else if is_iso_date_or_datetime(raw) {
        raw.to_string()
    } else {
        return Err(anyhow!(
            "invalid {flag} date value '{raw}': expected `today`, \
             an ISO 8601 date (YYYY-MM-DD), or an ISO 8601 datetime"
        ));
    };
    Ok((field, date))
}

/// True when `value` is a valid ISO 8601 date (`YYYY-MM-DD`) or datetime at
/// second OR minute precision — a naive datetime (`YYYY-MM-DDThh:mm[:ss]`) or an
/// offset-bearing datetime (`YYYY-MM-DDThh:mm[:ss]±hh:mm`, and `Z` at second
/// precision via RFC 3339). Minute precision (`YYYY-MM-DDThh:mm`) is a valid ISO
/// 8601 reduced-precision form and the dominant stored-frontmatter shape, so it
/// must validate — dropping it would refuse a value that previously compared
/// correctly. chrono rejects impossible dates (`2026-13-45`) and non-date
/// garbage.
fn is_iso_date_or_datetime(value: &str) -> bool {
    use chrono::{DateTime, NaiveDate, NaiveDateTime};
    NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
        || DateTime::parse_from_rfc3339(value).is_ok()
        || DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M%z").is_ok()
        || NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").is_ok()
        || NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").is_ok()
}

/// The coercion class a declared field type imposes on a value-comparison
/// predicate value (NRN-426). The declarable vocabulary carries no numeric or
/// boolean type, so only two classes exist: string-shaped (accept any token) and
/// temporal (must be an ISO 8601 date/datetime or the predicate refuses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TypeClass {
    /// `string` / `text` / `list_of_strings` / `wikilink` / `wikilink_or_list` —
    /// the value is a literal string; every token is a valid string, so there is
    /// no refusal and no numeric/bool coercion.
    StringLike,
    /// `date` / `datetime` — the value must parse as an ISO 8601 date or datetime
    /// (the shared NRN-427 grammar), else the predicate refuses (ADR 0023).
    Temporal,
}

fn classify_type(type_name: &str) -> Option<TypeClass> {
    match type_name {
        "date" | "datetime" => Some(TypeClass::Temporal),
        "string" | "text" | "list_of_strings" | "wikilink" | "wikilink_or_list" => {
            Some(TypeClass::StringLike)
        }
        // An unrecognized type name (config rejects these at load, so this is
        // defensive) contributes no opinion to the vault-wide agreement.
        _ => None,
    }
}

/// One field's vault-wide resolved coercion class, plus the declared type
/// name(s) that produced it (for the refusal message).
#[derive(Debug, Clone)]
struct ResolvedFieldType {
    class: TypeClass,
    declared: BTreeSet<String>,
}

/// Vault-wide predicate typing resolved from the validate schema (NRN-426
/// rule 1). Built once per query build. A field resolves to a class ONLY when
/// every rule declaring it agrees on one class; disagreement, or no declaration
/// at all, leaves the field absent (the fallback dual-typing then applies).
#[derive(Debug, Clone, Default)]
pub struct PredicateFieldTypes {
    resolved: HashMap<String, ResolvedFieldType>,
}

impl PredicateFieldTypes {
    /// No schema knowledge — every value-comparison field falls back to
    /// dual-typing. The value model default for a config-less run and for tests.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Resolve from the vault config's validate schema, if any.
    pub fn from_config(config: Option<&VaultConfig>) -> Self {
        config
            .map(|c| Self::from_validate(&c.validate))
            .unwrap_or_default()
    }

    /// Resolve from a validate config: group every rule's `field_types` by field,
    /// keep a field only when all declaring rules map to one coercion class.
    pub fn from_validate(validate: &ValidateConfig) -> Self {
        let mut acc: HashMap<String, (BTreeSet<TypeClass>, BTreeSet<String>)> = HashMap::new();
        for rule in &validate.rules {
            for (field, spec) in &rule.field_types {
                let Some(type_name) = spec.type_name() else {
                    continue;
                };
                let Some(class) = classify_type(type_name) else {
                    continue;
                };
                let entry = acc.entry(field.clone()).or_default();
                entry.0.insert(class);
                entry.1.insert(type_name.to_string());
            }
        }
        let resolved = acc
            .into_iter()
            .filter_map(|(field, (classes, declared))| {
                // Exactly one class across every declaring rule ⇒ the schema is
                // the authority; otherwise disagreement, so fall back.
                let mut it = classes.into_iter();
                let class = it.next()?;
                if it.next().is_some() {
                    return None;
                }
                Some((field, ResolvedFieldType { class, declared }))
            })
            .collect();
        Self { resolved }
    }

    fn resolved(&self, field: &str) -> Option<&ResolvedFieldType> {
        self.resolved.get(field)
    }
}

/// The typed form of one predicate token.
#[derive(Debug, Clone, PartialEq)]
enum TypedValue {
    /// A single value compared as-is — a schema-declared string/date field, or a
    /// fallback token that isn't numeric/bool-looking.
    Single(Value),
    /// Fallback dual-type (NRN-426 rules 2 & 5): match EITHER the string form OR
    /// the parsed scalar (number/bool). Ordered `(string, scalar)`.
    Dual(Value, Value),
}

/// Type one predicate token against the resolved schema. A declared temporal
/// field refuses a non-ISO value (ADR 0023); a declared string-shaped field
/// takes the value verbatim; an undeclared/disagreeing field dual-types a
/// numeric-/bool-looking token and otherwise keeps it a literal string.
fn type_value(
    field: &str,
    raw: &str,
    flag: &str,
    types: &PredicateFieldTypes,
) -> Result<TypedValue> {
    match types.resolved(field).map(|r| r.class) {
        Some(TypeClass::Temporal) => {
            // Value-comparison operators do NOT substitute `today` (only the date
            // operators do), so on a declared temporal field `today` is a
            // non-ISO literal and refuses like any other — consistent with rule 3.
            if is_iso_date_or_datetime(raw) {
                Ok(TypedValue::Single(Value::String(raw.to_string())))
            } else {
                let declared = types
                    .resolved(field)
                    .map(|r| r.declared.iter().cloned().collect::<Vec<_>>().join("/"))
                    .unwrap_or_else(|| "date".to_string());
                Err(anyhow!(
                    "invalid {flag} value for field '{field}': '{raw}' is not a valid \
                     {declared} value (expected an ISO 8601 date (YYYY-MM-DD) or datetime)"
                ))
            }
        }
        Some(TypeClass::StringLike) => Ok(TypedValue::Single(Value::String(raw.to_string()))),
        None => Ok(fallback_type(raw)),
    }
}

/// Eager JSON coercion of a `field:value` token for the apply owner-set
/// precondition path — a mutation gate, NOT the read query surface, so NRN-426
/// dual-typing does not apply and the historical exact-typed match is preserved.
pub(crate) fn parse_eq_precondition(spec: &str, flag: &str) -> Result<(String, Value)> {
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
    Ok((field, coerce_eager(raw)))
}

/// The historical eager scalar coercion (`true`/`false`/i64/f64 → typed, else
/// string). Retained only for the apply precondition path above.
fn coerce_eager(s: &str) -> Value {
    if s == "true" {
        Value::Bool(true)
    } else if s == "false" {
        Value::Bool(false)
    } else if let Ok(n) = s.parse::<i64>() {
        Value::Number(n.into())
    } else if let Ok(n) = s.parse::<f64>() {
        match serde_json::Number::from_f64(n) {
            Some(num) => Value::Number(num),
            None => Value::String(s.to_string()),
        }
    } else {
        Value::String(s.to_string())
    }
}

/// Fallback typing for an undeclared/disagreeing field: a `true`/`false` token
/// or a parseable integer/float dual-types (match the string OR the scalar);
/// anything else is a literal string.
fn fallback_type(raw: &str) -> TypedValue {
    if raw == "true" {
        TypedValue::Dual(Value::String("true".to_string()), Value::Bool(true))
    } else if raw == "false" {
        TypedValue::Dual(Value::String("false".to_string()), Value::Bool(false))
    } else if let Ok(n) = raw.parse::<i64>() {
        TypedValue::Dual(Value::String(raw.to_string()), Value::Number(n.into()))
    } else if let Ok(n) = raw.parse::<f64>() {
        match serde_json::Number::from_f64(n) {
            Some(num) => TypedValue::Dual(Value::String(raw.to_string()), Value::Number(num)),
            None => TypedValue::Single(Value::String(raw.to_string())),
        }
    } else {
        TypedValue::Single(Value::String(raw.to_string()))
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
        build_document_query(params, TODAY, &PredicateFieldTypes::empty()).unwrap()
    }

    /// A resolver where `field` is declared with one type name across the vault.
    fn declared(field: &str, type_name: &str) -> PredicateFieldTypes {
        let mut rule = crate::standards::ValidateRule::default();
        rule.field_types.insert(
            field.to_string(),
            crate::standards::FieldTypeSpec::Bare(type_name.to_string()),
        );
        let validate = ValidateConfig {
            rules: vec![rule],
            ..Default::default()
        };
        PredicateFieldTypes::from_validate(&validate)
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
        assert!(build_document_query(&a, TODAY, &PredicateFieldTypes::empty()).is_err());
    }

    #[test]
    fn eq_bool_dual_types_under_fallback() {
        // NRN-426 rule 5: an undeclared bool token matches YAML `true` AND the
        // string "true" — routed to membership (`published IN ["true", true]`),
        // not the old eager `Bool(true)` eq that missed a stored `"true"`.
        let mut a = empty();
        a.eq = vec!["published:true".into()];
        let q = build(&a);
        assert!(q.frontmatter_eq.is_empty());
        assert_eq!(
            q.frontmatter_in,
            vec![("published".to_string(), vec![json!("true"), json!(true)])]
        );

        let mut a = empty();
        a.eq = vec!["draft:false".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_in,
            vec![("draft".to_string(), vec![json!("false"), json!(false)])]
        );
    }

    #[test]
    fn eq_integer_dual_types_under_fallback() {
        // NRN-426 rule 2: an undeclared numeric token matches the number AND the
        // string form (`priority IN ["5", 5]`), curing the silent zero-match on a
        // stored `priority: "5"`.
        let mut a = empty();
        a.eq = vec!["priority:5".into()];
        let q = build(&a);
        assert!(q.frontmatter_eq.is_empty());
        assert_eq!(
            q.frontmatter_in,
            vec![("priority".to_string(), vec![json!("5"), json!(5)])]
        );
    }

    #[test]
    fn eq_leading_zero_numeric_dual_types() {
        // The accepted leading-zero overmatch (NRN-426 rule 2): `zip:07030`
        // parses to 7030, so it matches a stored `"07030"` string AND a stored
        // numeric 7030 — strictly better than the silent miss it replaces.
        let mut a = empty();
        a.eq = vec!["zip:07030".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_in,
            vec![("zip".to_string(), vec![json!("07030"), json!(7030)])]
        );
    }

    #[test]
    fn not_eq_dual_types_via_not_in_de_morgan() {
        // NRN-426 rule 2: `--not-eq` fallback excludes docs matching EITHER
        // representation — De Morgan folds to `zip NOT IN ["07030", 7030]`.
        let mut a = empty();
        a.not_eq = vec!["zip:07030".into()];
        let q = build(&a);
        assert!(q.frontmatter_not_eq.is_empty());
        assert_eq!(
            q.frontmatter_not_in,
            vec![("zip".to_string(), vec![json!("07030"), json!(7030)])]
        );
    }

    #[test]
    fn in_list_dual_types_each_element_independently() {
        // NRN-426 rule 4: each `--in` element is typed independently; a numeric
        // element fans into both forms within the one value set.
        let mut a = empty();
        a.r#in = vec!["code:07030,alpha,42".into()];
        let q = build(&a);
        assert_eq!(
            q.frontmatter_in,
            vec![(
                "code".to_string(),
                vec![
                    json!("07030"),
                    json!(7030),
                    json!("alpha"),
                    json!("42"),
                    json!(42)
                ]
            )]
        );
    }

    #[test]
    fn declared_string_field_takes_value_verbatim_no_dual() {
        // NRN-426 rule 1: a schema-declared string-family field compiles the
        // value as a plain string — a numeric-looking token stays a single
        // string eq (no dual), because the schema is the type authority.
        let mut a = empty();
        a.eq = vec!["zip:07030".into()];
        let q = build_document_query(&a, TODAY, &declared("zip", "string")).unwrap();
        assert!(q.frontmatter_in.is_empty());
        assert_eq!(q.frontmatter_eq, vec![("zip".to_string(), json!("07030"))]);
    }

    #[test]
    fn declared_date_field_accepts_iso_value() {
        // NRN-426 rule 3: a declared date/datetime field takes a valid ISO value
        // as a single string eq (reusing the NRN-427 grammar).
        let mut a = empty();
        a.eq = vec!["due:2026-05-01".into()];
        let q = build_document_query(&a, TODAY, &declared("due", "date")).unwrap();
        assert_eq!(
            q.frontmatter_eq,
            vec![("due".to_string(), json!("2026-05-01"))]
        );
    }

    #[test]
    fn declared_date_field_refuses_non_iso_value() {
        // NRN-426 rule 3: a declared date field refuses a non-ISO `--eq` value,
        // naming the field, the declared type, and the value (ADR 0023 class).
        for (flag_setter, _) in [
            (
                (|a: &mut FilterParams| a.eq = vec!["due:not-a-date".into()])
                    as fn(&mut FilterParams),
                "--eq",
            ),
            (
                (|a: &mut FilterParams| a.not_eq = vec!["due:not-a-date".into()])
                    as fn(&mut FilterParams),
                "--not-eq",
            ),
            (
                (|a: &mut FilterParams| a.r#in = vec!["due:not-a-date".into()])
                    as fn(&mut FilterParams),
                "--in",
            ),
        ] {
            let mut a = empty();
            flag_setter(&mut a);
            let err = build_document_query(&a, TODAY, &declared("due", "date"))
                .expect_err("declared-date field must refuse a non-ISO value");
            let msg = err.to_string();
            assert!(msg.contains("due"), "message names the field: {msg}");
            assert!(
                msg.contains("date"),
                "message names the declared type: {msg}"
            );
            assert!(msg.contains("not-a-date"), "message names the value: {msg}");
        }
    }

    #[test]
    fn declared_datetime_field_refuses_non_iso_value() {
        let mut a = empty();
        a.eq = vec!["created:sometime".into()];
        let err = build_document_query(&a, TODAY, &declared("created", "datetime"))
            .expect_err("declared-datetime field must refuse a non-ISO value");
        let msg = err.to_string();
        assert!(msg.contains("created") && msg.contains("datetime") && msg.contains("sometime"));
    }

    #[test]
    fn disagreeing_declarations_fall_back_to_dual() {
        // NRN-426 rule 1: two rules declaring `code` as different classes
        // (string vs date) disagree, so the field falls back to dual-typing.
        let mut string_rule = crate::standards::ValidateRule::default();
        string_rule.field_types.insert(
            "code".to_string(),
            crate::standards::FieldTypeSpec::Bare("string".to_string()),
        );
        let mut date_rule = crate::standards::ValidateRule::default();
        date_rule.field_types.insert(
            "code".to_string(),
            crate::standards::FieldTypeSpec::Bare("date".to_string()),
        );
        let validate = ValidateConfig {
            rules: vec![string_rule, date_rule],
            ..Default::default()
        };
        let types = PredicateFieldTypes::from_validate(&validate);
        let mut a = empty();
        a.eq = vec!["code:07030".into()];
        let q = build_document_query(&a, TODAY, &types).unwrap();
        assert_eq!(
            q.frontmatter_in,
            vec![("code".to_string(), vec![json!("07030"), json!(7030)])]
        );
    }

    #[test]
    fn agreeing_date_and_datetime_declarations_resolve_temporal() {
        // Two rules, one `date` one `datetime`, agree on the temporal class, so
        // the field refuses a non-ISO value rather than falling back.
        let mut date_rule = crate::standards::ValidateRule::default();
        date_rule.field_types.insert(
            "when".to_string(),
            crate::standards::FieldTypeSpec::Bare("date".to_string()),
        );
        let mut dt_rule = crate::standards::ValidateRule::default();
        dt_rule.field_types.insert(
            "when".to_string(),
            crate::standards::FieldTypeSpec::Bare("datetime".to_string()),
        );
        let validate = ValidateConfig {
            rules: vec![date_rule, dt_rule],
            ..Default::default()
        };
        let types = PredicateFieldTypes::from_validate(&validate);
        let mut a = empty();
        a.eq = vec!["when:bogus".into()];
        assert!(build_document_query(&a, TODAY, &types).is_err());
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
                build_document_query(&a, TODAY, &PredicateFieldTypes::empty()).is_err(),
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

    // ── NRN-427: date-operator values validate or refuse ──────────────────

    #[test]
    fn date_operator_accepts_iso_date_datetime_and_today() {
        // Accepted forms round-trip unchanged (today → injected date).
        for (value, expected) in [
            ("2026-05-01", "2026-05-01"),                               // ISO date
            ("2026-05-01T12:30", "2026-05-01T12:30"),                   // naive, minute precision
            ("2026-05-01T12:30:00", "2026-05-01T12:30:00"),             // naive, second precision
            ("2026-05-01T12:30:00Z", "2026-05-01T12:30:00Z"),           // RFC 3339 UTC
            ("2026-05-01T12:30+02:00", "2026-05-01T12:30+02:00"),       // offset, minute precision
            ("2026-05-01T12:30:00+02:00", "2026-05-01T12:30:00+02:00"), // offset, second precision
            // Fractional seconds accept deliberately (real vaults store them;
            // they lexically order correctly) — pinned so the contract wording
            // ("minute, second, or fractional-second precision") stays true.
            ("2026-05-01T12:30:00.123Z", "2026-05-01T12:30:00.123Z"), // RFC 3339 fractional
            (
                "2026-05-01T12:30:00.123+02:00",
                "2026-05-01T12:30:00.123+02:00",
            ), // offset fractional
            ("today", TODAY),
        ] {
            for flag_setter in [
                |a: &mut FilterParams, s: String| a.before = vec![s],
                |a: &mut FilterParams, s: String| a.after = vec![s],
                |a: &mut FilterParams, s: String| a.on = vec![s],
            ] {
                let mut a = empty();
                flag_setter(&mut a, format!("created:{value}"));
                let q = build_document_query(&a, TODAY, &PredicateFieldTypes::empty())
                    .unwrap_or_else(|e| panic!("value {value:?} should parse, got {e}"));
                let got = q
                    .date_before
                    .iter()
                    .chain(&q.date_after)
                    .chain(&q.date_on)
                    .next()
                    .expect("one date predicate");
                assert_eq!(got, &("created".to_string(), expected.to_string()));
            }
        }
    }

    #[test]
    fn date_operator_refuses_non_iso_values() {
        // `yesterday` (a relative word promising nothing), an impossible date,
        // and pure garbage all refuse rather than lexically comparing verbatim.
        for value in ["yesterday", "2026-13-45", "not-a-date", "2026/05/01"] {
            let mut a = empty();
            a.before = vec![format!("due:{value}")];
            let err = build_document_query(&a, TODAY, &PredicateFieldTypes::empty())
                .expect_err(&format!("value {value:?} must refuse"));
            let msg = err.to_string();
            assert!(
                msg.contains("--before"),
                "message names the operator: {msg}"
            );
            assert!(msg.contains(value), "message names the value: {msg}");
            assert!(
                msg.contains("ISO 8601"),
                "message names accepted forms: {msg}"
            );
        }
    }

    #[test]
    fn date_operator_refusal_covers_each_flag() {
        for setter in [
            |a: &mut FilterParams| a.before = vec!["d:yesterday".into()],
            |a: &mut FilterParams| a.after = vec!["d:yesterday".into()],
            |a: &mut FilterParams| a.on = vec!["d:yesterday".into()],
        ] {
            let mut a = empty();
            setter(&mut a);
            assert!(build_document_query(&a, TODAY, &PredicateFieldTypes::empty()).is_err());
        }
    }

    // ── NRN-428: `--path` globs parse or refuse ───────────────────────────

    #[test]
    fn valid_path_glob_passes_through() {
        let mut a = empty();
        a.path = vec!["Workspaces/**/*.md".into()];
        let q = build(&a);
        assert_eq!(q.path_globs, vec!["Workspaces/**/*.md".to_string()]);
    }

    #[test]
    fn malformed_path_glob_refuses() {
        let mut a = empty();
        a.path = vec!["{unclosed".into()];
        let err = build_document_query(&a, TODAY, &PredicateFieldTypes::empty())
            .expect_err("malformed glob must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--path"), "message names the flag: {msg}");
        assert!(
            msg.contains("{unclosed"),
            "message names the bad pattern: {msg}"
        );
    }
}
