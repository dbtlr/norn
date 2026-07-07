//! Forgiving-input CLI grammar (ADR 0010: "one canonical form, forgiving
//! inputs"; NRN-206 / NRN-207 / NRN-209).
//!
//! Every norn grammar has exactly one canonical spelling — the form docs
//! teach, help shows, and errors echo. This module owns the *input-side*
//! forgiveness that normalizes predictable, evidence-mined variants to
//! canonical before clap ever sees them. Nothing here is taught; canonical
//! spellings are unchanged in help/errors.
//!
//! Three concerns, one seam:
//!
//! - **Separator forgiveness (T1, [`split_field_value`]).** Predicate tokens
//!   canonically take `FIELD:VALUE`; assignment tokens canonically take
//!   `KEY=VALUE`. Both families accept EITHER separator — the split point is
//!   the FIRST `:` or `=`, whichever comes first. Deterministic because keys
//!   contain neither, so a value-embedded `:` (datetime/URL) or `=` parses
//!   correctly under first-separator-wins. Shared by the query predicate
//!   parsers (`filter_args`) and the mutate assignment parsers (`set`/`new`).
//!
//! - **Dynamic field predicates (T2, [`normalize_argv`] + [`gate_dynamic_fields`]).**
//!   On the query family only, an unknown `--key value` desugars to
//!   `--eq key:value`. Reserved flags always win; the key must resolve against
//!   the vault's known-field universe or it hard-errors with did-you-mean.
//!
//! - **Alias pack (T3, [`normalize_argv`]).** `--where`/`--filter` → `--eq`,
//!   `--group-by` → `--by`, `count --all` → no-op. Hidden: accepted, never in
//!   help. Resolved BEFORE the dynamic pass so they are never reinterpreted as
//!   field predicates.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};

/// Split an assignment/predicate token into `(key, value)` at the FIRST `:` or
/// `=`, whichever comes first (ADR 0010 separator forgiveness, T1). Returns
/// `None` when the token contains neither separator.
///
/// Deterministic key/value boundary: keys contain neither `:` nor `=`, so the
/// first separator always delimits the key. A value-embedded `:` (a datetime
/// like `2026-07-01T10:30`, a URL) or `=` is preserved verbatim in the value.
pub fn split_field_value(token: &str) -> Option<(&str, &str)> {
    let idx = token.bytes().position(|b| b == b':' || b == b'=')?;
    Some((&token[..idx], &token[idx + 1..]))
}

/// The three query-family commands that embed `FilterArgs` and therefore
/// accept dynamic field predicates + the alias pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCmd {
    Find,
    Count,
    Describe,
}

impl QueryCmd {
    fn from_subcommand(name: &str) -> Option<QueryCmd> {
        match name {
            "find" => Some(QueryCmd::Find),
            "count" => Some(QueryCmd::Count),
            "describe" => Some(QueryCmd::Describe),
            _ => None,
        }
    }
}

/// Result of the pre-clap argv normalization pass.
#[derive(Debug)]
pub struct Normalized {
    /// Rewritten argv (including argv[0]) ready to feed clap.
    pub argv: Vec<String>,
    /// Keys that were desugared from a dynamic `--key value` into a predicate.
    /// The field-universe gate ([`gate_dynamic_fields`]) validates these AFTER
    /// the cache is open; canonical `--eq` predicates are never gated.
    pub dynamic_keys: Vec<String>,
}

/// Long-flag names (without the `--`) that take a value, across the union of
/// the query commands plus the globals. A flag in this set consumes the next
/// token as its value in space form, so a value that looks like anything (a
/// bare word, a subcommand name) is never misread.
const KNOWN_VALUE_FLAGS: &[&str] = &[
    // FilterArgs predicates
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
    // sort / paging (find)
    "sort",
    "limit",
    "starts-at",
    // count / describe
    "by",
    "col",
    "format",
    // globals
    "cwd",
    "config",
    "color",
];

