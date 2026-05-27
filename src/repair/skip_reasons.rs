//! Glob matching for `--skip-reason` filter values.
//!
//! The codes themselves live on `crate::standards::repair::SkipReason::code()`.
//! This module holds only the matching/filter glue specific to the CLI surface.

use globset::Glob;

/// Returns the user-facing prose for a stable skip-reason code.
///
/// This is the single point of evolution for skip-reason prose; if a new code is
/// added in the future to `crate::standards::repair::SkipReason::code()`, add the
/// matching arm here.
pub fn prose_for(code: &str) -> &'static str {
    match code {
        "missing-default" => "missing field has no configured deterministic default",
        "link-decision-needed" => "link repair requires an explicit path/link decision",
        "no-rule-matched" => "no configured deterministic repair rule matched",
        "alias-shadowed" => "alias shadowed by a doc stem cannot be repaired deterministically",
        "graph-diagnostic" => "graph diagnostic cannot be repaired deterministically",
        "ambiguous-target" => "ambiguous link target",
        "missing-hash" => "index missing hash for finding's path",
        "precondition-failed" => "rule precondition blocked producing a change",
        _ => "(unknown skip reason)",
    }
}

/// True if `code` matches any of the supplied patterns. Empty pattern list = no filter (matches all).
/// Patterns may be exact strings (`"missing-default"`) or glob patterns (`"link-*"`).
pub fn code_matches_any(code: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| pattern_matches(code, p))
}

fn pattern_matches(code: &str, pattern: &str) -> bool {
    match Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(code),
        Err(_) => code == pattern, // malformed glob falls back to exact match
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patterns_match_all() {
        assert!(code_matches_any("missing-default", &[]));
        assert!(code_matches_any("anything", &[]));
    }

    #[test]
    fn exact_string_matches() {
        let patterns = vec!["missing-default".to_string()];
        assert!(code_matches_any("missing-default", &patterns));
        assert!(!code_matches_any("link-decision-needed", &patterns));
    }

    #[test]
    fn glob_pattern_matches() {
        let patterns = vec!["link-*".to_string()];
        assert!(code_matches_any("link-decision-needed", &patterns));
        assert!(!code_matches_any("missing-default", &patterns));
    }

    #[test]
    fn multiple_patterns_or_together() {
        let patterns = vec!["missing-*".to_string(), "ambiguous-*".to_string()];
        assert!(code_matches_any("missing-default", &patterns));
        assert!(code_matches_any("ambiguous-target", &patterns));
        assert!(!code_matches_any("no-rule-matched", &patterns));
    }
}
