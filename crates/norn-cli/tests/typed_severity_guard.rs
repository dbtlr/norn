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
fn production_source(text: &str) -> String {
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
            // No item (brace block or `;`-terminated statement) follows the
            // attribute — nothing left that is safely known to be
            // production, so stop here (the old conservative behavior).
            None => break,
        }
    }
    kept
}

/// The byte offset just past the end of the item a `#[cfg(test)]` attribute
/// (found at `from`, right after the attribute) annotates: the matching `}`
/// of its first `{...}` block (`mod`/`fn`/`struct`/`impl`/…), or the `;` of a
/// brace-free statement item (`use`/`const`/…) — whichever delimiter is
/// reached first, so a stray `{`/`;` inside a string literal is never
/// miscounted. Other attributes stacked on the same item (`#[cfg(test)]
/// #[allow(dead_code)] fn f() {..}`) are skipped over transparently since
/// neither delimiter appears inside them.
fn cfg_test_item_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(from) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => return find_matching_brace(text, i).map(|close| close + 1),
            b';' => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// Find the `}` matching the `{` at byte offset `open`, tracking string-quote
/// state so a brace inside a `"..."` literal is never miscounted. Sibling of
/// [`find_matching_paren`].
fn find_matching_brace(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The stderr-emission macros a raw annotation-prefix literal must never
/// appear inside (invariant 2).
const RAW_EMISSION_MACROS: &[&str] = &["writeln!", "eprintln!"];

/// The closed stderr-annotation prefixes, as they appear at the START of a
/// string literal (leading `"`, trailing `: ` so a plural data count like
/// `"warnings: N"` never matches).
const RAW_PREFIX_NEEDLES: &[&str] = &["\"warning: ", "\"warn: ", "\"error: ", "\"note: "];

/// Find the `)` matching the `(` at byte offset `open`, tracking string-quote
/// state so a paren inside a `"..."` literal is never miscounted.
fn find_matching_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The balanced-paren argument body of every `writeln!(...)` / `eprintln!(...)`
/// call in `text` (a raw stderr-emission call site, invariant 2's target).
fn macro_call_bodies(text: &str) -> Vec<&str> {
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
                None => break,
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
    let production = production_source(src);
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
    let production = production_source(src);
    assert!(production.contains("fn a()"));
    assert!(
        !production.contains(":?}"),
        "the trailing test module's Debug placeholder must not leak into production: \
         {production:?}"
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
        if production_source(text).contains(":?}") {
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
            let production = production_source(text);
            for body in macro_call_bodies(&production) {
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
