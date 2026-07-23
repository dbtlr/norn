//! Link-target classification, splitting, and decoding — the Markdown-link half
//! of the `anchor` helpers that stay with the link model.
//!
//! A Markdown `[text](dest)` destination is a URL reference, so this module
//! models it by URL-parser rules (RFC 3986 / WHATWG) rather than ad-hoc string
//! prefixes:
//!
//! - **Classification (link-model).** [`is_local_file_target`] decides whether a
//!   destination names a local vault file or an external resource, by generic
//!   scheme detection ([`has_uri_scheme`]), protocol-relative `//`, and a Windows
//!   drive-letter detector ([`is_windows_drive_path`]) — not a hand-rolled
//!   lowercase prefix list.
//! - **Split-then-decode (URL semantics).** [`split_and_decode_destination`]
//!   separates the fragment from the RAW reference BEFORE any percent-decoding,
//!   then decodes path and fragment SEPARATELY, classifying a `#^id` fragment as
//!   a block reference exactly like a wikilink.
//!
//! ## Parser choice: `percent-encoding` + a raw-fragment split, not the `url` crate
//!
//! The fragment split is the RFC 3986 rule "everything after the first `#`",
//! i.e. `str::split_once('#')`, reused from `norn-frontmatter`'s
//! `split_anchor_or_block_ref` (the syntax-layer primitive shared with
//! wikilinks). Decoding uses the `percent-encoding` crate.
//!
//! The WHATWG `url` crate was evaluated and rejected: a relative reference needs
//! a base, and round-tripping a vault path through a synthetic base
//! (`thismessage:/…`) is **not lossless** — the parser resolves dot-segments
//! (`a/../b.md` collapses to `b.md`), percent-re-encodes unicode and other path
//! characters (`café.md` becomes `caf%C3%A9.md` in `Url::path()`), and still
//! hands the fragment back percent-encoded. Preserving the authored
//! vault-relative `target` verbatim (dot-segments intact for the resolver's own
//! `normalize_relative`, unicode intact) is exactly what the link model needs,
//! and `url` would destroy it while buying no free decode. So the fragment is
//! split off the raw reference and each component decoded directly.

use norn_frontmatter::wikilink::split_anchor_or_block_ref;

