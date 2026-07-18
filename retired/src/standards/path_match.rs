//! Path pattern matching with named-variable captures.
//!
//! Supports glob-style wildcards (`*`, `**`, `?`), glob alternation (`{a,b,c}` —
//! single-brace, comma-separated), and named single-segment captures
//! (`{{name}}` — double-brace).
//!
//! **Two-tier matching:**
//! - Patterns with no named captures (`{{name}}`) compile to a `globset::GlobMatcher`
//!   for boolean matching. Globset is DFA-backed and significantly faster for the
//!   common case (pure glob patterns without capture variables).
//! - Patterns with named captures additionally compile a `regex::Regex` for capture
//!   extraction. The glob fast-path is still used for boolean matching; the regex
//!   only runs when captures need to be extracted.
//!
//! The regex translation: `[^/]*` for `*`, `.*` for `**`, `[^/]` for `?`,
//! `(?:a|b|c)` for glob alternation, `(?P<name>[^/]+)` for captures.

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum PathPatternError {
    #[error("unclosed `{{{{` in path pattern at byte {0}")]
    UnclosedBrace(usize),
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(String),
    #[error("invalid regex generated from path pattern: {0}")]
    InvalidRegex(String),
}

/// Parsed path pattern. Use [`PathPattern::parse`] to build from a glob/template
/// string, then [`PathPattern::match_path`] to test against a path.
#[derive(Debug, Clone)]
pub struct PathPattern {
    /// Globset matcher for fast boolean matching (always present).
    /// For patterns with named captures, `{{name}}` is replaced with `*` in the
    /// glob so the shape still matches; the regex then extracts the actual captures.
    glob: GlobMatcher,
    /// Regex for capture extraction. Only present when the pattern has named
    /// captures (`{{name}}`); `None` for pure-glob patterns.
    regex: Option<Regex>,
    declared_vars: Vec<String>,
}

impl PathPattern {
    pub fn parse(pattern: &str) -> Result<Self, PathPatternError> {
        // Strip a single leading `/` to match the legacy matcher's normalization.
        // This lets patterns like `/Archive/**` work identically to `Archive/**`.
        // Trailing slashes are intentionally NOT stripped (patterns don't end in `/`
        // for file-matching; stripping could mask user errors).
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);

        // Detect named captures. The globset fast-path handles pure-glob patterns;
        // the regex is only built when `{{name}}` captures are present.
        let has_named_captures = pattern.contains("{{");

        // Build the glob pattern: named captures become `*` (single-segment wildcard)
        // so globset can match the structural shape of the path.
        let glob_pattern = if has_named_captures {
            replace_captures_with_glob_star(pattern)?
        } else {
            // No named captures; validate by parsing directly.
            pattern.to_string()
        };

        // Build globset with literal_separator=true so that `*` does NOT match
        // path separators (`/`), matching the same semantics as our regex `[^/]*`.
        // `**` continues to match across path separators (globset's default for `**`).
        let glob = GlobBuilder::new(&glob_pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| PathPatternError::InvalidGlob(e.to_string()))?
            .compile_matcher();

        // Build the regex only when named captures are needed.
        let (regex, declared) = if has_named_captures {
            let (rx, decl) = build_regex_with_captures(pattern)?;
            (Some(rx), decl)
        } else {
            (None, Vec::new())
        };

        Ok(Self {
            glob,
            regex,
            declared_vars: declared,
        })
    }

    /// Try to match the path; on success, return the captured variables.
    ///
    /// For pure-glob patterns (no `{{name}}`), uses the fast globset path and
    /// returns an empty map on success. For patterns with named captures, also
    /// runs the regex to extract variable values.
    pub fn match_path(&self, path: &str) -> Option<BTreeMap<String, String>> {
        if !self.glob.is_match(path) {
            return None;
        }
        if let Some(regex) = &self.regex {
            // Named captures present — extract them via the regex.
            let caps = regex.captures(path)?;
            let mut out = BTreeMap::new();
            for name in &self.declared_vars {
                if let Some(m) = caps.name(name) {
                    out.insert(name.clone(), m.as_str().to_string());
                }
            }
            Some(out)
        } else {
            // Pure-glob pattern — glob already matched, return empty map.
            Some(BTreeMap::new())
        }
    }

    /// The list of named variables declared by `{{name}}` in the pattern.
    /// Each unique name is listed once, in first-occurrence order.
    pub fn declared_variables(&self) -> Vec<String> {
        self.declared_vars.clone()
    }
}

