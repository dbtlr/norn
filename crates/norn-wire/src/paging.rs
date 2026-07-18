//! The shared sort / limit / paging wire vocabulary.

use serde::{Deserialize, Serialize};

/// The sort, limit, and paging knobs shared by the read verbs (`find`, `get`,
/// …), wire-named exactly as the tool surface expects.
///
/// The typed successor to the donor's `insert_paging`. Defaults are OMITTED from
/// the wire: `sort: None`, `desc: false`, `limit: None`, `no_limit: false`, and
/// `starts_at == 1` (the 1-indexed default) all send nothing, so a fully-default
/// value serializes to `{}`. An absent key deserializes to the default, so the
/// value round-trips.
///
/// A verb-specific default for an absent `limit` (e.g. `find`'s implicit 10) is
/// applied by the consuming verb, never encoded here — an omitted `limit` stays
/// absent on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SortPaginateParams {
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,

    /// Sort descending (only meaningful with `sort`).
    #[serde(skip_serializing_if = "is_false")]
    pub desc: bool,

    /// Maximum number of records to return. Absent means the verb applies its
    /// own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// Return all records; no limit. Overrides `limit`.
    #[serde(skip_serializing_if = "is_false")]
    pub no_limit: bool,

    /// 1-indexed starting offset for paging. Default 1; omitted when default.
    ///
    /// Deliberately NOT a non-zero type: the v0.48 tool surface accepts `0`
    /// and floors it to 1 verb-side ("default to 1 and floor at 1"), so the
    /// wire stays permissive to reproduce that behavior frame-for-frame under
    /// the parity harness. Producers (the CLI) clamp before construction;
    /// flooring is the consuming verb's job.
    #[serde(skip_serializing_if = "is_default_start")]
    pub starts_at: usize,
}

/// The default paging start (1-indexed).
const DEFAULT_STARTS_AT: usize = 1;

impl Default for SortPaginateParams {
    fn default() -> Self {
        Self {
            sort: None,
            desc: false,
            limit: None,
            no_limit: false,
            starts_at: DEFAULT_STARTS_AT,
        }
    }
}

/// `skip_serializing_if` predicate: a `false` bool is a default the wire omits.
fn is_false(b: &bool) -> bool {
    !*b
}

/// `skip_serializing_if` predicate: the default 1-indexed start is omitted.
fn is_default_start(n: &usize) -> bool {
    *n == DEFAULT_STARTS_AT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_serializes_to_empty_object() {
        assert_eq!(
            serde_json::to_value(SortPaginateParams::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn set_fields_map_one_to_one() {
        let params = SortPaginateParams {
            sort: Some("created".into()),
            desc: true,
            limit: Some(5),
            no_limit: false,
            starts_at: 3,
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(wire["sort"], "created");
        assert_eq!(wire["desc"], true);
        assert_eq!(wire["limit"], 5);
        assert_eq!(wire["starts_at"], 3);
        assert!(wire.get("no_limit").is_none());
    }

    #[test]
    fn starts_at_one_is_omitted() {
        let params = SortPaginateParams {
            starts_at: 1,
            ..SortPaginateParams::default()
        };
        let wire = serde_json::to_value(&params).unwrap();
        assert!(wire.get("starts_at").is_none());
    }

    #[test]
    fn no_limit_and_desc_omitted_when_false() {
        let wire = serde_json::to_value(SortPaginateParams::default()).unwrap();
        assert!(wire.get("no_limit").is_none());
        assert!(wire.get("desc").is_none());
        assert!(wire.get("limit").is_none());
        assert!(wire.get("sort").is_none());
    }

    #[test]
    fn absent_starts_at_deserializes_to_one() {
        let back: SortPaginateParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(back.starts_at, 1);
        assert_eq!(back, SortPaginateParams::default());
    }

    #[test]
    fn round_trips() {
        let params = SortPaginateParams {
            sort: Some("title".into()),
            desc: true,
            limit: Some(42),
            no_limit: true,
            starts_at: 7,
        };
        let wire = serde_json::to_value(&params).unwrap();
        let back: SortPaginateParams = serde_json::from_value(wire).unwrap();
        assert_eq!(back, params);
    }
}
