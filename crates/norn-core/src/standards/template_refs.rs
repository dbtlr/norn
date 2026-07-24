//! Template-reference scanning for config-load validation.
//!
//! The `frontmatter_defaults`
//! config checks need to see which `{{path.X}}` variables and which `{{… | t}}`
//! transforms a template references, without rendering it. The rendering engine
//! itself (`substitution.rs`) and the match-to-defaults resolver
//! (`applicable_rules` / `merge_defaults` / `resolve_to_fixpoint`) are the
//! mutation-verb port and are deliberately absent here.
//!
//! [`is_known_transform`] answers "does the renderer recognize this transform
//! name" directly off [`crate::standards::substitution::TRANSFORMS`] — the
//! renderer's dispatch table is the single source; this module keeps no
//! transform-name list of its own to drift out of sync with it.

use std::collections::BTreeSet;

/// Iterate `{{…}}` substitution groups in a template, yielding the inner
/// expression (trimmed) for each. Quad-brace escapes (`{{{{` / `}}}}`) are
/// skipped — they render as literal `{{`/`}}` and don't contain a real var.
///
/// Shared by [`collect_path_var_refs`] and [`collect_transform_refs`] so both
/// helpers agree with the runtime renderer about what counts as a substitution
/// group.
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

/// Whether `name` is a transform the renderer recognizes — read straight off
/// [`crate::standards::substitution::TRANSFORMS`], so a transform added or
/// removed there is reflected here with no second list to update (NRN-419).
pub(crate) fn is_known_transform(name: &str) -> bool {
    super::substitution::TRANSFORMS
        .iter()
        .any(|(candidate, _)| *candidate == name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
