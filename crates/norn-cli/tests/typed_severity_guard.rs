//! Typed-severity guard (NRN-407): severity and enum labels come from typed
//! fields, never from message text.
//!
//! Three static invariants, enforced across the live tree so they hold by
//! construction rather than by review:
//!
//! 1. **No message-text severity sniff.** A surface decides exit / `isError`
//!    from a typed [`Severity`](norn_wire::Severity) — the read report's
//!    [`Note`](norn_wire::Note) or the mutation report's outcome — never by
//!    matching a prose prefix like `error:` / `warning:`. So no `src/` may
//!    `starts_with` (or `contains`) one of the annotation prefixes: that is the
//!    text sniff this channel replaced.
//! 2. **No raw stderr-prefix emission.** Every `note:` / `warning:` / `error:`
//!    annotation on stderr is emitted through a `Conversation` constructor
//!    (`note`/`warning`/`error`/`report_note`) — never a hardcoded prefix
//!    literal handed straight to `writeln!`/`eprintln!`. Scoped to
//!    `src/display/render/` + `src/output/` (the CLI's presentation layer,
//!    the tree NRN-407's fix round converged onto the constructors) and to
//!    PRODUCTION source only: a test module legitimately asserts the
//!    rendered `"warning: ..."` text, which is not an emission site, so each
//!    `#[cfg(test)]`-annotated item's own body — not everything from its
//!    first occurrence to end-of-file — is excluded from the scan.
//!    `conversation.rs` — the one place these prefixes are
//!    legitimately literal — lives outside this scoped tree already. The
//!    needle requires the colon immediately after the bare word, so a
//!    stdout data payload like set-records' `"warnings: N"` (a plural
//!    count, not a singular annotation prefix) never false-positives, and
//!    only an actual `writeln!`/`eprintln!` call body is inspected, so
//!    `conv.line(&format!("error: {msg}"))` (`shared::render_refusal` and
//!    friends — an existing, undisturbed pattern, not in this fix round's
//!    scope) is not flagged either.
//! 3. **No `Debug` label in a renderer.** A user-facing enum label is the value's
//!    `#[serde(rename_all = "kebab-case")]` name via `display::serde_label`, not
//!    `Debug` — neither the positional `format!("{:?}", value)` nor its
//!    inline-capture sibling `format!("{value:?}")` (both derive the variant
//!    identifier and only accidentally lowercase). So no PRODUCTION line
//!    under `src/display/render/` may carry a `:?}` in either form. Test
//!    assertions legitimately print `Debug` for failure messages (`{err:?}`,
//!    `{s:?}`, …), so this too only scans each file's non-`#[cfg(test)]`
//!    body.
//!
//!    **Scope decision (NRN-448):** this scans `src/display/render/` only,
//!    matching the boundary `docs/architecture.md` invariant 2 states
//!    (`format!("{:?}")` never appears in DISPLAY code). The same `{:?}`
//!    shape also appears outside display code: `norn-core`'s
//!    `mutate/delete.rs` and `mutate/move_doc.rs` build an ambiguous-target
//!    refusal message by `Debug`-formatting the candidate path list
//!    (`{candidates:?}`). That is a `norn-core` message-construction site,
//!    not a renderer, so it sits outside this guard's scope; wording it to
//!    match `mutate/edit.rs` and `mutate/set.rs`'s comma-joined candidate
//!    list is a separate task, not this invariant's job.
//!
//! It lives in `tests/` (outside every scanned `src/` tree) so its own needle
//! literals are not scanned by invariant 1, and its `{:?}`/`:?}`-free source is
//! not scanned by invariant 3.

use std::fs;
use std::path::{Path, PathBuf};

/// Crates whose `src/` is exempt from the severity-sniff scan: the parity
/// harness legitimately classifies captured output text.
const SNIFF_EXEMPT_CRATES: &[&str] = &["norn-parity", "norn-fixtures"];

/// The forbidden message-text severity sniffs: matching an annotation prefix to
/// recover severity. Each is a `starts_with` / `contains` on a closed prefix.
const SNIFF_NEEDLES: &[&str] = &[
    "starts_with(\"error:",
    "starts_with(\"warning:",
    "starts_with(\"warn:",
    "starts_with(\"note:",
    "contains(\"error:\")",
    "contains(\"warning:\")",
    "contains(\"warn:\")",
    "contains(\"note:\")",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above the crate manifest dir")
        .to_path_buf()
}

