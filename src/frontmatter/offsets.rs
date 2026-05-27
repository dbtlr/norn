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
/// The scanner is line-based and does not fully parse YAML. Cases it handles
/// correctly: plain scalars, single- and double-quoted scalars, block lists,
/// block mappings, empty values, block literal (`|`) and folded (`>`) scalars,
/// flow sequences and mappings on a single line. Comments on the property's
/// key line are preserved inside `line_range` but excluded from `value_range`.
///
/// Multi-line flow values (e.g., a `[` opened on one line and `]` on a later
/// line) are treated as having `value_range = None` and style
/// `FlowSequence`/`FlowMapping` for safety — `apply_file_changes` rejects
/// mutating those.
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

        let Some(colon_pos) = trimmed_line.find(':') else {
            index += 1;
            continue;
        };

        let name = trimmed_line[..colon_pos].to_string();
        if !is_valid_key_name(&name) {
            index += 1;
            continue;
        }

        let after_colon = colon_pos + 1;
        let rest = &trimmed_line[after_colon..];

        let (value_range, style, ends_on_key_line) = classify_value(line_start, after_colon, rest);

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
                // Stop on a non-indented, non-blank line — that's the next top-level key.
                if !cont.starts_with([' ', '\t']) && !cont.trim().is_empty() {
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

fn is_valid_key_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
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
        '[' => {
            if let Some(close) = value_text.find(']') {
                return (
                    Some(value_start_byte..value_start_byte + close + 1),
                    ValueStyle::FlowSequence,
                    true,
                );
            }
            (None, ValueStyle::FlowSequence, false)
        }
        '{' => {
            if let Some(close) = value_text.find('}') {
                return (
                    Some(value_start_byte..value_start_byte + close + 1),
                    ValueStyle::FlowMapping,
                    true,
                );
            }
            (None, ValueStyle::FlowMapping, false)
        }
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
        let content = "---\nworkspace: '[[vault-cli]]'\n---\n";
        let spans = top_level_property_spans(content, 4..30);
        let span = &spans[0];
        assert_eq!(span.name, "workspace");
        assert_eq!(span.style, ValueStyle::SingleQuoted);
        assert_eq!(
            &content[span.value_range.clone().unwrap()],
            "'[[vault-cli]]'"
        );
    }

    #[test]
    fn double_quoted_scalar_span_includes_quotes() {
        let content = "---\nworkspace: \"[[vault-cli]]\"\n---\n";
        let spans = top_level_property_spans(content, 4..30);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::DoubleQuoted);
        assert_eq!(
            &content[span.value_range.clone().unwrap()],
            "\"[[vault-cli]]\""
        );
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
