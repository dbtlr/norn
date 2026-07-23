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
/// # The scanner proposes, serde decides
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
/// - **(C) `line_range` boundaries are serde-confirmed, not scanner-guessed.**
///   The scanner detects candidate `key:` lines, but serde anchors values, not
///   spans, so a candidate line that is NOT a real serde key — a `- name:`
///   block-sequence-of-mappings item, a `key:`-looking line inside a multi-line
///   quoted/flow value — is a *phantom* that would truncate the preceding
///   field's range and orphan content on remove. So only a candidate whose name
///   resolves to exactly one serde key is a confirmed boundary; every other
///   candidate is absorbed into the preceding confirmed field. `line_range` runs
///   from a confirmed key-line to the next confirmed key-line (or block end),
///   so a `remove` deletes the whole property — including a list-of-mappings
///   block or an anchored block — never orphaning a tail.
///
/// **Ambiguity refuses.** If a serde key matches zero candidate lines, or more
/// than one candidate decodes to the same name (a key-escape collision such as
/// `"\x61"` + `x61` both decoding to `x61`), those keys get no editable span and
/// are not used as boundaries — `set`/`remove` cleanly reports not-editable
/// rather than splicing the wrong field.
///
/// Refusing (`value_range = None`) keeps the key visible and removable as a
/// block while declining an in-place value edit — the trust-preserving outcome:
/// a wrong span corrupts a document; a `None` merely declines an edit.
pub fn top_level_property_spans(
    content: &str,
    frontmatter_range: Range<usize>,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Vec<PropertySpan> {
    let candidates = scan_key_lines(content, &frontmatter_range);

    // Count how many candidate lines decode to each name. A name claimed by more
    // than one candidate is ambiguous (we cannot tell which line is the real
    // field), so it is neither editable nor a trustworthy boundary.
    let mut name_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for candidate in &candidates {
        *name_counts.entry(candidate.name.as_str()).or_insert(0) += 1;
    }

    // NRN-141 whole-document guard: a scanner span is only ever safe when the
    // scan agrees with serde across the ENTIRE block. If any serde key cannot be
    // located to exactly one candidate line — zero candidates (a key the scanner
    // mis-decodes, e.g. double-quoted `"\x61"` → serde `a`, scanner `x61`) or
    // more than one — then the scan and the parser disagree somewhere, and the
    // mis-located key's bytes would be absorbed into a neighbor's `line_range`
    // and silently deleted on `remove`/`set`. There is no safe per-field split
    // (the record's decision is whole-document), so refuse spans for everything:
    // `apply` then reports each field not-editable and never writes a corrupt
    // document. NRN-140 replaces this scanner with a real span parser.
    let every_key_uniquely_located = object
        .keys()
        .all(|key| name_counts.get(key.as_str()).copied() == Some(1));
    if !every_key_uniquely_located {
        return Vec::new();
    }

    // (A)+(C) A candidate is CONFIRMED — a real field and a trustworthy
    // `line_range` boundary — iff its name is a serde key. The whole-document
    // guard above already refused unless EVERY serde key is claimed by exactly
    // one candidate, so here a `contains_key` match is necessarily that unique
    // candidate; a per-candidate count check would be redundant. Phantoms (a
    // `- name:` item, a `key:`-lookalike inside a value) are not serde keys, so
    // they drop out and are absorbed into the preceding confirmed field's range
    // rather than truncating it. Collisions are the guard's to refuse, not this
    // filter's.
    let confirmed: Vec<&RawKeyLine> = candidates
        .iter()
        .filter(|c| object.contains_key(&c.name))
        .collect();

    let mut spans: Vec<PropertySpan> = Vec::with_capacity(confirmed.len());
    for (i, field) in confirmed.iter().enumerate() {
        // (C) line_range spans this confirmed key-line to the NEXT confirmed
        // key-line (or the frontmatter end), absorbing every phantom /
        // continuation / blank / comment line between them by construction.
        let line_end = confirmed
            .get(i + 1)
            .map(|next| next.key_line_start)
            .unwrap_or(frontmatter_range.end);
        let line_range = field.key_line_start..line_end;

        let serde_value = object
            .get(&field.name)
            .expect("a confirmed candidate is a serde key");

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
            index = absorb_open_value(&lines, index, proposed_style, rest);
        }
    }

    fields
}

