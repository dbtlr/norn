//! Wire-translation helpers shared by the CLI→daemon route seams
//! (`count::route`, `find::route`, `show::route`).
//!
//! One home for the arg→MCP-parameter mapping and the envelope-reading
//! primitives, so the routed commands cannot drift from each other — a new
//! `FilterArgs` / `SortPaginateArgs` field is a compile error here (exhaustive
//! destructures), not a silently-dropped wire field in one command.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cli::{FilterArgs, SortPaginateArgs};

/// Insert every [`FilterArgs`] predicate into a tool-parameter object, mapping
/// 1:1 to the identically-named tool fields (the `in` field is serde-renamed
/// from `r#in` on the tool side). Empty lists and default booleans are omitted,
/// not sent. The exhaustive destructure makes a new filter flag a compile error
/// here instead of a predicate silently missing from one command's wire.
pub(crate) fn insert_filter_args(map: &mut Map<String, Value>, f: &FilterArgs) {
    let FilterArgs {
        text,
        eq,
        not_eq,
        r#in,
        not_in,
        starts_with,
        ends_with,
        contains,
        has,
        missing,
        before,
        after,
        on,
        path,
        links_to,
        unresolved_links,
    } = f;

    if let Some(text) = text {
        map.insert("text".into(), Value::String(text.clone()));
    }
    insert_list(map, "eq", eq);
    insert_list(map, "not_eq", not_eq);
    insert_list(map, "in", r#in);
    insert_list(map, "not_in", not_in);
    insert_list(map, "starts_with", starts_with);
    insert_list(map, "ends_with", ends_with);
    insert_list(map, "contains", contains);
    insert_list(map, "has", has);
    insert_list(map, "missing", missing);
    insert_list(map, "before", before);
    insert_list(map, "after", after);
    insert_list(map, "on", on);
    insert_list(map, "path", path);
    insert_list(map, "links_to", links_to);
    if *unresolved_links {
        map.insert("unresolved_links".into(), Value::Bool(true));
    }
}

/// Insert the [`SortPaginateArgs`] sort/limit/paging knobs, name-for-name with
/// the `vault.find` / `vault.get` tool params. An omitted `limit` stays absent
/// (each tool applies its own CLI-matching default); `starts_at` is sent only
/// when non-default (both surfaces default to 1 and floor at 1).
pub(crate) fn insert_paging(map: &mut Map<String, Value>, p: &SortPaginateArgs) {
    let SortPaginateArgs {
        sort,
        desc,
        limit,
        no_limit,
        starts_at,
    } = p;

    if let Some(sort) = sort {
        map.insert("sort".into(), Value::String(sort.clone()));
    }
    if *desc {
        map.insert("desc".into(), Value::Bool(true));
    }
    if let Some(limit) = limit {
        map.insert("limit".into(), Value::Number((*limit).into()));
    }
    if *no_limit {
        map.insert("no_limit".into(), Value::Bool(true));
    }
    if *starts_at != 1 {
        map.insert("starts_at".into(), Value::Number((*starts_at).into()));
    }
}

/// Insert a string list under `key`, omitting the key entirely when empty.
pub(crate) fn insert_list(map: &mut Map<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        map.insert(
            key.into(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    }
}

/// Deserialize `obj[key]` into `Vec<T>`, treating an absent key as an empty vec
/// (a facet the projection did not include). Deserializes from a borrow of the
/// value — no clone of the (potentially large) facet payload.
pub(crate) fn take_vec<T: serde::de::DeserializeOwned>(
    obj: &Map<String, Value>,
    key: &str,
) -> Result<Vec<T>> {
    match obj.get(key) {
        Some(value) => Ok(Vec::<T>::deserialize(value)?),
        None => Ok(Vec::new()),
    }
}

/// The observed JSON shape of an (optional) value, for envelope-shape error
/// messages that name what was found WITHOUT embedding the payload.
pub(crate) fn json_type(v: Option<&Value>) -> &'static str {
    match v {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "a bool",
        Some(Value::Number(_)) => "a number",
        Some(Value::String(_)) => "a string",
        Some(Value::Array(_)) => "an array",
        Some(Value::Object(_)) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insert_filter_args_omits_defaults() {
        let mut map = Map::new();
        insert_filter_args(&mut map, &FilterArgs::default());
        assert!(map.is_empty(), "default filters must add nothing: {map:?}");
    }

    #[test]
    fn insert_filter_args_maps_all_set_fields() {
        let f = FilterArgs {
            text: Some("hello".into()),
            eq: vec!["type:note".into()],
            not_in: vec!["status:done,archived".into()],
            unresolved_links: true,
            ..FilterArgs::default()
        };
        let mut map = Map::new();
        insert_filter_args(&mut map, &f);
        assert_eq!(map["text"], "hello");
        assert_eq!(map["eq"], json!(["type:note"]));
        assert_eq!(map["not_in"], json!(["status:done,archived"]));
        assert_eq!(map["unresolved_links"], true);
        assert!(map.get("not_eq").is_none());
    }

    #[test]
    fn insert_paging_omits_defaults_and_maps_set_fields() {
        let mut map = Map::new();
        insert_paging(
            &mut map,
            &SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
        );
        assert!(map.is_empty(), "default paging must add nothing: {map:?}");

        insert_paging(
            &mut map,
            &SortPaginateArgs {
                sort: Some("created".into()),
                desc: true,
                limit: Some(5),
                no_limit: false,
                starts_at: 3,
            },
        );
        assert_eq!(map["sort"], "created");
        assert_eq!(map["desc"], true);
        assert_eq!(map["limit"], 5);
        assert_eq!(map["starts_at"], 3);
        assert!(map.get("no_limit").is_none());
    }

    #[test]
    fn take_vec_absent_is_empty_and_present_deserializes() {
        let obj = json!({ "xs": ["a", "b"] });
        let obj = obj.as_object().unwrap();
        let xs: Vec<String> = take_vec(obj, "xs").unwrap();
        assert_eq!(xs, vec!["a".to_string(), "b".to_string()]);
        let none: Vec<String> = take_vec(obj, "missing").unwrap();
        assert!(none.is_empty());
        assert!(take_vec::<String>(&json!({"xs": 3}).as_object().unwrap().clone(), "xs").is_err());
    }

    #[test]
    fn json_type_names_shapes() {
        assert_eq!(json_type(None), "absent");
        assert_eq!(json_type(Some(&json!(null))), "null");
        assert_eq!(json_type(Some(&json!(1))), "a number");
        assert_eq!(json_type(Some(&json!([]))), "an array");
    }
}
