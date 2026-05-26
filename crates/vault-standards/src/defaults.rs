//! Helpers for resolving `frontmatter_defaults` against a vault config.
//!
//! Exposes [`path_variables`] for extracting named path-variable bindings,
//! [`applicable_rules`] + [`merge_defaults`] for the match-to-defaults pass,
//! and [`resolve_to_fixpoint`] for the iterative resolver used by `vault new`.

use crate::config::{CompiledConfig, CompiledRule, ValidateRule, VaultConfig};
use std::collections::{BTreeMap, BTreeSet};

/// Iterate `{{…}}` substitution groups in a template, yielding the inner
/// expression (trimmed) for each. Quad-brace escapes (`{{{{` / `}}}}`) are
/// skipped — they render as literal `{{`/`}}` and don't contain a real var.
///
/// Shared by [`collect_path_var_refs`] and [`collect_transform_refs`] so both
/// helpers agree with the runtime renderer (`substitution::render`) about
/// what counts as a substitution group.
fn substitution_groups(template: &str) -> impl Iterator<Item = &str> {
    let mut rest = template;
    std::iter::from_fn(move || {
        loop {
            let open = rest.find("{{")?;
            // Quad-brace `{{{{` is a literal-`{{` escape — skip past all four.
            if rest[open..].starts_with("{{{{") {
                rest = &rest[open + 4..];
                continue;
            }
            let after = &rest[open + 2..];
            let close = after.find("}}")?;
            let inner = after[..close].trim();
            rest = &after[close + 2..];
            return Some(inner);
        }
    })
}

/// Collect all `path.X` variable names referenced in a template string.
///
/// Scans for `{{path.X}}` patterns and returns the set of `X` names found.
/// Pipe transforms and colon-args are stripped; only the variable portion is
/// considered. Quad-brace escapes (`{{{{…}}}}`) are correctly skipped.
pub(crate) fn collect_path_var_refs(template: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for inner in substitution_groups(template) {
        // Strip pipe transforms — only the variable portion matters here.
        let var_part = inner.split('|').next().unwrap().trim();
        if let Some(name) = var_part.strip_prefix("path.") {
            // Strip any colon-arg form (path vars don't take colon args today, but be tolerant).
            let name = name.split(':').next().unwrap().trim();
            out.insert(name.to_string());
        }
    }
    out
}

/// Collect all transform names referenced in a template string.
///
/// Scans for `{{var | t1 | t2}}` patterns and returns all transform names
/// (the parts after `|`) found across the template. Quad-brace escapes
/// (`{{{{…}}}}`) are correctly skipped.
pub(crate) fn collect_transform_refs(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    for inner in substitution_groups(template) {
        for part in inner.split('|').skip(1) {
            out.push(part.trim().to_string());
        }
    }
    out
}

/// The canonical list of known transform names.
///
/// **Invariant:** must stay in sync with `apply_transform` in
/// `crates/vault-standards/src/substitution.rs`. Adding a transform there
/// without updating this list silently under-validates configs; removing one
/// here will reject configs that the renderer would still accept. There is no
/// compile-time enforcement of the sync — see the `KNOWN_TRANSFORMS_match`
/// test in `substitution.rs` if you need to pin it.
pub(crate) const KNOWN_TRANSFORMS: &[&str] = &[
    "titlecase",
    "sentencecase",
    "lower",
    "upper",
    "unsep",
    "strip_date_prefix",
    "slugify",
];

/// Extract the named path-variable bindings produced by a rule's `match.path`
/// pattern against `path`. Returns an empty map if the rule has no path
/// pattern or if the path does not match.
///
/// The rule's pattern is the pre-compiled [`crate::path_match::PathPattern`]
/// stored on [`CompiledRule`]. Pre-compilation happens at config-load time,
/// so this helper is cheap to call repeatedly within a single `vault new`
/// invocation.
pub fn path_variables(rule: &CompiledRule, path: &str) -> BTreeMap<String, String> {
    rule.path
        .as_ref()
        .and_then(|p| p.match_path(path))
        .unwrap_or_default()
}