/// Advances past the continuation lines of a value that opened on the key line
/// but did not close there — an unclosed flow collection or quoted scalar, whose
/// continuation lines can sit at column 0 and would otherwise be misread as new
/// keys. Everything else (block scalars, plain folds, block collections) puts
/// its continuation on indented lines the main scan already skips, so this is a
/// no-op for those.
///
/// A flow collection's closer (`]`/`}`) is matched only OUTSIDE a quoted scalar:
/// a `]` inside `"b]c"` or a `}` inside `a: "}"` is structural to the item, not
/// the flow, so stopping there would leave an interior `key:`-shaped line to be
/// misread as a phantom candidate (NRN-141). `key_line_rest` — the key line's
/// bytes after the `:` separator — seeds the flow quote state, so a quote opened
/// on the key line (`foo: ["a,`) is still open on the first continuation line.
/// A quoted-scalar value keeps the prior best-effort "stop at the first line
/// containing the closing quote".
fn absorb_open_value(
    lines: &[&str],
    index: usize,
    style: ValueStyle,
    key_line_rest: &str,
) -> usize {
    match style {
        ValueStyle::FlowSequence => absorb_flow_value(lines, index, b']', key_line_rest),
        ValueStyle::FlowMapping => absorb_flow_value(lines, index, b'}', key_line_rest),
        ValueStyle::SingleQuoted => absorb_until_line_contains(lines, index, '\''),
        ValueStyle::DoubleQuoted => absorb_until_line_contains(lines, index, '"'),
        _ => index,
    }
}

/// Best-effort absorb for an unclosed quoted scalar: stop after the first line
/// that contains the closing quote character. The pre-NRN-141 behavior for the
/// quoted-scalar styles, preserved exactly.
fn absorb_until_line_contains(lines: &[&str], mut index: usize, closer: char) -> usize {
    while index < lines.len() {
        let contains_closer = lines[index].contains(closer);
        index += 1;
        if contains_closer {
            break;
        }
    }
    index
}

/// Absorb the continuation lines of an unclosed flow collection, stopping after
/// the first line that contains `closer` OUTSIDE a quoted scalar. Quote state
/// (single-quote with `''` doubling, double-quote with `\` escapes) is tracked
/// across the absorbed lines, so a closer shadowed inside a quoted item does not
/// terminate the value early (NRN-141). The state is seeded by scanning
/// `key_line_rest` first: a quote opened on the key line after the bracket
/// (`foo: ["a,`) is still open on the first continuation line, and seeding
/// "outside quotes" there inverts every quote toggle that follows — misreading a
/// closing `"` as opening (skipping the real closer) or a shielded closer as
/// real (re-exposing an interior phantom line). The seed pass cannot itself find
/// the closer: absorb only runs when the key line contains no closer byte at all
/// (`ends_on_key_line` was false).
fn absorb_flow_value(lines: &[&str], mut index: usize, closer: u8, key_line_rest: &str) -> usize {
    let mut in_single = false;
    let mut in_double = false;
    scan_flow_line(
        key_line_rest.as_bytes(),
        closer,
        &mut in_single,
        &mut in_double,
    );
    while index < lines.len() {
        let found = scan_flow_line(
            lines[index].as_bytes(),
            closer,
            &mut in_single,
            &mut in_double,
        );
        index += 1;
        if found {
            break;
        }
    }
    index
}

