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
//! locks that in: it compares the clap derivation against
//! [`norn_core::grammar::frozen_known_flags`] — the SAME frozen fixture the
//! norn-core normalization tests consume — so adding a query/mutate flag to
//! `cli.rs` without updating that one fixture fails the test. One source of
//! truth, cross-crate; no local hand-copy to drift out of sync.

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

    /// The clap derivation MUST equal the single frozen surface norn-core
    /// exports — a query/mutate flag added to `cli.rs` changes the derivation
    /// and fails this test until `frozen_known_flags` is consciously updated
    /// (NRN-178 drift guard, one cross-crate source of truth).
    #[test]
    fn derived_known_flags_match_the_frozen_norn_core_fixture() {
        let derived = derive_known_flags();
        let frozen = norn_core::grammar::frozen_known_flags();
        assert_eq!(
            derived.query_value, frozen.query_value,
            "query value-flag surface drifted from norn-core's frozen fixture"
        );
        assert_eq!(
            derived.query_boolean, frozen.query_boolean,
            "query boolean-flag surface drifted from norn-core's frozen fixture"
        );
        assert_eq!(
            derived.mutate_value, frozen.mutate_value,
            "mutate value-flag surface drifted from norn-core's frozen fixture"
        );
    }
}
