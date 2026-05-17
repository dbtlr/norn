use pulldown_cmark::HeadingLevel;
use vault_core::SourceSpan;

pub fn split_anchor(raw: &str) -> (String, Option<String>) {
    match raw.split_once('#') {
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string())),
        None => (raw.to_string(), None),
    }
}

pub fn split_anchor_or_block_ref(raw: &str) -> (String, Option<String>, Option<String>) {
    match raw.split_once('#') {
        Some((target, reference)) if reference.starts_with('^') => {
            (target.to_string(), None, Some(reference[1..].to_string()))
        }
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string()), None),
        None => (raw.to_string(), None, None),
    }
}

pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_end_matches('-').to_string()
}

pub fn decode_percent_escapes(value: &str) -> String {
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

pub(crate) fn is_local_markdown_target(target: &str) -> bool {
    if !is_local_file_target(target) {
        return false;
    }

    let (target, _) = split_anchor(target);
    !target.is_empty()
}

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

pub(crate) fn source_span(content: &str, byte_offset: usize) -> SourceSpan {
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

pub(crate) fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