/// Replace `{{name}}` captures with `*` in a glob pattern string.
/// Used to build the globset fast-path matcher for patterns with named captures.
fn replace_captures_with_glob_star(pattern: &str) -> Result<String, PathPatternError> {
    let mut out = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let end = pattern[i + 2..]
                .find("}}")
                .ok_or(PathPatternError::UnclosedBrace(i))?;
            // Replace `{{name}}` with `*` (single-segment wildcard in globset).
            out.push('*');
            i += end + 4;
        } else {
            let ch = pattern[i..]
                .chars()
                .next()
                .expect("non-empty by loop guard");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// Build a regex string from a pattern containing `{{name}}` captures.
/// Returns `(Regex, Vec<declared_var_names>)`.
fn build_regex_with_captures(pattern: &str) -> Result<(Regex, Vec<String>), PathPatternError> {
    let mut declared = Vec::new();
    let mut regex_str = String::from("^");
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `{{name}}` named capture (double-brace)
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let end = pattern[i + 2..]
                .find("}}")
                .ok_or(PathPatternError::UnclosedBrace(i))?;
            let name = pattern[i + 2..i + 2 + end].trim();
            if declared.contains(&name.to_string()) {
                // Duplicate: use a non-capturing group (regex forbids dup named groups)
                regex_str.push_str("[^/]+");
            } else {
                regex_str.push_str(&format!("(?P<{name}>[^/]+)"));
                declared.push(name.to_string());
            }
            i += end + 4;
            continue;
        }
        // `{a,b,c}` glob alternation (single-brace) → `(?:a|b|c)`
        if bytes[i] == b'{' {
            if let Some(end) = pattern[i + 1..].find('}') {
                let body = &pattern[i + 1..i + 1 + end];
                let alt = body
                    .split(',')
                    .map(|p| regex::escape(p.trim()))
                    .collect::<Vec<_>>()
                    .join("|");
                regex_str.push_str(&format!("(?:{alt})"));
                i += end + 2;
                continue;
            }
        }
        // `**/` → `(?:.*/)?` — matches any path prefix (including empty)
        if i + 2 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' && bytes[i + 2] == b'/' {
            regex_str.push_str("(?:.*/)?");
            i += 3;
            continue;
        }
        // `**` (at end or not followed by `/`) → `.*`
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            regex_str.push_str(".*");
            i += 2;
            continue;
        }
        // `*` → `[^/]*`
        if bytes[i] == b'*' {
            regex_str.push_str("[^/]*");
            i += 1;
            continue;
        }
        // `?` → `[^/]`
        if bytes[i] == b'?' {
            regex_str.push_str("[^/]");
            i += 1;
            continue;
        }
        // Literal char (UTF-8 safe, regex-escape)
        let ch = pattern[i..]
            .chars()
            .next()
            .expect("non-empty by loop guard");
        regex_str.push_str(&regex::escape(&ch.to_string()));
        i += ch.len_utf8();
    }
    regex_str.push('$');

    let regex =
        Regex::new(&regex_str).map_err(|e| PathPatternError::InvalidRegex(e.to_string()))?;
    Ok((regex, declared))
}

