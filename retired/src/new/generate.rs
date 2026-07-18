//! Forward path generation — render a concrete document path from a rule's
//! `target` template plus caller-supplied inputs.

use std::collections::BTreeMap;

use crate::standards::{substitution, VaultConfig};

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum GeneratePathError {
    #[error("missing required template variable `{name}` (supply with --var {name}=...)")]
    MissingVar { name: String },
    #[error("this target needs a title (supply with --title)")]
    MissingTitle,
    #[error("template error: {0}")]
    Render(String),
    #[error("`{{{{seq}}}}` is only supported once, in the file name of a rule target")]
    SeqPlacement,
}

impl GeneratePathError {
    /// The stable, machine-branchable kebab code for this refusal (NRN-230), so
    /// an MCP `vault.new` consumer branches on the code — `missing-var`,
    /// `missing-title`, … — rather than the prose. `Display` is unchanged
    /// (byte-identical CLI/stderr output); the code rides alongside via
    /// [`NewResolveError::GeneratePath`](crate::new::NewResolveError::GeneratePath)'s
    /// transparent delegation.
    pub fn code(&self) -> &'static str {
        match self {
            GeneratePathError::MissingVar { .. } => "missing-var",
            GeneratePathError::MissingTitle => "missing-title",
            GeneratePathError::Render(_) => "template-render-failed",
            GeneratePathError::SeqPlacement => "seq-misplaced",
        }
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

pub struct GenerateInputs<'a> {
    pub title: Option<&'a str>,
    pub vars: &'a BTreeMap<String, String>,
}

// ── Scanning helpers ──────────────────────────────────────────────────────────

/// Collect the base name of every `{{ ... }}` token in `target` (before any
/// `|` transform, before any namespace prefix). E.g. `{{title|slugify}}` →
/// `"title"`, `{{var.workspace}}` → `"var.workspace"`.
fn referenced_tokens(target: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = target;
    while let Some(s) = rest.find("{{") {
        let after = &rest[s + 2..];
        let Some(e) = after.find("}}") else { break };
        let inner = after[..e].split('|').next().unwrap_or("").trim();
        if !inner.is_empty() && !out.contains(&inner.to_string()) {
            out.push(inner.to_string());
        }
        rest = &after[e + 2..];
    }
    out
}

/// Return the names of `var.` / `path.` variables referenced by `target`.
///
/// E.g. `"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"` →
/// `["workspace"]`.
pub fn referenced_vars(target: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in referenced_tokens(target) {
        if let Some(n) = token
            .strip_prefix("var.")
            .or_else(|| token.strip_prefix("path."))
        {
            if !n.is_empty() && !out.contains(&n.to_string()) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// Returns `true` if `target` references the `title` variable (bare or with
/// transforms, e.g. `{{title|slugify}}`).
fn references_title(target: &str) -> bool {
    referenced_tokens(target).iter().any(|t| t == "title")
}

// ── Core function ─────────────────────────────────────────────────────────────

/// Render `target` with `inputs` against `cfg`'s template settings.
///
/// Errors:
/// - [`GeneratePathError::MissingVar`] — a `var.NAME` / `path.NAME` referenced
///   in `target` is absent from `inputs.vars`.
/// - [`GeneratePathError::MissingTitle`] — `target` references `{{title}}` but
///   `inputs.title` is `None`.
/// - [`GeneratePathError::Render`] — the substitution engine returned an error.
pub fn generate_path(
    target: &str,
    inputs: &GenerateInputs,
    cfg: &VaultConfig,
) -> Result<String, GeneratePathError> {
    use chrono::Local;

    // Guard: all referenced vars must be present.
    for name in referenced_vars(target) {
        if !inputs.vars.contains_key(&name) {
            return Err(GeneratePathError::MissingVar { name });
        }
    }

    // Guard: title must be supplied when the template references it.
    if references_title(target) && inputs.title.is_none() {
        return Err(GeneratePathError::MissingTitle);
    }

    let ctx = substitution::Context {
        now: Local::now().naive_local(),
        title: inputs.title.unwrap_or("").to_string(),
        path_vars: inputs.vars.clone(),
        date_format: cfg.templates.date_format.clone(),
        time_format: cfg.templates.time_format.clone(),
    };

    // `{{seq}}` is an incremental-path token (NRN-101): resolved at APPLY time via
    // filesystem max+1 under the mutation lock, not here. Shield it from the
    // substitution engine (which would reject the unknown `seq` variable) so it
    // survives rendering as a literal `{{seq}}` in the emitted path template.
    let protected = target.replace(SEQ_TOKEN, SEQ_SENTINEL);
    let rendered = substitution::render(&protected, &ctx)
        .map_err(|e| GeneratePathError::Render(e.to_string()))?;
    let out = rendered.replace(SEQ_SENTINEL, SEQ_TOKEN);
    // Reject an unresolvable `{{seq}}` (in a directory component, or more than
    // once) here at plan time so dry-run and apply agree — rather than letting a
    // dry-run preview succeed and only refusing at apply (NRN-101).
    if crate::seq_alloc::seq_misplaced(camino::Utf8Path::new(&out)) {
        return Err(GeneratePathError::SeqPlacement);
    }
    Ok(out)
}

/// The incremental-path token, resolved to the next id at apply time.
pub const SEQ_TOKEN: &str = "{{seq}}";
/// Internal placeholder that survives the substitution engine untouched (NUL
/// bytes never occur in a template or a real path).
const SEQ_SENTINEL: &str = "\u{0}norn_seq\u{0}";

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_concrete_path_with_slugified_title() {
        let mut vars = BTreeMap::new();
        vars.insert("workspace".to_string(), "norn".to_string());
        let inputs = GenerateInputs {
            title: Some("Fix the audit reader"),
            vars: &vars,
        };
        let cfg = VaultConfig::default();
        let p = generate_path(
            "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md",
            &inputs,
            &cfg,
        )
        .unwrap();
        assert_eq!(p, "Workspaces/norn/tasks/fix-the-audit-reader.md");
    }

    #[test]
    fn missing_required_var_is_refused_by_name() {
        let vars = BTreeMap::new();
        let inputs = GenerateInputs {
            title: Some("X"),
            vars: &vars,
        };
        let cfg = VaultConfig::default();
        let err = generate_path(
            "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md",
            &inputs,
            &cfg,
        )
        .unwrap_err();
        assert!(
            matches!(err, GeneratePathError::MissingVar { ref name } if name == "workspace"),
            "got {err:?}"
        );
    }

    #[test]
    fn missing_title_is_refused() {
        let vars = BTreeMap::new();
        let inputs = GenerateInputs {
            title: None,
            vars: &vars,
        };
        let cfg = VaultConfig::default();
        let err = generate_path("notes/{{title|slugify}}.md", &inputs, &cfg).unwrap_err();
        assert!(
            matches!(err, GeneratePathError::MissingTitle),
            "got {err:?}"
        );
    }
}
