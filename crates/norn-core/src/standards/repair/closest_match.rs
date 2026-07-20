//! Closest-match suggestion algorithm for link-target-missing findings.
//!
//! Pipeline: normalize → score against stems → pick best (skip on tie) →
//! band by similarity.

/// A closest-match outcome for one broken link target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// Single best candidate, post-normalize identity. Safe to apply.
    High { candidate_stem: String },
    /// Single best candidate, small residual edit distance.
    Medium {
        candidate_stem: String,
        normalized_distance: usize,
    },
    /// Multiple candidates tied at the top similarity. Caller should route
    /// to skipped with the candidate list.
    Tied { candidate_stems: Vec<String> },
    /// No candidate above the medium threshold. Stays unsupported.
    NoMatch,
}

/// Normalize a string for closest-match comparison.
///
/// - lowercase (Unicode-aware via `str::to_lowercase`)
/// - ASCII whitespace (space, tab) and `_` → `-`
/// - collapse repeated `-`
/// - trim leading/trailing `-`
pub fn normalize_for_match(input: &str) -> String {
    let lowercased = input.to_lowercase();
    let mut out = String::with_capacity(lowercased.len());
    let mut prev_was_sep = true; // leading separators stripped by treating start as "sep"
    for ch in lowercased.chars() {
        let is_sep = ch == ' ' || ch == '\t' || ch == '_' || ch == '-';
        if is_sep {
            if !prev_was_sep {
                out.push('-');
            }
            prev_was_sep = true;
        } else {
            out.push(ch);
            prev_was_sep = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn lowercases_ascii() {
        assert_eq!(normalize_for_match("Norn Brand"), "norn-brand");
    }

    #[test]
    fn whitespace_to_hyphen() {
        assert_eq!(normalize_for_match("hello world"), "hello-world");
        assert_eq!(normalize_for_match("hello\tworld"), "hello-world");
    }

    #[test]
    fn underscore_to_hyphen() {
        assert_eq!(normalize_for_match("hello_world"), "hello-world");
    }

    #[test]
    fn collapses_repeated_separators() {
        assert_eq!(normalize_for_match("hello   world"), "hello-world");
        assert_eq!(normalize_for_match("hello___world"), "hello-world");
        assert_eq!(normalize_for_match("hello _ world"), "hello-world");
    }

    #[test]
    fn trims_leading_and_trailing_separators() {
        assert_eq!(normalize_for_match("  hello  "), "hello");
        assert_eq!(normalize_for_match("---hello---"), "hello");
    }

    #[test]
    fn empty_input() {
        assert_eq!(normalize_for_match(""), "");
        assert_eq!(normalize_for_match("   "), "");
    }

    #[test]
    fn unicode_lowercase() {
        assert_eq!(normalize_for_match("Über"), "über");
    }
}

/// Find the closest-match candidate for a broken target among a set of stems.
///
/// `medium_threshold` is the similarity ratio below which Medium → NoMatch
/// (eyeball default 0.7; tuned during atlas dogfood).
pub fn closest_match(
    broken_target: &str,
    candidate_stems: &[&str],
    medium_threshold: f64,
) -> MatchOutcome {
    let normalized_target = normalize_for_match(broken_target);
    if normalized_target.is_empty() || candidate_stems.is_empty() {
        return MatchOutcome::NoMatch;
    }

    // Score every candidate.
    let mut scored: Vec<(String, &str, f64, usize)> = candidate_stems
        .iter()
        .map(|stem| {
            let normalized_stem = normalize_for_match(stem);
            let distance = strsim::levenshtein(&normalized_target, &normalized_stem);
            let max_len = normalized_target
                .chars()
                .count()
                .max(normalized_stem.chars().count());
            let ratio = if max_len == 0 {
                0.0
            } else {
                1.0 - (distance as f64 / max_len as f64)
            };
            (normalized_stem, *stem, ratio, distance)
        })
        .collect();

    // Sort descending by ratio.
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let top_ratio = scored[0].2;

    // Threshold gate FIRST: if the top match doesn't clear the medium bar,
    // nothing else matters — return NoMatch. This prevents ties at the bottom
    // (e.g., 47 unrelated stems all scoring 0.10) from surfacing as candidates.
    if top_ratio < medium_threshold {
        return MatchOutcome::NoMatch;
    }

    // Top ratio meets threshold; now check for ties at the top.
    let tied: Vec<&str> = scored
        .iter()
        .take_while(|(_, _, ratio, _)| *ratio == top_ratio)
        .map(|(_, stem, _, _)| *stem)
        .collect();

    if tied.len() > 1 {
        return MatchOutcome::Tied {
            candidate_stems: tied.into_iter().map(String::from).collect(),
        };
    }

    let (_, candidate_stem, ratio, distance) = &scored[0];

    if (*ratio - 1.0).abs() < f64::EPSILON {
        MatchOutcome::High {
            candidate_stem: (*candidate_stem).to_string(),
        }
    } else {
        MatchOutcome::Medium {
            candidate_stem: (*candidate_stem).to_string(),
            normalized_distance: *distance,
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    #[test]
    fn high_confidence_on_normalize_identity() {
        let result = closest_match("Norn Brand", &["norn-brand", "other-doc"], 0.7);
        assert_eq!(
            result,
            MatchOutcome::High {
                candidate_stem: "norn-brand".to_string(),
            }
        );
    }

    #[test]
    fn medium_confidence_on_small_edit_distance() {
        // "norn-brnd" vs "norn-brand" — one insertion, ratio = 1 - 1/10 = 0.9
        let result = closest_match("norn-brnd", &["norn-brand", "other-doc"], 0.7);
        match result {
            MatchOutcome::Medium {
                candidate_stem,
                normalized_distance,
            } => {
                assert_eq!(candidate_stem, "norn-brand");
                assert_eq!(normalized_distance, 1);
            }
            other => panic!("expected Medium, got {other:?}"),
        }
    }

    #[test]
    fn no_match_below_threshold() {
        let result = closest_match("xyzzy", &["norn-brand", "vault-memory"], 0.7);
        assert_eq!(result, MatchOutcome::NoMatch);
    }

    #[test]
    fn tied_candidates_at_normalize_identity() {
        // Two stems that normalize to the same string -> tied.
        let result = closest_match("Norn Brand", &["norn-brand", "Norn-Brand"], 0.7);
        match result {
            MatchOutcome::Tied { candidate_stems } => {
                assert_eq!(candidate_stems.len(), 2);
                assert!(candidate_stems.iter().any(|s| s == "norn-brand"));
                assert!(candidate_stems.iter().any(|s| s == "Norn-Brand"));
            }
            other => panic!("expected Tied, got {other:?}"),
        }
    }

    #[test]
    fn empty_candidate_pool() {
        let result = closest_match("anything", &[], 0.7);
        assert_eq!(result, MatchOutcome::NoMatch);
    }

    #[test]
    fn empty_broken_target() {
        let result = closest_match("", &["norn-brand"], 0.7);
        assert_eq!(result, MatchOutcome::NoMatch);
    }

    #[test]
    fn high_beats_medium_runner_up() {
        // Top candidate is post-normalize identity; second is a typo-near-miss.
        // Ties only count at the TOP ratio, so the result is High (not Tied).
        let result = closest_match("norn-brand", &["norn-brand", "norn-brnd"], 0.7);
        assert_eq!(
            result,
            MatchOutcome::High {
                candidate_stem: "norn-brand".to_string(),
            }
        );
    }

    #[test]
    fn no_match_when_all_candidates_tie_below_threshold() {
        // "xyzzy" is far from every candidate. Multiple stems score equally
        // low — the algorithm must NOT return Tied (which would surface them
        // as candidates in a SkippedFinding with Ambiguous). The threshold
        // gate should reject all of them and return NoMatch.
        let result = closest_match("xyzzy", &["norn-brand", "vault-memory", "other-doc"], 0.7);
        assert_eq!(result, MatchOutcome::NoMatch);
    }
}
