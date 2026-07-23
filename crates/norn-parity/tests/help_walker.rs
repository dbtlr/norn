//! Generated help-parity walker (NRN-421 harness-fitness).
//!
//! The hand-kept help net was three cases (`help-bare` / `help-validate` /
//! `help-find`): the top-level page and two representative subcommands. The 21
//! other verbs and every nested group (`cache`, `config`, `service`,
//! `completions`, `vault`) went unchecked — which is exactly where a
//! `service status` panic once hid. This walker replaces that fixed net with an
//! ENUMERATED one: it discovers every subcommand path from the live clap tree
//! (recursively parsing each `--help`'s COMMANDS section on BOTH binaries and
//! taking the union), then over every path asserts the one thing the divergence
//! ledger actually gates — the uniform GLOBAL OPTIONS reshape (ADR 0017 /
//! NRN-345, ledgered ONCE as PD-101 for the top-level page and PD-102 for
//! subcommands): the rewrite drops the resolver-derived `--config`, unhides
//! `--vault NAME`, and shows the full unclamped `-C/--cwd` description.
//!
//! Because the path set is derived, a future new verb is covered the moment it
//! is added — no hand-list to update. A verb whose `--help` crashes (the
//! `service status` panic class) renders no GLOBAL OPTIONS block, so the
//! per-path assertion fails loudly; a verb whose global-options block does not
//! match the canonical reshape fails as a fresh, unledgered divergence.
//!
//! Scope note (present-tense): the walker asserts the GLOBAL OPTIONS reshape,
//! the deleted service-local `--vault <PATH>` flag (PD-134), and the
//! rewrite-only `vault` registry namespace (PD-101) — the divergences the
//! ledger records. It does NOT assert byte-parity of the rest of each help page:
//! the rewrite's custom renderer emits the clap short summary only, dropping the
//! oracle's multi-paragraph `long_about` prose on several verbs. That prose drop
//! is a separate, currently-unledgered class surfaced by this enumeration; it is
//! deliberately not gated here (turning it into a hard assertion would require a
//! ledger decision the harness cannot make on its own). The three byte-exact
//! `help-*` parity cases in `cases.rs` remain the precise per-surface pins.
//!
//! Determinism: the enumerated path set is collected into a sorted `BTreeSet`,
//! so iteration order never varies; the canonical global-options blocks are
//! extracted from the same run's own top-level `--help`, so environment-specific
//! rendering (e.g. the oracle's fixed `--help` clamp) affects the canonical and
//! per-path blocks identically and cannot flake the comparison.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use norn_parity::exec::{self, RawOutput};

/// Per-invocation wall-clock bound. `--help` is effectively instant; a call that
/// blocks past this is the hang/crash regression class the walker exists to
/// catch, so a timeout is a hard failure naming the path (see `run_help`).
const HELP_TIMEOUT: Duration = Duration::from_secs(20);

/// Run `<path...> --help` against `binary` with cwd `vault`, bounded so a
/// hung/crashing help path fails loudly rather than wedging the run.
fn run_help(binary: &Path, path: &[String], vault: &Path) -> RawOutput {
    let mut argv: Vec<&str> = path.iter().map(String::as_str).collect();
    argv.push("--help");
    exec::run_argv_bounded(binary, &argv, None, vault, HELP_TIMEOUT).unwrap_or_else(|e| {
        panic!(
            "`{} {} --help` could not be driven to completion: {e}",
            binary.display(),
            path.join(" ")
        )
    })
}

/// The subcommand names listed in a help page's COMMANDS section, in render
/// order — the first token of each 4-space-indented entry line, until the next
/// unindented section header. A leaf command (no COMMANDS section) yields none.
fn parse_commands(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.starts_with("COMMANDS") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        // A non-indented, non-empty line is the next section header — stop.
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        // Command entries sit at exactly 4 spaces of indent; a wrapped
        // description (deeper) or a blank line is not an entry.
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if rest.starts_with(' ') || rest.is_empty() {
            continue;
        }
        let name = rest.split_whitespace().next().unwrap_or("");
        // Subcommand names are lowercase kebab tokens; the filter rejects any
        // stray description text that could share the column.
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            names.push(name.to_string());
        }
    }
    names
}

