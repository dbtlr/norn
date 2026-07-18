//! Clap arg groups shared by the read commands, single-sourced so the surface
//! cannot drift between `find`, `count`, `describe`, and `get`. Each group owns
//! its `to_params` mapping into the [`norn_wire`] vocabulary — the CLI's whole
//! job on the request side is turning these flags into Params (ADR 0016).
//!
//! Help text is donor-exact (NRN-329): the doc comments below reproduce the
//! retired `src/cli.rs` `FilterArgs` / `SortPaginateArgs` verbatim so the custom
//! help renderer emits byte-identical output to the parity oracle. clap derives
//! the flag help from the doc comment and strips its single trailing period —
//! the oracle relies on exactly that, so the periods here are load-bearing.

use clap::Args;
use norn_wire::{FilterParams, SortPaginateParams};

/// The filter predicates shared by the read commands, one flag per
/// [`FilterParams`] field.
#[derive(Args, Debug, Default, Clone, PartialEq, Eq)]
pub struct FilterArgs {
    /// Full-text body substring. Case-insensitive. Empty string is a no-op.
    #[arg(long, value_name = "NEEDLE", help_heading = "Filter options")]
    pub text: Option<String>,

    /// Frontmatter equality predicate `field:value`. JSON-typed. An unknown
    /// `--field value` filters as `--eq field:value` for fields this vault knows.
    #[arg(
        long = "eq",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub eq: Vec<String>,

    /// Frontmatter `field` is NOT equal to `value`.
    #[arg(
        long = "not-eq",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub not_eq: Vec<String>,

    /// Frontmatter `field` is one of the comma-separated values (ANY-of).
    #[arg(
        long = "in",
        value_name = "FIELD:V1,V2,...",
        help_heading = "Filter options"
    )]
    pub r#in: Vec<String>,

    /// Frontmatter `field` is NOT one of the comma-separated values.
    #[arg(
        long = "not-in",
        value_name = "FIELD:V1,V2,...",
        help_heading = "Filter options"
    )]
    pub not_in: Vec<String>,

    /// Frontmatter `field` (or any array element) starts with `VALUE`. Case-sensitive.
    #[arg(
        long = "starts-with",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub starts_with: Vec<String>,

    /// Frontmatter `field` (or any array element) ends with `VALUE`. Case-sensitive.
    #[arg(
        long = "ends-with",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub ends_with: Vec<String>,

    /// Frontmatter `field` (or any array element) contains `VALUE`. Case-sensitive.
    #[arg(
        long = "contains",
        value_name = "FIELD:VALUE",
        help_heading = "Filter options"
    )]
    pub contains: Vec<String>,

    /// Frontmatter `field` is present (non-null).
    #[arg(long = "has", value_name = "FIELD", help_heading = "Filter options")]
    pub has: Vec<String>,

    /// Frontmatter `field` is absent or null.
    #[arg(
        long = "missing",
        value_name = "FIELD",
        help_heading = "Filter options"
    )]
    pub missing: Vec<String>,

    /// Frontmatter `field` (a date) is before `DATE`. ISO 8601 expected.
    #[arg(
        long = "before",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub before: Vec<String>,

    /// Frontmatter `field` (a date) is after `DATE`.
    #[arg(
        long = "after",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub after: Vec<String>,

    /// Frontmatter `field` (a date) is exactly `DATE`. Accepts `today`.
    #[arg(
        long = "on",
        value_name = "FIELD:DATE",
        help_heading = "Filter options"
    )]
    pub on: Vec<String>,

    /// Path glob pattern.
    #[arg(long = "path", value_name = "GLOB", help_heading = "Filter options")]
    pub path: Vec<String>,

    /// Documents whose outgoing links resolve to TARGET (path, stem, or
    /// `[[wikilink]]`). Repeatable; multiple targets are AND'd. Resolved-only —
    /// TARGET must resolve to an existing document.
    #[arg(
        long = "links-to",
        value_name = "TARGET",
        help_heading = "Filter options"
    )]
    pub links_to: Vec<String>,

    /// Documents with at least one unresolved link.
    #[arg(long = "unresolved-links", help_heading = "Filter options")]
    pub unresolved_links: bool,
}

