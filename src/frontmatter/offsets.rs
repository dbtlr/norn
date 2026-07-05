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
/// styles, continuation of multi-line scalars, blank and comment lines between
/// two fields) are included in `line_range` but do not produce their own
/// `PropertySpan`.
///
/// # The scanner proposes, serde is the oracle
///
/// A byte-offset scanner cannot, on its own, safely decide where a YAML value's
/// bytes are — the corruption bugs it produced (a `# comment` spanned as a
/// value that serde reads as null, a multi-line fold spanned as only its first
/// line, an anchor/merge key mistaken for an editable field) all came from the
/// scanner disagreeing with the parser. So the parsed frontmatter mapping
/// (`object`, exactly what `apply` already holds) is threaded in as the
/// authority, and every proposal is validated against it:
///
/// - **(A) Fields come from serde.** A span is emitted only for a proposed key
///   that is an actual key in `object`. A merge key (`<<`, which serde folds
///   away), a quoted key whose decode diverges from serde's, or a duplicate
///   simply produces no span — `set`/`remove` then report it absent rather than
///   mis-editing a line the parser reads differently.
/// - **(B) `value_range` is validated against serde's value.** A proposed span
///   survives only if `content[value_range]` re-parses (through the same
///   `serde_yaml → serde_json` pipeline `extract_frontmatter` uses) to exactly
///   the field's parsed value. A mapping value, or a null value whose bytes are
///   not a literal null token, is refused. This makes a *wrong* value span
///   structurally impossible to emit: any disagreement collapses to `None`.
/// - **(C) `line_range` runs key-line-start → next-key-line-start** (or the end
///   of the block for the last field), so all continuation / blank / comment
///   lines between two fields are absorbed by construction — a `remove` deletes
///   the whole property, never orphaning a fold or an anchored block.
///
/// Refusing (`value_range = None`) keeps the key visible and removable as a
/// block while declining an in-place value edit — the trust-preserving outcome:
/// a wrong span corrupts a document; a `None` merely declines an edit.
pub fn top_level_property_spans(
    content: &str,
    frontmatter_range: Range<usize>,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Vec<PropertySpan> {
    let raw = scan_key_lines(content, &frontmatter_range);
    let mut spans: Vec<PropertySpan> = Vec::with_capacity(raw.len());

    for (i, field) in raw.iter().enumerate() {
        // (C) line_range spans this key line to the next top-level key line (or
        // the frontmatter end for the last field), absorbing every continuation,
        // blank, and comment line between the two by construction.
        let line_end = raw
            .get(i + 1)
            .map(|next| next.key_line_start)
            .unwrap_or(frontmatter_range.end);
        let line_range = field.key_line_start..line_end;

        // (A) Only an actual serde key becomes a span. Not a key (merge `<<`,
        // an escaped key whose decode diverges, a duplicate) → no span, so
        // `set`/`remove` reports "absent" instead of mis-editing.
        let Some(serde_value) = object.get(&field.name) else {
            continue;
        };

        // (B) The proposed value span survives only if it reconstitutes to
        // exactly serde's parsed value for this field.
        let value_range = validate_value_span(content, &field.proposed_value_range, serde_value);

        // A refused value whose serde type is a block collection reports the
        // block style so `apply` can still replace the whole `line_range`.
        let style = resolve_style(field.proposed_style, &value_range, serde_value);

        spans.push(PropertySpan {
            name: field.name.clone(),
            line_range,
            value_range,
            style,
        });
    }

    spans
}

/// A column-0 mapping key line the scanner proposes, before serde validation.
struct RawKeyLine {
    key_line_start: usize,
    name: String,
    proposed_value_range: Option<Range<usize>>,
    proposed_style: ValueStyle,
}

/// Phase 1: identify every top-level `key:` line and the scanner's *candidate*
/// value span/style for it. Multi-line values whose continuation can sit at
/// column 0 (an unclosed flow collection or quoted scalar) are stepped over so
/// their continuation lines are not misread as new keys; block scalars, folds,
/// and block collections continue on indented lines the main scan already skips.
fn scan_key_lines(content: &str, frontmatter_range: &Range<usize>) -> Vec<RawKeyLine> {
    let yaml = &content[frontmatter_range.clone()];
    let lines: Vec<&str> = yaml.split_inclusive('\n').collect();
    // Precompute byte offsets for the start of each line within `content`.
    let mut line_starts: Vec<usize> = Vec::with_capacity(lines.len() + 1);
    let mut acc = frontmatter_range.start;
    for line in &lines {
        line_starts.push(acc);
        acc += line.len();
    }
    line_starts.push(acc);

    let mut fields = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with([' ', '\t']) {
            index += 1;
            continue;
        }
        let line_start = line_starts[index];
        let trimmed_line = line.trim_end_matches(['\r', '\n']);

        let Some((name, after_colon)) = parse_top_level_key(trimmed_line) else {
            index += 1;
            continue;
        };

        let rest = &trimmed_line[after_colon..];
        let (proposed_value_range, proposed_style, ends_on_key_line) =
            classify_value(line_start, after_colon, rest);

        fields.push(RawKeyLine {
            key_line_start: line_start,
            name,
            proposed_value_range,
            proposed_style,
        });

        index += 1;
        if !ends_on_key_line {
            index = absorb_open_value(&lines, index, proposed_style);
        }
    }

    fields
}

