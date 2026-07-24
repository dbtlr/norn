//! Resolver for `frontmatter_defaults` against a vault config.
//!
//! Exposes [`path_variables`] for extracting named path-variable bindings,
//! [`applicable_rules`] + [`merge_defaults`] for the match-to-defaults pass,
//! and [`resolve_to_fixpoint`] for the iterative resolver used by `norn new`.
//!
//! The `{{…}}` reference-scanning the config checks need
//! (`collect_path_var_refs` / `collect_transform_refs` / `is_known_transform`,
//! which derives its answer from [`crate::standards::substitution::TRANSFORMS`])
//! lives in [`crate::standards::template_refs`]; the `{{…}}` renderer lives in
//! [`crate::standards::substitution`]. This module is the one match-and-merge
//! entry point over those two — it does not re-implement either.
//!
//! **Value-in clock (ADR 0018):** [`resolve_to_fixpoint`] takes the
//! current instant as a parameter rather than reading `Local::now()`. No
//! ambient reads — the caller injects the clock, consistent with the grammar
//! `today` injection (NRN-342). `{{seq}}` placeholders are likewise an
//! apply-time input the caller supplies value-in; nothing here reads them.

use crate::standards::config::{CompiledConfig, CompiledRule, ValidateRule, VaultConfig};
use crate::standards::predicates::frontmatter_predicate_matches;
use chrono::NaiveDateTime;
use std::collections::BTreeMap;

/// Extract the named path-variable bindings produced by a rule's `match.path`
/// pattern against `path`. Returns an empty map if the rule has no path
/// pattern or if the path does not match.
///
/// The rule's pattern is the pre-compiled [`crate::standards::path_match::PathPattern`]
/// stored on [`CompiledRule`]. Pre-compilation happens at config-load time,
/// so this helper is cheap to call repeatedly within a single `norn new`
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
///   predicate accepts its field value (scalar = exact equality, list = any-of
///   — the same selector semantics the validate engine applies).
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
            let all_match = rule.r#match.frontmatter.iter().all(|(k, v)| {
                fm_obj
                    .get(k)
                    .is_some_and(|actual| frontmatter_predicate_matches(actual, v))
            });
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
/// `now` is injected value-in (no ambient `Local::now()` read); the caller owns
/// the clock. String defaults are rendered through
/// [`crate::standards::substitution::render`] against it.
///
/// Returns the fully-resolved frontmatter map plus the names of all rules
/// whose defaults contributed.
pub fn resolve_to_fixpoint(
    cfg: &VaultConfig,
    compiled: &CompiledConfig,
    path: &str,
    now: NaiveDateTime,
    operator_overrides: &BTreeMap<String, serde_json::Value>,
    path_vars_for_substitution: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, serde_json::Value>, Vec<String>), ResolveError> {
    let mut frontmatter: BTreeMap<String, serde_json::Value> = operator_overrides.clone();
    let mut applied_rules: Vec<String> = Vec::new();

    let sub_ctx = crate::standards::substitution::Context {
        now,
        title: path
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".md")
            .to_string(),
        path_vars: path_vars_for_substitution.clone(),
        date_format: cfg.templates.date_format.clone(),
        time_format: cfg.templates.time_format.clone(),
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
                let rendered = crate::standards::substitution::render(s, &sub_ctx)
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
    use crate::standards::config::{parse_config_compiled, VaultConfig};
    use camino::Utf8Path;

    fn build(yaml: &str) -> (VaultConfig, crate::standards::config::CompiledConfig) {
        parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).unwrap()
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
    fn applicable_rules_list_selector_matches_any_listed_value() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: node-base
      match:
        path: "**/*.md"
        frontmatter:
          type: [task, phase]
      frontmatter_defaults:
        status: backlog
"#,
        );
        for ty in ["task", "phase"] {
            let fm = serde_json::json!({ "type": ty });
            let rules = applicable_rules(&cfg, &compiled, "anything.md", Some(&fm));
            assert_eq!(rules.len(), 1, "type={ty} should match the any-of selector");
        }

        let fm = serde_json::json!({"type": "note"});
        let rules = applicable_rules(&cfg, &compiled, "anything.md", Some(&fm));
        assert!(
            rules.is_empty(),
            "type outside the any-of set must not match"
        );

        // The list enumerates candidate scalars — a literal array value is not
        // a match, mirroring the engine predicate.
        let fm = serde_json::json!({"type": ["task", "phase"]});
        let rules = applicable_rules(&cfg, &compiled, "anything.md", Some(&fm));
        assert!(rules.is_empty(), "array-valued field must not match");
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
mod path_variable_tests {
    use super::*;
    use crate::standards::config::parse_config_compiled;
    use camino::Utf8Path;

    fn compile(yaml: &str) -> crate::standards::config::CompiledConfig {
        let (_, compiled) =
            parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).unwrap();
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
        let vars = path_variables(&compiled.rules[0], "Workspaces/norn/tasks/foo.md");
        assert_eq!(vars.get("workspace"), Some(&"norn".to_string()));
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
}

#[cfg(test)]
mod fixpoint_tests {
    use super::*;
    use crate::standards::config::{parse_config_compiled, VaultConfig};
    use camino::Utf8Path;
    use chrono::{NaiveDate, NaiveTime};

    fn build(yaml: &str) -> (VaultConfig, crate::standards::config::CompiledConfig) {
        parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).unwrap()
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 5, 25)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(18, 30, 0).unwrap())
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
            now(),
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
        let (frontmatter, _rules) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "foo.md",
            now(),
            &overrides,
            &BTreeMap::new(),
        )
        .unwrap();

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
        let path_vars = BTreeMap::from([("workspace".to_string(), "norn".to_string())]);
        let (frontmatter, _rules) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "Workspaces/norn/tasks/design-foo.md",
            now(),
            &BTreeMap::new(),
            &path_vars,
        )
        .unwrap();

        assert_eq!(
            frontmatter.get("workspace"),
            Some(&serde_json::json!("[[norn]]"))
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
            now(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(frontmatter.is_empty());
        assert!(rules_applied.is_empty());
    }

    #[test]
    fn fixpoint_uses_injected_clock_and_configured_date_format() {
        let (cfg, compiled) = build(
            r#"
templates:
  date_format: "DD/MM/YYYY"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        when: "{{date}}"
"#,
        );
        let (frontmatter, _) = resolve_to_fixpoint(
            &cfg,
            &compiled,
            "foo.md",
            now(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        // The injected clock (2026-05-25) rendered through DD/MM/YYYY is exact
        // and deterministic — no more approximate length/shape probing.
        assert_eq!(
            frontmatter.get("when"),
            Some(&serde_json::json!("25/05/2026"))
        );
    }
}