/// Rules from the config that apply to `path` (and to `frontmatter`, when supplied).
///
/// Returns paired references to both the (uncompiled) [`ValidateRule`] (which carries
/// `frontmatter_defaults`, `required_frontmatter`, etc.) and the matching
/// [`CompiledRule`] (which carries the pre-compiled path patterns).
///
/// A rule matches when:
/// - Its `match.path` is `None`, OR the path matches its compiled `PathPattern`.
/// - Its `match.frontmatter` is empty, OR `frontmatter` is `Some(fm)` and every
///   `(key, value)` predicate is present in `fm`.
pub fn applicable_rules<'a>(
    cfg: &'a VaultConfig,
    compiled: &'a CompiledConfig,
    path: &str,
    frontmatter: Option<&serde_json::Value>,
) -> Vec<(&'a ValidateRule, &'a CompiledRule)> {
    let mut out = Vec::new();
    for (rule, compiled_rule) in cfg.validate.rules.iter().zip(compiled.rules.iter()) {
        // Path matcher
        if let Some(pat) = &compiled_rule.path {
            if pat.match_path(path).is_none() {
                continue;
            }
        }
        // Frontmatter matchers — if the rule has any, frontmatter must be provided and match all.
        if !rule.r#match.frontmatter.is_empty() {
            let Some(fm) = frontmatter else { continue };
            let Some(fm_obj) = fm.as_object() else {
                continue;
            };
            let all_match = rule
                .r#match
                .frontmatter
                .iter()
                .all(|(k, v)| fm_obj.get(k) == Some(v));
            if !all_match {
                continue;
            }
        }
        out.push((rule, compiled_rule));
    }
    out
}

/// Collect `frontmatter_defaults` from a slice of matching rules.
///
/// Earlier-in-slice wins on field collision (config-load already refused
/// rule-level conflicts with different values; identical values are safe).
pub fn merge_defaults<'a>(
    rules: &[(&'a ValidateRule, &'a CompiledRule)],
) -> BTreeMap<String, &'a serde_json::Value> {
    let mut out: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for (rule, _) in rules {
        for (field, value) in &rule.frontmatter_defaults {
            out.entry(field.clone()).or_insert(value);
        }
    }
    out
}

/// Errors produced by [`resolve_to_fixpoint`].
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("substitution failed: {0}")]
    Substitution(String),
}

/// Iteratively resolve `frontmatter_defaults` from all matching rules to a fixpoint.
///
/// Pass 1 matches rules by path only (no synthesized frontmatter yet); subsequent
/// passes re-match including frontmatter predicates so rules keyed on
/// just-applied fields can now match. Operator overrides always win and never
/// get overwritten.
///
/// Returns the fully-resolved frontmatter map plus the names of all rules
/// whose defaults contributed.
pub fn resolve_to_fixpoint(
    cfg: &VaultConfig,
    compiled: &CompiledConfig,
    path: &str,
    operator_overrides: &BTreeMap<String, serde_json::Value>,
    path_vars_for_substitution: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, serde_json::Value>, Vec<String>), ResolveError> {
    use chrono::Local;

    let mut frontmatter: BTreeMap<String, serde_json::Value> = operator_overrides.clone();
    let mut applied_rules: Vec<String> = Vec::new();

    let sub_ctx = crate::substitution::Context {
        now: Local::now().naive_local(),
        title: path
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".md")
            .to_string(),
        path_vars: path_vars_for_substitution.clone(),
        // Phase 4 will wire in cfg.templates.date_format/time_format.
        // For now use standard defaults that match the substitution module's own tests.
        date_format: "YYYY-MM-DD".to_string(),
        time_format: "HH:mm".to_string(),
    };

    // Hard cap on iterations. Real schemas reach fixpoint in 2-3; cap at 16
    // to refuse pathological configs early without hanging.
    const MAX_PASSES: usize = 16;
    for pass in 0..MAX_PASSES {
        let fm_value = serde_json::Value::Object(
            frontmatter
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        let rules = applicable_rules(cfg, compiled, path, Some(&fm_value));
        let merged = merge_defaults(&rules);

        let mut changed = false;
        for (field, raw_value) in merged {
            if frontmatter.contains_key(&field) {
                continue;
            }
            let resolved = if let Some(s) = raw_value.as_str() {
                let rendered = crate::substitution::render(s, &sub_ctx)
                    .map_err(|e| ResolveError::Substitution(format!("rule pass {pass}: {e}")))?;
                serde_json::Value::String(rendered)
            } else {
                raw_value.clone()
            };
            frontmatter.insert(field, resolved);
            changed = true;
        }
        for (r, _) in &rules {
            if let Some(n) = r.name.as_deref() {
                if !applied_rules.contains(&n.to_string()) {
                    applied_rules.push(n.to_string());
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok((frontmatter, applied_rules))
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use crate::config::{parse_config_compiled, VaultConfig};
    use camino::Utf8Path;

    fn build(yaml: &str) -> (VaultConfig, crate::config::CompiledConfig) {
        parse_config_compiled(yaml, Utf8Path::new(".vault/config.yaml")).unwrap()
    }

    #[test]
    fn applicable_rules_path_only_match() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: any
      match:
        path: "**/*.md"
    - name: task
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
"#,
        );
        let rules = applicable_rules(&cfg, &compiled, "Workspaces/foo/tasks/bar.md", None);
        let names: Vec<_> = rules
            .iter()
            .filter_map(|(r, _)| r.name.as_deref())
            .collect();
        assert!(names.contains(&"any"));
        assert!(names.contains(&"task"));
    }

    #[test]
    fn applicable_rules_skips_non_matching_path() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: task
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
"#,
        );
        let rules = applicable_rules(&cfg, &compiled, "Logs/2026/foo.md", None);
        assert!(rules.is_empty());
    }

    #[test]
    fn applicable_rules_frontmatter_matcher_requires_match() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: task-base
      match:
        path: "**/*.md"
        frontmatter:
          type: task
"#,
        );
        // Without frontmatter: rule has a frontmatter matcher → does NOT match.
        let rules = applicable_rules(&cfg, &compiled, "anything.md", None);
        assert!(rules.is_empty());

        // With frontmatter type=task: matches.
        let fm = serde_json::json!({"type": "task"});
        let rules = applicable_rules(&cfg, &compiled, "anything.md", Some(&fm));
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn merge_defaults_collects_across_rules() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: any
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
    - name: with-status
      match:
        path: "**/*.md"
      frontmatter_defaults:
        status: backlog