/// Every subcommand path reachable from `binary`'s clap tree, root included
/// (the empty path == the top-level `--help`). Recurses through COMMANDS
/// sections; the returned set is sorted (a `BTreeSet`) for deterministic
/// iteration regardless of traversal order.
fn enumerate(binary: &Path, vault: &Path) -> BTreeSet<Vec<String>> {
    let mut paths: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut frontier: Vec<Vec<String>> = vec![Vec::new()];
    while let Some(path) = frontier.pop() {
        if !paths.insert(path.clone()) {
            continue;
        }
        let out = run_help(binary, &path, vault);
        let help = String::from_utf8_lossy(&out.stdout);
        for sub in parse_commands(&help) {
            let mut child = path.clone();
            child.push(sub);
            frontier.push(child);
        }
    }
    paths
}

/// The GLOBAL OPTIONS block of a help page: from the `GLOBAL OPTIONS` header
/// through the last line before the `Documentation:` footer (or EOF), each line
/// right-trimmed and the whole block trailing-trimmed so a trailing blank line
/// never perturbs equality. Empty when the page has no such block (an unknown
/// command on the oracle, or a crash).
fn global_options(help: &str) -> String {
    let mut block: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in help.lines() {
        if line.starts_with("GLOBAL OPTIONS") {
            in_block = true;
        }
        if in_block {
            if line.starts_with("Documentation:") {
                break;
            }
            block.push(line.trim_end());
        }
    }
    block.join("\n").trim_end().to_string()
}

fn pretty(path: &[String]) -> String {
    if path.is_empty() {
        "<top-level>".to_string()
    } else {
        path.join(" ")
    }
}