/// One line of the flow-absorb scan: advances the quote state across `bytes`
/// and reports whether `closer` occurred outside quotes. The single copy of the
/// flow quote-state machine — the key-line seed pass and the continuation-line
/// loop both run through here. An unquoted `#` at line start or preceded by
/// whitespace begins a YAML comment (legal inside a flow collection): the rest
/// of the line is comment text, so scanning stops there — a quote or closer
/// inside a comment is neither a quote toggle nor a real closer.
fn scan_flow_line(bytes: &[u8], closer: u8, in_single: &mut bool, in_double: &mut bool) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if *in_single {
            // A doubled `''` is an escaped quote, still inside the scalar.
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                *in_single = false;
            }
        } else if *in_double {
            if b == b'\\' {
                // Skip the escaped byte (e.g. `\"`).
                i += 2;
                continue;
            }
            if b == b'"' {
                *in_double = false;
            }
        } else if b == b'\'' {
            *in_single = true;
        } else if b == b'"' {
            *in_double = true;
        } else if b == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t')) {
            // Comment through end of line; quote state is unaffected by it.
            return false;
        } else if b == closer {
            return true;
        }
        i += 1;
    }
    false
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
/// pipeline `extract_frontmatter` uses, so representations match serde's
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
        // Flow collections are never precisely value-spanned — quotes, nesting,
        // and trailing content all defeat a byte scan. They carry
        // `value_range = None` and, for sequences, route through whole-field
        // `line_range` replacement in `apply` (`is_sequence_style`), so a flow
        // list stays editable without fragile flow parsing. `ends_on_key_line`
        // is whether the closer appears on the key line; an unclosed flow spills
        // onto continuation lines that `absorb_open_value` steps over.
        '[' => (None, ValueStyle::FlowSequence, value_text.contains(']')),
        '{' => (None, ValueStyle::FlowMapping, value_text.contains('}')),
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

    /// Parses `content[range]` into the serde map and locates spans —
    /// the same wiring `apply` uses (parse → `top_level_property_spans`).
    fn spans_for(content: &str, range: Range<usize>) -> Vec<PropertySpan> {
        let object = serde_map(content, range.clone());
        top_level_property_spans(content, range, &object)
    }

    fn serde_map(content: &str, range: Range<usize>) -> serde_json::Map<String, serde_json::Value> {
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
        // NRN-133 round 3: flow sequences are no longer precisely value-spanned
        // (a quote/nest/trailing-text inside `[...]` defeats a byte scan). They
        // carry value_range = None and are `set` via whole-`line_range` replace;
        // style is preserved so `apply` keeps the flow form.
        let content = "---\naliases: [a, b]\n---\n";
        let spans = spans_for(content, 4..20);
        let span = &spans[0];
        assert_eq!(span.style, ValueStyle::FlowSequence);
        assert!(span.value_range.is_none());
        assert_eq!(&content[span.line_range.clone()], "aliases: [a, b]\n");
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
}