/// Return the effective path glob for a validate rule.
///
/// For a conventional rule (`match.path` present), that glob is returned as-is.
/// For a creatable rule (`match.path` absent, `target` present), the glob is
/// derived from the target template via [`glob_from_target`].  When neither is
/// set, `None` is returned (the rule is path-unconstrained).
///
/// This is the single, canonical derivation used by both `engine::rule_matches`
/// and the conflict-check in `config::post_validate` — keep the logic here, not
/// inlined in callers.
pub fn effective_match_glob(match_path: Option<&str>, target: Option<&str>) -> Option<String> {
    match_path
        .map(|p| p.to_string())
        .or_else(|| target.and_then(|t| glob_from_target(t).ok()))
}

/// Lower a generative `target` template into a glob string.
///
/// Bare `{{var.NAME}}` / `{{path.NAME}}` segments become named captures
/// `{{NAME}}`; any token carrying a transform (`|`) or not matching the
/// `var.`/`path.` prefix becomes `*`. Literal segments are preserved.
///
/// Named captures are only emitted when they occupy a full path segment
/// (i.e. the token spans the whole space between two `/` boundaries). If a
/// `{{var.NAME}}` token is embedded in a larger segment (e.g. `prefix-{{var.x}}.md`),
/// it is lowered to `*` to avoid producing an invalid glob.
///
/// This is the string-form lowering; use [`pattern_from_target`] to get a
/// compiled [`PathPattern`] ready for matching.
pub fn glob_from_target(target: &str) -> Result<String, PathPatternError> {
    let mut glob = String::new();
    let mut rest = target;
    while let Some(start) = rest.find("{{") {
        let literal = &rest[..start];
        glob.push_str(literal);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or(PathPatternError::UnclosedBrace(glob.len() + start))?;
        let inner = after[..end].trim();
        // Determine if the token occupies a full path segment:
        // the preceding literal ends at '/' (or is empty) and the
        // remainder after '}}' starts with '/' (or is empty / just '.md').
        let preceded_by_sep = literal.is_empty() || literal.ends_with('/');
        let suffix = &after[end + 2..];
        let followed_by_sep = suffix.is_empty() || suffix.starts_with('/');
        let full_segment = preceded_by_sep && followed_by_sep;
        let lowered = lower_token(inner, full_segment);
        glob.push_str(&lowered);
        rest = suffix;
    }
    glob.push_str(rest);
    Ok(glob)
}

/// Lower a generative `target` template into a compiled [`PathPattern`].
///
/// Delegates the glob-string lowering to [`glob_from_target`] and then
/// parses the result into a [`PathPattern`]. Behavior is unchanged from the
/// previous monolithic implementation.
pub fn pattern_from_target(target: &str) -> Result<PathPattern, PathPatternError> {
    PathPattern::parse(&glob_from_target(target)?)
}

