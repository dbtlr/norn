//! Hand-authored canned `--help` examples per command path.
//!
//! Per the Phase 2 design spec, examples are biased toward fewer — empty is
//! the correct answer for many commands. The match below returns `vec![]`
//! for any command path with no authored examples; the renderer skips the
//! `EXAMPLES` section when the table is empty.
//!
//! Each entry is `(command_line, comment)`. Comments are ≤60 chars, lowercase
//! except for required literals, no trailing period. The command line uses
//! the literal `vault` prefix; the renderer styles tokens per palette.

/// Return canned examples for the given command path string (e.g. `"vault find"`).
///
/// Returns `vec![]` for unknown paths and for paths intentionally without
/// examples. The renderer's `EXAMPLES` section is suppressed when the table
/// is empty.
pub fn examples_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "vault" => &[
            (
                "vault find --eq type:note --limit 5",
                "5 notes by default sort",
            ),
            (
                "vault validate --format json",
                "machine-readable validation findings",
            ),
            (
                "vault repair plan --out plan.json",
                "generate a frontmatter repair plan",
            ),
            (
                "vault links unresolved --format paths",
                "list docs with broken outgoing links",
            ),
        ],
        "vault find" => &[
            (
                "vault find --eq type:note --limit 5",
                "5 notes; default sort",
            ),
            (
                "vault find --text reorg --format paths",
                "full-text search; pipe-friendly paths",
            ),
            (
                "vault find --has aliases --col title,aliases",
                "docs that declare aliases",
            ),
            (
                "vault find --in type:note,log --sort modified --desc",
                "two types, newest first",
            ),
        ],
        "vault validate" => &[
            (
                "vault validate",
                "human-readable findings on the configured vault",
            ),
            (
                "vault validate --format json",
                "machine-readable findings for pipelines",
            ),
            (
                "vault validate --severity error",
                "errors only; skip warnings",
            ),
        ],
        "vault repair plan" => &[
            (
                "vault repair plan --out plan.json",
                "write a frontmatter repair plan",
            ),
            (
                "vault repair plan --format json",
                "preview the plan on stdout",
            ),
            (
                "vault repair plan --severity error",
                "plan only error-level findings",
            ),
        ],
        "vault repair apply" => &[
            (
                "vault repair apply plan.json --dry-run",
                "preview changes without writing",
            ),
            (
                "vault repair apply plan.json",
                "apply a previously-generated plan",
            ),
            (
                "vault repair apply plan.json --verify",
                "apply then re-validate",
            ),
        ],
        "vault links unresolved" => &[
            (
                "vault links unresolved",
                "every doc with unresolved or ambiguous links",
            ),
            (
                "vault links unresolved --format paths",
                "just paths; pipe-friendly",
            ),
            (
                "vault links unresolved --format json",
                "machine-readable findings",
            ),
        ],

        // ── Default tier: 1-2 examples each ─────────────────────────────────
        "vault init" => &[(
            "vault init",
            "scaffold .vault/config.yaml in the current directory",
        )],
        "vault config show" => &[
            ("vault config show", "effective config: paths + counts"),
            (
                "vault config show --format json",
                "machine-readable config for pipelines",
            ),
        ],
        "vault links list" => &[(
            "vault links list --format paths",
            "every link source as a path; pipe-friendly",
        )],
        "vault links backlinks" => &[
            (
                "vault links backlinks path/to/doc.md",
                "incoming links for an exact path",
            ),
            (
                "vault links backlinks my-note",
                "stem match; case-insensitive when unique",
            ),
        ],
        "vault docs inspect" => &[(
            "vault docs inspect path/to/doc.md",
            "one doc plus incoming, outgoing, unresolved",
        )],
        "vault cache rebuild" => &[(
            "vault cache rebuild",
            "delete and rebuild the cache from scratch",
        )],
        "vault cache status" => &[(
            "vault cache status",
            "cache path, size, doc and link counts",
        )],
        "vault cache index" => &[
            (
                "vault cache index",
                "incremental refresh via mtime+size check",
            ),
            (
                "vault cache index --force-hash",
                "hash every file; bypass cheap-check",
            ),
        ],
        "vault files" => &[
            ("vault files", "every inventoried file under the vault"),
            ("vault files --format paths", "just paths; pipe-friendly"),
        ],
        "vault repair links" => &[
            (
                "vault repair links",
                "report link and path repair risks; no writes",
            ),
            (
                "vault repair links --target old.md --move-to new.md",
                "preview link risk if target were moved",
            ),
        ],

        // ── Thin tier: 0-1 examples each ────────────────────────────────────
        "vault completions init" => &[(
            "vault completions init zsh",
            "emit zsh completion script to stdout",
        )],
        "vault completions install" => &[(
            "vault completions install",
            "install for the shell detected from $SHELL",
        )],

        // Thin commands without arms (intentionally empty — flag block self-explains):
        // vault docs summary, vault cache clear, vault config validate,
        // vault config migrate, vault config edit
        _ => &[],
    };
    pairs
        .iter()
        .map(|(cmd, comment)| (cmd.to_string(), comment.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_path_returns_empty() {
        assert!(examples_for("vault nonexistent").is_empty());
    }

    #[test]
    fn root_path_has_examples() {
        assert!(!examples_for("vault").is_empty());
    }

    #[test]
    fn find_path_has_examples() {
        let ex = examples_for("vault find");
        assert!(!ex.is_empty());
        // At least one example should demonstrate the `--eq` predicate.
        assert!(ex.iter().any(|(cmd, _)| cmd.contains("--eq")));
    }
}