/// Advances past the continuation lines of a value that opened on the key line
/// but did not close there — an unclosed flow collection or quoted scalar, whose
/// continuation lines can sit at column 0 and would otherwise be misread as new
/// keys. Everything else (block scalars, plain folds, block collections) puts
/// its continuation on indented lines the main scan already skips, so this is a
/// no-op for those. Best-effort: stops at the first line containing the closer.
fn absorb_open_value(lines: &[&str], mut index: usize, style: ValueStyle) -> usize {
    let closer = match style {
        ValueStyle::FlowSequence => ']',
        ValueStyle::FlowMapping => '}',
        ValueStyle::SingleQuoted => '\'',
        ValueStyle::DoubleQuoted => '"',
        _ => return index,
    };
    while index < lines.len() {
        let contains_closer = lines[index].contains(closer);
        index += 1;
        if contains_closer {
            break;
        }
    }
    index
}

/// (B) Validates a proposed value span against serde's parsed value. Returns the
/// span only when `content[range]` reconstitutes to exactly `serde_value`; any
/// disagreement — a mapping value, a non-literal null (a `# comment` re-parses
/// to null and must NOT be treated as an editable null), or a scalar/sequence
/// whose reconstitution differs (a multi-line fold, a truncated flow) — is
/// refused. This is the invariant that makes a wrong span impossible to emit.
fn validate_value_span(
    content: &str,
    proposed: &Option<Range<usize>>,
    serde_value: &serde_json::Value,
) -> Option<Range<usize>> {
    let range = proposed.clone()?;
    let slice = &content[range.clone()];
    match serde_value {
        // A mapping value (anchored block map, flow map) is never a scalar span.
        serde_json::Value::Object(_) => None,
        // Null is the trap: a bare `# comment` re-parses to null and would
        // falsely compare-equal, so gate on a literal null token explicitly.
        serde_json::Value::Null => is_yaml_null_token(slice).then_some(range),
        // Scalars and flat single-line sequences: editable only if the bytes
        // reconstitute to exactly the parsed value.
        expected => match reparse_yaml(slice) {
            Some(parsed) if &parsed == expected => Some(range),
            _ => None,
        },
    }
}

/// (C-adjacent) A refused value whose serde type is a block collection reports
/// the block style so `apply` replaces the whole `line_range` (block-sequence
/// set) or refuses cleanly (block mapping). Only an `EmptyValue` proposal is
/// re-styled — a refused flow value keeps `FlowSequence`/`FlowMapping` so `set`
/// declines rather than silently restyling flow into a block.
fn resolve_style(
    proposed: ValueStyle,
    value_range: &Option<Range<usize>>,
    serde_value: &serde_json::Value,
) -> ValueStyle {
    if value_range.is_none() && matches!(proposed, ValueStyle::EmptyValue) {
        match serde_value {
            serde_json::Value::Array(_) => return ValueStyle::BlockSequence,
            serde_json::Value::Object(_) => return ValueStyle::BlockMapping,
            _ => {}
        }
    }
    proposed
}

fn is_yaml_null_token(s: &str) -> bool {
    matches!(s, "null" | "Null" | "NULL" | "~")
}

