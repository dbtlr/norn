//! The shared filter-predicate wire vocabulary.

use serde::{Deserialize, Serialize};

/// The filter predicates shared by the read verbs (`find`, `count`, …), one
/// field per predicate, wire-named exactly as the tool surface expects.
///
/// This is the typed wire encoding, expressed as serde attributes instead of
/// imperative map inserts. Every default/empty predicate is OMITTED from the
/// wire (an empty
/// list or `text: None` sends nothing), and deserialization treats an absent key
/// as the default, so a fully-default value round-trips through `{}`.
///
/// The `in` predicate is serialized as `in` (a Rust keyword on the field, hence
/// the `r#in` identifier and the explicit `#[serde(rename = "in")]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterParams {
    /// Full-text body substring. Case-insensitive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Frontmatter equality predicate `field:value`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub eq: Vec<String>,

    /// Frontmatter `field` is NOT equal to `value`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_eq: Vec<String>,

    /// Frontmatter `field` is one of the comma-separated values (ANY-of).
    #[serde(rename = "in", skip_serializing_if = "Vec::is_empty")]
    pub r#in: Vec<String>,

    /// Frontmatter `field` is NOT one of the comma-separated values.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_in: Vec<String>,

    /// Frontmatter `field` (or any array element) starts with `value`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub starts_with: Vec<String>,

    /// Frontmatter `field` (or any array element) ends with `value`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ends_with: Vec<String>,

    /// Frontmatter `field` (or any array element) contains `value`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,

    /// Frontmatter `field` is present (non-null).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<String>,

    /// Frontmatter `field` is absent or null.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,

    /// Frontmatter `field` (a date) is before `date`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,

    /// Frontmatter `field` (a date) is after `date`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,

    /// Frontmatter `field` (a date) is exactly `date`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub on: Vec<String>,

    /// Path glob pattern.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,

    /// Documents whose outgoing links resolve to the target.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links_to: Vec<String>,

    /// Documents with at least one unresolved link.
    #[serde(skip_serializing_if = "is_false")]
    pub unresolved_links: bool,
}

/// `skip_serializing_if` predicate: a `false` bool is a default the wire omits.
fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_serializes_to_empty_object() {
        assert_eq!(
            serde_json::to_value(FilterParams::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn set_fields_map_one_to_one() {
        let params = FilterParams {
            text: Some("hello".into()),
            eq: vec!["type:note".into()],
            not_in: vec!["status:done,archived".into()],
            unresolved_links: true,
            ..FilterParams::default()
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(wire["text"], "hello");
        assert_eq!(wire["eq"], json!(["type:note"]));
        assert_eq!(wire["not_in"], json!(["status:done,archived"]));
        assert_eq!(wire["unresolved_links"], true);
        assert!(wire.get("not_eq").is_none());
        assert!(wire.get("in").is_none());
    }

    #[test]
    fn in_is_wire_named_in() {
        let params = FilterParams {
            r#in: vec!["status:backlog,active".into()],
            ..FilterParams::default()
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(wire["in"], json!(["status:backlog,active"]));
        assert!(wire.as_object().unwrap().get("r#in").is_none());
    }

    #[test]
    fn every_predicate_omitted_when_default() {
        let wire = serde_json::to_value(FilterParams::default()).unwrap();
        for key in [
            "text",
            "eq",
            "not_eq",
            "in",
            "not_in",
            "starts_with",
            "ends_with",
            "contains",
            "has",
            "missing",
            "before",
            "after",
            "on",
            "path",
            "links_to",
            "unresolved_links",
        ] {
            assert!(
                wire.get(key).is_none(),
                "`{key}` must be omitted when default"
            );
        }
    }

    #[test]
    fn round_trips() {
        let params = FilterParams {
            text: Some("x".into()),
            r#in: vec!["a:1".into()],
            has: vec!["title".into()],
            unresolved_links: true,
            ..FilterParams::default()
        };
        let wire = serde_json::to_value(&params).unwrap();
        let back: FilterParams = serde_json::from_value(wire).unwrap();
        assert_eq!(back, params);
    }

    #[test]
    fn absent_keys_deserialize_to_default() {
        let back: FilterParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(back, FilterParams::default());
    }
}
