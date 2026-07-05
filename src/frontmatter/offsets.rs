use std::ops::Range;

pub struct FrontmatterPropertyString<'a> {
    pub property: String,
    pub text: &'a str,
    pub offset: Option<usize>,
}

pub fn frontmatter_property_strings<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    content: &str,
    frontmatter_range: Option<Range<usize>>,
) -> Vec<FrontmatterPropertyString<'a>> {
    let mut strings = Vec::new();

    for (property, value) in object {
        match value {
            serde_json::Value::String(text) => strings.push(FrontmatterPropertyString {
                property: property.to_string(),
                text,
                offset: frontmatter_scalar_offset(
                    content,
                    frontmatter_range.clone(),
                    property,
                    text,
                ),
            }),
            serde_json::Value::Array(values) => {
                for text in values.iter().filter_map(|value| value.as_str()) {
                    strings.push(FrontmatterPropertyString {
                        property: property.to_string(),
                        text,
                        offset: frontmatter_list_item_offset(
                            content,
                            frontmatter_range.clone(),
                            property,
                            text,
                        ),
                    });
                }
            }
            _ => {}
        }
    }

    strings
}

fn frontmatter_scalar_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) && trimmed.starts_with(&property_prefix) {
            return line.find(text).map(|offset| line_start + offset);
        }
        line_start += line.len();
    }

    None
}

fn frontmatter_list_item_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut in_property = false;
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) {
            in_property = trimmed.starts_with(&property_prefix);
            line_start += line.len();
            continue;
        }

        if in_property && trimmed.starts_with('-') {
            if let Some(offset) = line.find(text) {
                return Some(line_start + offset);
            }
        }

        line_start += line.len();
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertySpan {
    pub name: String,
    pub line_range: Range<usize>,
    /// `None` for block-style values, empty values, or anything whose value cannot
    /// be located as a single contiguous byte range on the key line.
    pub value_range: Option<Range<usize>>,
    pub style: ValueStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    BlockLiteral,
    BlockFolded,
    FlowSequence,
    FlowMapping,
    BlockSequence,
    BlockMapping,
    EmptyValue,
}

