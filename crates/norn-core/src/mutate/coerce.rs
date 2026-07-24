//! Shared schema-aware coercion for the `set` and `new` mutation seams.
//!
//! The declared-type lookup, the raw-string → typed-`Value` coercion, and the
//! known-field predicate both verbs share. Pure functions over `VaultConfig` +
//! `Document`; no IO.

use crate::domain::Document;
use crate::standards::{FieldTypeSpec, ValidateRule, VaultConfig};
use norn_wire::MutationWarning;
use serde_json::Value;

/// A schema-coercion refusal for a single `--field` value. `code()` gives the
/// stable kebab discriminator the wire `CodedError` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoerceError {
    InvalidDatetime {
        value: String,
    },
    InvalidDate {
        value: String,
    },
    InvalidWikilink {
        value: String,
    },
    ValueTooLong {
        value: String,
        bound: u32,
        field_type: String,
    },
    UnknownFieldType {
        field_type: String,
    },
}

impl std::fmt::Display for CoerceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoerceError::InvalidDatetime { value } => write!(
                f,
                "value '{value}' is not a valid datetime (expected YYYY-MM-DDTHH:MM[:SS])"
            ),
            CoerceError::InvalidDate { value } => {
                write!(f, "value '{value}' is not a valid date (expected YYYY-MM-DD)")
            }
            CoerceError::InvalidWikilink { value } => write!(
                f,
                "value '{value}' is not shape-valid as a wikilink (need non-empty stem inside [[…]])"
            ),
            CoerceError::ValueTooLong {
                value,
                bound,
                field_type,
            } => write!(
                f,
                "value '{value}' exceeds max_length {bound} for field type '{field_type}'"
            ),
            CoerceError::UnknownFieldType { field_type } => {
                write!(f, "unknown field_type: {field_type}")
            }
        }
    }
}

impl CoerceError {
    pub fn code(&self) -> &'static str {
        match self {
            // The date/datetime/wikilink family means "the caller supplied a bad
            // value" (retryable). An unsupported DECLARED type is a schema defect
            // only a human can fix, so it carries its own code.
            CoerceError::InvalidDatetime { .. }
            | CoerceError::InvalidDate { .. }
            | CoerceError::InvalidWikilink { .. } => "field-type-invalid",
            CoerceError::ValueTooLong { .. } => "value-too-long",
            CoerceError::UnknownFieldType { .. } => "field-type-unsupported",
        }
    }
}

/// Split `KEY=VALUE` (or `KEY:VALUE`) at the first separator (ADR 0010). Returns
/// `None` on a missing separator or an empty key.
pub fn split_kv(raw: &str) -> Option<(String, String)> {
    let (k, v) = crate::grammar::split_field_value(raw)?;
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

/// Light type inference for schema-silent values: `true`/`false` → bool,
/// integer-shaped → i64, `null` → Null, else string.
pub fn infer_scalar(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        s => s
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(s.to_string())),
    }
}

/// The declared schema type for `field` on `doc`, or `None` when no matching
/// rule declares a type.
pub fn lookup_field_type(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<String> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(spec) = rule.field_types.get(field) {
            if let Some(ty) = spec.type_name() {
                return Some(ty.to_string());
            }
        }
    }
    None
}

/// The effective `max_length` bound for `field`'s declared type on `doc`.
pub fn lookup_field_max_length(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<u32> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(spec) = rule.field_types.get(field) {
            return spec.effective_max_length();
        }
    }
    None
}

/// Coerce a raw CLI value into a typed JSON `Value` matching the declared type.
pub fn coerce_value_for_type(
    field_type: &str,
    raw: &str,
    max_length: Option<u32>,
) -> Result<Value, CoerceError> {
    match field_type {
        "datetime" => {
            if crate::standards::predicates::is_datetime_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(CoerceError::InvalidDatetime {
                    value: raw.to_string(),
                })
            }
        }
        "date" => {
            if crate::standards::predicates::is_date_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(CoerceError::InvalidDate {
                    value: raw.to_string(),
                })
            }
        }
        "wikilink" | "wikilink_or_list" => {
            let wrapped = wrap_wikilink(raw);
            if !crate::standards::predicates::is_wikilink_string(&wrapped) {
                return Err(CoerceError::InvalidWikilink {
                    value: raw.to_string(),
                });
            }
            Ok(Value::String(wrapped))
        }
        "list_of_strings" => {
            check_max_length(raw, max_length, "list_of_strings")?;
            Ok(Value::Array(vec![Value::String(raw.to_string())]))
        }
        "string" => {
            check_max_length(raw, max_length, "string")?;
            Ok(Value::String(raw.to_string()))
        }
        "text" => Ok(Value::String(raw.to_string())),
        unknown => Err(CoerceError::UnknownFieldType {
            field_type: unknown.to_string(),
        }),
    }
}

fn check_max_length(
    raw: &str,
    max_length: Option<u32>,
    field_type: &str,
) -> Result<(), CoerceError> {
    if let Some(bound) = max_length {
        if raw.chars().count() > bound as usize {
            return Err(CoerceError::ValueTooLong {
                value: raw.to_string(),
                bound,
                field_type: field_type.to_string(),
            });
        }
    }
    Ok(())
}

