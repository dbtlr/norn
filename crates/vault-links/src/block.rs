use regex::Regex;

pub fn parse_block_ids(body: &str) -> Vec<String> {
    let block_re = Regex::new(r"(?:^|\s)\^([A-Za-z0-9_-]+)\s*$").expect("valid block id regex");
    body.lines()
        .filter_map(|line| {
            block_re
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|block_id| block_id.as_str().to_string())
        })
        .collect()
}
