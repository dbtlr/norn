//! The document-filter option set shared by the query commands.
//!
//! `DocumentFilterOptions` carries the raw field predicates, path globs, and
//! `has` / `missing` key checks a `find` / `count` / `get` invocation asked
//! for. `query.rs` owns the adapter that parses and applies it against the
//! graph; this module is just the borrowed-slice option shape both sides agree
//! on.

#[derive(Debug)]
pub struct DocumentFilterOptions<'a> {
    pub filters: &'a [String],
    pub paths: &'a [String],
    pub has: &'a [String],
    pub missing: &'a [String],
}
