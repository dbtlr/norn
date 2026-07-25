//! How large an observed divergence is, per channel, in regions.
//!
//! A ledger entry cites the cases it covers, and citation alone is what makes
//! a differing case pass. Citation says nothing about WHAT differs, so a
//! divergence that grows — another change landing on the same surface — keeps
//! passing under an entry whose `old`/`new` text describes only the original
//! difference. The extent computed here is the entry's declared `observed`
//! count: a region appearing or disappearing fails the run and sends the
//! author back to the diff.
//!
//! A **region** is one maximal run of differing lines in a line-level diff of
//! one stream, plus one for an exit-code difference, one per post-mutation
//! tree path that differs, and — for an MCP case — one per differing response
//! frame, extra response id and duplicate response id. Counting regions
//! rather than hashing bytes keeps the number stable under noise that does
//! not change the shape of the divergence, and keeps a failure legible: a
//! count that moved names a region that appeared or vanished.
//!
//! What this is honest about:
//!
//! - it is a TRIPWIRE on the divergence's shape, not a measure of its
//!   meaning. Regions are counted per channel precisely so one region
//!   vanishing while another appears cannot cancel out — but a same-size
//!   reshaping WITHIN one channel (a region changing content, not count) is
//!   not caught, and nothing here reads what the entry's prose says;
//! - the count is not a quota of sentences. An entry must describe every
//!   CAUSE of its divergence; one cause can spray many regions (a reordered
//!   JSON array is one decision and eight regions) and needs one description.
//!   A count that moved is the signal to re-read the diff, not a demand for
//!   more prose;
//! - it is only meaningful from a run the host could not reach — see
//!   `crate::exec::SpawnEnv`, since a stray daemon line lands on `stderr` and
//!   inflates every case;
//! - it is PLATFORM-INVARIANT by contract. A count that differs between two
//!   platforms is a bug in the case or in its normalization, never a number
//!   to encode.

use crate::mcp::McpDivergence;
use crate::normalize::NormalizedOutput;
use crate::poststate::PostStateDiff;

/// The largest LCS table this counts exactly. Beyond it the changed span is
/// reported as one region: an exact count of a diff that large tells an
/// author nothing they could act on, and the quadratic table would dominate
/// the run.
const MAX_LCS_CELLS: usize = 1_000_000;

/// A divergence's size, split by the channel it appears on. Splitting is what
/// keeps the number a tripwire: a scalar total lets a region vanish from one
/// channel while another appears somewhere else, and reads as no change at
/// all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    pub stdout: usize,
    pub stderr: usize,
    pub exit: usize,
    pub tree: usize,
    pub mcp: usize,
}

impl Extent {
    /// The named channel's slot, for reading a declared extent out of TOML.
    /// `None` names no channel.
    pub fn channel_mut(&mut self, name: &str) -> Option<&mut usize> {
        match name {
            "stdout" => Some(&mut self.stdout),
            "stderr" => Some(&mut self.stderr),
            "exit" => Some(&mut self.exit),
            "tree" => Some(&mut self.tree),
            "mcp" => Some(&mut self.mcp),
            _ => None,
        }
    }

