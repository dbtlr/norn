//! How LARGE an observed divergence is, in regions.
//!
//! A ledger entry cites the cases it covers, and citation alone is what
//! makes a differing case pass. Citation says nothing about WHAT differs, so
//! a divergence that grows — another change landing on the same surface —
//! keeps passing under an entry whose `old`/`new` text describes only the
//! original difference. The extent computed here is the entry's declared
//! `observed` count: a region appearing or disappearing fails the run and
//! sends the author back to the diff.
//!
//! A **region** is one maximal run of differing lines in a line-level diff of
//! one stream, plus one for an exit-code difference, one per post-mutation
//! tree path that differs, and — for an MCP case — one per differing response
//! frame, extra response id, and duplicate response id. Counting regions
//! rather than hashing bytes keeps the number stable under noise that does
//! not change the shape of the divergence.

use crate::mcp::McpDivergence;
use crate::normalize::NormalizedOutput;
use crate::poststate::PostStateDiff;

/// The largest LCS table this counts exactly. Beyond it the changed span is
/// reported as one region: an exact count of a diff that large tells an
/// author nothing they could act on, and the quadratic table would dominate
/// the run.
const MAX_LCS_CELLS: usize = 1_000_000;

/// The number of maximal runs of differing lines between `oracle` and
/// `candidate`. Identical text is 0 regions; one contiguous edit is 1,
/// however many lines it spans; edits separated by at least one common line
/// count separately.
pub fn changed_regions(oracle: &str, candidate: &str) -> usize {
    let a: Vec<&str> = oracle.lines().collect();
    let b: Vec<&str> = candidate.lines().collect();

    // Common prefix and suffix are outside every region by definition, and
    // dropping them keeps the LCS table small for the usual localized diff.
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    let a = &a[head..a.len() - tail];
    let b = &b[head..b.len() - tail];

    match (a.is_empty(), b.is_empty()) {
        (true, true) => return 0,
        // One side is pure insertion or pure deletion: a single run.
        (true, false) | (false, true) => return 1,
        (false, false) => {}
    }
    if a.len().saturating_mul(b.len()) > MAX_LCS_CELLS {
        return 1;
    }

    // lcs[i][j] = length of the longest common subsequence of a[i..], b[j..].
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut regions = 0;
    let mut in_region = false;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            in_region = false;
            i += 1;
            j += 1;
            continue;
        }
        if !in_region {
            regions += 1;
            in_region = true;
        }
        if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Whatever remains on either side is one more run unless it continues the
    // region the loop was already inside.
    if (i < a.len() || j < b.len()) && !in_region {
        regions += 1;
    }
    regions
}

/// The extent of an ordinary (argv/stdout/stderr) case: differing regions
/// across both streams, the exit code, and — for a mutating case — the
/// post-mutation vault tree.
pub fn output_extent(
    oracle: &NormalizedOutput,
    candidate: &NormalizedOutput,
    post_state: Option<&PostStateDiff>,
) -> usize {
    let mut extent = changed_regions(&oracle.stdout, &candidate.stdout)
        + changed_regions(&oracle.stderr, &candidate.stderr);
    if oracle.exit_code != candidate.exit_code {
        extent += 1;
    }
    if let Some(diff) = post_state {
        extent +=
            diff.only_in_oracle.len() + diff.only_in_candidate.len() + diff.content_differs.len();
    }
    extent
}

/// The extent of an MCP case: one region per differing response frame, per
/// unsolicited response id, per duplicate response id, and one for a process
/// exit-code mismatch.
pub fn mcp_extent(divergence: &McpDivergence) -> usize {
    divergence.diffs.len()
        + divergence.extra_response_ids.len()
        + divergence.duplicate_response_ids.len()
        + usize::from(divergence.exit_mismatch.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_regions() {
        assert_eq!(changed_regions("a\nb\nc\n", "a\nb\nc\n"), 0);
    }

    #[test]
    fn one_changed_line_is_one_region() {
        assert_eq!(changed_regions("a\nb\nc\n", "a\nB\nc\n"), 1);
    }

    #[test]
    fn adjacent_changed_lines_are_one_region() {
        assert_eq!(changed_regions("a\nb\nc\nd\n", "a\nB\nC\nd\n"), 1);
    }

    #[test]
    fn changes_separated_by_a_common_line_are_two_regions() {
        assert_eq!(changed_regions("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n"), 2);
    }

    #[test]
    fn a_deleted_block_is_one_region() {
        assert_eq!(changed_regions("a\nb\nc\nd\n", "a\nd\n"), 1);
    }

    #[test]
    fn a_pure_insertion_at_the_end_is_one_region() {
        assert_eq!(changed_regions("a\nb\n", "a\nb\nc\nd\n"), 1);
    }

    #[test]
    fn empty_against_content_is_one_region() {
        assert_eq!(changed_regions("", "a\nb\n"), 1);
        assert_eq!(changed_regions("a\nb\n", ""), 1);
    }

    #[test]
    fn separate_deletion_and_insertion_count_separately() {
        // `b` is dropped; `x` is added three lines later.
        assert_eq!(changed_regions("a\nb\nc\nd\ne\n", "a\nc\nd\nx\ne\n"), 2);
    }
}