/// Returns one [`PropertySpan`] per top-level property in the frontmatter slice
/// `content[frontmatter_range]`.
///
/// Top-level means: the property's key starts at column 0 of its line (no
/// leading whitespace). Continuation lines (indented children for block
/// styles, continuation of multi-line scalars) are included in `line_range`
/// but do not produce their own `PropertySpan`.
///
/// The locator is line-based and deliberately does **not** parse YAML. Its
/// contract is "span precisely where certain, refuse everywhere else": a
/// `value_range` is emitted only for values we can locate with certainty —
/// single-line plain scalars, and single-line single- or double-quoted scalars
/// (honoring `''` / `\"` / `\\` escapes; quoted spans include the surrounding
/// quotes, plain spans exclude a trailing ` #comment` and trailing whitespace).
/// Everything with real YAML complexity gets `value_range = None` (the key is
/// still visible via `line_range`, so it can be listed and removed-as-a-block,
/// but `apply` declines a minimal in-place value edit rather than risk
/// corrupting the document): block scalars (`|`, `>`), any multi-line value,
/// anchors / aliases / tags (`&`, `*`, `!`), flow mappings (`{…}`), nested or
/// non-flat flow sequences, and block sequences / mappings. A flat single-line
/// flow sequence (`[a, b]`) is the one flow form spanned precisely.
///
/// Keys are recognized structurally: any scalar key at column 0, including
/// quoted keys (`"…"`, `'…'`), numeric-leading keys (`123:`), and the merge key
/// (`<<:`). The decoded key name matches how `serde_yaml` keys the parsed map.
///
/// Correctness invariant: never emit a `value_range` unless it is exactly the
/// value's bytes. Any ambiguity resolves to `None` — a wrong span corrupts a
/// document; a `None` merely declines an edit.
pub fn top_level_property_spans(
    content: &str,
    frontmatter_range: Range<usize>,
) -> Vec<PropertySpan> {
    let yaml = &content[frontmatter_range.clone()];
    let mut spans: Vec<PropertySpan> = Vec::new();

    let lines: Vec<&str> = yaml.split_inclusive('\n').collect();
    // Precompute byte offsets for the start of each line within `content`.
    let mut line_starts: Vec<usize> = Vec::with_capacity(lines.len() + 1);
    let mut acc = frontmatter_range.start;
    for line in &lines {
        line_starts.push(acc);
        acc += line.len();
    }
    line_starts.push(acc);

    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let line_start = line_starts[index];
        let line_end = line_starts[index + 1];

        let trimmed_line = line.trim_end_matches(['\r', '\n']);
        if line.starts_with([' ', '\t']) {
            index += 1;
            continue;
        }

        let Some((name, after_colon)) = parse_top_level_key(trimmed_line) else {
            index += 1;
            continue;
        };

        let rest = &trimmed_line[after_colon..];

        let (value_range, style, ends_on_key_line) = classify_value(line_start, after_colon, rest);

        // Multi-line plain/quoted scalar fold: an inline value on the key line
        // followed by a more-indented continuation. YAML folds that continuation
        // into the value, so the value cannot be spanned with certainty — refuse
        // the in-place value edit (`value_range = None`) and absorb the
        // continuation lines into `line_range`, so a --remove covers the whole
        // scalar instead of orphaning the fold. (NRN-133 corruption fix.)
        if ends_on_key_line
            && value_range.is_some()
            && lines
                .get(index + 1)
                .is_some_and(|next| next.starts_with([' ', '\t']) && !next.trim().is_empty())
        {
            let mut j = index + 1;
            while j < lines.len() && lines[j].starts_with([' ', '\t']) {
                j += 1;
            }
            spans.push(PropertySpan {
                name,
                line_range: line_start..line_starts[j],
                value_range: None,
                style,
            });
            index = j;
            continue;
        }

        let mut span = PropertySpan {
            name,
            line_range: line_start..line_end,
            value_range,
            style,
        };

        // Determine if we should consume continuation lines.
        let needs_continuation = !ends_on_key_line || matches!(style, ValueStyle::EmptyValue);

        if needs_continuation {
            let mut consume_index = index + 1;
            let mut consume_end = line_end;
            let mut upgraded_style = style;
            let mut flow_open: Option<char> = match style {
                ValueStyle::FlowSequence if !ends_on_key_line => Some('['),
                ValueStyle::FlowMapping if !ends_on_key_line => Some('{'),
                _ => None,
            };
            let mut quoted_open: Option<char> = match style {
                ValueStyle::SingleQuoted if !ends_on_key_line => Some('\''),
                ValueStyle::DoubleQuoted if !ends_on_key_line => Some('"'),
                _ => None,
            };
            while consume_index < lines.len() {
                let cont = lines[consume_index];
                // A no-indent block sequence (`key:` then `- item` at column 0)
                // is the common hand-authored / Obsidian layout. Its item lines
                // are continuation of the value, not the next top-level key, so
                // absorb them while the value is (still) empty or a block
                // sequence — otherwise the span covers only the `key:` line and
                // a --remove/replace orphans the items (NRN-128).
                let is_no_indent_seq_item = matches!(
                    upgraded_style,
                    ValueStyle::EmptyValue | ValueStyle::BlockSequence
                ) && is_block_sequence_item(cont);
                // Stop on a non-indented, non-blank line — that's the next top-level key.
                if !cont.starts_with([' ', '\t'])
                    && !cont.trim().is_empty()
                    && !is_no_indent_seq_item
                {
                    break;
                }
                // If we were EmptyValue, the first non-blank indented line tells us the
                // block style: starts with `-` → BlockSequence, otherwise BlockMapping.
                if matches!(upgraded_style, ValueStyle::EmptyValue) {
                    let cont_trimmed = cont.trim_start();
                    if cont_trimmed.starts_with('-') {
                        upgraded_style = ValueStyle::BlockSequence;
                    } else if !cont_trimmed.is_empty() {
                        upgraded_style = ValueStyle::BlockMapping;
                    }
                }
                // For unclosed flow values, keep absorbing until the closing bracket appears.
                if let Some(open) = flow_open {
                    let close = if open == '[' { ']' } else { '}' };
                    if cont.contains(close) {
                        flow_open = None;
                    }
                }
                // For unclosed quoted scalars, keep absorbing until matching quote.
                if let Some(_q) = quoted_open {
                    // Best-effort: stop scanning when we hit a closing quote on this line.
                    // We do not produce a value_range in this case.
                    quoted_open = None;
                }
                consume_end = line_starts[consume_index + 1];
                consume_index += 1;
            }
            span.line_range = line_start..consume_end;
            span.style = upgraded_style;
            // If style upgraded from EmptyValue to a block style, value_range stays None.
            if matches!(
                upgraded_style,
                ValueStyle::BlockSequence | ValueStyle::BlockMapping
            ) {
                span.value_range = None;
            }
            index = consume_index;
        } else {
            index += 1;
        }

        spans.push(span);
    }

    spans
}