/// Decode `%XX` percent escapes in a single Markdown link component (a path or a
/// fragment). Invalid or truncated escapes are left intact and the decoded bytes
/// are interpreted as UTF-8 lossily via the `percent-encoding` crate's
/// `decode_utf8_lossy`. Decoding is a single pass: `%2523` decodes once to
/// `%23`, never recursively to `#`.
pub(crate) fn decode_percent_escapes(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

/// Split a Markdown link destination into decoded `(target, anchor, block_ref)`
/// by URL-parser rules.
///
/// The fragment is separated from the **raw** reference on the first `#` BEFORE
/// any percent-decoding; the path and the fragment are then percent-decoded
/// **separately**. A `#^id` fragment is a block reference (one resolution
/// semantics with the wikilink path); any other `#frag` is a heading anchor.
///
/// Splitting on the raw reference is what makes `note%23draft.md` stay a single
/// path segment — the `%23` is not a literal `#`, so no fragment is split and it
/// decodes to a file literally named `note#draft.md` — while
/// `My%20Note.md#Heading` still yields target `My Note.md` + anchor `Heading`.
///
/// The block-vs-anchor decision is made on the raw fragment's structural `^`
/// (mirroring the wikilink path, which never decodes), so a percent-encoded
/// caret `#%5Eid` is a heading anchor literally named `^id`, not a block ref.
pub(crate) fn split_and_decode_destination(raw: &str) -> (String, Option<String>, Option<String>) {
    let (raw_target, raw_anchor, raw_block_ref) = split_anchor_or_block_ref(raw);
    (
        decode_percent_escapes(&raw_target),
        raw_anchor.as_deref().map(decode_percent_escapes),
        raw_block_ref.as_deref().map(decode_percent_escapes),
    )
}

/// True when a Markdown `[text](dest)` target names a local Markdown file: it is
/// a local file target (see [`is_local_file_target`]) and, once its raw fragment
/// is stripped, still has a non-empty path component.
pub(crate) fn is_local_markdown_target(target: &str) -> bool {
    if !is_local_file_target(target) {
        return false;
    }

    let (path, _) = split_target_anchor(target);
    !path.is_empty()
}

/// True when a destination refers to a local vault file rather than an external
/// resource or a same-document anchor.
///
/// A destination is **external** (per URL rules) when it bears a valid URI
/// scheme (see [`has_uri_scheme`]), is protocol-relative (`//host/…`), or is a
/// Windows drive-letter path (see [`is_windows_drive_path`]). A bare `#fragment`
/// is a same-document self-reference — not a link to another file — and is also
/// excluded. Everything else is a relative reference to a vault file.
pub(crate) fn is_local_file_target(target: &str) -> bool {
    !is_external_destination(target) && !target.starts_with('#')
}

/// True when a destination is external per URL rules: a valid URI scheme, a
/// protocol-relative `//` reference, or a Windows drive-letter path.
fn is_external_destination(dest: &str) -> bool {
    // The drive-letter check is first and explicit: `C:/…` also matches the
    // generic scheme grammar (scheme `c`), so classification lands the same
    // either way, but naming the case documents that a single-letter "scheme"
    // is a drive path normalized to a `file:` URL, not a bespoke protocol.
    is_windows_drive_path(dest) || dest.starts_with("//") || has_uri_scheme(dest)
}

/// RFC 3986 scheme detection: a destination begins with a valid scheme when it
/// matches `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`, case-insensitively.
/// Anything with such a scheme (`https:`, `mailto:`, `tel:`, `file:`, `git+ssh:`,
/// mixed-case `hTTp:`) is an external resource, never a vault file.
fn has_uri_scheme(dest: &str) -> bool {
    let mut chars = dest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// A Windows drive-letter path: a single ASCII letter, a colon, then a `/` or
/// `\` separator (`C:/Users/x`, `d:\notes`). WHATWG/RFC would read the `C:` as a
/// URL scheme; treating it as a drive path normalizes it to a well-formed
/// `file:` URL — an absolute, external destination, never a relative vault path.
fn is_windows_drive_path(dest: &str) -> bool {
    let bytes = dest.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Split a target on its first `#` into `(path, fragment)`. A private `&str`
/// splitter for the non-empty-path predicate above — kept off the text-layer
/// boundary and allocation-free for the hot classification path.
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
    fn decode_percent_escapes_is_single_pass() {
        // A double-encoded `#` (`%2523`) decodes ONCE to `%23`, never recursively
        // to a literal `#`. So a `note%2523draft.md` destination names a file
        // literally called `note%23draft.md`.
        assert_eq!(
            decode_percent_escapes("note%2523draft.md"),
            "note%23draft.md"
        );
    }

    // ---- split_and_decode_destination (NRN-356): split raw, then decode.

    #[test]
    fn encoded_hash_stays_one_path_segment() {
        // `%23` is not a literal `#`, so the raw split finds no fragment: the
        // whole reference is the path and decodes to a file named `note#draft.md`.
        let (target, anchor, block_ref) = split_and_decode_destination("note%23draft.md");
        assert_eq!(target, "note#draft.md");
        assert_eq!(anchor, None);
        assert_eq!(block_ref, None);
    }

    #[test]
    fn encoded_space_path_with_heading_anchor_decodes_both_separately() {
        let (target, anchor, block_ref) = split_and_decode_destination("My%20Note.md#Heading");
        assert_eq!(target, "My Note.md");
        assert_eq!(anchor.as_deref(), Some("Heading"));
        assert_eq!(block_ref, None);
    }

    #[test]
    fn heading_fragment_is_percent_decoded() {
        let (target, anchor, _) = split_and_decode_destination("note.md#My%20Heading");
        assert_eq!(target, "note.md");
        assert_eq!(anchor.as_deref(), Some("My Heading"));
    }

    #[test]
    fn block_ref_fragment_classifies_as_block_ref() {
        // `#^id` is a block reference (like the wikilink path), not a heading
        // anchor — the `^` is stripped and the id decodes.
        let (target, anchor, block_ref) = split_and_decode_destination("note.md#^blk1");
        assert_eq!(target, "note.md");
        assert_eq!(anchor, None);
        assert_eq!(block_ref.as_deref(), Some("blk1"));
    }

    #[test]
    fn encoded_caret_fragment_is_a_heading_anchor_not_a_block_ref() {
        // Classification is made on the RAW fragment's structural `^` (matching
        // wikilinks, which never decode). A percent-encoded caret `%5E` is not a
        // structural `^`, so `#%5Eid` is a heading anchor literally named `^id`.
        let (target, anchor, block_ref) = split_and_decode_destination("note.md#%5Eid");
        assert_eq!(target, "note.md");
        assert_eq!(anchor.as_deref(), Some("^id"));
        assert_eq!(block_ref, None);
    }

    #[test]
    fn empty_fragment_is_an_empty_anchor() {
        // A trailing `#` with nothing after it is a present-but-empty fragment,
        // preserved as `Some("")`, not `None`.
        let (target, anchor, block_ref) = split_and_decode_destination("note.md#");
        assert_eq!(target, "note.md");
        assert_eq!(anchor.as_deref(), Some(""));
        assert_eq!(block_ref, None);
    }

    #[test]
    fn encoded_slash_in_path_decodes_to_a_literal_slash() {
        // `%2F` decodes to `/`. Strict WHATWG keeps it encoded (a literal slash
        // inside one segment), but the vault has no filenames-containing-slashes
        // use case, and the resolver's
        // `normalize_relative` neutralizes any resulting traversal (absolute
        // roots and `..` cannot escape the vault root). Documented, deliberate.
        let (target, _, _) = split_and_decode_destination("a%2Fb.md");
        assert_eq!(target, "a/b.md");
    }

    #[test]
    fn unicode_path_round_trips_whether_raw_or_encoded() {
        let (raw_target, _, _) = split_and_decode_destination("café.md");
        assert_eq!(raw_target, "café.md");
        let (encoded_target, _, _) = split_and_decode_destination("caf%C3%A9.md");
        assert_eq!(encoded_target, "café.md");
    }

    #[test]
    fn dot_segments_are_preserved_for_the_resolver() {
        // The authored relative path is kept verbatim (unlike the `url` crate,
        // which would collapse `a/../b.md` to `b.md`); the resolver normalizes.
        let (target, _, _) = split_and_decode_destination("a/../b.md");
        assert_eq!(target, "a/../b.md");
    }

    // ---- is_local_file_target / scheme classification (NRN-357).

    #[test]
    fn is_local_file_target_rejects_scheme_bearing_destinations() {
        assert!(!is_local_file_target("http://example.com"));
        assert!(!is_local_file_target("https://example.com"));
        assert!(!is_local_file_target("mailto:hi@example.com"));
        assert!(!is_local_file_target("tel:+15551234"));
        assert!(!is_local_file_target("file:///etc/hosts"));
        assert!(!is_local_file_target("git+ssh://host/repo"));
    }

    #[test]
    fn is_local_file_target_rejects_scheme_case_insensitively() {
        // The old lowercase prefix list let `HTTPS://` and `hTTp:` through as
        // local; RFC 3986 schemes are case-insensitive, so they are external.
        assert!(!is_local_file_target("HTTPS://example.com"));
        assert!(!is_local_file_target("hTTp://example.com"));
        assert!(!is_local_file_target("MailTo:hi@example.com"));
    }

    #[test]
    fn is_local_file_target_rejects_protocol_relative_and_drive_letters() {
        assert!(!is_local_file_target("//example.com/x"));
        assert!(!is_local_file_target("C:/Users/x/n.md"));
        assert!(!is_local_file_target("d:\\notes\\n.md"));
    }

    #[test]
    fn is_local_file_target_rejects_same_document_fragments() {
        assert!(!is_local_file_target("#Heading"));
        assert!(!is_local_file_target("#^blk"));
    }

    #[test]
    fn is_local_file_target_accepts_relative_references() {
        assert!(is_local_file_target("folder/note.md"));
        assert!(is_local_file_target("note%23draft.md"));
        assert!(is_local_file_target("../sibling/note.md"));
        assert!(is_local_file_target("café.md"));
        // A single path segment with a dot is a relative file, not a scheme:
        // the `.` is not preceded by a `:` and no colon appears before a slash.
        assert!(is_local_file_target("note.md"));
    }

    #[test]
    fn has_uri_scheme_matches_the_rfc_3986_grammar() {
        assert!(has_uri_scheme("http:x"));
        assert!(has_uri_scheme("h:x")); // single-letter scheme is valid
        assert!(has_uri_scheme("git+ssh://x"));
        assert!(has_uri_scheme("a-b.c+d:x"));
        // Must start with ALPHA, not a digit or other char.
        assert!(!has_uri_scheme("1http:x"));
        assert!(!has_uri_scheme("-http:x"));
        // A colon appearing after a non-scheme char (e.g. a slash) is not a
        // scheme: a relative path with a colon in a later segment stays local.
        assert!(!has_uri_scheme("folder/a:b.md"));
        assert!(!has_uri_scheme("no-colon-here"));
    }

    #[test]
    fn is_windows_drive_path_detects_forward_and_back_slashes() {
        assert!(is_windows_drive_path("C:/Users"));
        assert!(is_windows_drive_path("c:\\Users"));
        assert!(is_windows_drive_path("Z:/"));
        // Not a drive path: no separator after the colon, or multi-char "drive".
        assert!(!is_windows_drive_path("C:"));
        assert!(!is_windows_drive_path("http://x"));
        assert!(!is_windows_drive_path("ab:/x"));
    }

    #[test]
    fn is_local_markdown_target_requires_non_empty_path() {
        assert!(is_local_markdown_target("note.md"));
        assert!(is_local_markdown_target("note.md#Heading"));
        assert!(is_local_markdown_target("note%23draft.md"));
        // A same-document fragment has no path component.
        assert!(!is_local_markdown_target("#Heading"));
    }
}
