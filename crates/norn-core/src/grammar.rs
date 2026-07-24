//! Forgiving-input CLI grammar (ADR 0010: "one canonical form, forgiving
//! inputs"; NRN-206 / NRN-207 / NRN-209) — the core-side, clap-free logic.
//!
//! Every norn grammar has exactly one canonical spelling — the form docs
//! teach, help shows, and errors echo. This module owns the *input-side*
//! forgiveness that normalizes predictable, evidence-mined variants to
//! canonical before the CLI's parser ever sees them. Nothing here is taught;
//! canonical spellings are unchanged in help/errors.
//!
//! Three concerns, one seam:
//!
//! - **Separator forgiveness (T1, [`split_field_value`]).** Predicate tokens
//!   canonically take `FIELD:VALUE`; assignment tokens canonically take
//!   `KEY=VALUE`. Both families accept EITHER separator — the split point is
//!   the FIRST `:` or `=`, whichever comes first. Deterministic because keys
//!   contain neither, so a value-embedded `:` (datetime/URL) or `=` parses
//!   correctly under first-separator-wins. Shared by the query predicate
//!   parsers ([`crate::query`]) and the mutate assignment parsers.
//!
//! - **Dynamic field predicates (T2, [`normalize_argv`] + [`gate_dynamic_fields`]).**
//!   On the query family only, an unknown `--key value` desugars to
//!   `--eq key:value`. Reserved flags always win; the key must resolve against
//!   the vault's known-field universe ([`schema_field_names`] ∪ observed
//!   frontmatter keys) or it hard-errors with did-you-mean.
//!
//! - **Alias pack (T3, [`normalize_argv`]).** `--where`/`--filter` → `--eq`,
//!   `--group-by` → `--by`, `count --all` → no-op. Hidden: accepted, never in
//!   help. Resolved BEFORE the dynamic pass so they are never reinterpreted as
//!   field predicates.
//!
//! # Desugaring map (how sugared query forms lower to canonical predicates)
//!
//! [`normalize_argv`] rewrites argv in place, emitting only canonical flags:
//!
//! | Sugared input                       | Canonical output              |
//! |-------------------------------------|-------------------------------|
//! | `--type note` (one occurrence)      | `--eq type:note`              |
//! | `--status active --status done`     | `--in status:active,done`     |
//! | `--status=active`                   | `--eq status:active`          |
//! | `--where type:note`                 | `--eq type:note`              |
//! | `--filter status:active`            | `--eq status:active`          |
//! | `count --group-by type`             | `count --by type`             |
//! | `count --all`                       | (dropped — no-op)             |
//!
//! A single dynamic occurrence lowers to `--eq` (its value is one literal, so
//! an embedded comma stays literal); a repeated key lowers to any-of `--in`
//! (and is refused if a value contains a comma, since that collides with the
//! any-of separator — ADR 0010 is "forgiving but NEVER silently wrong"). The
//! desugared keys ride out in [`Normalized::dynamic_keys`] for the field-universe
//! gate ([`gate_dynamic_fields`]); canonical `--eq`/`--in` are never gated.
//!
//! # clap-derivation seam (ADR 0018)
//!
//! norn-core never links clap. Deriving the known-flag sets from clap's
//! own `Command` (the NRN-178 anti-drift lesson: a flag added to the CLI cannot
//! silently degrade into a dynamic predicate) is a CLI concern
//! and stays in `norn-cli`; the pure normalization algorithm here consumes the
//! derived sets as an injected [`KnownFlags`] value. The CLI builds a `KnownFlags`
//! once per process from `Cli::command()` (globals + `find`/`count`/`describe`
//! args into `query_value`/`query_boolean`, and each mutate subcommand's value
//! args into `mutate_value`) and hands it to [`normalize_argv`]. The
//! clap-derivation drift-guard test lives on that seam, not here.
//!
//! Also deferred with the cache/daemon ports: the daemon-side dynamic-field
//! refusal carrier (`DynamicFieldRefusal`, `gate_dynamic_refusal`,
//! `field_universe`) — it rides MCP `structuredContent`, needs the warm `Cache`
//! and a `schemars` schema, and is transport plumbing rather than grammar
//! semantics. The vault-model half of the field universe ([`schema_field_names`])
//! ports here; the observed-frontmatter half comes from the cache engine.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};