/// Recognizes a top-level `key:` mapping entry at column 0 of `line` (the key
/// line with its trailing newline already stripped). Returns the *decoded* key
/// name — matching how `serde_yaml` keys the parsed map, so `apply` can pair a
/// span with the field it edits — and the byte offset in `line` immediately
/// after the `:` separator.
///
/// Recognized structurally (no identifier allowlist): plain keys, single- and
/// double-quoted keys (`"key with spaces"`, `'it''s'`), numeric-leading keys
/// (`123:`), and the merge key (`<<:`). Returns `None` for a YAML comment line
/// or any line that is not a mapping entry (no `:` separator).
fn parse_top_level_key(line: &str) -> Option<(String, usize)> {
    // A `#` at column 0 begins a YAML comment, never a key.
    if line.starts_with('#') {
        return None;
    }
    match line.as_bytes().first()? {
        b'\'' => {
            let (name, end) = scan_single_quoted_key(line)?;
            let after_colon = colon_after(line, end)?;
            Some((name, after_colon))
        }
        b'"' => {
            let (name, end) = scan_double_quoted_key(line)?;
            let after_colon = colon_after(line, end)?;
            Some((name, after_colon))
        }
        _ => {
            let sep = find_plain_key_separator(line)?;
            let name = line[..sep].trim_end();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), sep + 1))
        }
    }
}

/// Byte index of the first `:` that acts as a YAML block-mapping separator — a
/// colon followed by whitespace or end-of-line. Returns `None` when the line has
/// no separator (e.g. a bare plain scalar), so such a line is not mistaken for a
/// key. A `time: 12:30` value is unaffected: the first `:` (after `time`) is the
/// separator; the later `12:30` colon has no following whitespace.
fn find_plain_key_separator(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            match bytes.get(i + 1) {
                None | Some(b' ') | Some(b'\t') => return Some(i),
                _ => {}
            }
        }
    }
    None
}

/// Skips inline whitespace from `from` and returns the byte offset just past a
/// `:` separator, or `None` if the next non-space byte is not a colon.
fn colon_after(line: &str, from: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = from;
    while matches!(bytes.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if bytes.get(i) == Some(&b':') {
        Some(i + 1)
    } else {
        None
    }
}

/// Decodes a single-quoted key starting at byte 0 of `line`, honoring the `''`
/// escape. Returns `(decoded_name, byte_offset_after_closing_quote)` or `None`
/// if the quote never closes.
fn scan_single_quoted_key(line: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = line[1..].char_indices();
    while let Some((rel, c)) = chars.next() {
        let abs = 1 + rel;
        if c == '\'' {
            if line[abs + 1..].starts_with('\'') {
                out.push('\'');
                chars.next();
                continue;
            }
            return Some((out, abs + 1));
        }
        out.push(c);
    }
    None
}

/// Decodes a double-quoted key starting at byte 0 of `line`, honoring `\"`,
/// `\\`, and the common control escapes. Returns
/// `(decoded_name, byte_offset_after_closing_quote)` or `None` if unclosed.
fn scan_double_quoted_key(line: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = line[1..].char_indices();
    while let Some((rel, c)) = chars.next() {
        let abs = 1 + rel;
        match c {
            '\\' => {
                let (_, e) = chars.next()?;
                out.push(match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    other => other,
                });
            }
            '"' => return Some((out, abs + 1)),
            _ => out.push(c),
        }
    }
    None
}

/// True if `line` is a YAML block-sequence item — `- value` or a bare `-` —
/// ignoring leading indentation and the trailing newline. A top-level mapping
/// key can never take this shape, so a column-0 match is unambiguously a
/// continuation of the preceding block-sequence value, not the next key.
fn is_block_sequence_item(line: &str) -> bool {
    let t = line.trim();
    t == "-" || t.starts_with("- ")
}