#[test]
fn help_walker_every_verb_reshapes_global_options_uniformly() {
    if common::oracle_missing("help_walker") {
        return;
    }
    let oracle: PathBuf = common::oracle_path();
    let rewrite: PathBuf = common::rewrite_debug_binary();
    // `--help` never reads the vault; a stable empty cwd keeps every call cheap
    // and side-effect-free (any oracle cache warming lands on stderr, which the
    // walker never inspects — it compares stdout help text only).
    let cwd = common::workspace_root();

    // Canonical global-options blocks, derived from each binary's own top-level
    // `--help` in THIS run. Comparing every other path against these makes the
    // check robust to environment-specific rendering: the canonical and the
    // per-path block are produced by the same binary under the same conditions.
    let canonical_oracle = global_options(&String::from_utf8_lossy(
        &run_help(&oracle, &[], &cwd).stdout,
    ));
    let canonical_rewrite = global_options(&String::from_utf8_lossy(
        &run_help(&rewrite, &[], &cwd).stdout,
    ));
    assert!(
        !canonical_oracle.is_empty() && !canonical_rewrite.is_empty(),
        "failed to extract a top-level GLOBAL OPTIONS block from one of the binaries"
    );

    // The ledgered reshape (PD-101 top-level / PD-102 subcommands), asserted
    // ONCE on the canonical blocks. Every per-path check below then just pins
    // each verb to its side's canonical block, so this delta is stated a single
    // time rather than re-encoded per verb.
    assert!(
        canonical_oracle.contains("--config") && !canonical_oracle.contains("--vault"),
        "oracle GLOBAL OPTIONS should list `--config` and no `--vault` (PD-101/PD-102 baseline):\n{canonical_oracle}"
    );
    assert!(
        canonical_rewrite.contains("--vault <NAME>") && !canonical_rewrite.contains("--config"),
        "rewrite GLOBAL OPTIONS should drop `--config` and add `--vault <NAME>` (PD-101/PD-102):\n{canonical_rewrite}"
    );
    assert!(
        canonical_rewrite.contains("else the current directory)"),
        "rewrite `-C/--cwd` should show its full unclamped description in --help (PD-101/PD-102):\n{canonical_rewrite}"
    );
    assert!(
        canonical_oracle.contains('\u{2026}'),
        "oracle `-C/--cwd` should be clamped with an ellipsis in --help (the pre-reshape baseline):\n{canonical_oracle}"
    );
    for shared in ["--verbose", "--no-cache-refresh", "--color"] {
        assert!(
            canonical_oracle.contains(shared) && canonical_rewrite.contains(shared),
            "both GLOBAL OPTIONS blocks should retain the shared `{shared}` flag"
        );
    }

    // Enumerate the union of both clap trees (the rewrite adds the `vault`
    // registry namespace the oracle predates). Sorted set → deterministic.
    let mut paths = enumerate(&rewrite, &cwd);
    paths.extend(enumerate(&oracle, &cwd));

    // Enumeration sanity: a broken parser that collapsed to just the root must
    // fail rather than vacuously pass. These sentinels are a floor on the
    // walker itself, not a coverage list — the coverage IS the enumeration.
    assert!(
        paths.len() >= 30,
        "expected the enumerated help tree to have >=30 paths, found {}: {:?}",
        paths.len(),
        paths.iter().map(|p| pretty(p)).collect::<Vec<_>>()
    );
    for sentinel in [
        vec!["find".to_string()],
        vec!["validate".to_string()],
        vec!["service".to_string(), "status".to_string()],
        vec!["vault".to_string(), "register".to_string()],
    ] {
        assert!(
            paths.contains(&sentinel),
            "enumeration should discover `{}` — the COMMANDS walk is broken",
            pretty(&sentinel)
        );
    }

    let mut vault_namespace_paths = 0usize;
    let mut checked_paths = 0usize;
    for path in &paths {
        let rewrite_help =
            String::from_utf8_lossy(&run_help(&rewrite, path, &cwd).stdout).to_string();
        let rewrite_block = global_options(&rewrite_help);

        // The rewrite must render a GLOBAL OPTIONS block on every verb, and it
        // must be the canonical reshaped block — a missing block is the crash
        // class, a differing block is a fresh divergence.
        assert!(
            !rewrite_block.is_empty(),
            "rewrite `{} --help` rendered no GLOBAL OPTIONS block (crash/regression?):\n{rewrite_help}",
            pretty(path)
        );
        assert_eq!(
            rewrite_block,
            canonical_rewrite,
            "rewrite `{} --help` GLOBAL OPTIONS diverges from the canonical reshaped block",
            pretty(path)
        );

        let oracle_help =
            String::from_utf8_lossy(&run_help(&oracle, path, &cwd).stdout).to_string();
        let oracle_block = global_options(&oracle_help);
        if oracle_block.is_empty() {
            // The oracle does not know this command: the only such family is the
            // rewrite-only `vault` registry namespace (PD-101). Anything else is
            // a real find (a rewrite command the pinned oracle should have).
            assert_eq!(
                path.first().map(String::as_str),
                Some("vault"),
                "`{}` renders no oracle GLOBAL OPTIONS but is not under the rewrite-only \
                 `vault` namespace (PD-101) — unexpected oracle-absent command",
                pretty(path)
            );
            vault_namespace_paths += 1;
            continue;
        }
        assert_eq!(
            oracle_block,
            canonical_oracle,
            "oracle `{} --help` GLOBAL OPTIONS is not uniform with its own top-level block",
            pretty(path)
        );
        checked_paths += 1;
    }

    assert!(
        checked_paths > 0 && vault_namespace_paths > 0,
        "expected to check shared verbs ({checked_paths}) and at least one vault-namespace path \
         ({vault_namespace_paths})"
    );
}

/// The deleted service-local `--vault <PATH>` flag (PD-134): it collided with
/// the ADR 0017 global `--vault <NAME>` selector and panicked the rewrite, so it
/// was removed. The walker positively verifies that ledgered divergence on the
/// `service status` surface the entry names — the oracle still advertises the
/// local flag in its OPTIONS block, the rewrite does not.
#[test]
fn help_walker_service_local_vault_flag_is_deleted() {
    if common::oracle_missing("help_walker") {
        return;
    }
    let oracle = common::oracle_path();
    let rewrite = common::rewrite_debug_binary();
    let cwd = common::workspace_root();
    let path = vec!["service".to_string(), "status".to_string()];

    let oracle_help = String::from_utf8_lossy(&run_help(&oracle, &path, &cwd).stdout).to_string();
    let rewrite_help = String::from_utf8_lossy(&run_help(&rewrite, &path, &cwd).stdout).to_string();

    assert!(
        oracle_help.contains("--vault <PATH>"),
        "expected the oracle `service status --help` to still advertise the local `--vault <PATH>` \
         flag (the PD-134 pre-deletion baseline):\n{oracle_help}"
    );
    assert!(
        !rewrite_help.contains("--vault <PATH>"),
        "the rewrite must NOT carry the deleted service-local `--vault <PATH>` flag (PD-134); its \
         only vault flag is the global `--vault <NAME>` selector:\n{rewrite_help}"
    );
}