fn lower_token(inner: &str, full_segment: bool) -> String {
    // Transform present → wildcard regardless of position.
    if inner.contains('|') {
        return "*".to_string();
    }
    if full_segment {
        let name = inner
            .strip_prefix("var.")
            .or_else(|| inner.strip_prefix("path."));
        match name {
            Some(n) if !n.is_empty() => return format!("{{{{{n}}}}}"),
            _ => {}
        }
    }
    "*".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn captures(pattern: &str, path: &str) -> Option<BTreeMap<String, String>> {
        PathPattern::parse(pattern).unwrap().match_path(path)
    }

    #[test]
    fn matches_plain_glob() {
        assert!(captures("**/*.md", "Workspaces/foo/notes/bar.md").is_some());
        assert!(captures("*.md", "foo.md").is_some());
        assert!(captures("*.md", "subdir/foo.md").is_none());
    }

    #[test]
    fn captures_single_named_variable() {
        let caps = captures(
            "Workspaces/{{workspace}}/tasks/*.md",
            "Workspaces/norn/tasks/foo.md",
        )
        .unwrap();
        assert_eq!(caps.get("workspace"), Some(&"norn".to_string()));
    }

    #[test]
    fn captures_multiple_named_variables() {
        let caps = captures("Log/{{year}}/{{month}}/*.md", "Log/2026/05/foo.md").unwrap();
        assert_eq!(caps.get("year"), Some(&"2026".to_string()));
        assert_eq!(caps.get("month"), Some(&"05".to_string()));
    }

    #[test]
    fn capture_does_not_match_slash() {
        // {{name}} matches a single segment; should not match across '/'.
        assert!(captures(
            "Workspaces/{{workspace}}/tasks/*.md",
            "Workspaces/norn/sub/tasks/foo.md",
        )
        .is_none());
    }

    #[test]
    fn glob_alternation_braces_untouched() {
        // {note,task} is glob alternation; not a path variable.
        assert!(captures("**/*.{note,task}.md", "foo.task.md").is_some());
        assert!(captures("**/*.{note,task}.md", "foo.other.md").is_none());
    }

    #[test]
    fn declared_path_variables_listed() {
        let parsed = PathPattern::parse("Workspaces/{{workspace}}/tasks/*.md").unwrap();
        assert_eq!(parsed.declared_variables(), vec!["workspace".to_string()]);
    }

    #[test]
    fn declared_variables_distinct_when_repeated() {
        // Same name twice — declared once.
        let parsed = PathPattern::parse("{{w}}/{{w}}/foo.md").unwrap();
        assert_eq!(parsed.declared_variables(), vec!["w".to_string()]);
    }

    #[test]
    fn parse_rejects_unclosed_brace() {
        assert!(PathPattern::parse("Workspaces/{{workspace/foo.md").is_err());
    }

    #[test]
    fn leading_slash_normalized() {
        let p = PathPattern::parse("/Archive/**").unwrap();
        assert!(p.match_path("Archive/foo.md").is_some());
        assert!(p.match_path("Archive/sub/foo.md").is_some());
        assert!(p.match_path("Other/foo.md").is_none());
    }

    #[test]
    fn lowers_target_to_glob_pattern() {
        let p =
            pattern_from_target("Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md").unwrap();
        // captures workspace from a generated path; filename is a wildcard
        let caps = p
            .match_path("Workspaces/norn/tasks/fix-the-thing.md")
            .expect("matches");
        assert_eq!(caps.get("workspace").map(String::as_str), Some("norn"));
    }

    #[test]
    fn transformed_or_filename_holes_become_wildcards() {
        let p = pattern_from_target("notes/{{date}}/{{title|slugify}}.md").unwrap();
        assert!(p.match_path("notes/2026-06-23/some-note.md").is_some());
        assert!(p.match_path("notes/anything/whatever.md").is_some());
    }

    #[test]
    fn round_trip_generated_path_matches_derived_pattern() {
        // property: a path generated from the target always matches the derived glob
        let target = "Workspaces/{{var.workspace}}/notes/{{title|slugify}}.md";
        let p = pattern_from_target(target).unwrap();
        // generated by Task 4's generate_path with workspace=norn, title="My Note"
        assert!(p.match_path("Workspaces/norn/notes/my-note.md").is_some());
    }

    #[test]
    fn glob_from_target_produces_glob_string() {
        // var.NAME → named capture {{NAME}}; transform → wildcard *; literals preserved.
        let glob =
            glob_from_target("Workspaces/{{var.workspace}}/notes/{{title|slugify}}.md").unwrap();
        assert_eq!(glob, "Workspaces/{{workspace}}/notes/*.md");
    }

    #[test]
    fn glob_from_target_consistent_with_pattern_from_target() {
        // pattern_from_target must produce the same match behaviour as
        // PathPattern::parse(glob_from_target(target)).
        let target = "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md";
        let from_glob = PathPattern::parse(&glob_from_target(target).unwrap()).unwrap();
        let from_pattern = pattern_from_target(target).unwrap();
        let path = "Workspaces/norn/tasks/fix-the-thing.md";
        assert_eq!(
            from_glob.match_path(path).is_some(),
            from_pattern.match_path(path).is_some(),
            "glob_from_target and pattern_from_target must agree on match"
        );
    }
}