/// Classifies the value portion of a key line.
///
/// `line_start_byte` is the byte offset of the key line in the original `content`.
/// `after_colon` is the byte offset (within the trimmed key line) immediately
/// after the `:`. `rest` is the trimmed text after the colon (no leading space
/// trimming yet — we need to know where the value starts).
///
/// Returns `(value_range_in_content, style, ends_on_key_line)` where
/// `ends_on_key_line` is true if the value is complete on the key line and no
/// continuation lines should be absorbed.
fn classify_value(
    line_start_byte: usize,
    after_colon: usize,
    rest: &str,
) -> (Option<Range<usize>>, ValueStyle, bool) {
    let value_offset_in_rest = rest
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i);

    let Some(value_offset) = value_offset_in_rest else {
        return (None, ValueStyle::EmptyValue, false);
    };

    let value_start_byte = line_start_byte + after_colon + value_offset;
    let value_text = &rest[value_offset..];
    let first_char = value_text.chars().next().unwrap();

    match first_char {
        '|' => (None, ValueStyle::BlockLiteral, false),
        '>' => (None, ValueStyle::BlockFolded, false),
        // Anchors (`&a`), aliases (`*a`), and tags (`!!str`, `!foo`) are YAML
        // markup we cannot span as literal value bytes — deleting or rewriting
        // the marker would corrupt the document. Refuse (`None`); the key stays
        // visible via `line_range` so a whole-line remove still works. No
        // dedicated `ValueStyle`, so report `Plain` — `apply` never serializes
        // it because `value_range` is `None`.
        '&' | '*' | '!' => (None, ValueStyle::Plain, true),
        '\'' => {
            let bytes = value_text.as_bytes();
            let mut i = 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    return (
                        Some(value_start_byte..value_start_byte + i + 1),
                        ValueStyle::SingleQuoted,
                        true,
                    );
                }
                i += 1;
            }
            (None, ValueStyle::SingleQuoted, false)
        }
        '"' => {
            let bytes = value_text.as_bytes();
            let mut i = 1;
            let mut escaped = false;
            while i < bytes.len() {
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'"' {
                    return (
                        Some(value_start_byte..value_start_byte + i + 1),
                        ValueStyle::DoubleQuoted,
                        true,
                    );
                }
                i += 1;
            }
            (None, ValueStyle::DoubleQuoted, false)
        }
        '[' => match value_text.find(']') {
            // Only a *flat* single-line flow sequence is spanned precisely: the
            // bracketed content must contain no nested `[]`/`{}` and no quotes
            // (which could hide a bracket or comma), and nothing but whitespace
            // or a comment may follow the `]`. Anything else — nested,
            // quote-bearing, or trailing content — is refused, because the naive
            // first-`]` scan would truncate or misplace the span.
            Some(close) => {
                let inner = &value_text[1..close];
                let after = &value_text[close + 1..];
                let flat = !inner
                    .bytes()
                    .any(|b| matches!(b, b'[' | b']' | b'{' | b'}' | b'\'' | b'"'));
                let after_ok = after.is_empty() || after.starts_with([' ', '\t']);
                if flat && after_ok {
                    (
                        Some(value_start_byte..value_start_byte + close + 1),
                        ValueStyle::FlowSequence,
                        true,
                    )
                } else {
                    (None, ValueStyle::FlowSequence, true)
                }
            }
            None => (None, ValueStyle::FlowSequence, false),
        },
        // Flow mappings are always refused: a `{a: {b: 1}}` first-`}` scan
        // truncates, and there is no green contract requiring a precise
        // single-line flow-mapping span. When in doubt, refuse.
        '{' => match value_text.find('}') {
            Some(_) => (None, ValueStyle::FlowMapping, true),
            None => (None, ValueStyle::FlowMapping, false),
        },
        _ => {
            let value_bytes = value_text.as_bytes();
            let mut end = value_bytes.len();
            for i in 0..value_bytes.len() {
                if value_bytes[i] == b'#'
                    && i > 0
                    && (value_bytes[i - 1] == b' ' || value_bytes[i - 1] == b'\t')
                {
                    end = i;
                    while end > 0 && (value_bytes[end - 1] == b' ' || value_bytes[end - 1] == b'\t')
                    {
                        end -= 1;
                    }
                    break;
                }
            }
            while end > 0 && (value_bytes[end - 1] == b'\r' || value_bytes[end - 1] == b'\n') {
                end -= 1;
            }
            (
                Some(value_start_byte..value_start_byte + end),
                ValueStyle::Plain,
                true,
            )
        }
    }
}

