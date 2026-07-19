//! Path-glob matching for the `files.ignore` list.
//!
//! A lightweight segment-recursive matcher: `*` matches within one path segment,
//! `**` spans segments. This is the graph's own ignore matcher and is
//! deliberately distinct from the validate-rule [`PathPattern`](crate::standards::PathPattern)
//! machinery — the two answer different questions and must not be conflated, since
//! swapping matchers would silently change which files the graph excludes.

use camino::Utf8Path;

pub fn pattern_matches_path(pattern: &str, path: &Utf8Path) -> bool {
    let pattern = pattern.trim().trim_matches('/');
    if pattern.is_empty() {
        return false;
    }

    let path = path.as_str().trim_matches('/');
    let pattern_segments = pattern.split('/').collect::<Vec<_>>();
    let path_segments = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect::<Vec<_>>()
    };

    pattern_segments_match(&pattern_segments, &path_segments)
}

fn pattern_segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            pattern_segments_match(&pattern[1..], path)
                || path
                    .get(1..)
                    .is_some_and(|remaining_path| pattern_segments_match(pattern, remaining_path))
        }
        (Some(_), None) => false,
        (Some(pattern_segment), Some(path_segment)) => {
            segment_matches(pattern_segment, path_segment)
                && pattern_segments_match(&pattern[1..], &path[1..])
        }
    }
}

fn segment_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if !pattern.contains('*') {
        return pattern == path;
    }

    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut remainder = path;

    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if index == 0 {
            let Some(stripped) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = stripped;
            continue;
        }

        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
    }

    pattern.ends_with('*') || remainder.is_empty()
}

#[cfg(test)]
mod pattern_match_tests {
    use super::pattern_matches_path;
    use camino::Utf8Path;

    #[test]
    fn single_star_does_not_cross_path_segments() {
        assert!(pattern_matches_path(
            "Workspaces/*/*.md",
            Utf8Path::new("Workspaces/app/root.md")
        ));
        assert!(!pattern_matches_path(
            "Workspaces/*/*.md",
            Utf8Path::new("Workspaces/app/agent-artifacts/nested.md")
        ));
    }

    #[test]
    fn double_star_matches_recursive_path_segments() {
        assert!(pattern_matches_path(
            "Workspaces/**/*.md",
            Utf8Path::new("Workspaces/app/root.md")
        ));
        assert!(pattern_matches_path(
            "Workspaces/**/*.md",
            Utf8Path::new("Workspaces/app/agent-artifacts/nested.md")
        ));
    }

    #[test]
    fn double_star_can_anchor_later_segments() {
        assert!(pattern_matches_path(
            "Workspaces/**/notes/*.md",
            Utf8Path::new("Workspaces/app/notes/note.md")
        ));
        assert!(pattern_matches_path(
            "Workspaces/**/notes/*.md",
            Utf8Path::new("Workspaces/app/areas/notes/note.md")
        ));
        assert!(!pattern_matches_path(
            "Workspaces/**/notes/*.md",
            Utf8Path::new("Workspaces/app/notes/deep/note.md")
        ));
    }
}