/// Long-flag names (without the `--`) that take no value.
const KNOWN_BOOL_FLAGS: &[&str] = &[
    "unresolved-links",
    "all",
    "all-cols",
    "no-pager",
    "desc",
    "no-limit",
    "data",
    "stats",
    // globals
    "verbose",
    "no-cache-refresh",
    "help",
];

fn is_known_flag(name: &str) -> bool {
    KNOWN_VALUE_FLAGS.contains(&name) || KNOWN_BOOL_FLAGS.contains(&name)
}

fn known_flag_takes_value(name: &str) -> bool {
    KNOWN_VALUE_FLAGS.contains(&name)
}

/// The reserved long flags for a query command, `--`-prefixed, for the gate's
/// did-you-mean candidate set. A superset is harmless (did-you-mean only ever
/// surfaces the closest match), so this returns the shared union rather than a
/// per-command subset.
pub fn query_known_flags(cmd: QueryCmd) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    for f in KNOWN_VALUE_FLAGS.iter().chain(KNOWN_BOOL_FLAGS.iter()) {
        // Skip pure globals that a user would not confuse with a field.
        flags.push(format!("--{f}"));
    }
    // Per-command real flags already covered by the union above; `cmd` is kept
    // in the signature so the caller documents intent and a future divergence
    // has a seam.
    let _ = cmd;
    flags.sort();
    flags.dedup();
    flags
}

/// Map a query-family alias to its canonical long-flag name, or `None` if the
/// token is not an alias. Resolved before the dynamic-predicate pass so an
/// alias is never reinterpreted as a field predicate (T3).
fn resolve_alias(cmd: QueryCmd, name: &str) -> Option<&'static str> {
    match name {
        "where" | "filter" => Some("eq"),
        // `--group-by` aliases `--by` on the commands that HAVE `--by`.
        "group-by" if matches!(cmd, QueryCmd::Count | QueryCmd::Describe) => Some("by"),
        _ => None,
    }
}

/// Strip a leading `--` (long flag) and split an inline `=value`. Returns
/// `(name, inline_value)` for a long flag, or `None` for anything else (short
/// flags, positionals, bare `--`).
fn parse_long_flag(tok: &str) -> Option<(&str, Option<&str>)> {
    let rest = tok.strip_prefix("--")?;
    if rest.is_empty() {
        return None; // bare `--`
    }
    match rest.split_once('=') {
        Some((name, val)) => Some((name, Some(val))),
        None => Some((rest, None)),
    }
}

/// Whether a token can serve as the value of a dynamic `--key value` predicate.
/// A token that begins with `-` is treated as the next flag, not a value.
fn is_value_token(tok: &str) -> bool {
    !tok.starts_with('-')
}

/// Pre-clap argv normalization (T1 value-side is handled in the parsers; this
/// covers T2 dynamic predicates + T3 aliases). `args` includes argv[0].
///
/// - Non-query subcommands pass through unchanged, except a mutate-family
///   cross-family teaching error (`set --eq …` → point at `--field`).
/// - Query subcommands: aliases resolve first, then unknown `--key value`
///   desugars to `--eq`/`--in`, collecting the dynamic keys for the later
///   field-universe gate.
pub fn normalize_argv(args: Vec<String>) -> Result<Normalized> {
    // Locate the subcommand token, skipping leading globals + their values.
    let Some(sub_idx) = find_subcommand_index(&args) else {
        return Ok(Normalized {
            argv: args,
            dynamic_keys: Vec::new(),
        });
    };
    let sub = args[sub_idx].as_str();

    // Mutate-family teaching error: assignment lives on `--field key=value`,
    // never on the predicate flags.
    if is_mutate_family(sub) {
        if let Some(bad) = first_predicate_flag(&args[sub_idx + 1..]) {
            return Err(anyhow!(
                "`norn {sub}` assigns frontmatter with `--field key=value`, not `{bad}` \
                 (predicate flags like `{bad}` belong to the query family: find / count / describe)"
            ));
        }
        return Ok(Normalized {
            argv: args,
            dynamic_keys: Vec::new(),
        });
    }

    let Some(cmd) = QueryCmd::from_subcommand(sub) else {
        return Ok(Normalized {
            argv: args,
            dynamic_keys: Vec::new(),
        });
    };

    normalize_query(args, sub_idx, cmd)
}