/// Inserts a new `field: value` line into the frontmatter block, immediately
/// before the closing `---` delimiter.
///
/// `frontmatter_range` is the byte range of the YAML content between the
/// opening `---\n` and closing `---\n` markers — the range produced by
/// [`super::extract_frontmatter`]. For an empty frontmatter block, the range
/// is empty (e.g., `4..4` for `"---\n---\n..."`).
///
/// The value is rendered via [`super::quote::serialize_value_preserving_style`]
/// starting from [`ValueStyle::Plain`] — meaning plain when safe, upgraded to
/// single-quoted when the value needs quoting. Never produces double quotes
/// unless the value contains a single quote.
///
/// Returns the full content with the new line spliced in just before the
/// closing `---` delimiter.
// Superseded by the set/repair_apply mutation paths; safe to delete in a cleanup pass.
#[cfg(test)]
pub fn append_frontmatter_field(
    content: &str,
    frontmatter_range: Range<usize>,
    field: &str,
    value: &serde_json::Value,
) -> Result<String, super::quote::QuoteError> {
    let rendered_value = super::quote::serialize_value_preserving_style(value, ValueStyle::Plain)?;

    let new_line = format!("{field}: {rendered_value}\n");

    let mut result = String::with_capacity(content.len() + new_line.len());
    result.push_str(&content[..frontmatter_range.end]);
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&new_line);
    result.push_str(&content[frontmatter_range.end..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(content: &str, range: Range<usize>) -> Vec<FrontmatterPropertyString<'_>> {
        let yaml = &content[range.clone()];
        let value: serde_json::Value = serde_yaml::from_str(yaml).unwrap();
        let value: serde_json::Value = serde_json::to_value(value).unwrap();
        let object = value.as_object().unwrap().clone();
        // Property strings need ownership of the map for the &'a borrow.
        // Box::leak is fine in a test for simplicity.
        let object: &'static serde_json::Map<String, serde_json::Value> =
            Box::leak(Box::new(object));
        frontmatter_property_strings(object, content, Some(range))
    }

    #[test]
    fn scalar_offset_is_returned_inside_property_value_range() {
        let content = "---\ntitle: hello world\n---\n# body\n";
        let strings = props(content, 4..23);
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].property, "title");
        assert_eq!(strings[0].text, "hello world");
        let offset = strings[0].offset.unwrap();
        assert!(content[offset..].starts_with("hello world"));
    }

    #[test]
    fn list_item_offsets_are_returned_for_top_level_list_of_strings() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let strings = props(content, 4..29);
        assert_eq!(strings.len(), 2);
        for s in &strings {
            assert_eq!(s.property, "aliases");
            let offset = s.offset.unwrap();
            assert!(content[offset..].starts_with(s.text));
        }
    }

    #[test]
    fn nested_yaml_objects_are_skipped() {
        // Current behavior: nested mappings are not surfaced as property strings.
        let content = "---\nmeta:\n  inner: value\n---\n";
        let strings = props(content, 4..25);
        // `meta` is an object, neither string nor list-of-strings, so no property strings.
        assert!(strings.is_empty());
    }

    #[test]
    fn property_value_containing_another_property_name_as_substring_is_pinned() {
        // Documents the known fragility: line.find(text) inside frontmatter_scalar_offset
        // may collide if a property's value text appears on another property's line first.
        // The current impl scans line-by-line and matches the first property prefix, so
        // this case is actually OK — but the test pins the behavior so Slice 2's
        // minimal-edit replacement can either preserve or deliberately fix it.
        let content = "---\nname: foo\nalias: name\n---\n";
        let strings = props(content, 4..26);
        // Both name and alias should produce property strings.
        let alias = strings.iter().find(|s| s.property == "alias").unwrap();
        let alias_offset = alias.offset.unwrap();
        // alias's offset should point into the second line, not the first.
        // The second line starts at content[14..] = "alias: name\n"; "name" is at offset 14 + "alias: ".len() = 21.
        assert_eq!(alias_offset, 21);
    }

    #[test]
    fn same_line_comment_inside_value_pins_current_behavior() {
        // line.find(text) does not consider comments. With "title: hello # comment", finding "hello"
        // returns the offset of the actual value text. The current implementation does NOT
        // distinguish a comment from value text. This test documents that — Slice 2's minimal-edit
        // work will likely need to handle this case more carefully.
        let content = "---\ntitle: hello # comment\n---\n";
        let strings = props(content, 4..26);
        let title = strings.iter().find(|s| s.property == "title").unwrap();
        // serde_yaml drops the comment when producing the JSON value; the text is "hello".
        assert_eq!(title.text, "hello");
        let offset = title.offset.unwrap();
        assert!(content[offset..].starts_with("hello"));
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;

    #[test]
    fn plain_scalar_span_isolates_value_bytes() {
        let content = "---\ntitle: hello world\n---\n# body\n";
        let spans = top_level_property_spans(content, 4..23);
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "title");
        assert_eq!(&content[span.line_range.clone()], "title: hello world\n");
        assert_eq!(
            content[span.value_range.clone().unwrap()].to_string(),
            "hello world"
        );
        assert_eq!(span.style, ValueStyle::Plain);
    }

    #[test]
    fn single_quoted_scalar_span_includes_quotes() {
        let content = "---\nworkspace: '[[norn]]'\n---\n";
        let spans = top_level_property_spans(content, 4..30);
        let span = &spans[0];
        assert_eq!(span.name, "workspace");
        assert_eq!(span.style, ValueStyle::SingleQuoted);
        assert_eq!(&content[span.value_range.clone().unwrap()], "'[[norn]]'");
    }

    #[test]
    fn double_quoted_scalar_span_includes_quotes() {
        let content = "---\nworkspace: \"[[norn]]\"\n---\n";
        let spans = top_level_property_spans(content, 4..30);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::DoubleQuoted);
        assert_eq!(&content[span.value_range.clone().unwrap()], "\"[[norn]]\"");
    }

    #[test]
    fn empty_value_followed_by_block_sequence() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let spans = top_level_property_spans(content, 4..29);
        let span = &spans[0];
        assert_eq!(span.name, "aliases");
        assert_eq!(span.style, ValueStyle::BlockSequence);
        assert!(span.value_range.is_none());
        assert_eq!(
            &content[span.line_range.clone()],
            "aliases:\n  - one\n  - two\n"
        );
    }

    #[test]
    fn empty_value_followed_by_no_indent_block_sequence() {
        // The no-indent list layout (`aliases:` then `- x` at column 0) is the
        // common hand-authored / Obsidian form. The span must absorb the item
        // lines so a --remove/replace covers the whole sequence (NRN-128).
        let content = "---\naliases:\n- one\n- two\ntype: note\n---\n";
        let spans = top_level_property_spans(content, 4..36);
        assert_eq!(spans.len(), 2);
        let span = &spans[0];
        assert_eq!(span.name, "aliases");
        assert_eq!(span.style, ValueStyle::BlockSequence);
        assert!(span.value_range.is_none());
        assert_eq!(
            &content[span.line_range.clone()],
            "aliases:\n- one\n- two\n"
        );
        assert_eq!(spans[1].name, "type");
    }

    #[test]
    fn empty_value_followed_by_block_mapping() {
        let content = "---\nmeta:\n  inner: value\n---\n";
        let spans = top_level_property_spans(content, 4..25);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::BlockMapping);
        assert!(span.value_range.is_none());
    }

    #[test]
    fn flow_sequence_on_single_line() {
        let content = "---\naliases: [a, b]\n---\n";
        let spans = top_level_property_spans(content, 4..20);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::FlowSequence);
        assert_eq!(&content[span.value_range.clone().unwrap()], "[a, b]");
    }

    #[test]
    fn plain_scalar_with_same_line_comment_excludes_comment_from_value_range() {
        let content = "---\ntitle: hello  # comment\n---\n";
        let spans = top_level_property_spans(content, 4..27);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::Plain);
        assert_eq!(&content[span.value_range.clone().unwrap()], "hello");
        assert!(content[span.line_range.clone()].contains("# comment"));
    }

    #[test]
    fn multiple_properties_return_separate_spans_in_order() {
        let content = "---\ntitle: hello\nstatus: draft\nworkspace: '[[demo]]'\n---\n";
        let spans = top_level_property_spans(content, 4..52);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "title");
        assert_eq!(spans[1].name, "status");
        assert_eq!(spans[2].name, "workspace");
        assert_eq!(spans[2].style, ValueStyle::SingleQuoted);
    }

    #[test]
    fn block_literal_value_range_is_none() {
        let content = "---\ndescription: |\n  line one\n  line two\n---\n";
        let spans = top_level_property_spans(content, 4..41);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::BlockLiteral);
        assert!(span.value_range.is_none());
        assert!(content[span.line_range.clone()].contains("line two"));
    }

    #[test]
    fn indented_lines_are_not_top_level_keys() {
        let content = "---\nparent:\n  child: not a top-level key\n---\n";
        let spans = top_level_property_spans(content, 4..41);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "parent");
    }

    #[test]
    fn append_field_to_existing_frontmatter() {
        let content = "---\ntitle: hi\n---\n# body\n";
        let frontmatter_range = 4..14; // "title: hi\n"
        let result = append_frontmatter_field(
            content,
            frontmatter_range,
            "kind",
            &serde_json::json!("research"),
        )
        .unwrap();
        assert_eq!(result, "---\ntitle: hi\nkind: research\n---\n# body\n");
    }

    #[test]
    fn append_field_with_special_chars_quotes_value() {
        let content = "---\ntitle: hi\n---\n";
        let frontmatter_range = 4..14;
        let result = append_frontmatter_field(
            content,
            frontmatter_range,
            "url",
            &serde_json::json!("a: b"),
        )
        .unwrap();
        assert!(result.contains("url: 'a: b'") || result.contains("url: \"a: b\""));
    }

    #[test]
    fn append_field_with_wikilink_value_single_quotes_safely() {
        let content = "---\ntitle: hi\n---\n";
        let frontmatter_range = 4..14;
        let result = append_frontmatter_field(
            content,
            frontmatter_range,
            "workspace",
            &serde_json::json!("[[demo]]"),
        )
        .unwrap();
        assert!(result.contains("workspace: '[[demo]]'"));
    }

    #[test]
    fn append_field_to_empty_frontmatter_block() {
        let content = "---\n---\n# body\n";
        let frontmatter_range = 4..4;
        let result = append_frontmatter_field(
            content,
            frontmatter_range,
            "title",
            &serde_json::json!("hi"),
        )
        .unwrap();
        assert_eq!(result, "---\ntitle: hi\n---\n# body\n");
    }

    #[test]
    fn append_field_numeric_value_plain() {
        let content = "---\ntitle: hi\n---\n";
        let frontmatter_range = 4..14;
        let result =
            append_frontmatter_field(content, frontmatter_range, "count", &serde_json::json!(42))
                .unwrap();
        assert!(result.contains("count: 42"));
    }
}