fn scan_rs<F: FnMut(&Path, &str)>(dir: &Path, visit: &mut F) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan_rs(&path, visit);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read source file");
        visit(&path, &src);
    }
}

/// The PRODUCTION slice of a file's source — every `#[cfg(test)]`-annotated
/// item's own body excised, not everything from the first occurrence to
/// end-of-file. This codebase's convention is a single trailing `#[cfg(test)]
/// mod tests { .. }`, for which excising just that item's body is
/// indistinguishable from the old truncate-at-first-occurrence behavior. But a
/// file is not guaranteed to hold to that convention: an early `#[cfg(test)]`
/// helper (item, not the trailing module) followed by more production code
/// must not silently exit the scan — only the annotated item's own span is
/// test-only. Test modules legitimately assert against rendered annotation
/// text (`"warning: ..."`) and print `Debug` in failure messages (`{err:?}`);
/// neither is an emission site, so invariants 2 and 3 only scan what actually
/// ships.
///
/// When `cfg_test_item_end` cannot locate the item's end, this panics naming
/// `path` and the byte offset rather than silently stopping the scan: a
/// silent stop would drop every byte after that point from the
/// production-code scan invariants 2 and 3 run — a false pass in the
/// dangerous direction. A panic here means the guard test fails loudly
/// instead, which is the correct outcome for a source shape the scanner
/// cannot parse.
fn production_source(path: &Path, text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(idx) = rest.find("#[cfg(test)]") else {
            kept.push_str(rest);
            break;
        };
        kept.push_str(&rest[..idx]);
        let after_attr = idx + "#[cfg(test)]".len();
        match cfg_test_item_end(rest, after_attr) {
            Some(end) => rest = &rest[end..],
            None => {
                let offset = text.len() - rest.len() + after_attr;
                panic!(
                    "typed_severity_guard: {}: byte {offset} — could not locate the end \
                     (matching `}}` or `;`) of the #[cfg(test)] item starting there; the \
                     item-span scanner does not recognize this source shape. Extend \
                     `cfg_test_item_end` / `skip_lexical_token` to cover it rather than let \
                     this silently drop the remainder of the file from the production-code \
                     scan.",
                    path.display()
                );
            }
        }
    }
    kept
}