impl FilterArgs {
    /// Map the parsed flags one-to-one onto the shared wire vocabulary.
    pub fn to_params(&self) -> FilterParams {
        FilterParams {
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

/// The sort / limit / paging knobs shared by the read commands, one flag per
/// [`SortPaginateParams`] field.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SortPaginateArgs {
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[arg(long, value_name = "FIELD", help_heading = "Sort and paging")]
    pub sort: Option<String>,

    /// Sort descending (only meaningful with --sort).
    #[arg(long, help_heading = "Sort and paging")]
    pub desc: bool,

    /// Maximum number of records to return. `find` defaults to 10; `get`
    /// returns every named target.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "no_limit",
        help_heading = "Sort and paging"
    )]
    pub limit: Option<usize>,

    /// Return all records; no limit. Overrides --limit.
    #[arg(long = "no-limit", help_heading = "Sort and paging")]
    pub no_limit: bool,

    /// 1-indexed starting offset for paging. Default 1.
    #[arg(
        long = "starts-at",
        value_name = "N",
        default_value_t = 1,
        help_heading = "Sort and paging"
    )]
    pub starts_at: usize,
}

impl SortPaginateArgs {
    /// Map the parsed flags one-to-one onto the shared wire vocabulary. The
    /// permissive `starts_at` (0 → 1) flooring is the consuming verb's job, not
    /// the adapter's — the wire stays permissive on purpose (see
    /// [`SortPaginateParams`]).
    pub fn to_params(&self) -> SortPaginateParams {
        SortPaginateParams {
            sort: self.sort.clone(),
            desc: self.desc,
            limit: self.limit,
            no_limit: self.no_limit,
            starts_at: self.starts_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn find_args(argv: &[&str]) -> crate::commands::find::FindArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Find(a) => a,
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn find_eq_and_limit_map_to_params() {
        let args = find_args(&["norn", "find", "--eq", "type:note", "--limit", "5"]);
        let (filter, paging) = args.to_params();
        assert_eq!(
            filter,
            FilterParams {
                eq: vec!["type:note".to_string()],
                ..FilterParams::default()
            }
        );
        assert_eq!(
            paging,
            SortPaginateParams {
                limit: Some(5),
                starts_at: 1,
                ..SortPaginateParams::default()
            }
        );
    }

    #[test]
    fn every_filter_predicate_maps_across() {
        let args = find_args(&[
            "norn",
            "find",
            "--text",
            "hello",
            "--eq",
            "type:note",
            "--not-eq",
            "status:done",
            "--in",
            "status:a,b",
            "--not-in",
            "kind:x,y",
            "--starts-with",
            "title:Q",
            "--ends-with",
            "title:z",
            "--contains",
            "tags:urgent",
            "--has",
            "title",
            "--missing",
            "due",
            "--before",
            "created:2020-01-01",
            "--after",
            "created:2019-01-01",
            "--on",
            "created:2019-06-01",
            "--path",
            "notes/**",
            "--links-to",
            "alpha",
            "--unresolved-links",
        ]);
        let (filter, _) = args.to_params();
        assert_eq!(
            filter,
            FilterParams {
                text: Some("hello".into()),
                eq: vec!["type:note".into()],
                not_eq: vec!["status:done".into()],
                r#in: vec!["status:a,b".into()],
                not_in: vec!["kind:x,y".into()],
                starts_with: vec!["title:Q".into()],
                ends_with: vec!["title:z".into()],
                contains: vec!["tags:urgent".into()],
                has: vec!["title".into()],
                missing: vec!["due".into()],
                before: vec!["created:2020-01-01".into()],
                after: vec!["created:2019-01-01".into()],
                on: vec!["created:2019-06-01".into()],
                path: vec!["notes/**".into()],
                links_to: vec!["alpha".into()],
                unresolved_links: true,
            }
        );
    }

    #[test]
    fn sort_and_paging_map_across() {
        let args = find_args(&[
            "norn",
            "find",
            "--all",
            "--sort",
            "created",
            "--desc",
            "--no-limit",
            "--starts-at",
            "3",
        ]);
        let (_, paging) = args.to_params();
        assert_eq!(
            paging,
            SortPaginateParams {
                sort: Some("created".into()),
                desc: true,
                limit: None,
                no_limit: true,
                starts_at: 3,
            }
        );
    }

    #[test]
    fn bare_read_flags_produce_default_params() {
        let args = find_args(&["norn", "find", "--all"]);
        let (filter, paging) = args.to_params();
        assert_eq!(filter, FilterParams::default());
        assert_eq!(paging, SortPaginateParams::default());
    }
}