    /// Every channel with its count, in render order.
    pub fn channels(&self) -> [(&'static str, usize); 5] {
        [
            ("stdout", self.stdout),
            ("stderr", self.stderr),
            ("exit", self.exit),
            ("tree", self.tree),
            ("mcp", self.mcp),
        ]
    }

    pub fn total(&self) -> usize {
        self.channels().iter().map(|(_, n)| n).sum()
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    /// The TOML inline table this extent is written as — `{ stdout = 3 }`,
    /// omitting every zero channel, `{}` when nothing differs.
    pub fn render(&self) -> String {
        let body: Vec<String> = self
            .channels()
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name} = {n}"))
            .collect();
        if body.is_empty() {
            "{}".to_string()
        } else {
            format!("{{ {} }}", body.join(", "))
        }
    }
}

/// The number of differing regions between two versions of ONE stream.
///
/// Byte-identical text is 0. Anything else is at least 1: a difference the
/// line walk cannot see — a line terminator, a missing trailing newline — is
/// still a divergence, and an extent of 0 would let `observed = {}` cover it
/// and leave a pure-newline divergence unrecordable.
pub fn stream_regions(oracle: &str, candidate: &str) -> usize {
    if oracle == candidate {
        return 0;
    }
    changed_regions(oracle, candidate).max(1)
}

/// The number of maximal runs of differing LINES between `oracle` and
/// `candidate`. Identical lines are 0 regions; one contiguous edit is 1,
/// however many lines it spans; edits separated by at least one common line
/// count separately. Line terminators are not compared — [`stream_regions`]
/// is the function callers want.
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

/// The extent of an ordinary (argv/stdout/stderr) case: differing regions on
/// each stream, the exit code, and — for a mutating case — the post-mutation
/// vault tree.
pub fn output_extent(
    oracle: &NormalizedOutput,
    candidate: &NormalizedOutput,
    post_state: Option<&PostStateDiff>,
) -> Extent {
    Extent {
        stdout: stream_regions(&oracle.stdout, &candidate.stdout),
        stderr: stream_regions(&oracle.stderr, &candidate.stderr),
        exit: usize::from(oracle.exit_code != candidate.exit_code),
        tree: post_state.map_or(0, |diff| {
            diff.only_in_oracle.len() + diff.only_in_candidate.len() + diff.content_differs.len()
        }),
        mcp: 0,
    }
}

/// The extent of an MCP case: one region per differing response frame, per
/// unsolicited response id and per duplicate response id, plus the process
/// exit code on its own channel.
pub fn mcp_extent(divergence: &McpDivergence) -> Extent {
    Extent {
        mcp: divergence.diffs.len()
            + divergence.extra_response_ids.len()
            + divergence.duplicate_response_ids.len(),
        exit: usize::from(divergence.exit_mismatch.is_some()),
        ..Extent::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_regions() {
        assert_eq!(stream_regions("a\nb\nc\n", "a\nb\nc\n"), 0);
    }

    #[test]
    fn one_changed_line_is_one_region() {
        assert_eq!(stream_regions("a\nb\nc\n", "a\nB\nc\n"), 1);
    }

    #[test]
    fn adjacent_changed_lines_are_one_region() {
        assert_eq!(stream_regions("a\nb\nc\nd\n", "a\nB\nC\nd\n"), 1);
    }

    #[test]
    fn changes_separated_by_a_common_line_are_two_regions() {
        assert_eq!(stream_regions("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n"), 2);
    }

    #[test]
    fn a_deleted_block_is_one_region() {
        assert_eq!(stream_regions("a\nb\nc\nd\n", "a\nd\n"), 1);
    }

    #[test]
    fn a_pure_insertion_at_the_end_is_one_region() {
        assert_eq!(stream_regions("a\nb\n", "a\nb\nc\nd\n"), 1);
    }

    #[test]
    fn empty_against_content_is_one_region() {
        assert_eq!(stream_regions("", "a\nb\n"), 1);
        assert_eq!(stream_regions("a\nb\n", ""), 1);
    }

    #[test]
    fn separate_deletion_and_insertion_count_separately() {
        // `b` is dropped; `x` is added three lines later.
        assert_eq!(stream_regions("a\nb\nc\nd\ne\n", "a\nc\nd\nx\ne\n"), 2);
    }

    #[test]
    fn a_line_terminator_difference_is_one_region_not_zero() {
        // `lines()` yields the same lines for both, so the walk sees nothing.
        // The bytes still differ, and a zero here would leave the divergence
        // unrecordable.
        assert_eq!(changed_regions("a\nb\n", "a\r\nb\r\n"), 0);
        assert_eq!(stream_regions("a\nb\n", "a\r\nb\r\n"), 1);
    }

    #[test]
    fn a_missing_trailing_newline_is_one_region_not_zero() {
        assert_eq!(changed_regions("a\nb\n", "a\nb"), 0);
        assert_eq!(stream_regions("a\nb\n", "a\nb"), 1);
    }

    /// The invariant `run::run_suites` relies on to declare
    /// `RunError::UnmeasuredDivergence` unreachable: no pair of DIFFERING
    /// stream contents measures zero regions, whatever shape the difference
    /// takes.
    #[test]
    fn the_floor_holds_for_every_differing_shape() {
        let pairs = [
            ("a\n", "b\n"),             // content
            ("a\n", "a\r\n"),           // line terminator
            ("a\n", "a"),               // trailing newline
            ("", "\n"),                 // empty vs one blank line
            ("a\n", ""),                // everything removed
            ("a\nb\n", "a\nb\n\n"),     // trailing blank line
            (" a\n", "a\n"),            // leading whitespace
            ("a \n", "a\n"),            // trailing whitespace
            ("a\n\n\nb\n", "a\n\nb\n"), // a dropped blank line
        ];
        for (oracle, candidate) in pairs {
            assert!(
                stream_regions(oracle, candidate) > 0,
                "{oracle:?} vs {candidate:?} differs but measured zero regions"
            );
        }
    }

    #[test]
    fn channels_are_counted_separately_so_a_swap_cannot_cancel_out() {
        let one_on_stdout = Extent {
            stdout: 1,
            ..Extent::default()
        };
        let one_on_stderr = Extent {
            stderr: 1,
            ..Extent::default()
        };
        assert_eq!(one_on_stdout.total(), one_on_stderr.total());
        assert_ne!(
            one_on_stdout, one_on_stderr,
            "the same total on a different channel is a different extent"
        );
    }

    #[test]
    fn render_omits_zero_channels() {
        assert_eq!(Extent::default().render(), "{}");
        assert_eq!(
            Extent {
                stdout: 3,
                exit: 1,
                ..Extent::default()
            }
            .render(),
            "{ stdout = 3, exit = 1 }"
        );
    }
}
