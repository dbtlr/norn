//! CLI→service routing translation for `norn count` (NRN-94).
//!
//! `count` is routable byte-identically because the `vault.count` MCP tool's
//! `CountEnvelope` is a *lossless* re-encoding of [`CountOutput`]: the client
//! can rebuild the exact `CountOutput` the daemon computed and render it through
//! the SAME `count::render` functions the direct path uses, so routed and direct
//! stdout are byte-for-byte equal. (Contrast `find`/`get`, whose MCP tools drop
//! render-critical state — see `try_route_read` in `src/lib.rs`.)
//!
//! Both functions here are pure so they unit-test without a live daemon; the
//! probe + wire round-trip live in the routing seam (`src/lib.rs`).

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::cli::CountArgs;
use crate::count::{CountOutput, GroupNode};
use crate::route_wire::{get_usize, insert_filter_args, json_type};

/// Translate parsed `norn count` args into the `vault.count` tool's parameter
/// object (the `CountParams` shape in `src/mcp/tools/count.rs`).
///
/// The `--format` flag is deliberately absent: it is a CLI-only rendering knob
/// (the client renders the returned structured data), never a query input.
pub fn to_mcp_arguments(args: &CountArgs) -> Value {
    let mut map = Map::new();

    // `--by` is a repeatable/comma-delimited field list on the CLI (a
    // `Vec<String>`); the tool takes a single comma-joined token it re-splits.
    // The join→split round-trip is faithful (values never contain commas — clap
    // already split on them), and `count::run` trims segments on both surfaces.
    if !args.by.is_empty() {
        map.insert("by".into(), Value::String(args.by.join(",")));
    }

    // Filter predicate lists map 1:1 to identically-named tool fields (the `in`
    // field is serde-renamed from `r#in` on the tool side).
    insert_filter_args(&mut map, &args.filters);

    Value::Object(map)
}

/// Rebuild a [`CountOutput`] from a `vault.count` `structuredContent` object.
///
/// The `by` field discriminates the variant (absent/null → total; string →
/// single-key group; array → multi-key group); `groups` and `by` deserialize
/// back into the exact BTreeMap/enum shapes `count::run` produced. Rendering the
/// result through `count::render` then yields byte-identical output to a direct
/// `norn count`. Any shape mismatch is an `Err`, which the caller maps to a
/// verified direct open.
pub fn reconstruct(structured: &Value) -> Result<CountOutput> {
    let total = get_usize(structured, "vault.count", "total")?;

    match structured.get("by") {
        None | Some(Value::Null) => Ok(CountOutput::Total { total }),
        Some(Value::String(field)) => {
            let groups: BTreeMap<String, usize> = groups_value(structured)?;
            Ok(CountOutput::Grouped {
                by: field.clone(),
                total,
                groups,
            })
        }
        Some(by @ Value::Array(_)) => {
            let by: Vec<String> = serde_json::from_value(by.clone())?;
            let groups: BTreeMap<String, GroupNode> = groups_value(structured)?;
            Ok(CountOutput::GroupedMulti { by, total, groups })
        }
        Some(other) => anyhow::bail!(
            "vault.count envelope: `by` must be null, a string, or an array, got {}",
            json_type(Some(other))
        ),
    }
}

