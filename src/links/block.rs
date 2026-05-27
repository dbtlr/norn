use std::sync::LazyLock;

use regex::Regex;

static BLOCK_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)\^([A-Za-z0-9_-]+)\s*$").expect("valid block id regex"));

pub fn parse_block_ids(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            BLOCK_ID_RE
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|block_id| block_id.as_str().to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_block_ids_finds_trailing_block_ref_with_leading_space() {
        let body = "Some paragraph. ^block-1\n";
        assert_eq!(parse_block_ids(body), vec!["block-1"]);
    }

    #[test]
    fn parse_block_ids_finds_block_ref_at_line_start() {
        let body = "^block-2\n";
        assert_eq!(parse_block_ids(body), vec!["block-2"]);
    }

    #[test]
    fn parse_block_ids_collects_multiple_block_refs() {
        let body = "first ^a\nsecond ^b\nthird with no marker\n";
        assert_eq!(parse_block_ids(body), vec!["a", "b"]);
    }

    #[test]
    fn parse_block_ids_rejects_block_ref_with_unsupported_characters() {
        // Regex requires [A-Za-z0-9_-]; punctuation breaks the match.
        let body = "hello ^bad.id\n";
        assert!(parse_block_ids(body).is_empty());
    }

    #[test]
    fn parse_block_ids_allows_trailing_whitespace() {
        let body = "hello ^ok  \n";
        assert_eq!(parse_block_ids(body), vec!["ok"]);
    }

    #[test]
    fn parse_block_ids_returns_empty_for_no_markers() {
        let body = "paragraph one\nparagraph two\n";
        assert!(parse_block_ids(body).is_empty());
    }
}
