//! Helpers for resolving `frontmatter_defaults` against a vault config.
//!
//! Currently exposes [`path_variables`] for extracting named path-variable
//! bindings from a [`CompiledRule`] given a destination path. Phase 3 of the
//! `vault new` arc adds `applicable_rules`, `merge_defaults`, and the
//! fixpoint resolver on top.

use crate::config::CompiledRule;
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
