//! A 1-based line/column plus 0-based byte offset into a source string.
//!
//! The text layer's single position type — headings and wikilink tokens both
//! carry one so a caller can point a human (or a diagnostic) at the exact byte
//! a construct begins.

use serde::{Deserialize, Serialize};

/// A location in a source string: 1-based `line` and `column`, 0-based
/// `byte_offset`. Column counts bytes from the start of the line, matching the
/// donor's editor-agnostic convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub line: usize,
    pub column: usize,
    pub byte_offset: usize,
}

impl SourceSpan {
    /// Compute the span of `byte_offset` within `content`. An offset past the
    /// end of `content` is clamped to its length.
    pub fn at(content: &str, byte_offset: usize) -> Self {
        let prefix = &content[..byte_offset.min(content.len())];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit_once('\n')
            .map_or(prefix.len() + 1, |(_, tail)| tail.len() + 1);
        SourceSpan {
            line,
            column,
            byte_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_byte_is_line_one_column_one() {
        let span = SourceSpan::at("hello\nworld\n", 0);
        assert_eq!(span.line, 1);
        assert_eq!(span.column, 1);
        assert_eq!(span.byte_offset, 0);
    }

    #[test]
    fn offset_on_second_line_reports_line_two() {
        let content = "hello\nworld\n";
        let offset = content.find("world").unwrap();
        let span = SourceSpan::at(content, offset);
        assert_eq!(span.line, 2);
        assert_eq!(span.column, 1);
        assert_eq!(span.byte_offset, offset);
    }

    #[test]
    fn column_counts_from_line_start() {
        let content = "abc def\n";
        let offset = content.find("def").unwrap();
        let span = SourceSpan::at(content, offset);
        assert_eq!(span.line, 1);
        assert_eq!(span.column, 5);
    }

    #[test]
    fn offset_past_end_is_clamped() {
        let content = "ab";
        let span = SourceSpan::at(content, 999);
        assert_eq!(span.byte_offset, 999);
        assert_eq!(span.line, 1);
        assert_eq!(span.column, 3);
    }
}
