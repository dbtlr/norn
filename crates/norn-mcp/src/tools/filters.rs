//! The find-filter predicate surface the read tools share.
//!
//! `vault.find`, `vault.count`, and `vault.describe` all narrow the same
//! document population with the same sixteen predicates, and each publishes
//! those predicates as part of its own `inputSchema`. [`filter_params`] is the
//! single definition they are generated from: field names, types,
//! `#[serde(default)]` markers, and the doc comments that BECOME the published
//! descriptions live here once, so the three tools cannot drift from each other.
//!
//! A macro rather than `#[serde(flatten)]`: flatten is incompatible with
//! `deny_unknown_fields`, which every tool's param struct requires so a client
//! typo fails loudly instead of silently running an unintended query. A nested
//! sub-struct would also nest the predicates a level deep in the published
//! schema, changing the wire shape. Generating the fields keeps both the flat
//! wire shape and per-tool strictness.

/// Declare an MCP param struct that carries the shared find-filter predicates.
///
/// The invocation supplies the struct's attributes, its name, and its
/// verb-specific fields; the expansion appends the sixteen predicate fields and
/// a `to_filter` accessor that builds the wire [`FilterParams`](norn_wire::FilterParams).
///
/// ```text
/// filter_params! {
///     /// Parameters for `vault.example`.
///     #[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
///     #[serde(deny_unknown_fields)]
///     pub struct ExampleParams {
///         /// A verb-specific knob.
///         #[serde(default)]
///         pub by: Option<String>,
///     }
/// }
/// ```
macro_rules! filter_params {
    (
        $(#[$struct_meta:meta])*
        pub struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                pub $field:ident : $ty:ty,
            )*
        }
    ) => {
        $(#[$struct_meta])*
        pub struct $name {
            $(
                $(#[$field_meta])*
                pub $field: $ty,
            )*

            // ── Filter predicates (the shared find-filter surface) ───────────
            /// Full-text body substring. Case-insensitive.
            #[serde(default)]
            pub text: Option<String>,

            /// Frontmatter equality predicates `field:value`. Repeatable; all must match.
            #[serde(default)]
            pub eq: Vec<String>,

            /// Frontmatter inequality predicates `field:value`. Repeatable.
            #[serde(default)]
            pub not_eq: Vec<String>,

            /// Frontmatter ANY-of predicates `field:V1,V2,...`. Repeatable.
            #[serde(default)]
            #[serde(rename = "in")]
            pub r#in: Vec<String>,

            /// Frontmatter NOT-in predicates `field:V1,V2,...`. Repeatable.
            #[serde(default)]
            pub not_in: Vec<String>,

            /// Frontmatter prefix predicates `field:VALUE` — the field (or any array
            /// element) starts with VALUE. Case-sensitive. Repeatable; all must match.
            #[serde(default)]
            pub starts_with: Vec<String>,

            /// Frontmatter suffix predicates `field:VALUE` — the field (or any array
            /// element) ends with VALUE. Case-sensitive. Repeatable.
            #[serde(default)]
            pub ends_with: Vec<String>,

            /// Frontmatter substring predicates `field:VALUE` — the field (or any
            /// array element) contains VALUE. Case-sensitive. Repeatable.
            #[serde(default)]
            pub contains: Vec<String>,

            /// Frontmatter fields that must be present (non-null). Repeatable.
            #[serde(default)]
            pub has: Vec<String>,

            /// Frontmatter fields that must be absent or null. Repeatable.
            #[serde(default)]
            pub missing: Vec<String>,

            /// Date-before predicates `field:DATE`. ISO 8601. Repeatable.
            #[serde(default)]
            pub before: Vec<String>,

            /// Date-after predicates `field:DATE`. ISO 8601. Repeatable.
            #[serde(default)]
            pub after: Vec<String>,

            /// Date-on predicates `field:DATE`. Accepts `today`. Repeatable.
            #[serde(default)]
            pub on: Vec<String>,

            /// Path glob patterns. Repeatable.
            #[serde(default)]
            pub path: Vec<String>,

            /// Documents whose outgoing links resolve to TARGET. Repeatable; AND'd.
            #[serde(default)]
            pub links_to: Vec<String>,

            /// Include only documents with at least one unresolved link.
            #[serde(default)]
            pub unresolved_links: bool,
        }

        impl $name {
            /// The predicates as the shared wire filter request.
            pub(crate) fn to_filter(&self) -> ::norn_wire::FilterParams {
                ::norn_wire::FilterParams {
                    text: self.text.clone(),
                    eq: self.eq.clone(),
                    not_eq: self.not_eq.clone(),
                    r#in: self.r#in.clone(),
                    not_in: self.not_in.clone(),
                    starts_with: self.starts_with.clone(),
                    ends_with: self.ends_with.clone(),
                    contains: self.contains.clone(),
                    has: self.has.clone(),
                    missing: self.missing.clone(),
                    before: self.before.clone(),
                    after: self.after.clone(),
                    on: self.on.clone(),
                    path: self.path.clone(),
                    links_to: self.links_to.clone(),
                    unresolved_links: self.unresolved_links,
                }
            }
        }
    };
}

pub(crate) use filter_params;

/// Every predicate the shared surface publishes, in wire spelling. Named here
/// so the pinning tests below assert against one list rather than each tool's
/// own schema.
#[cfg(test)]
pub(crate) const FILTER_KEYS: &[&str] = &[
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
];

#[cfg(test)]
mod tests {
    use super::FILTER_KEYS;
    use crate::tools::{count::CountParams, describe::DescribeParams, find::FindParams};
    use serde_json::{Map, Value};

    /// The `properties` object of a param struct's published `inputSchema`.
    fn properties<T: schemars::JsonSchema>() -> Map<String, Value> {
        let schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
        schema["properties"]
            .as_object()
            .expect("a param schema publishes properties")
            .clone()
    }

    #[test]
    fn the_three_read_tools_publish_one_identical_filter_surface() {
        // The whole point of the shared declaration: name, type, default, AND
        // description are one definition, so no tool can publish a narrower or
        // staler account of a predicate than its siblings.
        let find = properties::<FindParams>();
        let count = properties::<CountParams>();
        let describe = properties::<DescribeParams>();
        for key in FILTER_KEYS {
            let expected = find
                .get(*key)
                .unwrap_or_else(|| panic!("find publishes {key}"));
            assert_eq!(
                count.get(*key),
                Some(expected),
                "vault.count's `{key}` schema differs from vault.find's"
            );
            assert_eq!(
                describe.get(*key),
                Some(expected),
                "vault.describe's `{key}` schema differs from vault.find's"
            );
        }
    }

    #[test]
    fn every_composing_tool_still_denies_unknown_params() {
        // `deny_unknown_fields` is a per-invocation obligation, not a property
        // of the macro: the strictness attribute rides each tool's own struct
        // attrs, so a new invocation that forgets it would silently accept a
        // client typo and run an unintended query.
        macro_rules! assert_denies_unknown {
            ($($t:ty),+ $(,)?) => {$({
                let err = serde_json::from_value::<$t>(serde_json::json!({"bogus": 1}))
                    .unwrap_err();
                let message = err.to_string();
                assert!(
                    message.contains("unknown field") && message.contains("bogus"),
                    "{} accepted an unknown param, got: {message}",
                    stringify!($t)
                );
            })+};
        }
        assert_denies_unknown!(FindParams, CountParams, DescribeParams);
    }

    #[test]
    fn the_filter_surface_is_exactly_these_sixteen_predicates() {
        // Adding or removing a predicate moves the published contract of three
        // tools at once, so it is a deliberate act that updates this list.
        let find = properties::<FindParams>();
        let verb_specific = [
            "sort",
            "desc",
            "limit",
            "no_limit",
            "starts_at",
            "col",
            "all_cols",
        ];
        let mut published: Vec<&str> = find
            .keys()
            .map(String::as_str)
            .filter(|k| !verb_specific.contains(k))
            .collect();
        published.sort_unstable();
        let mut expected: Vec<&str> = FILTER_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(published, expected);
    }

    #[test]
    fn every_predicate_reaches_the_wire_filter() {
        let p: FindParams = serde_json::from_value(serde_json::json!({
            "text": "body",
            "eq": ["type:note"],
            "not_eq": ["status:done"],
            "in": ["status:a,b"],
            "not_in": ["status:c"],
            "starts_with": ["title:A"],
            "ends_with": ["title:Z"],
            "contains": ["title:mid"],
            "has": ["project"],
            "missing": ["archived"],
            "before": ["created:2026-01-01"],
            "after": ["created:2025-01-01"],
            "on": ["created:today"],
            "path": ["notes/**"],
            "links_to": ["alpha"],
            "unresolved_links": true,
        }))
        .expect("the filter surface deserializes");
        let filter = p.to_filter();
        assert_eq!(filter.text.as_deref(), Some("body"));
        assert_eq!(filter.eq, vec!["type:note".to_string()]);
        assert_eq!(filter.not_eq, vec!["status:done".to_string()]);
        assert_eq!(filter.r#in, vec!["status:a,b".to_string()]);
        assert_eq!(filter.not_in, vec!["status:c".to_string()]);
        assert_eq!(filter.starts_with, vec!["title:A".to_string()]);
        assert_eq!(filter.ends_with, vec!["title:Z".to_string()]);
        assert_eq!(filter.contains, vec!["title:mid".to_string()]);
        assert_eq!(filter.has, vec!["project".to_string()]);
        assert_eq!(filter.missing, vec!["archived".to_string()]);
        assert_eq!(filter.before, vec!["created:2026-01-01".to_string()]);
        assert_eq!(filter.after, vec!["created:2025-01-01".to_string()]);
        assert_eq!(filter.on, vec!["created:today".to_string()]);
        assert_eq!(filter.path, vec!["notes/**".to_string()]);
        assert_eq!(filter.links_to, vec!["alpha".to_string()]);
        assert!(filter.unresolved_links);
    }
}