use crate::standards::ValidateConfig;

/// Split an assignment/predicate token into `(key, value)` at the FIRST `:` or
/// `=`, whichever comes first (ADR 0010 separator forgiveness, T1). Returns
/// `None` when the token contains neither separator.
///
/// Deterministic key/value boundary: keys contain neither `:` nor `=`, so the
/// first separator always delimits the key. A value-embedded `:` (a datetime
/// like `2026-07-01T10:30`, a URL) or `=` is preserved verbatim in the value.
///
/// Known exotic limitation: the dynamic-predicate gate validates the raw flag
/// name (which may contain a `:`, e.g. `--a:b value`), but this split then cuts
/// the emitted `a:b:value` token at the first `:` — so a field name that
/// literally contains a colon is gated and filtered on divergent keys. Field
/// names with colons are unsupported; use canonical `--eq` for such a field.
pub fn split_field_value(token: &str) -> Option<(&str, &str)> {
    let idx = token.bytes().position(|b| b == b':' || b == b'=')?;
    Some((&token[..idx], &token[idx + 1..]))
}

/// The three query-family commands that accept dynamic field predicates + the
/// alias pack.
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

/// Result of the pre-parser argv normalization pass.
#[derive(Debug)]
pub struct Normalized {
    /// Rewritten argv (including argv[0]) ready to feed the CLI parser.
    pub argv: Vec<String>,
    /// Keys that were desugared from a dynamic `--key value` into a predicate.
    /// The field-universe gate ([`gate_dynamic_fields`]) validates these against
    /// the vault's field universe; canonical `--eq` predicates are never gated.
    pub dynamic_keys: Vec<String>,
}

/// The CLI's known-flag surface, derived from clap on the CLI side and injected
/// here so the normalization algorithm stays clap-free (see the module's
/// clap-derivation seam). Pure data.
///
/// - `query_value` — query-family longs (globals + `find`/`count`/`describe`
///   args) whose action consumes a value.
/// - `query_boolean` — query-family longs that take no value.
/// - `mutate_value` — per-mutate-subcommand (`set`/`new`/`edit`) value-flag
///   longs (that subcommand's args plus the globals), for the cross-family
///   teaching-error scanner's value-consumption model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownFlags {
    pub query_value: BTreeSet<String>,
    pub query_boolean: BTreeSet<String>,
    pub mutate_value: HashMap<String, BTreeSet<String>>,
}