fn is_mutate_family(sub: &str) -> bool {
    matches!(sub, "set" | "new" | "edit")
}

/// The predicate flags that must never appear on the mutate family.
const PREDICATE_FLAGS: &[&str] = &[
    "eq",
    "not-eq",
    "in",
    "not-in",
    "starts-with",
    "ends-with",
    "contains",
    "before",
    "after",
    "on",
];

fn first_predicate_flag(toks: &[String]) -> Option<String> {
    for tok in toks {
        if let Some((name, _)) = parse_long_flag(tok) {
            if PREDICATE_FLAGS.contains(&name) {
                return Some(format!("--{name}"));
            }
        }
    }
    None
}

/// Find the index of the subcommand token: the first non-global token after
/// argv[0]. Skips global flags and (for the value-taking ones in space form)
/// their values, so a global value equal to a subcommand name is not mistaken
/// for the subcommand.
fn find_subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let tok = &args[i];
        if let Some((name, inline)) = parse_long_flag(tok) {
            // A global value flag in space form consumes the next token.
            if inline.is_none() && known_flag_takes_value(name) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Short global `-C`/`--cwd` value form.
        if tok == "-C" {
            i += 2;
            continue;
        }
        if tok.starts_with('-') && tok != "-" {
            // Any other short/long flag: skip the flag itself.
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn normalize_query(args: Vec<String>, sub_idx: usize, cmd: QueryCmd) -> Result<Normalized> {
    // Everything up to and including the subcommand passes through verbatim.
    let mut out: Vec<String> = args[..=sub_idx].to_vec();
    let toks = &args[sub_idx + 1..];

    // Dynamic predicates, grouped by key in first-seen order so a repeated key
    // desugars to a single `--in key:v1,v2` (any-of) rather than an always-empty
    // AND-of-two-equalities.
    let mut dyn_order: Vec<String> = Vec::new();
    let mut dyn_values: HashMap<String, Vec<String>> = HashMap::new();

    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];

        let Some((name, inline)) = parse_long_flag(tok) else {
            // Short flag / positional (`-C` value handled below) — pass through.
            if tok == "-C" {
                out.push(tok.clone());
                if let Some(v) = toks.get(i + 1) {
                    out.push(v.clone());
                    i += 2;
                    continue;
                }
            }
            out.push(tok.clone());
            i += 1;
            continue;
        };

        // `count --all` is an accepted no-op (find symmetry, T3): drop it.
        if cmd == QueryCmd::Count && name == "all" {
            i += 1;
            continue;
        }

        // Reserved built-in flags always win — never reinterpreted (T2a).
        if is_known_flag(name) {
            out.push(tok.clone());
            if inline.is_none() && known_flag_takes_value(name) {
                if let Some(v) = toks.get(i + 1) {
                    out.push(v.clone());
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        // Hidden aliases resolve before the dynamic pass (T3).
        if let Some(canonical) = resolve_alias(cmd, name) {
            match inline {
                Some(v) => out.push(format!("--{canonical}={v}")),
                None => {
                    out.push(format!("--{canonical}"));
                    if let Some(v) = toks.get(i + 1) {
                        out.push(v.clone());
                        i += 2;
                        continue;
                    }
                }
            }
            i += 1;
            continue;
        }

        // Dynamic field predicate (T2): unknown `--key value` → `--eq key:value`.
        let key = name.to_string();
        let value = match inline {
            Some(v) => v.to_string(),
            None => match toks.get(i + 1) {
                Some(v) if is_value_token(v) => {
                    i += 1;
                    v.clone()
                }
                _ => {
                    return Err(anyhow!(
                        "unknown flag `--{key}` requires a value: dynamic field predicates \
                         desugar `--{key} VALUE` to `--eq {key}:VALUE` for known vault fields \
                         (a bare `--{key}` is not a boolean)"
                    ));
                }
            },
        };
        if !dyn_values.contains_key(&key) {
            dyn_order.push(key.clone());
        }
        dyn_values.entry(key).or_default().push(value);
        i += 1;
    }

    // Emit desugared predicates after the reserved tokens. Order among clap
    // flags is irrelevant (they accumulate into the same Vec), so appending
    // keeps canonical-only invocations byte-identical.
    let mut dynamic_keys: Vec<String> = Vec::new();
    for key in dyn_order {
        let values = &dyn_values[&key];
        if values.len() == 1 {
            out.push("--eq".to_string());
            out.push(format!("{key}:{}", values[0]));
        } else {
            out.push("--in".to_string());
            out.push(format!("{key}:{}", values.join(",")));
        }
        dynamic_keys.push(key);
    }

    Ok(Normalized {
        argv: out,
        dynamic_keys,
    })
}

/// Validate every dynamically-desugared key against the vault's field universe
/// (T2b). A key that resolves is a legitimate predicate; a key that does not is
/// a hard error with did-you-mean across both real flags and known fields —
/// the guardrail that stops a typo'd real flag (`--formt json`) from becoming a
/// silent empty query.
pub fn gate_dynamic_fields(
    dynamic_keys: &[String],
    universe: &BTreeSet<String>,
    known_flags: &[String],
) -> Result<()> {
    for key in dynamic_keys {
        if universe.contains(key) {
            continue;
        }
        let mut candidates: Vec<String> = known_flags.to_vec();
        candidates.extend(universe.iter().cloned());
        let suggestion = closest(key, &candidates);
        return Err(match suggestion {
            Some(s) => anyhow!(
                "unknown field `{key}` — did you mean `{s}`? \
                 (filter a known field with `--eq {key}:value`; \
                 dynamic `--{key} value` only works for fields this vault knows)"
            ),
            None => anyhow!(
                "unknown field `{key}`: not a known vault field or flag \
                 (filter a known field with `--eq field:value`)"
            ),
        });
    }
    Ok(())
}

/// Union of schema-declared fields (config) and observed frontmatter keys
/// (cache) — the vault-specific field universe the dynamic-predicate gate
/// resolves against.
pub fn field_universe(
    cache: &crate::cache::Cache,
    config: &crate::config_loader::LoadedConfig,
) -> Result<BTreeSet<String>> {
    let mut universe = cache.observed_field_names()?;
    universe.extend(schema_field_names(config));
    Ok(universe)
}

/// Frontmatter field names declared anywhere in the validate config — covers
/// declared-but-not-yet-present fields so a query for them is not a typo.
pub fn schema_field_names(config: &crate::config_loader::LoadedConfig) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    let v = &config.validate;
    fields.extend(v.required_frontmatter.iter().cloned());
    for rule in &v.rules {
        fields.extend(rule.required_frontmatter.iter().cloned());
        fields.extend(rule.forbidden_frontmatter.iter().cloned());
        fields.extend(rule.field_types.keys().cloned());
        fields.extend(rule.allowed_values.keys().cloned());
        fields.extend(rule.frontmatter_defaults.keys().cloned());
        fields.extend(rule.field_references.keys().cloned());
    }
    fields
}

/// Closest candidate to `key` by Levenshtein distance, if one is near enough to
/// be a plausible typo. Candidates carry their own `--` prefix (flags) or none
/// (fields); the comparison is against the whole candidate string, so a flag
/// typo (`formt` vs `--format`) still matches on the shared stem.
fn closest(key: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for cand in candidates {
        // Compare against the flag/field spelling with any leading dashes
        // stripped, so `formt` is distance 1 from `--format`.
        let bare = cand.trim_start_matches('-');
        let d = strsim::levenshtein(key, bare);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    let (dist, cand) = best?;
    // Accept only genuinely-close matches: a small absolute edit distance that
    // is also a minority of the token length.
    let threshold = 2.max(key.len() / 3);
    if dist <= threshold && dist < key.len().max(1) {
        Some(cand.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── T1: separator forgiveness ──────────────────────────────────────────
    #[test]
    fn split_prefers_first_separator_colon() {
        assert_eq!(split_field_value("type:note"), Some(("type", "note")));
    }

    #[test]
    fn split_accepts_equals_on_predicate() {
        assert_eq!(
            split_field_value("modified=2026-07-01"),
            Some(("modified", "2026-07-01"))
        );
    }

    #[test]
    fn split_first_wins_colon_before_equals() {
        assert_eq!(split_field_value("a:b=c"), Some(("a", "b=c")));
    }

    #[test]
    fn split_first_wins_equals_before_colon() {
        assert_eq!(split_field_value("a=b:c"), Some(("a", "b:c")));
    }

    #[test]
    fn split_value_embedded_colon_datetime() {
        assert_eq!(
            split_field_value("created:2026-07-01T10:30"),
            Some(("created", "2026-07-01T10:30"))
        );
    }

    #[test]
    fn split_no_separator_is_none() {
        assert_eq!(split_field_value("nocolon"), None);
    }

    // ── T2/T3: argv normalization ──────────────────────────────────────────
    fn norm(args: &[&str]) -> Normalized {
        normalize_argv(args.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    #[test]
    fn canonical_find_is_byte_identical() {
        let input = ["norn", "find", "--eq", "type:note", "--limit", "3"];
        let n = norm(&input);
        assert_eq!(
            n.argv,
            input.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn dynamic_predicate_desugars_to_eq() {
        let n = norm(&["norn", "find", "--type", "note", "--limit", "3"]);
        assert_eq!(
            n.argv,
            vec!["norn", "find", "--limit", "3", "--eq", "type:note"]
        );
        assert_eq!(n.dynamic_keys, vec!["type".to_string()]);
    }

    #[test]
    fn dynamic_predicate_inline_equals() {
        let n = norm(&["norn", "count", "--status=active"]);
        assert_eq!(n.argv, vec!["norn", "count", "--eq", "status:active"]);
        assert_eq!(n.dynamic_keys, vec!["status".to_string()]);
    }

    #[test]
    fn repeated_dynamic_key_becomes_in_any_of() {
        let n = norm(&["norn", "find", "--status", "active", "--status", "done"]);
        assert_eq!(n.argv, vec!["norn", "find", "--in", "status:active,done"]);
        assert_eq!(n.dynamic_keys, vec!["status".to_string()]);
    }

    #[test]
    fn reserved_flag_always_wins_over_dynamic() {
        // `--format` is a real flag: never reinterpreted as a `format` field.
        let n = norm(&["norn", "find", "--format", "json", "--type", "note"]);
        assert_eq!(
            n.argv,
            vec!["norn", "find", "--format", "json", "--eq", "type:note"]
        );
        assert_eq!(n.dynamic_keys, vec!["type".to_string()]);
    }

    #[test]
    fn bare_unknown_flag_without_value_errors() {
        let err = normalize_argv(
            ["norn", "find", "--draft"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires a value"), "{err}");
    }

    #[test]
    fn bare_unknown_flag_before_another_flag_errors() {
        let err = normalize_argv(
            ["norn", "find", "--draft", "--limit", "3"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires a value"), "{err}");
    }

    #[test]
    fn where_and_filter_alias_to_eq() {
        let n = norm(&[
            "norn",
            "find",
            "--where",
            "type:note",
            "--filter",
            "status:active",
        ]);
        assert_eq!(
            n.argv,
            vec!["norn", "find", "--eq", "type:note", "--eq", "status:active"]
        );
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn group_by_aliases_to_by_on_count() {
        let n = norm(&["norn", "count", "--group-by", "type"]);
        assert_eq!(n.argv, vec!["norn", "count", "--by", "type"]);
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn count_all_is_dropped_noop() {
        let n = norm(&["norn", "count", "--all", "--eq", "type:note"]);
        assert_eq!(n.argv, vec!["norn", "count", "--eq", "type:note"]);
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn find_all_is_preserved() {
        let n = norm(&["norn", "find", "--all"]);
        assert_eq!(n.argv, vec!["norn", "find", "--all"]);
    }

    #[test]
    fn value_flag_value_never_reinterpreted() {
        // `--eq`'s value looks flag-ish only if it started with `-`; here the
        // value `type:note` must be consumed as the eq value, not a subcommand.
        let n = norm(&["norn", "find", "--sort", "status", "--type", "note"]);
        assert_eq!(
            n.argv,
            vec!["norn", "find", "--sort", "status", "--eq", "type:note"]
        );
    }

    #[test]
    fn global_before_subcommand_is_skipped() {
        let n = norm(&["norn", "-C", "/vault", "find", "--type", "note"]);
        assert_eq!(
            n.argv,
            vec!["norn", "-C", "/vault", "find", "--eq", "type:note"]
        );
        assert_eq!(n.dynamic_keys, vec!["type".to_string()]);
    }

    #[test]
    fn non_query_subcommand_passes_through() {
        let input = ["norn", "get", "note.md", "--format", "json"];
        let n = norm(&input);
        assert_eq!(
            n.argv,
            input.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn set_eq_is_cross_family_teaching_error() {
        let err = normalize_argv(
            ["norn", "set", "note.md", "--eq", "status:done"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--field key=value"), "{msg}");
        assert!(msg.contains("--eq"), "{msg}");
    }

    #[test]
    fn set_field_passes_through() {
        let input = ["norn", "set", "note.md", "--field", "status=done"];
        let n = norm(&input);
        assert_eq!(
            n.argv,
            input.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    // ── T2b: field-universe gate ───────────────────────────────────────────
    fn universe(fields: &[&str]) -> BTreeSet<String> {
        fields.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gate_accepts_known_field() {
        let u = universe(&["type", "status"]);
        let flags = query_known_flags(QueryCmd::Find);
        assert!(gate_dynamic_fields(&["type".to_string()], &u, &flags).is_ok());
    }

    #[test]
    fn gate_rejects_typo_of_real_flag_with_suggestion() {
        // `--formt json` desugared to key `formt`; not a field → hard error
        // pointing at the real `--format` flag (the silent-empty trap killer).
        let u = universe(&["type", "status"]);
        let flags = query_known_flags(QueryCmd::Find);
        let err = gate_dynamic_fields(&["formt".to_string()], &u, &flags).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--format"),
            "expected --format suggestion, got: {msg}"
        );
    }

    #[test]
    fn gate_rejects_unknown_non_field_key() {
        let u = universe(&["type", "status"]);
        let flags = query_known_flags(QueryCmd::Find);
        let err = gate_dynamic_fields(&["zzqqxx".to_string()], &u, &flags).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn gate_suggests_near_field() {
        // `priorty` is a clear typo of the `priority` field and far from any
        // real flag, so the suggestion must be the field.
        let u = universe(&["status", "priority"]);
        let flags = query_known_flags(QueryCmd::Find);
        let err = gate_dynamic_fields(&["priorty".to_string()], &u, &flags).unwrap_err();
        assert!(err.to_string().contains("priority"), "{err}");
    }
}