/// NRN-133 span-locator matrix: the scanner proposes, serde vetoes. Grounded in
/// the YAML 1.2 spec (no third-party corpus). Every input is real, parseable
/// frontmatter — the locator only ever runs after `extract_frontmatter`
/// succeeds — so the serde map is built the same way `apply` builds it.
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

    /// Builds the serde map from the frontmatter, exactly as `apply` does.
    fn serde_map(content: &str) -> serde_json::Map<String, serde_json::Value> {
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
        top_level_property_spans(content, fm_range(content), &serde_map(content))
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
    fn flat_flow_sequence_refuses_precise_span_but_is_line_editable() {
        // Round 3: flow is not precisely value-spanned; set routes through the
        // whole line_range (see apply::is_sequence_style). Style is preserved.
        let content = "---\ntags: [a, b]\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::FlowSequence);
        assert!(s[0].value_range.is_none());
        assert_eq!(&content[s[0].line_range.clone()], "tags: [a, b]\n");
    }

    #[test]
    fn quoted_element_flow_sequence_refuses_precise_span() {
        // The regression this round fixes: a quote inside `[...]` used to
        // over-refuse the whole field. Now flow is line-editable regardless.
        let content = "---\ntags: [\"a\", \"b\"]\n---\n";
        let s = spans(content);
        assert_eq!(s[0].style, ValueStyle::FlowSequence);
        assert!(s[0].value_range.is_none());
        assert_eq!(
            serde_map(content)
                .get("tags")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
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
        assert_eq!(
            serde_map(content).get("key"),
            Some(&serde_json::Value::Null)
        );
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
    /// calls `apply_merge`). So the locator follows serde: `<<` is a
    /// normal mapping-valued field — refused for `set` (`value_range` None) and
    /// removable as its own line — exactly consistent with what `validate`/`find`
    /// see. Nothing is silently dropped, because in norn's model there are no
    /// separately-inherited fields to drop. Emitting it is *safer* than omitting
    /// it: omission would let `find --has "<<"` show the key while `remove` claims
    /// it absent.
    #[test]
    fn bug7_merge_key_follows_serde_as_a_mapping_field() {
        let content = "---\ndefaults: &d\n  x: 1\n<<: *d\nname: y\n---\n";
        let o = serde_map(content);
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
            "remove is line-scoped and consistent with serde"
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
        let o = serde_map(content);
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

    /// #9 (now subsumed): flow is never precisely value-spanned, so trailing
    /// text after `]` can no longer cause a wrong span — every flow sequence
    /// carries `value_range = None` and is edited via whole-`line_range` replace.
    #[test]
    fn bug9_flow_is_never_precisely_spanned() {
        for content in [
            "---\ntags: [a, b]\n---\n",
            "---\ntags: [a, [b]]\n---\n",
            "---\ntags: [\"a\", \"b\"]\n---\n",
        ] {
            let s = spans(content);
            assert!(
                s[0].value_range.is_none(),
                "flow value must not be precisely spanned: {content:?}"
            );
            assert_eq!(s[0].style, ValueStyle::FlowSequence);
        }
    }

    // ---- Round 3: serde-confirmed line_range boundaries -------------------

    /// PHANTOM #1: a no-indent list of mappings. The `- name:` item lines parse
    /// as `- name` — NOT a serde key — so they are absorbed into `contacts`'s
    /// line_range instead of truncating it. A `remove contacts` deletes the whole
    /// block; `owner` is a clean separate span.
    #[test]
    fn phantom_list_of_mappings_line_range_covers_whole_block() {
        let content = "---\ncontacts:\n- name: Bob\n- name: Alice\nowner: x\n---\n";
        let s = spans(content);
        let contacts = find(&s, "contacts").unwrap();
        assert_eq!(
            &content[contacts.line_range.clone()],
            "contacts:\n- name: Bob\n- name: Alice\n",
            "remove must delete every item line, not orphan them"
        );
        assert!(
            find(&s, "- name").is_none(),
            "`- name` is a phantom, not a field"
        );
        assert!(find(&s, "owner").is_some());
    }

    /// PHANTOM #2: a multi-line quoted value whose continuation looks like a key
    /// (`word: ...`). It is absorbed into `desc`'s line_range; `word` is not a
    /// field; `next` stays a clean separate span.
    #[test]
    fn phantom_key_inside_multiline_quoted_value_is_absorbed() {
        let content = "---\ndesc: 'line one\nword: two'\nnext: keep\n---\n";
        let s = spans(content);
        let desc = find(&s, "desc").unwrap();
        assert!(desc.value_range.is_none());
        assert_eq!(
            &content[desc.line_range.clone()],
            "desc: 'line one\nword: two'\n"
        );
        assert!(find(&s, "word").is_none());
        assert!(find(&s, "next").is_some());
    }

    /// COLLISION: two candidate lines decode to the same name (`"\x61"` → `x61`
    /// by the scanner, and a literal `x61`), while serde's real keys are `a` and
    /// `x61`. Both x61-decoding candidates are ambiguous → neither is editable
    /// nor a boundary, and `a` (matched by zero candidates) is not editable —
    /// so `set x61=NEW` can never clobber serde's `a`.
    #[test]
    fn key_escape_collision_refuses_both_ambiguous_keys() {
        let content = "---\n\"\\x61\": v\nx61: w\n---\n";
        let o = serde_map(content);
        // Precondition: serde sees two distinct keys, `a` and `x61`.
        assert!(
            o.contains_key("a") && o.contains_key("x61"),
            "keys: {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        // No confident span for either colliding/unmatched key.
        assert!(
            find(&s, "x61").is_none(),
            "ambiguous x61 must not be editable"
        );
        assert!(
            find(&s, "a").is_none(),
            "unmatched `a` must not be editable"
        );
    }

    /// NRN-141 GUARD (zero candidates): `"\x61"` decodes to serde `a`, but the
    /// scanner decodes the key line as `x61`, so serde key `a` matches no
    /// candidate line. Without a whole-document refusal, `a`'s bytes would be
    /// absorbed into the preceding confirmed field (`title`) — a `remove`/`set`
    /// of `title` would then silently delete the unrelated real `a`. The scanner
    /// must instead refuse spans for the whole document.
    #[test]
    fn unlocatable_serde_key_refuses_whole_document() {
        let content = "---\ntitle: hi\n\"\\x61\": 1\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("a") && o.contains_key("title"),
            "precondition: serde sees keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        assert!(
            s.is_empty(),
            "a serde key with no candidate line must refuse the whole doc, got {s:?}"
        );
    }

    /// NRN-141 GUARD (whole-doc, not per-field): a key-escape collision (`"\x61"`
    /// and a literal `x61` both scanner-decode to `x61`; serde sees distinct keys
    /// `a` and `x61`) sits above a perfectly well-formed `keep`. Pre-guard, `keep`
    /// resolved to exactly one candidate and stayed editable — but `a` is
    /// unlocatable, so trusting `keep`'s boundaries means trusting a scan that
    /// already disagrees with serde elsewhere in the block. The refusal decision
    /// is whole-document: `keep` gets no span either.
    #[test]
    fn collision_plus_wellformed_field_refuses_whole_document() {
        let content = "---\n\"\\x61\": v\nx61: w\nkeep: z\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("a") && o.contains_key("x61") && o.contains_key("keep"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        assert!(
            s.is_empty(),
            "an unlocatable sibling must refuse the whole doc, including well-formed `keep`, got {s:?}"
        );
    }

    // ---- The core invariant (B), proven over the whole matrix -------------

    /// NRN-141 quote-aware absorb: a flow mapping whose closing `}` is shadowed
    /// inside a quoted scalar (`a: "}"`) must not terminate the absorb early. The
    /// buggy absorb stopped at that quoted `}`, exposing the interior `next: 1` as
    /// a second `next` candidate (`name_counts[next] == 2` → whole-doc refusal).
    /// The fix steps over the quoted `}`, so the top-level `title`, `meta`, and
    /// the single `next` (value `2`) are each uniquely located and editable.
    #[test]
    fn quote_aware_absorb_does_not_expose_interior_flow_map_key() {
        let content = "---\ntitle: hi\nmeta: {\na: \"}\",\nnext: 1\n}\nnext: 2\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("title") && o.contains_key("meta") && o.contains_key("next"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        assert!(
            find(&s, "title").is_some(),
            "the well-formed sibling must stay editable"
        );
        let next = find(&s, "next").expect("the single top-level next must be located");
        assert_eq!(
            &content[next.value_range.clone().unwrap()],
            "2",
            "next must point at the top-level value 2, not the interior 1"
        );
    }

    /// NRN-141 quote-aware absorb: the V2 phantom doc. `foo: [a,` / `"b]c",` /
    /// `bar: v]` is a single flow sequence (its last element is a nested
    /// `{bar: v}`); the only top-level keys are `foo` and `bar` (the latter from
    /// `"\x62ar"`, which the scanner mis-decodes as `x62ar`). With the buggy
    /// absorb the `]` inside `"b]c"` stopped the scan, exposing the interior
    /// `bar: v]` as a phantom `bar` that stood in for the mis-decoded real key —
    /// the guard passed and a `remove bar` corrupted the doc. Quote-aware absorb
    /// steps over the quoted `]`, so serde key `bar` has zero candidates and the
    /// whole document refuses (empty spans).
    #[test]
    fn quote_aware_absorb_v2_phantom_leaves_serde_key_unlocatable() {
        let content = "---\nfoo: [a,\n\"b]c\",\nbar: v]\n\"\\x62ar\": realvalue\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("foo") && o.contains_key("bar"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        assert!(
            s.is_empty(),
            "serde key `bar` has no candidate line, so the whole doc must refuse, got {s:?}"
        );
    }

    /// NRN-141 round 2 (a): a double quote opened on the KEY line after the
    /// bracket. The continuation's `"` is a CLOSING quote, so the `]` after it
    /// is a real closer — the absorb must stop there and `title` must stay a
    /// separate editable field. Seeding the quote state at the first
    /// continuation line (ignoring the key line) misread that `"` as opening,
    /// skipped the real `]`, and absorbed `title` → whole-doc refusal of a
    /// valid document.
    #[test]
    fn flow_absorb_seeds_quote_state_from_key_line() {
        let content = "---\nfoo: [\"a,\nb\", c]\ntitle: hi\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("foo") && o.contains_key("title"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        let title = find(&s, "title").expect("title must not be absorbed into foo");
        assert_eq!(&content[title.value_range.clone().unwrap()], "hi");
        let foo = find(&s, "foo").expect("foo must be located");
        assert_eq!(
            &content[foo.line_range.clone()],
            "foo: [\"a,\nb\", c]\n",
            "foo's range covers exactly the flow value's lines"
        );
    }

    /// NRN-141 round 2 (b): the quote opened on the key line spans the
    /// continuation, shielding the `]` inside `b]: c` — it must NOT terminate
    /// the absorb. With the seeded state, the interior `phantom: v]` line stays
    /// absorbed into `tags`, so the mis-decoded serde key `phantom` (from
    /// `"\x70hantom"`, scanner-decoded `x70hantom`) has zero candidates and the
    /// whole document refuses. The unseeded absorb stopped at the shielded `]`
    /// and re-exposed the phantom as `phantom`'s candidate — the V2 class.
    #[test]
    fn flow_absorb_key_line_quote_shields_continuation_closer() {
        let content = "---\ntags: [\"a,\nb]: c\",\nphantom: v]\n\"\\x70hantom\": real\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("tags") && o.contains_key("phantom"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        assert!(
            s.is_empty(),
            "serde key `phantom` must have no candidate line (whole-doc refusal), got {s:?}"
        );
    }

    /// NRN-141 round 3 (a): a trailing comment on the KEY line inside an
    /// unclosed flow (`foo: [ # "x`) — comments are legal in flow context, and
    /// the `"` inside the comment is comment text, not a quote opener. Scanning
    /// it as content set the double-quote state, shielded the real `]` on the
    /// continuation, and absorbed `title` → false whole-doc refusal of a valid
    /// document. The scan must stop at an unquoted whitespace-preceded `#`.
    #[test]
    fn flow_absorb_ignores_key_line_comment() {
        let content = "---\nfoo: [ # \"x\n  a, b ]\ntitle: hi\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("foo") && o.contains_key("title"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        let title = find(&s, "title").expect("title must not be absorbed into foo");
        assert_eq!(&content[title.value_range.clone().unwrap()], "hi");
    }

    /// NRN-141 round 3 (b): a comment on a CONTINUATION line containing a quote
    /// (`b, # it's`) — the `'` is comment text; scanning it as content set the
    /// single-quote state and shielded the real `]` on the next line, absorbing
    /// `title`. The comment-aware scan stops at the `#`, so the closer is found.
    #[test]
    fn flow_absorb_ignores_continuation_line_comment() {
        let content = "---\nfoo: [ a,\n  b, # it's\n  c ]\ntitle: hi\n---\n";
        let o = serde_map(content);
        assert!(
            o.contains_key("foo") && o.contains_key("title"),
            "precondition: serde keys {:?}",
            o.keys().collect::<Vec<_>>()
        );
        let s = spans(content);
        let title = find(&s, "title").expect("title must not be absorbed into foo");
        assert_eq!(&content[title.value_range.clone().unwrap()], "hi");
        let foo = find(&s, "foo").expect("foo must be located");
        assert_eq!(
            &content[foo.line_range.clone()],
            "foo: [ a,\n  b, # it's\n  c ]\n",
            "foo's range covers exactly the flow value's lines"
        );
    }

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
            let o = serde_map(content);
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
