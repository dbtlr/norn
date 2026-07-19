//! Link-target classification and decoding — the Markdown-link half of the
//! donor's `anchor` helpers that stayed with the link model.
//!
//! The anchor-splitting and slug helpers the donor bundled here
//! (`split_anchor`, `split_anchor_or_block_ref`, `slugify`) are wikilink/heading
//! *syntax* and now live in `norn-frontmatter`. What remains is the part that is
//! about *links to a vault*: deciding whether a Markdown `dest_url` names a local
//! file worth modeling, and decoding the percent escapes such URLs carry.

/// Decode `%XX` percent escapes in a Markdown link target. Invalid or truncated
/// escapes are left intact; the decoded bytes are interpreted as UTF-8 lossily.
pub(crate) fn decode_percent_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                output.push((high << 4) | low);
                index += 3;
                continue;
            }
        }

        output.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// True when a Markdown `[text](dest)` target names a local Markdown file: it is
/// a local file target (see [`is_local_file_target`]) and, once its anchor is
/// stripped, still has a non-empty path component.
pub(crate) fn is_local_markdown_target(target: &str) -> bool {
    if !is_local_file_target(target) {
        return false;
    }

    let (target, _) = split_target_anchor(target);
    !target.is_empty()
}

/// True when a target refers to a local file rather than an external resource
/// (`http`/`https`/`mailto`) or a same-document anchor (`#…`).
pub(crate) fn is_local_file_target(target: &str) -> bool {
    if target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with('#')
    {
        return false;
    }

    true
}

/// Split a target on its first `#` into `(target, anchor)`. Local helper mirroring
/// `norn_frontmatter::wikilink::split_anchor`, kept private so the local-target
/// predicate above does not reach across the text-layer boundary for a one-liner.
fn split_target_anchor(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once('#') {
        Some((target, anchor)) => (target, Some(anchor)),
        None => (raw, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_percent_escapes_decodes_valid_sequences() {
        assert_eq!(decode_percent_escapes("Hello%20World"), "Hello World");
        assert_eq!(decode_percent_escapes("a%2Bb"), "a+b");
    }

    #[test]
    fn decode_percent_escapes_leaves_invalid_sequences_intact() {
        assert_eq!(decode_percent_escapes("a%ZZb"), "a%ZZb");
        assert_eq!(decode_percent_escapes("a%"), "a%");
        assert_eq!(decode_percent_escapes("a%X"), "a%X");
    }

    #[test]
    fn decode_percent_escapes_handles_multibyte_utf8_sequences() {
        assert_eq!(decode_percent_escapes("caf%C3%A9"), "café");
    }

    #[test]
    fn is_local_file_target_rejects_external_and_same_note() {
        assert!(!is_local_file_target("http://example.com"));
        assert!(!is_local_file_target("https://example.com"));
        assert!(!is_local_file_target("mailto:hi@example.com"));
        assert!(!is_local_file_target("#Heading"));
        assert!(is_local_file_target("folder/note.md"));
    }

    #[test]
    fn is_local_markdown_target_requires_non_empty_path() {
        assert!(is_local_markdown_target("note.md"));
        assert!(is_local_markdown_target("note.md#Heading"));
        // Anchor-only target has an empty path once the anchor is stripped.
        assert!(!is_local_markdown_target("#Heading"));
    }
}
