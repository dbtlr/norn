//! The clap-derived [`KnownFlags`] the forgiving-input grammar consumes.
//!
//! norn-core's `normalize_argv` (ADR 0010 dynamic-predicate desugar + alias
//! pack) is clap-free: it takes the known-flag surface as an injected value so
//! it never links clap. This module is the derivation seam — it walks the frozen
//! `Cli` command tree once and produces the [`KnownFlags`] to hand to
//! `normalize_argv`.
//!
//! # NRN-178 drift guard
//!
//! The whole point of deriving (rather than hand-listing) the flag surface is
//! that a flag added to `cli.rs` cannot silently degrade into a dynamic field
//! predicate — the derivation picks it up automatically. The drift-guard test
//! locks that in: it compares the derived surface against an explicit expected
//! fixture, so adding a query/mutate flag to `cli.rs` without updating the
//! fixture fails the test (forcing a conscious decision about the new flag).

use std::collections::{BTreeSet, HashMap};

use clap::{ArgAction, Command, CommandFactory};

use norn_core::grammar::KnownFlags;

use crate::cli::Cli;

/// The query-family subcommands that accept dynamic predicates + the alias pack.
const QUERY_SUBCOMMANDS: &[&str] = &["find", "count", "describe"];

/// The mutate-family subcommands whose value-flag surface the cross-family
/// teaching-error scanner models.
const MUTATE_SUBCOMMANDS: &[&str] = &["set", "new", "edit"];

/// Whether a clap action consumes a value (`Set`/`Append`) vs. is a boolean
/// flag (`SetTrue`/`SetFalse`/`Count`).
fn takes_value(action: &ArgAction) -> bool {
    matches!(action, ArgAction::Set | ArgAction::Append)
}

fn subcommand<'a>(root: &'a Command, name: &str) -> Option<&'a Command> {
    root.get_subcommands().find(|c| c.get_name() == name)
}

/// Derive the [`KnownFlags`] surface from the frozen `Cli` command tree.
pub fn derive_known_flags() -> KnownFlags {
    let root = Cli::command();

    // The value-taking / boolean globals (`global = true`, defined on the root).
    let mut value_globals: BTreeSet<String> = BTreeSet::new();
    let mut boolean_globals: BTreeSet<String> = BTreeSet::new();
    for arg in root.get_arguments() {
        if !arg.is_global_set() {
            continue;
        }
        let Some(long) = arg.get_long() else { continue };
        if takes_value(arg.get_action()) {
            value_globals.insert(long.to_string());
        } else {
            boolean_globals.insert(long.to_string());
        }
    }

    // Query family: each subcommand's own args, unioned with the globals.
    let mut query_value = value_globals.clone();
    let mut query_boolean = boolean_globals.clone();
    for name in QUERY_SUBCOMMANDS {
        let Some(sub) = subcommand(&root, name) else {
            continue;
        };
        for arg in sub.get_arguments() {
            if arg.is_global_set() {
                continue;
            }
            let Some(long) = arg.get_long() else { continue };
            if takes_value(arg.get_action()) {
                query_value.insert(long.to_string());
            } else {
                query_boolean.insert(long.to_string());
            }
        }
    }

    // Mutate family: each subcommand's own VALUE args, plus the value globals.
    let mut mutate_value: HashMap<String, BTreeSet<String>> = HashMap::new();
    for name in MUTATE_SUBCOMMANDS {
        let Some(sub) = subcommand(&root, name) else {
            continue;
        };
        let mut set = value_globals.clone();
        for arg in sub.get_arguments() {
            if arg.is_global_set() {
                continue;
            }
            let Some(long) = arg.get_long() else { continue };
            if takes_value(arg.get_action()) {
                set.insert(long.to_string());
            }
        }
        mutate_value.insert(name.to_string(), set);
    }

    KnownFlags {
        query_value,
        query_boolean,
        mutate_value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The value-taking globals — the anti-drift anchor for the globals surface.
    const VALUE_GLOBALS: &[&str] = &["cwd", "color", "vault"];

    fn mutate_flags(own: &[&str]) -> BTreeSet<String> {
        let mut s = set_of(own);
        s.extend(VALUE_GLOBALS.iter().map(|g| g.to_string()));
        s
    }

    /// The expected frozen flag surface (NRN-329), mirroring the norn-core
    /// grammar fixture. The derived surface MUST equal this — a query/mutate
    /// flag added to `cli.rs` changes the derivation and fails this test until
    /// the fixture is consciously updated (NRN-178 drift guard).
    fn expected() -> KnownFlags {
        let mut query_value = set_of(&[
            "text",
            "eq",
            "not-eq",
            "in",
            "not-in",
            "starts-with",
            "ends-with",
            "contains",
            "has",
            "missing",
            "before",
            "after",
            "on",
            "path",
            "links-to",
            "sort",
            "limit",
            "starts-at",
            "by",
            "col",
            "format",
        ]);
        query_value.extend(VALUE_GLOBALS.iter().map(|g| g.to_string()));
        let query_boolean = set_of(&[
            "unresolved-links",
            "all",
            "all-cols",
            "no-pager",
            "desc",
            "no-limit",
            "data",
            "stats",
            "verbose",
            "no-cache-refresh",
            "help",
        ]);
        let mut mutate_value = HashMap::new();
        mutate_value.insert(
            "set".to_string(),
            mutate_flags(&["field", "field-json", "push", "pop", "remove", "format"]),
        );
        mutate_value.insert(
            "new".to_string(),
            mutate_flags(&["as", "title", "var", "field", "field-json", "format"]),
        );
        mutate_value.insert(
            "edit".to_string(),
            mutate_flags(&[
                "edits-json",
                "ops-file",
                "str-replace",
                "replace-section",
                "append-to-section",
                "delete-section",
                "insert-before-heading",
                "insert-after-heading",
                "new",
                "content",
                "expected-hash",
                "format",
            ]),
        );
        KnownFlags {
            query_value,
            query_boolean,
            mutate_value,
        }
    }

    #[test]
    fn derived_known_flags_match_the_frozen_fixture() {
        let derived = derive_known_flags();
        let expected = expected();
        assert_eq!(
            derived.query_value, expected.query_value,
            "query value-flag surface drifted from the frozen fixture"
        );
        assert_eq!(
            derived.query_boolean, expected.query_boolean,
            "query boolean-flag surface drifted from the frozen fixture"
        );
        assert_eq!(
            derived.mutate_value, expected.mutate_value,
            "mutate value-flag surface drifted from the frozen fixture"
        );
    }
}