/// NRN-133 span-locator matrix: precise-span certainty vs. refusal, grounded in
/// the YAML 1.2 spec (no third-party corpus). Each case asserts on the byte
/// range the locator emits (or that it refuses with `value_range == None`).
#[cfg(test)]
mod locator_matrix_tests {
    use super::*;

    /// Byte range of the YAML content between the opening `---\n` and the
    /// closing `---` line, matching what `extract_frontmatter` produces
    /// (includes the trailing newline before the closing fence).
    fn fm_range(content: &str) -> Range<usize> {
        let start = 4; // past the leading "---\n"
        let rel = content[start..].find("\n---").expect("closing fence");
        start..(start + rel + 1)
    }

    fn spans(content: &str) -> Vec<PropertySpan> {
        top_level_property_spans(content, fm_range(content))
    }

    // ---- Precise-span cases (value_range = Some, exact bytes) -------------

    #[test]
    fn plain_single_line_scalar_is_precise() {
        let content = "---\ntitle: hello world\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::Plain);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "hello world");
    }

    #[test]
    fn single_quoted_honors_doubled_quote_escape() {
        let content = "---\nk: 'it''s a test'\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::SingleQuoted);
        // Span includes the surrounding quotes and the whole `''`-escaped body.
        assert_eq!(
            &content[s[0].value_range.clone().unwrap()],
            "'it''s a test'"
        );
    }