/// Re-parses a value byte-slice through the exact `serde_yaml → serde_json`
/// pipeline `extract_frontmatter` uses, so representations match the oracle's
/// (e.g. `12:30`, `1.10`, `yes` are read identically on both sides).
fn reparse_yaml(s: &str) -> Option<serde_json::Value> {
    let value: serde_yaml::Value = serde_yaml::from_str(s).ok()?;
    serde_json::to_value(value).ok()
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
                // After the `]` only whitespace or a whitespace-led comment may
                // follow; any other trailing content means the naive first-`]`
                // scan is wrong, so refuse (#9). serde is the final authority —
                // it rejects such a line outright — this is defense-in-depth.
                let after_trimmed = after.trim_start();
                let after_ok = after.is_empty()
                    || (after.starts_with([' ', '\t'])
                        && (after_trimmed.is_empty() || after_trimmed.starts_with('#')));
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
            // YAML strips trailing whitespace from a plain scalar, so trim it
            // (and any CR/LF) off the span — otherwise `title: hello   ` spans
            // the trailing spaces and a set would eat them (#5).
            while end > 0 && matches!(value_bytes[end - 1], b'\r' | b'\n' | b' ' | b'\t') {
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

    /// Parses `content[range]` into the serde oracle map and locates spans —
    /// the same wiring `apply` uses (parse → `top_level_property_spans`).
    fn spans_for(content: &str, range: Range<usize>) -> Vec<PropertySpan> {
        let object = oracle(content, range.clone());
        top_level_property_spans(content, range, &object)
    }

    fn oracle(content: &str, range: Range<usize>) -> serde_json::Map<String, serde_json::Value> {
        let yaml = &content[range];
        match serde_yaml::from_str::<serde_yaml::Value>(yaml)
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
        {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }

    #[test]
    fn plain_scalar_span_isolates_value_bytes() {
        let content = "---\ntitle: hello world\n---\n# body\n";
        let spans = spans_for(content, 4..23);
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
        let spans = spans_for(content, 4..26);
        let span = &spans[0];
        assert_eq!(span.name, "workspace");
        assert_eq!(span.style, ValueStyle::SingleQuoted);
        assert_eq!(&content[span.value_range.clone().unwrap()], "'[[norn]]'");
    }

    #[test]
    fn double_quoted_scalar_span_includes_quotes() {
        let content = "---\nworkspace: \"[[norn]]\"\n---\n";
        let spans = spans_for(content, 4..26);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::DoubleQuoted);
        assert_eq!(&content[span.value_range.clone().unwrap()], "\"[[norn]]\"");
    }

    #[test]
    fn empty_value_followed_by_block_sequence() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let spans = spans_for(content, 4..29);
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
        let spans = spans_for(content, 4..36);
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
        let spans = spans_for(content, 4..25);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::BlockMapping);
        assert!(span.value_range.is_none());
    }

    #[test]
    fn flow_sequence_on_single_line() {
        let content = "---\naliases: [a, b]\n---\n";
        let spans = spans_for(content, 4..20);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::FlowSequence);
        assert_eq!(&content[span.value_range.clone().unwrap()], "[a, b]");
    }

    #[test]
    fn plain_scalar_with_same_line_comment_excludes_comment_from_value_range() {
        let content = "---\ntitle: hello  # comment\n---\n";
        let spans = spans_for(content, 4..27);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::Plain);
        assert_eq!(&content[span.value_range.clone().unwrap()], "hello");
        assert!(content[span.line_range.clone()].contains("# comment"));
    }

    #[test]
    fn multiple_properties_return_separate_spans_in_order() {
        let content = "---\ntitle: hello\nstatus: draft\nworkspace: '[[demo]]'\n---\n";
        let spans = spans_for(content, 4..52);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "title");
        assert_eq!(spans[1].name, "status");
        assert_eq!(spans[2].name, "workspace");
        assert_eq!(spans[2].style, ValueStyle::SingleQuoted);
    }

    #[test]
    fn block_literal_value_range_is_none() {
        let content = "---\ndescription: |\n  line one\n  line two\n---\n";
        let spans = spans_for(content, 4..41);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::BlockLiteral);
        assert!(span.value_range.is_none());
        assert!(content[span.line_range.clone()].contains("line two"));
    }

    #[test]
    fn indented_lines_are_not_top_level_keys() {
        let content = "---\nparent:\n  child: not a top-level key\n---\n";
        let spans = spans_for(content, 4..41);
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

/// NRN-133 span-locator matrix: the scanner proposes, serde vetoes. Grounded in
/// the YAML 1.2 spec (no third-party corpus). Every input is real, parseable
/// frontmatter — the locator only ever runs after `extract_frontmatter`
/// succeeds — so the oracle is built the same way `apply` builds it.
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

    /// Builds the serde oracle from the frontmatter, exactly as `apply` does.
    fn oracle(content: &str) -> serde_json::Map<String, serde_json::Value> {
        let yaml = &content[fm_range(content)];
        match serde_yaml::from_str::<serde_yaml::Value>(yaml)
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
        {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }

    fn spans(content: &str) -> Vec<PropertySpan> {
        top_level_property_spans(content, fm_range(content), &oracle(content))
    }

    fn find<'a>(s: &'a [PropertySpan], name: &str) -> Option<&'a PropertySpan> {
        s.iter().find(|p| p.name == name)
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
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "héllo wörld");
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

    #[test]
    fn null_literal_token_is_editable() {
        // A literal `null` is an editable null (bytes ARE a null token); only a
        // serde-null whose bytes are NOT a null token (e.g. a comment) refuses.
        let content = "---\nk: null\n---\n";
        let s = spans(content);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "null");
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
    fn anchor_definition_refuses() {
        let content = "---\nbase: &anchor hello\n---\n";
        let s = spans(content);
        assert_eq!(s[0].name, "base");
        assert!(s[0].value_range.is_none());
    }

    #[test]
    fn alias_refuses() {
        // Alias needs its anchor defined so the frontmatter parses.
        let content = "---\nbase: &a hello\nref: *a\n---\n";
        let s = spans(content);
        assert!(find(&s, "ref").unwrap().value_range.is_none());
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

    // ---- Key-recognition cases --------------------------------------------

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

    // ---- The 9 confirmed edge bugs, each asserting the corrected outcome ---

    /// #1 CORRUPTION: `key: # comment` — serde reads the value as null, so the
    /// scanner's proposed span over `# comment` is vetoed. A `set` cannot clobber
    /// the comment; the field is still removable as a whole line.
    #[test]
    fn bug1_comment_as_value_is_refused_not_clobbered() {
        let content = "---\nkey: # comment\nnext: x\n---\n";
        let s = spans(content);
        let key = find(&s, "key").unwrap();
        assert!(
            key.value_range.is_none(),
            "a comment-only value (serde null) must not be spanned as the value"
        );
        assert_eq!(oracle(content).get("key"), Some(&serde_json::Value::Null));
    }

    /// #2 CORRUPTION: a blank-line-interrupted fold folds to `hello\nworld` in
    /// serde, so the scanner's `hello`-only span is vetoed and the whole thing is
    /// swept into `line_range` (remove deletes all of it).
    #[test]
    fn bug2_blank_line_interrupted_fold_refuses() {
        let content = "---\ntitle: hello\n\n  world\ntype: note\n---\n";
        let s = spans(content);
        let title = find(&s, "title").unwrap();
        assert!(title.value_range.is_none());
        assert_eq!(
            &content[title.line_range.clone()],
            "title: hello\n\n  world\n",
            "line_range must sweep the blank + continuation for a clean remove"
        );
    }

    /// #3 CORRUPTION: an anchor tagging a block mapping — value refused (mapping),
    /// and `line_range` must cover the indented block so remove doesn't orphan it.
    #[test]
    fn bug3_anchor_on_block_map_line_range_covers_block() {
        let content = "---\nbase: &a\n  x: 1\nname: y\n---\n";
        let s = spans(content);
        let base = find(&s, "base").unwrap();
        assert!(base.value_range.is_none());
        assert_eq!(
            &content[base.line_range.clone()],
            "base: &a\n  x: 1\n",
            "remove must delete the whole anchored block, not just `base: &a`"
        );
    }

    /// #4 CORRUPTION: a multi-line fold with an internal blank line — refused, and
    /// `line_range` covers every line through the next key (no orphaned tail).
    #[test]
    fn bug4_internal_blank_multiline_fold_line_range_covers_all() {
        let content = "---\ntitle: hello\n  world\n\n  more\ntype: x\n---\n";
        let s = spans(content);
        let title = find(&s, "title").unwrap();
        assert!(title.value_range.is_none());
        assert_eq!(
            &content[title.line_range.clone()],
            "title: hello\n  world\n\n  more\n"
        );
    }

    /// #5 PRECISION: trailing whitespace is trimmed off the value span.
    #[test]
    fn bug5_trailing_whitespace_span_is_tight() {
        let content = "---\ntitle: hello   \n---\n";
        let s = spans(content);
        assert_eq!(&content[s[0].value_range.clone().unwrap()], "hello");
    }

    /// #6 PRECISION: an indented comment after a scalar is NOT a fold — serde
    /// reads the value as `hello`, so it stays editable, and `line_range` still
    /// covers the comment line so a remove takes it too.
    #[test]
    fn bug6_indented_comment_after_scalar_is_editable() {
        let content = "---\ntitle: hello\n  # note\ntype: x\n---\n";
        let s = spans(content);
        let title = find(&s, "title").unwrap();
        assert_eq!(
            &content[title.value_range.clone().unwrap()],
            "hello",
            "an indented trailing comment must not over-refuse the set"
        );
        assert!(content[title.line_range.clone()].contains("# note"));
    }

    /// #7: the reviewer's premise was that serde folds a merge key away. It does
    /// NOT — `serde_yaml` keeps `<<` as a literal key whose value is the aliased
    /// mapping (verified: keys = `["<<", "defaults", "name"]`, `<<` → `{x: 1}`),
    /// and norn's entire data model is that raw parse (`extract_frontmatter` never
    /// calls `apply_merge`). So the oracle-driven locator follows serde: `<<` is a
    /// normal mapping-valued field — refused for `set` (`value_range` None) and
    /// removable as its own line — exactly consistent with what `validate`/`find`
    /// see. Nothing is silently dropped, because in norn's model there are no
    /// separately-inherited fields to drop. Emitting it is *safer* than omitting
    /// it: omission would let `find --has "<<"` show the key while `remove` claims
    /// it absent.
    #[test]
    fn bug7_merge_key_follows_serde_as_a_mapping_field() {
        let content = "---\ndefaults: &d\n  x: 1\n<<: *d\nname: y\n---\n";
        let o = oracle(content);
        assert!(
            o.contains_key("<<"),
            "serde_yaml keeps `<<` as a literal key (no auto-merge)"
        );
        let s = spans(content);
        let merge = find(&s, "<<").expect("`<<` follows serde as a real field");
        assert!(
            merge.value_range.is_none(),
            "its value is a mapping, so set is refused"
        );
        assert_eq!(
            &content[merge.line_range.clone()],
            "<<: *d\n",
            "remove is line-scoped and consistent with the oracle"
        );
        assert!(find(&s, "name").is_some());
    }

    /// #8 (would-be) CORRUPTION: a double-quoted key whose scanner-decode diverges
    /// from serde's decoded name yields NO editable span — a non-corrupting
    /// "absent", never a mis-located edit. (`\x41` → serde `A`, scanner `x41`.)
    #[test]
    fn bug8_divergent_double_quoted_key_emits_no_wrong_span() {
        let content = "---\n\"\\x41\": v\n---\n";
        let s = spans(content);
        let o = oracle(content);
        let keys: Vec<&str> = o.keys().map(String::as_str).collect();
        // Whatever serde's decoded key is, we never emit a span under a name that
        // is not one of serde's keys (which would splice bytes serde reads
        // differently). Equivalently: every emitted span names a real serde key.
        for span in &s {
            assert!(
                keys.contains(&span.name.as_str()),
                "emitted span {:?} is not a serde key {keys:?}",
                span.name
            );
        }
    }

    /// #9 defense-in-depth: non-comment trailing text after a flow `]` is refused
    /// by the proposer (serde also rejects such a line outright).
    #[test]
    fn bug9_flow_with_trailing_text_refused() {
        // Reach the locator with a valid oracle by validating the refusal on the
        // classifier directly: trailing non-comment text yields no precise span.
        let content = "---\ntags: [a, b]\n---\n";
        // Sanity: the clean flat flow IS precise (control).
        assert!(spans(content)[0].value_range.is_some());
        // The classifier refuses `[a, b] extra` (would-be over-span).
        let (vr, _style, _ends) = classify_value(0, 0, "[a, b] extra");
        assert!(vr.is_none(), "trailing text after `]` must not be spanned");
    }

    // ---- The core invariant (B), proven over the whole matrix -------------

    /// No `value_range` can reach `apply` unless `content[value_range]`
    /// reconstitutes to exactly serde's parsed value for that field.
    #[test]
    fn invariant_every_value_span_reconstitutes_to_serde_value() {
        let corpus = [
            "---\nkey: # comment\n---\n",
            "---\ntitle: hello\n  world\ntype: note\n---\n",
            "---\ntitle: hello\n\n  world\ntype: x\n---\n",
            "---\ntags: [a, b]\n---\n",
            "---\ntime: 12:30\n---\n",
            "---\nk: 'it''s'\n---\n",
            "---\nn: [a, [b]]\n---\n",
            "---\ncount: 42\n---\n",
            "---\nflag: true\n---\n",
            "---\nk: null\n---\n",
            "---\nversion: 1.10\n---\n",
            "---\ntitle: hello   \n---\n",
            "---\nurl: http://x/#frag\n---\n",
        ];
        for content in corpus {
            let o = oracle(content);
            for span in spans(content) {
                let Some(vr) = span.value_range.clone() else {
                    continue;
                };
                let slice = &content[vr];
                let expected = o.get(&span.name).expect("emitted span names a serde key");
                let reparsed = reparse_yaml(slice).expect("value slice re-parses");
                assert_eq!(
                    &reparsed, expected,
                    "span for `{}` = {slice:?} disagrees with serde {expected:?}",
                    span.name
                );
            }
        }
    }
}