/// The byte offset just past the end of the item a `#[cfg(test)]` attribute
/// (found at `from`, right after the attribute) annotates: the matching `}`
/// of its first `{...}` block (`mod`/`fn`/`struct`/`impl`/…), or the `;` of a
/// brace-free statement item (`use`/`const`/…) — whichever delimiter is
/// reached first outside a string, char literal, raw string, line comment, or
/// block comment (`skip_lexical_token` steps over all five, so a `{`/`;`
/// inside one of them is never miscounted) AND at zero `(`/`)`/`[`/`]` depth,
/// so the `;` inside an array type or repeat expression (`[u8; 4]`, `fn
/// h(buf: [u8; 4])`) is never mistaken for the item's own terminator — that
/// would end the item too early and leave its real body (including the
/// closing `{...}`) misclassified as production code. Other attributes
/// stacked on the same item (`#[cfg(test)] #[allow(dead_code)] fn f() {..}`)
/// are skipped over transparently: their own `[`/`]` nest and unnest back to
/// zero depth before the item proper starts. Returns `None` when neither
/// delimiter is reached before end-of-input — including when a
/// string/comment/char literal that `skip_lexical_token` started never
/// terminates — and the caller (`production_source`) treats that as a fatal
/// scan desync, not a reason to stop quietly.
fn cfg_test_item_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    let mut bracket_depth = 0i32;
    let mut paren_depth = 0i32;
    while i < bytes.len() {
        if let Some(next) = skip_lexical_token(bytes, i) {
            i = next;
            continue;
        }
        let at_top_level = bracket_depth == 0 && paren_depth == 0;
        match bytes[i] {
            b'{' if at_top_level => return find_matching_brace(text, i).map(|close| close + 1),
            b';' if at_top_level => return Some(i + 1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth -= 1,
            b'(' => paren_depth += 1,
            b')' => paren_depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Find the `}` matching the `{` at byte offset `open`. Sibling of
/// [`find_matching_paren`]; both delegate to [`find_matching`].
fn find_matching_brace(text: &str, open: usize) -> Option<usize> {
    find_matching(text, open, b'{', b'}')
}

/// The stderr-emission macros a raw annotation-prefix literal must never
/// appear inside (invariant 2).
const RAW_EMISSION_MACROS: &[&str] = &["writeln!", "eprintln!"];

/// The closed stderr-annotation prefixes, as they appear at the START of a
/// string literal (leading `"`, trailing `: ` so a plural data count like
/// `"warnings: N"` never matches).
const RAW_PREFIX_NEEDLES: &[&str] = &["\"warning: ", "\"warn: ", "\"error: ", "\"note: "];

/// Find the `)` matching the `(` at byte offset `open`. Sibling of
/// [`find_matching_brace`]; both delegate to [`find_matching`].
fn find_matching_paren(text: &str, open: usize) -> Option<usize> {
    find_matching(text, open, b'(', b')')
}

/// Find the `close_byte` that matches the `open_byte` at byte offset `open`
/// (brace-for-brace or paren-for-paren, tracked via `depth`), skipping every
/// string, char literal, raw string, line comment, and block comment
/// encountered along the way (`skip_lexical_token`) so a delimiter inside any
/// of those five shapes is never miscounted. Returns `None` when `open` is
/// not `open_byte`, or when depth never returns to zero before end-of-input.
fn find_matching(text: &str, open: usize, open_byte: u8, close_byte: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&open_byte) {
        return None;
    }
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        if let Some(next) = skip_lexical_token(bytes, i) {
            i = next;
            continue;
        }
        let b = bytes[i];
        if b == open_byte {
            depth += 1;
        } else if b == close_byte {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Skips one lexical token that starts at byte offset `i`: a `//` line
/// comment (to end of line), a nesting-aware `/*` block comment, a raw string
/// (`r"..."` / `r#"..."#` / …, whose closer is matched by counting the same
/// number of trailing `#`), a `"..."` string (with backslash-escape
/// handling), or a char literal (`'{'`, `'"'`, `'\''`, `'\n'`, `'\xNN'`,
/// `'\u{...}'`). Returns the offset just past the token when one of these
/// five shapes starts at `i`, or `None` when `bytes[i]` is an ordinary byte
/// the caller inspects itself — including a `'` that opens a lifetime
/// (`'a`, `'static`) rather than a char literal: [`char_literal_end`] only
/// recognizes a char literal when a closing `'` is found within its
/// escape-or-single-character lookahead, so a lifetime is left alone.
///
/// An unterminated string, comment, or char literal is reported as running to
/// end-of-input rather than failing here (this function has no file path or
/// caller context to report against). The caller's own delimiter search then
/// fails to find its target and returns `None`, and `production_source`'s
/// fail-loud panic is what actually surfaces the desync — with the context
/// this function lacks.
fn skip_lexical_token(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes[i] {
        b'/' if bytes.get(i + 1) == Some(&b'/') => Some(skip_line_comment(bytes, i)),
        b'/' if bytes.get(i + 1) == Some(&b'*') => Some(skip_block_comment(bytes, i)),
        b'r' => raw_string_hash_count(bytes, i).map(|hashes| skip_raw_string(bytes, i, hashes)),
        b'"' => Some(skip_quoted_string(bytes, i)),
        b'\'' => char_literal_end(bytes, i),
        _ => None,
    }
}

/// End of the `//` line comment starting at `i`: the newline, or
/// end-of-input if the file has no trailing newline.
fn skip_line_comment(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j < bytes.len() && bytes[j] != b'\n' {
        j += 1;
    }
    j
}

/// End of the `/*` block comment starting at `i`, one past its matching `*/`.
/// Rust block comments nest, so this tracks nesting depth instead of stopping
/// at the first `*/`.
fn skip_block_comment(bytes: &[u8], i: usize) -> usize {
    let mut depth = 1u32;
    let mut j = i + 2;
    while j < bytes.len() && depth > 0 {
        if bytes[j..].starts_with(b"/*") {
            depth += 1;
            j += 2;
        } else if bytes[j..].starts_with(b"*/") {
            depth -= 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    j
}

/// End of the `"..."` string starting at its opening quote `i`, one past the
/// closing quote; a backslash-escaped quote (`\"`) does not close it early.
fn skip_quoted_string(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    j
}

/// `Some(hash_count)` when `bytes[i] == b'r'` opens a raw-string prefix
/// (`r"`, `r#"`, `r##"`, …): `i` is preceded by a non-identifier byte or is at
/// the start of the text (so a standalone `r`, not the tail of a longer
/// identifier like `r2d2`), and is followed by zero or more `#` and then a
/// `"`. `None` otherwise.
fn raw_string_hash_count(bytes: &[u8], i: usize) -> Option<usize> {
    if i > 0 {
        let prev = bytes[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let mut j = i + 1;
    let mut hashes = 0usize;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    (bytes.get(j) == Some(&b'"')).then_some(hashes)
}

/// End of the raw string opening at `i` (`bytes[i] == b'r'`) with `hashes`
/// leading `#` characters, one past the closing `"` followed by the same
/// number of `#`.
fn skip_raw_string(bytes: &[u8], i: usize, hashes: usize) -> usize {
    let content_start = i + 1 + hashes + 1; // `r` + `#`* + opening `"`
    let mut j = content_start;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            let remainder = &bytes[j + 1..];
            if remainder.len() >= hashes && remainder[..hashes].iter().all(|&b| b == b'#') {
                return j + 1 + hashes;
            }
        }
        j += 1;
    }
    bytes.len()
}

/// `Some(end)` when the `'` at `i` opens a char literal, one past its closing
/// `'`. `None` when it instead opens a lifetime (`'a`, `'static`): a lifetime
/// is never immediately followed by a closing `'` within the
/// escape-or-single-character lookahead this checks, so requiring that
/// closing `'` is what tells the two apart.
fn char_literal_end(bytes: &[u8], i: usize) -> Option<usize> {
    let after_quote = i + 1;
    if bytes.get(after_quote) == Some(&b'\\') {
        let escape = *bytes.get(after_quote + 1)?;
        let end_of_escape = match escape {
            b'x' => after_quote + 4,                              // `\xHH`
            b'u' => skip_unicode_escape(bytes, after_quote + 2)?, // `\u{...}`
            _ => after_quote + 2,                                 // `\\`, `\'`, `\n`, `\0`, ...
        };
        (bytes.get(end_of_escape) == Some(&b'\'')).then_some(end_of_escape + 1)
    } else {
        let ch_len = utf8_char_len(*bytes.get(after_quote)?);
        let close = after_quote + ch_len;
        (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
    }
}

/// End of a `\u{...}` escape's `{...}` body, whose `{` is expected at `open`:
/// one past the closing `}`, or `None` if `open` is not `{` or `}` is never
/// found.
fn skip_unicode_escape(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut j = open + 1;
    while bytes.get(j).is_some_and(|&b| b != b'}') {
        j += 1;
    }
    bytes.get(j)?;
    Some(j + 1)
}

/// Byte length of the UTF-8 character whose first byte is `first`.
fn utf8_char_len(first: u8) -> usize {
    if first & 0b1000_0000 == 0 {
        1
    } else if first & 0b1110_0000 == 0b1100_0000 {
        2
    } else if first & 0b1111_0000 == 0b1110_0000 {
        3
    } else {
        4
    }
}

/// The balanced-paren argument body of every `writeln!(...)` / `eprintln!(...)`
/// call in `text` (a raw stderr-emission call site, invariant 2's target).
/// `path` is diagnostic-only, named in the panic if a call's matching `)`
/// cannot be located — a source shape `find_matching_paren` cannot parse
/// stops the scan here loudly rather than silently omitting the rest of the
/// file's macro calls from invariant 2.
fn macro_call_bodies<'a>(path: &Path, text: &'a str) -> Vec<&'a str> {
    let mut bodies = Vec::new();
    for macro_name in RAW_EMISSION_MACROS {
        let mut cursor = 0;
        while let Some(rel) = text[cursor..].find(macro_name) {
            let open = cursor + rel + macro_name.len();
            match find_matching_paren(text, open) {
                Some(close) => {
                    bodies.push(&text[open + 1..close]);
                    cursor = close + 1;
                }
                None => panic!(
                    "typed_severity_guard: {}: byte {open} — could not locate the closing `)` \
                     of a `{macro_name}(...)` call; the paren-matching scanner does not \
                     recognize this source shape. Extend `skip_lexical_token` to cover it \
                     rather than let this silently drop the rest of the file's macro calls \
                     from invariant 2.",
                    path.display()
                ),
            }
        }
    }
    bodies
}

/// Sensitivity check (1/2, NRN-448): an early `#[cfg(test)]` item — not the
/// file's trailing test module — must exclude only its own body. Production
/// code that follows it, including a would-be `:?}` violation, stays IN
/// scope; a truncate-at-first-occurrence scan would have missed it entirely.
#[test]
fn production_source_keeps_code_after_an_early_cfg_test_item_in_scope() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod early_helpers {\n    \
                   fn t() { let _ = format!(\"{:?}\", 1); }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn a() {}") && production.contains("fn b()"),
        "production code before AND after the early item must survive: {production:?}"
    );
    assert!(
        !production.contains("early_helpers"),
        "the cfg(test) item's own body must still be excluded: {production:?}"
    );
    assert!(
        production.contains(":?}"),
        "fn b's Debug placeholder is production code and must be visible to invariant 3: \
         {production:?}"
    );
}

/// Sensitivity check (2/2, NRN-448): the common convention — one trailing
/// `#[cfg(test)] mod tests { .. }` at end of file — still excises the whole
/// test module, matching the pre-hardening truncate-at-first-occurrence
/// result for that shape.
#[test]
fn production_source_still_excludes_a_trailing_cfg_test_module() {
    let src = "fn a() { let _ = 1; }\n\
               #[cfg(test)]\n\
               mod tests {\n    \
                   fn t() { let _ = format!(\"{:?}\", 1); }\n\
               }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(production.contains("fn a()"));
    assert!(
        !production.contains(":?}"),
        "the trailing test module's Debug placeholder must not leak into production: \
         {production:?}"
    );
}

/// Sensitivity check (1/5, NRN-448 review round): a char literal containing
/// an open brace (`'{'`) inside an early `#[cfg(test)]` item's body must not
/// desync the depth count that finds the item's own closing `}` — if it did,
/// `fn b`'s planted `:?}` would be silently dropped from the production scan.
#[test]
fn production_source_skips_a_char_literal_containing_an_open_brace() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod char_brace {\n    \
                   fn t() { let c = '{'; let _ = c; }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "a `'{{'` char literal must not desync brace counting and swallow the \
         production code that follows: {production:?}"
    );
    assert!(
        !production.contains("char_brace"),
        "the cfg(test) item's own body must still be excluded: {production:?}"
    );
}

/// Sensitivity check (2/5): a char literal containing a double quote (`'"'`,
/// as in `assert_eq!(c, '"')`) must not be mistaken for the start of a
/// `"..."` string — that would desync the in-string state and, again, drop
/// `fn b`'s planted `:?}`.
#[test]
fn production_source_skips_a_char_literal_containing_a_quote() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod char_quote {\n    \
                   fn t() { let c = '\"'; assert_eq!(c, '\"'); }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "a `'\"'` char literal must not be mistaken for a string literal, desyncing \
         brace counting and swallowing the production code that follows: {production:?}"
    );
    assert!(!production.contains("char_quote"));
}

/// Sensitivity check (3/5): a `//` line comment mentioning a brace (`// opens
/// a {`) must not have that brace counted as real source structure.
#[test]
fn production_source_skips_a_line_comment_containing_a_brace() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod line_comment {\n    \
                   fn t() { // opens a {\n        \
                       let _ = 1;\n    \
                   }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "a `{{` inside a `//` line comment must not desync brace counting and swallow \
         the production code that follows: {production:?}"
    );
    assert!(!production.contains("line_comment"));
}

/// Sensitivity check (4/5): a `/* { */` block comment must likewise not have
/// its brace counted as real source structure.
#[test]
fn production_source_skips_a_block_comment_containing_a_brace() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod block_comment {\n    \
                   fn t() { /* { */ let _ = 1; }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "a `{{` inside a `/* */` block comment must not desync brace counting and \
         swallow the production code that follows: {production:?}"
    );
    assert!(!production.contains("block_comment"));
}

/// Sensitivity check (5/5): a raw string with an inner `"` (`r#"a "quote"#`)
/// must not desync the in-string quote-parity state either.
#[test]
fn production_source_skips_a_raw_string_containing_a_quote() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               mod raw_string {\n    \
                   fn t() { let s = r#\"a \"quote\"#; let _ = s; }\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "a raw string's inner `\"` must not desync quote-parity tracking and swallow \
         the production code that follows: {production:?}"
    );
    assert!(!production.contains("raw_string"));
}

/// Sensitivity check (6/6, CodeRabbit round on #248): a `;` inside an array
/// type or repeat expression (`[u8; 4]`) must not be mistaken for a
/// brace-free statement item's terminator — that is a false-FAIL direction:
/// ending the item too early leaves its real, still-test-only body
/// (including the `{...}` that follows) misclassified as production code,
/// which would spuriously flag legitimate test-only source as a violation.
#[test]
fn production_source_treats_array_repeat_semicolons_as_nested_not_terminating() {
    let src = "fn a() {}\n\
               #[cfg(test)]\n\
               fn h(buf: [u8; 4]) {\n    \
                   const T: [u8; 4] = [0; 4];\n    \
                   let _ = format!(\"{:?}\", buf);\n    \
                   let _ = T;\n\
               }\n\
               fn b() { let _ = format!(\"{:?}\", 2); }\n";
    let production = production_source(Path::new("<fixture>"), src);
    assert!(
        production.contains("fn b()") && production.contains(":?}"),
        "an in-bracket `;` inside `[u8; 4]` must not end the cfg(test) item early: \
         {production:?}"
    );
    assert!(
        !production.contains("fn h") && !production.contains("const T"),
        "the cfg(test) item's own body — including the code after its first in-bracket \
         `;` — must still be excluded in full: {production:?}"
    );
}

#[test]
fn no_surface_sniffs_severity_from_message_text() {
    let crates = workspace_root().join("crates");
    let mut hits = Vec::new();
    for entry in fs::read_dir(&crates).expect("read_dir crates") {
        let crate_dir = entry.expect("dir entry").path();
        let name = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !crate_dir.is_dir() || SNIFF_EXEMPT_CRATES.contains(&name) {
            continue;
        }
        let src = crate_dir.join("src");
        if !src.is_dir() {
            continue;
        }
        scan_rs(&src, &mut |path, text| {
            for needle in SNIFF_NEEDLES {
                if text.contains(needle) {
                    hits.push(format!("{}: `{needle}`", path.display()));
                }
            }
        });
    }
    assert!(
        hits.is_empty(),
        "a surface must derive severity from the typed Note / outcome, never by \
         matching an annotation prefix in message text (NRN-407):\n{hits:#?}"
    );
}

#[test]
fn renderers_label_enums_via_serde_name_not_debug() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/display/render");
    let mut hits = Vec::new();
    scan_rs(&dir, &mut |path, text| {
        if production_source(path, text).contains(":?}") {
            hits.push(path.display().to_string());
        }
    });
    assert!(
        hits.is_empty(),
        "a renderer must label enum values via display::serde_label (the serde \
         kebab name), never a `Debug` placeholder — positional `{{:?}}` or \
         inline-capture `{{ident:?}}` (NRN-407):\n{hits:#?}"
    );
}

#[test]
fn no_render_or_output_surface_emits_a_raw_stderr_prefix() {
    // Scope: the CLI's presentation layer — `src/display/render/` +
    // `src/output/` — the tree NRN-407's fix round converged onto the
    // `Conversation` constructors (`get`/`find`/`describe`/`count`'s
    // `--col`/`--by`/`--sort` warnings, `projection`'s shared `--col`/
    // `--section`-ignored warnings). `conversation.rs` lives one level up
    // (`src/display/`), outside this tree, and is the one legitimate home
    // for these literals.
    let dirs = [
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/display/render"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/output"),
    ];
    let mut hits = Vec::new();
    for dir in &dirs {
        scan_rs(dir, &mut |path, text| {
            let production = production_source(path, text);
            for body in macro_call_bodies(path, &production) {
                for needle in RAW_PREFIX_NEEDLES {
                    if body.contains(needle) {
                        hits.push(format!("{}: `{needle}`", path.display()));
                    }
                }
            }
        });
    }
    assert!(
        hits.is_empty(),
        "a render/output surface must emit stderr annotations through \
         Conversation::note/warning/error/report_note, never a raw \
         writeln!/eprintln! prefix literal (NRN-407):\n{hits:#?}"
    );
}
