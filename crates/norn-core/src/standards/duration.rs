//! Short-duration string parsing, shared by config deserialization and the
//! CLI's `--retention` value parser.
//!
//! Kept free of intra-crate dependencies so it can be lifted standalone.

/// Parse a short duration string: `<n>w` weeks, `<n>d` days, `<n>h` hours,
/// `<n>m` minutes. Returns None on anything unrecognized (best-effort). The
/// numeric part must parse as `u64`; a missing/unknown suffix or non-numeric
/// value yields None.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (num, unit_secs) = match s.chars().last()? {
        'w' => (&s[..s.len() - 1], 604_800u64),
        'd' => (&s[..s.len() - 1], 86_400),
        'h' => (&s[..s.len() - 1], 3_600),
        'm' => (&s[..s.len() - 1], 60),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(n * unit_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_handles_units() {
        assert_eq!(
            parse_duration("90d"),
            Some(std::time::Duration::from_secs(90 * 86_400))
        );
        assert_eq!(
            parse_duration("12h"),
            Some(std::time::Duration::from_secs(12 * 3_600))
        );
        assert_eq!(
            parse_duration("2w"),
            Some(std::time::Duration::from_secs(2 * 604_800))
        );
        assert_eq!(parse_duration("nonsense"), None);
        assert_eq!(parse_duration("10"), None); // no suffix
        assert_eq!(parse_duration(""), None);
    }
}