/// Deserialize the `groups` object of a count envelope into `T`.
fn groups_value<T: serde::de::DeserializeOwned>(structured: &Value) -> Result<T> {
    let groups = structured.get("groups").ok_or_else(|| {
        anyhow::anyhow!("vault.count envelope: grouped count is missing `groups`")
    })?;
    Ok(T::deserialize(groups)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CountFormat;
    use crate::filter_args::FilterArgs;
    use crate::mcp::tools::count::CountEnvelope;
    use serde_json::json;

    fn args(by: Vec<&str>, filters: FilterArgs) -> CountArgs {
        CountArgs {
            by: by.into_iter().map(String::from).collect(),
            filters,
            format: CountFormat::Text,
        }
    }

    #[test]
    fn to_mcp_arguments_maps_by_and_filters() {
        let filters = FilterArgs {
            eq: vec!["type:note".into()],
            text: Some("hello".into()),
            unresolved_links: true,
            ..FilterArgs::default()
        };
        let v = to_mcp_arguments(&args(vec!["type", "status"], filters));

        assert_eq!(v["by"], "type,status");
        assert_eq!(v["eq"], json!(["type:note"]));
        assert_eq!(v["text"], "hello");
        assert_eq!(v["unresolved_links"], true);
        // Empty predicate lists are omitted, not sent as empty arrays.
        assert!(v.get("not_eq").is_none());
    }

    #[test]
    fn to_mcp_arguments_total_mode_omits_by() {
        let v = to_mcp_arguments(&args(vec![], FilterArgs::default()));
        assert!(v.get("by").is_none(), "no --by must omit the field: {v}");
    }

    /// The reconstruction is the exact inverse of the daemon's envelope
    /// projection: `CountOutput` → `CountEnvelope` (what the tool serializes) →
    /// reconstruct → the ORIGINAL `CountOutput`, for every variant. This is the
    /// byte-identity guarantee (rendering the rebuilt value equals rendering the
    /// direct value).
    fn assert_round_trip(out: CountOutput) {
        let envelope = CountEnvelope::from_output(out.clone());
        let structured = serde_json::to_value(&envelope).unwrap();
        let rebuilt = reconstruct(&structured).unwrap();
        assert_eq!(rebuilt, out, "reconstruct must invert the envelope");
        // And the rendered bytes match in both formats.
        for fmt in [CountFormat::Json, CountFormat::Text] {
            let mut a = Vec::new();
            let mut b = Vec::new();
            crate::count::emit(&out, fmt, &mut a).unwrap();
            crate::count::emit(&rebuilt, fmt, &mut b).unwrap();
            assert_eq!(a, b, "rendered bytes must match for {fmt:?}");
        }
    }

    #[test]
    fn round_trip_total() {
        assert_round_trip(CountOutput::Total { total: 42 });
    }

    #[test]
    fn round_trip_grouped() {
        let groups = [("active".to_string(), 3), ("backlog".to_string(), 7)]
            .into_iter()
            .collect();
        assert_round_trip(CountOutput::Grouped {
            by: "status".into(),
            total: 10,
            groups,
        });
    }

    #[test]
    fn round_trip_grouped_multi() {
        let mut inner = BTreeMap::new();
        inner.insert("active".to_string(), GroupNode::Leaf(2));
        inner.insert("backlog".to_string(), GroupNode::Leaf(1));
        let mut groups = BTreeMap::new();
        groups.insert("note".to_string(), GroupNode::Branch(inner));
        groups.insert("task".to_string(), GroupNode::Leaf(4));
        assert_round_trip(CountOutput::GroupedMulti {
            by: vec!["type".into(), "status".into()],
            total: 7,
            groups,
        });
    }

    // ── Adversarial round-trip coverage (NRN-94 review F9): the reconstruct↔
    //    envelope isomorphism must survive every group-key shape the count path
    //    can actually emit, not just the tidy alpha-numeric ones above. ──

    /// An empty-string group key round-trips (a document with an empty-valued
    /// field lands under the `""` key).
    #[test]
    fn round_trip_empty_string_key() {
        let groups = [(String::new(), 5)].into_iter().collect();
        assert_round_trip(CountOutput::Grouped {
            by: "status".into(),
            total: 5,
            groups,
        });
    }

    /// Unicode and whitespace-laden keys round-trip byte-for-byte (JSON escaping
    /// and BTreeMap ordering must not mangle them).
    #[test]
    fn round_trip_unicode_and_whitespace_keys() {
        let groups = [
            ("café ☕".to_string(), 2),
            ("  leading+trailing  ".to_string(), 1),
            ("tab\there".to_string(), 3),
            ("日本語".to_string(), 4),
        ]
        .into_iter()
        .collect();
        assert_round_trip(CountOutput::Grouped {
            by: "title".into(),
            total: 10,
            groups,
        });
    }

    /// The `(missing)` / `(null)` sentinels count::run emits for absent/null
    /// field values are ordinary string keys on the wire and must round-trip.
    #[test]
    fn round_trip_missing_and_null_sentinels() {
        let groups = [
            ("(missing)".to_string(), 3),
            ("(null)".to_string(), 2),
            ("real".to_string(), 1),
        ]
        .into_iter()
        .collect();
        assert_round_trip(CountOutput::Grouped {
            by: "status".into(),
            total: 6,
            groups,
        });
    }

    /// Numeric-string keys (a field whose values look like numbers) must stay
    /// strings, never get coerced to JSON numbers by the envelope round-trip.
    #[test]
    fn round_trip_numeric_string_keys() {
        let groups = [
            ("0".to_string(), 1),
            ("42".to_string(), 2),
            ("007".to_string(), 3),
            ("3.14".to_string(), 4),
        ]
        .into_iter()
        .collect();
        assert_round_trip(CountOutput::Grouped {
            by: "priority".into(),
            total: 10,
            groups,
        });
    }

    /// Total = 0 with an empty group map (a `--by` over a vault where nothing
    /// matches) round-trips to the same empty distribution.
    #[test]
    fn round_trip_zero_total_empty_groups() {
        assert_round_trip(CountOutput::Total { total: 0 });
        assert_round_trip(CountOutput::Grouped {
            by: "status".into(),
            total: 0,
            groups: BTreeMap::new(),
        });
        assert_round_trip(CountOutput::GroupedMulti {
            by: vec!["type".into(), "status".into()],
            total: 0,
            groups: BTreeMap::new(),
        });
    }

    /// Nesting deeper than two levels (`--by a,b,c`) round-trips: Branch →
    /// Branch → Leaf must reconstruct the exact nested shape.
    #[test]
    fn round_trip_three_level_nesting() {
        let mut lvl3 = BTreeMap::new();
        lvl3.insert("high".to_string(), GroupNode::Leaf(2));
        lvl3.insert("low".to_string(), GroupNode::Leaf(1));
        let mut lvl2 = BTreeMap::new();
        lvl2.insert("active".to_string(), GroupNode::Branch(lvl3));
        lvl2.insert("done".to_string(), GroupNode::Leaf(3));
        let mut groups = BTreeMap::new();
        groups.insert("note".to_string(), GroupNode::Branch(lvl2));
        assert_round_trip(CountOutput::GroupedMulti {
            by: vec!["type".into(), "status".into(), "priority".into()],
            total: 6,
            groups,
        });
    }
}