fn wrap_wikilink(raw: &str) -> String {
    if raw.starts_with("[[") && raw.ends_with("]]") {
        raw.to_string()
    } else {
        format!("[[{raw}]]")
    }
}

/// Is `field` declared required-frontmatter by any rule matching `doc`?
pub fn is_required_field(cfg: &VaultConfig, doc: &Document, field: &str) -> bool {
    cfg.validate
        .rules
        .iter()
        .filter(|r| crate::standards::engine::rule_matches(doc, r))
        .any(|r| r.required_frontmatter.iter().any(|f| f == field))
}

/// Is `field` declared by any rule in `rules` via `field_types` (typed),
/// `allowed_values`, `required_frontmatter`, `field_references`, or
/// `forbidden_frontmatter`? The known-field predicate both verbs share.
pub fn field_known_in_rules<'a>(
    rules: impl IntoIterator<Item = &'a ValidateRule>,
    field: &str,
) -> bool {
    for rule in rules {
        let has_typed = rule
            .field_types
            .get(field)
            .is_some_and(|spec: &FieldTypeSpec| spec.type_name().is_some());
        if has_typed
            || rule.allowed_values.contains_key(field)
            || rule.required_frontmatter.iter().any(|f| f == field)
            || rule.field_references.contains_key(field)
            || rule.forbidden_frontmatter.iter().any(|f| f == field)
        {
            return true;
        }
    }
    false
}

/// Is `field` known to the schema for `doc`?
pub fn is_known_field(cfg: &VaultConfig, doc: &Document, field: &str) -> bool {
    let matching = cfg
        .validate
        .rules
        .iter()
        .filter(|rule| crate::standards::engine::rule_matches(doc, rule));
    field_known_in_rules(matching, field)
}

/// The allowed-value set for `field` from the first matching rule that declares
/// one.
pub fn lookup_allowed_values(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<Vec<Value>> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(values) = rule.allowed_values.get(field) {
            return Some(values.clone());
        }
    }
    None
}

/// Does `value` itself match one entry of `allowed`? Non-recursive: a nested
/// array never matches a scalar allowed entry, so a nested-array element is
/// always rejected here rather than being unpacked and matched element-wise.
/// Shared by [`value_in_allowed`]'s array arm and `standards::checks`'
/// per-element loop so both engines reject a nested-array element the same
/// way.
pub(crate) fn matches_one_allowed(value: &Value, allowed: &[Value]) -> bool {
    allowed
        .iter()
        .any(|a| crate::standards::predicates::frontmatter_value_matches(value, a))
}

/// Does `value` satisfy the `allowed` set? Scalars match one entry; arrays
/// require every element to match one entry (an element that is itself an
/// array never matches, since [`matches_one_allowed`] doesn't recurse).
pub fn value_in_allowed(value: &Value, allowed: &[Value]) -> bool {
    match value {
        Value::Array(items) => items.iter().all(|item| matches_one_allowed(item, allowed)),
        scalar => matches_one_allowed(scalar, allowed),
    }
}

/// Render an allowed-value set for an error message: `a, b, c`.
pub fn display_allowed(allowed: &[Value]) -> String {
    allowed
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render a single JSON scalar for a refusal message: a JSON string unwraps to
/// its bare text (`foo`, not `"foo"`); every other scalar uses JSON's own
/// display.
pub fn display_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

/// The `set`/`new` "value not allowed" refusal prose: one function both verbs
/// call for the same condition, so the message never drifts between them.
pub fn value_not_allowed_message(field: &str, value: &str, allowed: &str) -> String {
    format!(
        "value '{value}' is not allowed for '{field}' (allowed: {allowed}); use --force to override"
    )
}

/// The `--force` bypass warning both verbs emit when a schema check is
/// overridden rather than enforced.
pub fn force_bypass_warning(field: &str, what: &str) -> MutationWarning {
    MutationWarning {
        code: "force-bypass".into(),
        field: Some(field.to_string()),
        message: format!("--force bypassed {what} for '{field}'"),
    }
}

/// Build the post-state document for schema resolution: overlay the batch's
/// `--field` / `--field-json` changes onto the document's current frontmatter so
/// rule matching sees the INCOMING state (NRN-119). `--field` values overlay as
/// raw strings (a type-flipped enum discriminator stays a string predicate);
/// `--remove` / `--push` / `--pop` are NOT overlaid.
pub fn effective_match_doc(
    doc: &Document,
    current_frontmatter: &Value,
    fields: &[String],
    field_json: &[String],
) -> Document {
    let mut fm = current_frontmatter.as_object().cloned().unwrap_or_default();
    for kv in fields {
        if let Some((k, raw)) = split_kv(kv) {
            fm.insert(k, Value::String(raw));
        }
    }
    for kv in field_json {
        if let Some((k, raw)) = split_kv(kv) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                fm.insert(k, v);
            }
        }
    }
    let mut d = doc.clone();
    d.frontmatter = Some(Value::Object(fm));
    d
}