    #[test]
    fn double_quoted_honors_backslash_escapes() {
        // YAML: k: "a\"b\\c"  -> logical value a"b\c
        let content = "---\nk: \"a\\\"b\\\\c\"\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::DoubleQuoted);
        assert_eq!(
            &content[s[0].value_range.clone().unwrap()],
            "\"a\\\"b\\\\c\""
        );
    }

    #[test]
    fn utf8_multibyte_value_slices_to_correct_string() {
        let content = "---\ntitle: héllo wörld\n---\n";
        let s = spans(content);
        let vr = s[0].value_range.clone().unwrap();
        assert_eq!(&content[vr], "héllo wörld");
    }

    #[test]
    fn value_containing_colon_is_precise() {
        let content = "---\ntime: 12:30\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "time");
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "12:30");
    }

    #[test]
    fn hash_without_leading_space_is_not_a_comment() {
        let content = "---\nurl: http://x/#frag\n---\n";
        let s = spans(content);
        assert_eq!(
            &content[s[0].value_range.clone().unwrap()],
            "http://x/#frag"
        );
    }

    #[test]
    fn same_line_comment_excluded_from_plain_span() {
        let content = "---\ntitle: hello  # note\n---\n";
        let s = spans(content);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "hello");
        assert!(content[s[0].line_range.clone()].contains("# note"));
    }

    #[test]
    fn flat_single_line_flow_sequence_is_precise() {
        let content = "---\ntags: [a, b]\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::FlowSequence);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "[a, b]");
    }

    #[test]
    fn flat_flow_sequence_with_trailing_comment_is_precise() {
        let content = "---\ntags: [a, b] # note\n---\n";
        let s = spans(content);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "[a, b]");
    }

    // ---- Refuse cases (value_range = None) --------------------------------

    #[test]
    fn block_literal_refuses() {
        let content = "---\ndesc: |\n  line one\n  line two\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockLiteral);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn block_literal_with_chomping_refuses() {
        let content = "---\ndesc: |-\n  only\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockLiteral);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn block_folded_refuses() {
        let content = "---\ndesc: >\n  folded text\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockFolded);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn block_folded_with_keep_chomping_refuses() {
        let content = "---\ndesc: >+\n  folded\n\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockFolded);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn multi_line_plain_fold_refuses_and_absorbs() {
        let content = "---\ntitle: hello\n  world\ntype: note\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "title");
        assert!(
            s[0].value_range.is_none(),
            "multi-line plain fold must refuse a precise value span"
        );
        // Continuation absorbed so a remove covers the whole scalar.
        assert_eq!(&content[s[0].line_range.clone()], "title: hello\n  world\n");
        assert_eq!(s[1].name, "type");
        assert_eq!(&content[s[1].value_range.clone().unwrap()], "note");
    }

    #[test]
    fn multi_line_quoted_refuses() {
        let content = "---\nquote: 'line one\n  line two'\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "quote");
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn anchor_definition_refuses() {
        let content = "---\nbase: &anchor hello\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "base");
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn alias_refuses() {
        let content = "---\nref: *anchor\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "ref");
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn explicit_tag_refuses() {
        let content = "---\ncount: !!str 5\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "count");
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn flow_mapping_refuses() {
        let content = "---\nm: {a: 1}\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::FlowMapping);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn nested_flow_sequence_refuses() {
        let content = "---\nn: [a, [b]]\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::FlowSequence);
        assert!(
            s[0].value_range.is_none(),
            "nested flow must refuse: first-`]` scan would truncate"
        );
    }

    #[test]
    fn nested_flow_mapping_refuses() {
        let content = "---\nk: {a: {b: 1}}\n---\n";
        let s = spans(content);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn block_sequence_refuses() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockSequence);
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn block_mapping_refuses() {
        let content = "---\nmeta:\n  inner: value\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::BlockMapping);
        assert!(s[0].value_range.is_none());
    }

    // ---- Key-recognition cases (previously invisible keys) ----------------

    #[test]
    fn quoted_key_with_spaces_is_visible() {
        let content = "---\n\"key with spaces\": value\n---\n";
        let s = spans(content);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "key with spaces");
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "value");
    }

    #[test]
    fn single_quoted_key_decodes_escape() {
        let content = "---\n'it''s': value\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "it's");
    }

    #[test]
    fn numeric_leading_key_is_visible() {
        let content = "---\n123: value\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "123");
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "value");
    }

    #[test]
    fn merge_key_is_visible() {
        let content = "---\n<<: *base\nname: x\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "<<");
        assert!(s[0].value_range.is_none()); // alias value refused
        assert_eq!(s[1].name, "name");
    }

    #[test]
    fn comment_line_is_not_a_key() {
        let content = "---\n# just a comment\ntitle: hi\n---\n";
        let s = spans(content);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "title");
    }

    // ---- Order ------------------------------------------------------------

    #[test]
    fn multiple_properties_return_separate_spans_in_document_order() {
        let content = "---\na: 1\nb: 2\nc: 3\n---\n";
        let s = spans(content);
        assert_eq!(
            s.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }
}