"#,
        );
        let rules = applicable_rules(&cfg, &compiled, "foo.md", None);
        let merged = merge_defaults(&rules);
        assert_eq!(merged.get("type"), Some(&&serde_json::json!("note")));
        assert_eq!(merged.get("status"), Some(&&serde_json::json!("backlog")));
    }

    #[test]
    fn merge_defaults_earlier_rule_wins_on_collision() {
        // Both rules say `type` — identical values are allowed by config-load
        // (Phase 3.4); merge_defaults should pick the earlier one without panicking.
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: first
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
    - name: second
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#,
        );
        let rules = applicable_rules(&cfg, &compiled, "foo.md", None);
        let merged = merge_defaults(&rules);
        assert_eq!(merged.get("type"), Some(&&serde_json::json!("note")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config_compiled;
    use camino::Utf8Path;

    fn compile(yaml: &str) -> crate::config::CompiledConfig {
        let (_, compiled) =
            parse_config_compiled(yaml, Utf8Path::new(".vault/config.yaml")).unwrap();
        compiled
    }

    #[test]
    fn extracts_named_path_variable() {
        let compiled = compile(
            r#"
validate:
  rules:
    - name: task-in-workspace
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
"#,
        );
        let vars = path_variables(&compiled.rules[0], "Workspaces/vault-cli/tasks/foo.md");
        assert_eq!(vars.get("workspace"), Some(&"vault-cli".to_string()));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn returns_empty_when_path_does_not_match() {
        let compiled = compile(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
"#,
        );
        let vars = path_variables(&compiled.rules[0], "Logs/2026/foo.md");
        assert!(vars.is_empty());
    }

    #[test]
    fn returns_empty_when_rule_has_no_path_pattern() {
        let compiled = compile(
            r#"
validate:
  rules:
    - name: r
      match:
        frontmatter:
          type: note
"#,
        );
        let vars = path_variables(&compiled.rules[0], "anything.md");
        assert!(vars.is_empty());
    }

    #[test]
    fn extracts_multiple_path_variables() {
        let compiled = compile(
            r#"
validate:
  rules:
    - name: log-by-year-month
      match:
        path: "Log/{{year}}/{{month}}/*.md"
"#,
        );
        let vars = path_variables(&compiled.rules[0], "Log/2026/05/daily.md");
        assert_eq!(vars.get("year"), Some(&"2026".to_string()));
        assert_eq!(vars.get("month"), Some(&"05".to_string()));
    }

    #[test]
    fn quad_brace_escape_is_not_a_substitution_group() {
        // `{{{{...}}}}` is a literal-brace escape; nothing inside should be
        // interpreted as a path var or transform.
        assert!(collect_path_var_refs("{{{{path.workspace}}}}").is_empty());
        assert!(collect_transform_refs("{{{{title | bogus_transform}}}}").is_empty());
    }

    #[test]
    fn collect_path_var_refs_handles_pipes_and_colons() {
        assert!(collect_path_var_refs("{{path.workspace | titlecase}}").contains("workspace"));
        // Path vars don't take colon args today, but the helper tolerates the shape.
        assert!(collect_path_var_refs("{{path.workspace:ignored}}").contains("workspace"));
    }

    #[test]
    fn known_transforms_round_trip_through_renderer() {
        // Pin: every name in KNOWN_TRANSFORMS must be accepted by apply_transform
        // in substitution.rs. If apply_transform stops recognizing one or gains
        // a new one without updating this list, this test surfaces the drift.
        use crate::substitution::{render, Context, RenderError};
        use chrono::{NaiveDate, NaiveTime};

        let ctx = Context {
            now: NaiveDate::from_ymd_opt(2026, 5, 25)
                .unwrap()
                .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
            title: "hello-world".into(),
            path_vars: BTreeMap::new(),
            date_format: "YYYY-MM-DD".into(),
            time_format: "HH:mm".into(),
        };
        for name in KNOWN_TRANSFORMS {
            let template = format!("{{{{title | {name}}}}}");
            match render(&template, &ctx) {
                Ok(_) => {}
                Err(RenderError::UnknownTransform(t)) => panic!(
                    "KNOWN_TRANSFORMS lists `{name}` but renderer rejects it as unknown ({t}); \
                     defaults.rs::KNOWN_TRANSFORMS and substitution.rs::apply_transform have drifted"
                ),
                Err(other) => panic!("unexpected render error for known transform `{name}`: {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod fixpoint_tests {
    use super::*;
    use crate::config::{parse_config_compiled, VaultConfig};
    use camino::Utf8Path;

    fn build(yaml: &str) -> (VaultConfig, crate::config::CompiledConfig) {
        parse_config_compiled(yaml, Utf8Path::new(".vault/config.yaml")).unwrap()
    }

    #[test]
    fn fixpoint_resolves_two_phase_chain() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: by-path
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        type: task
    - name: by-type
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      frontmatter_defaults:
        status: backlog
"#,
        );
        let path_vars = BTreeMap::from([("workspace".to_string(), "foo".to_string())]);
        let (frontmatter, rules_applied) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "Workspaces/foo/tasks/bar.md",
            &BTreeMap::new(), // no operator overrides
            &path_vars,
        )
        .unwrap();

        assert_eq!(frontmatter.get("type"), Some(&serde_json::json!("task")));
        assert_eq!(
            frontmatter.get("status"),
            Some(&serde_json::json!("backlog"))
        );
        assert!(rules_applied.contains(&"by-path".to_string()));
        assert!(rules_applied.contains(&"by-type".to_string()));
    }

    #[test]
    fn operator_overrides_win_over_defaults() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
        status: backlog
"#,
        );
        let overrides = BTreeMap::from([("type".to_string(), serde_json::json!("custom"))]);
        let (frontmatter, _rules) =
            resolve_to_fixpoint(&cfg, &compiled, "foo.md", &overrides, &BTreeMap::new()).unwrap();

        assert_eq!(frontmatter.get("type"), Some(&serde_json::json!("custom")));
        assert_eq!(
            frontmatter.get("status"),
            Some(&serde_json::json!("backlog"))
        );
    }

    #[test]
    fn fixpoint_substitutes_string_templates() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        workspace: "[[{{path.workspace}}]]"
        title: "{{title | titlecase}}"
"#,
        );
        let path_vars = BTreeMap::from([("workspace".to_string(), "vault-cli".to_string())]);
        let (frontmatter, _rules) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "Workspaces/vault-cli/tasks/design-foo.md",
            &BTreeMap::new(),
            &path_vars,
        )
        .unwrap();

        assert_eq!(
            frontmatter.get("workspace"),
            Some(&serde_json::json!("[[vault-cli]]"))
        );
        assert_eq!(
            frontmatter.get("title"),
            Some(&serde_json::json!("Design Foo"))
        );
    }

    #[test]
    fn path_matches_no_rules_returns_empty() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        type: task
"#,
        );
        let (frontmatter, rules_applied) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "Logs/2026/foo.md",
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(frontmatter.is_empty());
        assert!(rules_applied.is_empty());
    }
}
