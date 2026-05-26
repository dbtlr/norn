//! Helpers for resolving `frontmatter_defaults` against a vault config.
//!
//! Currently exposes [`path_variables`] for extracting named path-variable
//! bindings from a [`CompiledRule`] given a destination path. Phase 3 of the
//! `vault new` arc adds `applicable_rules`, `merge_defaults`, and the
//! fixpoint resolver on top.

use crate::config::CompiledRule;
use std::collections::{BTreeMap, BTreeSet};

/// Collect all `path.X` variable names referenced in a template string.
///
/// Scans for `{{path.X}}` patterns and returns the set of `X` names found.
/// Pipe transforms and colon-args are stripped; only the variable portion is
/// considered.
pub(crate) fn collect_path_var_refs(template: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let inner = after[..close].trim();
        // Strip pipe transforms — only the variable portion matters here.
        let var_part = inner.split('|').next().unwrap().trim();
        if let Some(name) = var_part.strip_prefix("path.") {
            // Strip any colon-arg form (path vars don't take colon args today, but be tolerant).
            let name = name.split(':').next().unwrap().trim();
            out.insert(name.to_string());
        }
        rest = &after[close + 2..];
    }
    out
}

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
}
