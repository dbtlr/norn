//! Path pattern matching with named-variable captures.
//!
//! Supports glob-style wildcards (`*`, `**`, `?`), glob alternation (`{a,b,c}` —
//! single-brace, comma-separated), and named single-segment captures
//! (`{{name}}` — double-brace).
//!
//! Backed by `regex` internally: patterns are translated to a regex with
//! `(?P<name>[^/]+)` for captures, `[^/]*` for `*`, `.*` for `**`, `[^/]` for `?`,
//! and `(?:a|b|c)` for glob alternation. Other characters are regex-escaped.

use regex::Regex;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum PathPatternError {
    #[error("unclosed `{{{{` in path pattern at byte {0}")]
    UnclosedBrace(usize),
    #[error("invalid regex generated from path pattern: {0}")]
    InvalidRegex(String),
}

/// Parsed path pattern. Use [`PathPattern::parse`] to build from a glob/template
/// string, then [`PathPattern::match_path`] to test against a path.
#[derive(Debug, Clone)]
pub struct PathPattern {
    regex: Regex,
    declared_vars: Vec<String>,
}

impl PathPattern {
    pub fn parse(pattern: &str) -> Result<Self, PathPatternError> {
        // Strip a single leading `/` to match the legacy matcher's normalization.
        // This lets patterns like `/Archive/**` work identically to `Archive/**`.
        // Trailing slashes are intentionally NOT stripped (patterns don't end in `/`
        // for file-matching; stripping could mask user errors).
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);

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
            if i + 2 < bytes.len()
                && bytes[i] == b'*'
                && bytes[i + 1] == b'*'
                && bytes[i + 2] == b'/'
            {
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
        Ok(Self {
            regex,
            declared_vars: declared,
        })
    }

    /// Try to match the path; on success, return the captured variables.
    pub fn match_path(&self, path: &str) -> Option<BTreeMap<String, String>> {
        let caps = self.regex.captures(path)?;
        let mut out = BTreeMap::new();
        for name in &self.declared_vars {
            if let Some(m) = caps.name(name) {
                out.insert(name.clone(), m.as_str().to_string());
            }
        }
        Some(out)
    }

    /// The list of named variables declared by `{{name}}` in the pattern.
    /// Each unique name is listed once, in first-occurrence order.
    pub fn declared_variables(&self) -> Vec<String> {
        self.declared_vars.clone()
    }
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
            "Workspaces/vault-cli/tasks/foo.md",
        )
        .unwrap();
        assert_eq!(caps.get("workspace"), Some(&"vault-cli".to_string()));
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
            "Workspaces/vault-cli/sub/tasks/foo.md",
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
}