/// The frozen NRN-329 known-flag surface — the single source of truth both the
/// normalization tests here AND the CLI's clap-derivation drift guard
/// (`norn-cli`) compare against. Pure data; no clap. When the CLI derives a
/// different surface from `Cli::command()`, the drift guard fails — forcing a
/// conscious decision about a newly-added query/mutate flag (NRN-178).
///
/// `--vault` is the registered-vault global (exposed as of NRN-345) and
/// consumes a value; the global `--config` was deleted (ADR 0017
/// resolver-derived config).
pub fn frozen_known_flags() -> KnownFlags {
    /// The value-taking globals (`global = true`): present on every subcommand.
    const VALUE_GLOBALS: &[&str] = &["cwd", "color", "vault"];

    fn set_of(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn mutate_flags(own: &[&str]) -> BTreeSet<String> {
        let mut s = set_of(own);
        s.extend(VALUE_GLOBALS.iter().map(|g| g.to_string()));
        s
    }

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

impl KnownFlags {
    fn is_known_flag(&self, name: &str) -> bool {
        self.query_value.contains(name) || self.query_boolean.contains(name)
    }

    fn known_flag_takes_value(&self, name: &str) -> bool {
        self.query_value.contains(name)
    }

    /// The value flags one mutate subcommand accepts (its own args + the
    /// globals). Empty when the subcommand is unknown to the injected set.
    fn subcommand_value_flags(&self, sub: &str) -> BTreeSet<String> {
        self.mutate_value.get(sub).cloned().unwrap_or_default()
    }

    /// The reserved long flags for the query family, `--`-prefixed, for the
    /// gate's did-you-mean candidate set and the valueless-typo suggestion path
    /// (R1d). The shared query-family union is returned (a superset is harmless
    /// — did-you-mean only ever surfaces the closest match).
    pub fn query_known_flags(&self) -> Vec<String> {
        let mut flags: Vec<String> = self
            .query_value
            .iter()
            .chain(self.query_boolean.iter())
            .map(|f| format!("--{f}"))
            .collect();
        flags.sort();
        flags.dedup();
        flags
    }
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

/// Pre-parser argv normalization (T1 value-side is handled in the parsers; this
/// covers T2 dynamic predicates + T3 aliases). `args` includes argv[0].
///
/// - Non-query subcommands pass through unchanged, except a mutate-family
///   cross-family teaching error (`set --eq …` → point at `--field`).
/// - Query subcommands: aliases resolve first, then unknown `--key value`
///   desugars to `--eq`/`--in`, collecting the dynamic keys for the later
///   field-universe gate.
pub fn normalize_argv(args: Vec<String>, flags: &KnownFlags) -> Result<Normalized> {
    // Locate the subcommand token, skipping leading globals + their values.
    let Some(sub_idx) = find_subcommand_index(&args, flags) else {
        return Ok(Normalized {
            argv: args,
            dynamic_keys: Vec::new(),
        });
    };
    let sub = args[sub_idx].as_str();

    // Mutate-family teaching error: assignment lives on `--field key=value`,
    // never on the predicate flags.
    if is_mutate_family(sub) {
        if let Some(bad) = first_predicate_flag(sub, &args[sub_idx + 1..], flags) {
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

    normalize_query(args, sub_idx, cmd, flags)
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

fn first_predicate_flag(sub: &str, toks: &[String], flags: &KnownFlags) -> Option<String> {
    let value_flags = flags.subcommand_value_flags(sub);
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        // R1c: `--` ends option processing — nothing after it is a flag.
        if tok == "--" {
            break;
        }
        if let Some((name, inline)) = parse_long_flag(tok) {
            if PREDICATE_FLAGS.contains(&name) {
                return Some(format!("--{name}"));
            }
            // R4: skip a value flag's space-form value so a value that happens
            // to look like a predicate flag (`new --title --in`, a title whose
            // literal value is "--in") is not misclassified as a stray predicate.
            if inline.is_none() && value_flags.contains(name) {
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Find the index of the subcommand token: the first non-global token after
/// argv[0]. Skips global flags and (for the value-taking ones in space form)
/// their values, so a global value equal to a subcommand name is not mistaken
/// for the subcommand.
fn find_subcommand_index(args: &[String], flags: &KnownFlags) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let tok = &args[i];
        if let Some((name, inline)) = parse_long_flag(tok) {
            // A global value flag in space form consumes the next token — but
            // only a real value, never a flag-shaped one (R1a). Consuming a
            // `--`-leading token would mis-locate the subcommand; let the parser
            // surface the native missing-value error instead.
            if inline.is_none()
                && flags.known_flag_takes_value(name)
                && args.get(i + 1).is_some_and(|v| is_value_token(v))
            {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Short global `-C`/`--cwd` value form (same value-token guard).
        if tok == "-C" {
            if args.get(i + 1).is_some_and(|v| is_value_token(v)) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if tok.starts_with('-') && tok != "-" {
            // Any other short/long flag (including a bare `--`): skip it.
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn normalize_query(
    args: Vec<String>,
    sub_idx: usize,
    cmd: QueryCmd,
    flags: &KnownFlags,
) -> Result<Normalized> {
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

        // R1c: a bare `--` ends option processing — everything after it passes
        // through untouched (end-of-options semantics), never desugared.
        if tok == "--" {
            out.extend(toks[i..].iter().cloned());
            break;
        }

        let Some((name, inline)) = parse_long_flag(tok) else {
            // Short flag / positional — pass through. `-C VALUE` (short cwd)
            // consumes the next token ONLY when it is value-shaped, mirroring
            // find_subcommand_index and the reserved/alias branches' is_value_token
            // discipline: `find -C --type note` must leave `-C` value-less (let the
            // parser surface its native missing-value error) so `--type` still
            // desugars, and a trailing `-C` must emit exactly once.
            out.push(tok.clone());
            if tok == "-C" && toks.get(i + 1).is_some_and(|v| is_value_token(v)) {
                out.push(toks[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        };

        // `count --all` is an accepted no-op (find symmetry, T3): drop it.
        if cmd == QueryCmd::Count && name == "all" {
            i += 1;
            continue;
        }

        // Reserved built-in flags always win — never reinterpreted (T2a).
        if flags.is_known_flag(name) {
            out.push(tok.clone());
            if inline.is_none() && flags.known_flag_takes_value(name) {
                // R1a: consume the next token as the value ONLY if it is a real
                // value, never a flag-shaped token. `find --path --all` leaves
                // `--path` value-less so the parser emits its native "a value is
                // required" error (exit 2), instead of silently filtering
                // path="--all" and swallowing --all (the silent-empty trap the
                // whole gate exists to prevent).
                if toks.get(i + 1).is_some_and(|v| is_value_token(v)) {
                    out.push(toks[i + 1].clone());
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
                    // R1a: same value-token discipline as the reserved branch.
                    if toks.get(i + 1).is_some_and(|v| is_value_token(v)) {
                        out.push(toks[i + 1].clone());
                        i += 2;
                        continue;
                    }
                }
            }
            i += 1;
            continue;
        }

        // Minor: `find` has no grouping, so `--group-by` there is a user error
        // (not a field predicate). Point at the commands that DO group rather
        // than let it fall through to an "unknown field group-by" gate error.
        if cmd == QueryCmd::Find && name == "group-by" {
            return Err(anyhow!(
                "`find` has no grouping — use `count --by <field>` or \
                 `describe --by <field>` to group counts"
            ));
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
                // No consumable value. Route to a helpful error rather than a
                // bare "requires a value": a valueless-flag typo (`--al` for
                // `--all`) gets did-you-mean (R1d); a dash-leading value
                // (`--priority -1`) gets taught the inline form (R1e).
                next => {
                    return Err(dynamic_no_value_error(
                        cmd,
                        &key,
                        next.map(String::as_str),
                        flags,
                    ))
                }
            },
        };
        if !dyn_values.contains_key(&key) {
            dyn_order.push(key.clone());
        }
        dyn_values.entry(key).or_default().push(value);
        i += 1;
    }

    // Emit desugared predicates after the reserved tokens. Order among the query
    // flags is irrelevant (they accumulate into the same Vec), so appending
    // keeps canonical-only invocations unchanged.
    let mut dynamic_keys: Vec<String> = Vec::new();
    for key in dyn_order {
        let values = &dyn_values[&key];
        if values.len() == 1 {
            // A single occurrence carries one literal value; a `,` in it is a
            // literal (`--tag a,b` → `--eq tag:a,b`), not an any-of separator.
            out.push("--eq".to_string());
            out.push(format!("{key}:{}", values[0]));
        } else {
            // R3: a repeated key desugars to any-of `--in key:v1,v2`. `--in`
            // splits on commas, so a value that itself contains a comma cannot
            // round-trip losslessly (`--tag a,b --tag c` would read as three
            // values, not two). ADR 0010 is "forgiving but NEVER silently
            // wrong": refuse the ambiguous case rather than corrupt it.
            if values.iter().any(|v| v.contains(',')) {
                return Err(anyhow!(
                    "ambiguous repeated `--{key}`: a value contains a comma, which collides \
                     with the any-of separator — norn cannot tell two values from three. Use \
                     the canonical `--in {key}:v1,v2` with your own comma-joined values, or \
                     ensure no repeated `--{key}` value contains a comma"
                ));
            }
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

/// Error for a dynamic `--key` with no consumable value. A valueless-flag typo
/// (`--al` → `--all`) is routed through did-you-mean (R1d); otherwise, when the
/// next token is a dash-leading value (`--priority -1`), the inline form is
/// taught (R1e); otherwise the missing value is reported plainly.
fn dynamic_no_value_error(
    _cmd: QueryCmd,
    key: &str,
    next: Option<&str>,
    flags: &KnownFlags,
) -> anyhow::Error {
    let known = flags.query_known_flags();
    if let Some(suggestion) = closest(key, &known) {
        return anyhow!(
            "unknown flag `--{key}` — did you mean `{suggestion}`? (a dynamic `--{key} value` \
             predicate needs a value, and `--{key}` alone is not a boolean)"
        );
    }
    match next {
        Some(v) => anyhow!(
            "unknown flag `--{key}` requires a value, but `{v}` looks like a flag — to filter \
             on a dash-leading value use the inline form `--{key}={v}` (or canonical \
             `--eq {key}:{v}`)"
        ),
        None => anyhow!(
            "unknown flag `--{key}` requires a value: dynamic field predicates desugar \
             `--{key} VALUE` to `--eq {key}:VALUE` for known vault fields (a bare `--{key}` \
             is not a boolean)"
        ),
    }
}

/// A dynamic-field gate rejection, carried structurally so the owner maps it
/// straight onto `OwnerFrame::Rejected { message, hints }` (the NRN-361
/// soft-landing split): `message` is the stable headline naming the unknown
/// field, and `hints` carry the did-you-mean (via the one shared [`closest`]
/// heuristic) plus the canonical-`--eq` next step. The CLI renders the headline
/// as `norn: <message>` and each hint as a `hint:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRejection {
    pub message: String,
    pub hints: Vec<String>,
}

impl FieldRejection {
    /// A bare-headline rejection with no soft-landing hints — the shape a plain
    /// read-verb user error (a malformed predicate, an unresolvable
    /// `--links-to`) takes when it flows through the same Rejected channel.
    pub fn headline(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hints: Vec::new(),
        }
    }
}

impl From<String> for FieldRejection {
    fn from(message: String) -> Self {
        Self::headline(message)
    }
}

/// Validate every dynamically-desugared key against the vault's field universe
/// (T2b). A key that resolves is a legitimate predicate; a key that does not is
/// a hard rejection with a did-you-mean across both real flags and known fields
/// — the guardrail that stops a typo'd real flag (`--formt json`) or a mistyped
/// field (`--titel foo`) from becoming a silent empty query.
///
/// Returns a structured [`FieldRejection`] (headline + soft-landing hints) so
/// the owner surfaces it as `OwnerFrame::Rejected { message, hints }` under the
/// one soft-landing doctrine (NRN-361/367). The did-you-mean uses the single
/// shared [`closest`] heuristic — no second threshold.
pub fn gate_dynamic_fields(
    dynamic_keys: &[String],
    universe: &BTreeSet<String>,
    known_flags: &[String],
) -> Result<(), FieldRejection> {
    for key in dynamic_keys {
        if universe.contains(key) {
            continue;
        }
        // D1: a fresh vault (no schema declared + nothing indexed yet) has an
        // empty universe, so every forgiving `--key value` would hard-error with
        // an unhelpful did-you-mean over an empty field set. Say what is actually
        // wrong and point at canonical `--eq`, which bypasses the gate entirely.
        if universe.is_empty() {
            return Err(FieldRejection {
                message: format!(
                    "unknown field `{key}`: this vault has no known fields yet (no schema \
                     declared and no documents indexed)"
                ),
                hints: vec![format!(
                    "filter anyway with the canonical `--eq {key}:value`, which does not require \
                     a known field universe"
                )],
            });
        }
        let mut candidates: Vec<String> = known_flags.to_vec();
        candidates.extend(universe.iter().cloned());
        let suggestion = closest(key, &candidates);
        return Err(match suggestion {
            Some(s) => FieldRejection {
                message: format!("unknown field `{key}`"),
                hints: vec![
                    format!("did you mean `{s}`?"),
                    format!(
                        "filter a known field with `--eq {key}:value`; a dynamic `--{key} value` \
                         predicate only works for a field this vault knows"
                    ),
                ],
            },
            None => FieldRejection {
                message: format!("unknown field `{key}`"),
                hints: vec![format!(
                    "filter a known field with `--eq {key}:value` — `{key}` is not a known vault \
                     field or flag"
                )],
            },
        });
    }
    Ok(())
}

/// Frontmatter field names declared anywhere in the validate config — the
/// vault-model half of the dynamic-predicate field universe. The
/// observed-frontmatter half (indexed keys, which already covers a document's
/// `aliases` entry) is supplied by the cache engine and unioned in by the
/// caller (see the clap-derivation seam note on the deferred `field_universe`).
///
/// Covers declared-but-not-yet-present fields so a query for them is not a typo.
pub fn schema_field_names(validate: &ValidateConfig) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    fields.extend(validate.required_frontmatter.iter().cloned());
    for rule in &validate.rules {
        fields.extend(rule.required_frontmatter.iter().cloned());
        fields.extend(rule.forbidden_frontmatter.iter().cloned());
        fields.extend(rule.field_types.keys().cloned());
        fields.extend(rule.allowed_values.keys().cloned());
        fields.extend(rule.frontmatter_defaults.keys().cloned());
        fields.extend(rule.field_references.keys().cloned());
        // R2a: a rule's match-selector frontmatter keys declare fields too.
        // Rules commonly scope on `type` / `kind` / `archived` that appear ONLY
        // in the selector, never in `field_types` / `required_frontmatter`.
        // Omitting them false-rejects a valid `find --type note` on a schema
        // that gates rules by a field it never lists as managed.
        fields.extend(rule.r#match.frontmatter.keys().cloned());
    }
    fields
}

/// Closest candidate to `key` by Levenshtein distance, if one is near enough to
/// be a plausible typo. Candidates carry their own `--` prefix (flags) or none
/// (fields); the comparison is against the whole candidate string with leading
/// dashes stripped, so a flag typo (`formt` vs `--format`) still matches on the
/// shared stem.
///
/// Public so the CLI reuses this ONE did-you-mean heuristic wherever it computes
/// candidates (the dynamic-field gate here, plus the `--col` facet path in
/// `norn-cli`) rather than growing a second, drifting threshold (NRN-361).
pub fn closest(key: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for cand in candidates {
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

    // The known-flag fixture is the single frozen surface `frozen_known_flags`
    // exports — the same data the CLI's clap-derivation drift guard compares
    // against, so the normalization tests and the guard can never disagree.
    fn flags() -> KnownFlags {
        frozen_known_flags()
    }

    // ── T2/T3: argv normalization ──────────────────────────────────────────
    fn norm(args: &[&str]) -> Normalized {
        normalize_argv(args.iter().map(|s| s.to_string()).collect(), &flags()).unwrap()
    }

    fn norm_err(args: &[&str]) -> anyhow::Error {
        normalize_argv(args.iter().map(|s| s.to_string()).collect(), &flags()).unwrap_err()
    }

    #[test]
    fn canonical_find_argv_normalizes_to_itself() {
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
        let err = norm_err(&["norn", "find", "--draft"]);
        assert!(err.to_string().contains("requires a value"), "{err}");
    }

    #[test]
    fn bare_unknown_flag_before_another_flag_errors() {
        let err = norm_err(&["norn", "find", "--draft", "--limit", "3"]);
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
        // `--sort`'s value looks flag-ish only if it started with `-`; here the
        // value `status` must be consumed as the sort value, and `--type note`
        // still desugars.
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

    // ── `-C` after the subcommand: same is_value_token discipline as the ────
    // reserved branch (a correction for a degenerate-input class: this
    // `-C`-after-subcommand branch applies the same guard `find_subcommand_index`
    // uses).
    #[test]
    fn dash_c_does_not_swallow_following_flag() {
        // `find -C --type note`: `-C` is value-less (its value is flag-shaped),
        // so it passes through alone and `--type note` still desugars.
        let n = norm(&["norn", "find", "-C", "--type", "note"]);
        assert_eq!(n.argv, vec!["norn", "find", "-C", "--eq", "type:note"]);
        assert_eq!(n.dynamic_keys, vec!["type".to_string()]);
    }

    #[test]
    fn trailing_dash_c_emits_exactly_once() {
        let n = norm(&["norn", "find", "--all", "-C"]);
        assert_eq!(n.argv, vec!["norn", "find", "--all", "-C"]);
        assert_eq!(n.argv.iter().filter(|t| *t == "-C").count(), 1);
        assert!(n.dynamic_keys.is_empty());
    }

    #[test]
    fn dash_c_after_subcommand_consumes_value_shaped_next() {
        // The happy path: a real value-shaped token is still consumed as -C's value.
        let n = norm(&["norn", "find", "-C", "/vault", "--type", "note"]);
        assert_eq!(
            n.argv,
            vec!["norn", "find", "-C", "/vault", "--eq", "type:note"]
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
        let err = norm_err(&["norn", "set", "note.md", "--eq", "status:done"]);
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
        let known = flags().query_known_flags();
        assert!(gate_dynamic_fields(&["type".to_string()], &u, &known).is_ok());
    }

    #[test]
    fn gate_rejects_typo_of_real_flag_with_suggestion() {
        // `--formt json` desugared to key `formt`; not a field → hard rejection
        // whose did-you-mean hint points at the real `--format` flag (the
        // silent-empty trap killer). Headline names the field; hint carries the
        // suggestion (the NRN-361 soft-landing split).
        let u = universe(&["type", "status"]);
        let known = flags().query_known_flags();
        let rej = gate_dynamic_fields(&["formt".to_string()], &u, &known).unwrap_err();
        assert_eq!(rej.message, "unknown field `formt`");
        assert!(
            rej.hints.iter().any(|h| h.contains("--format")),
            "expected a --format did-you-mean hint, got: {:?}",
            rej.hints
        );
    }

    #[test]
    fn gate_rejects_unknown_non_field_key() {
        let u = universe(&["type", "status"]);
        let known = flags().query_known_flags();
        let rej = gate_dynamic_fields(&["zzqqxx".to_string()], &u, &known).unwrap_err();
        assert_eq!(rej.message, "unknown field `zzqqxx`");
        assert!(
            rej.hints.iter().any(|h| h.contains("--eq zzqqxx:value")),
            "expected a canonical --eq hint, got: {:?}",
            rej.hints
        );
    }

    #[test]
    fn gate_empty_universe_points_at_canonical_eq() {
        let u: BTreeSet<String> = BTreeSet::new();
        let known = flags().query_known_flags();
        let rej = gate_dynamic_fields(&["type".to_string()], &u, &known).unwrap_err();
        assert!(
            rej.message.contains("no known fields yet"),
            "{}",
            rej.message
        );
        assert!(
            rej.hints.iter().any(|h| h.contains("--eq type:value")),
            "expected a canonical --eq hint, got: {:?}",
            rej.hints
        );
    }

    #[test]
    fn gate_suggests_near_field() {
        // `priorty` is a clear typo of the `priority` field and far from any
        // real flag, so the suggestion hint must be the field.
        let u = universe(&["status", "priority"]);
        let known = flags().query_known_flags();
        let rej = gate_dynamic_fields(&["priorty".to_string()], &u, &known).unwrap_err();
        assert_eq!(rej.message, "unknown field `priorty`");
        assert!(
            rej.hints.iter().any(|h| h.contains("`priority`")),
            "expected a priority did-you-mean hint, got: {:?}",
            rej.hints
        );
    }

    // ── R1a: reserved value-flag must not swallow a flag-shaped next token ──
    #[test]
    fn reserved_value_flag_does_not_swallow_following_flag() {
        let n = norm(&["norn", "find", "--path", "--all"]);
        assert_eq!(n.argv, vec!["norn", "find", "--path", "--all"]);
        assert!(n.dynamic_keys.is_empty());
    }

    // ── R1c: `--` terminates option processing ─────────────────────────────
    #[test]
    fn double_dash_terminates_option_processing() {
        let n = norm(&["norn", "find", "--", "--type", "note"]);
        assert_eq!(n.argv, vec!["norn", "find", "--", "--type", "note"]);
        assert!(n.dynamic_keys.is_empty());
    }

    // ── R1d: valueless-flag typo routes to did-you-mean ────────────────────
    #[test]
    fn valueless_flag_typo_suggests_valueless_flag() {
        let err = norm_err(&["norn", "find", "--al"]);
        assert!(err.to_string().contains("--all"), "{err}");
    }

    #[test]
    fn valueless_flag_typo_desc_suggests_desc() {
        let err = norm_err(&["norn", "find", "--dsc"]);
        assert!(err.to_string().contains("--desc"), "{err}");
    }

    // ── R1e: dash-leading dynamic value teaches the inline form ────────────
    #[test]
    fn dash_leading_dynamic_value_teaches_inline_form() {
        let err = norm_err(&["norn", "find", "--priority", "-1"]);
        let msg = err.to_string();
        assert!(msg.contains("--priority=-1"), "{msg}");
    }

    // ── R3: repeated dynamic key with comma-containing values is refused ────
    #[test]
    fn repeated_dynamic_key_with_comma_value_is_refused() {
        let err = norm_err(&["norn", "find", "--tag", "a,b", "--tag", "c"]);
        assert!(err.to_string().contains("comma"), "{err}");
    }

    #[test]
    fn single_dynamic_key_with_comma_value_stays_eq() {
        // One occurrence carries one literal value — the comma is literal.
        let n = norm(&["norn", "find", "--tag", "a,b"]);
        assert_eq!(n.argv, vec!["norn", "find", "--eq", "tag:a,b"]);
        assert_eq!(n.dynamic_keys, vec!["tag".to_string()]);
    }

    // ── R4: a mutate flag VALUE shaped like a predicate is not misclassified ─
    #[test]
    fn mutate_flag_value_shaped_like_predicate_is_not_misclassified() {
        // `new --title --in`: the title's literal value is "--in"; it must not
        // trip the cross-family teaching error (which scans for predicate flags).
        let input = ["norn", "new", "--as", "note", "--title", "--in"];
        let n = norm(&input);
        assert_eq!(
            n.argv,
            input.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn set_eq_still_teaches_after_value_consumption_model() {
        // Regression guard for R4: the real cross-family miss still fires.
        let err = norm_err(&["norn", "set", "note.md", "--eq", "status:done"]);
        assert!(err.to_string().contains("--field key=value"), "{err}");
    }

    // ── Minor: `find --group-by` gets a grouping-specific error ─────────────
    #[test]
    fn find_group_by_reports_no_grouping() {
        let err = norm_err(&["norn", "find", "--group-by", "type"]);
        assert!(err.to_string().contains("has no grouping"), "{err}");
    }

    // ── PREDICATE_FLAGS stay a subset of the query value-flag surface ──────
    #[test]
    fn every_predicate_flag_is_a_real_query_value_flag() {
        let k = flags();
        for p in PREDICATE_FLAGS {
            assert!(
                k.query_value.contains(*p),
                "predicate flag `--{p}` is not a query value flag"
            );
        }
    }

    // ── R2: field universe includes match-selector keys ────────────────────
    #[test]
    fn schema_fields_include_match_selector_keys() {
        use crate::standards::ValidateRule;

        let mut rule = ValidateRule::default();
        rule.r#match
            .frontmatter
            .insert("kind".to_string(), serde_json::json!("reference"));
        let mut validate = ValidateConfig::default();
        validate.rules.push(rule);

        let fields = schema_field_names(&validate);
        assert!(
            fields.contains("kind"),
            "match-selector key missing: {fields:?}"
        );
    }
}
