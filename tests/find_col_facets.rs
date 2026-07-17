//! Integration tests for `norn find --col` structural facets — parity with the
//! facet set `norn get` supports (`.frontmatter`, `.body`, `.headings`,
//! `.outgoing_links`, `.unresolved_links`, `.incoming_links`), plus the
//! bare-name-vs-dot vocabulary and facet-aware warnings.

use std::process::Command;
use tempfile::TempDir;

/// Vault shape:
///   a.md — type:note, title:Alpha, `# A heading`, links [[b]] (resolves) and
///          [[ghost]] (unresolved). Body text "alpha body".
///   b.md — type:note, links [[a]] (so a.md has an incoming link from b.md).
fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-find-col-facets-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\ntitle: Alpha\n---\n# A heading\n\nalpha body [[b]] [[ghost]]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\ntitle: Bravo\n---\nbravo links [[a]]\n",
    )
    .unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Runs the binary with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn run(tmp: &TempDir, args: &[&str]) -> std::process::Output {
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    Command::new(norn_bin())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"))
        .arg("--cwd")
        .arg(tmp.path().join("vault"))
        .args(args)
        .output()
        .unwrap()
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("NRN-287 sweep isolation: pre-write throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"")
        .expect("NRN-287 sweep isolation: pre-write throttle marker");
}

fn json_out(tmp: &TempDir, args: &[&str]) -> serde_json::Value {
    let out = run(tmp, args);
    assert!(
        out.status.success(),
        "command failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The doc object for `a.md` in a `find --format json` payload.
fn doc_a(v: &serde_json::Value) -> serde_json::Value {
    v["documents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "a.md")
        .unwrap()
        .clone()
}

#[test]
fn col_frontmatter_facet_emits_whole_block() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".frontmatter",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    assert_eq!(a["frontmatter"]["type"], "note");
    assert_eq!(a["frontmatter"]["title"], "Alpha");
    // Only the requested facet (plus identity) appears.
    assert!(a.get("headings").is_none());
    assert!(a.get("body").is_none());
}

#[test]
fn col_body_facet_is_cheap_and_present() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".body",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    assert!(
        a["body"].as_str().unwrap().contains("alpha body"),
        "body facet missing content: {a}"
    );
    assert!(a.get("frontmatter").is_none());
    assert!(a.get("headings").is_none());
}

#[test]
fn col_headings_facet_joins_headings() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".headings",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    let headings = a["headings"].as_array().unwrap();
    assert_eq!(headings.len(), 1);
    assert_eq!(headings[0]["text"], "A heading");
    assert!(a.get("frontmatter").is_none());
}

#[test]
fn col_outgoing_links_facet_joins_resolved_links() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".outgoing_links",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    let links = a["outgoing_links"].as_array().unwrap();
    // [[b]] resolves; [[ghost]] does not (so it is NOT in outgoing_links).
    assert!(
        links.iter().any(|l| l["target"] == "b"),
        "expected resolved [[b]] in outgoing_links: {a}"
    );
}

#[test]
fn col_unresolved_links_facet_joins_unresolved_links() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".unresolved_links",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    let links = a["unresolved_links"].as_array().unwrap();
    assert!(
        links.iter().any(|l| l["target"] == "ghost"),
        "expected unresolved [[ghost]]: {a}"
    );
}

#[test]
fn col_incoming_links_facet_joins_backlinks() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".incoming_links",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    let links = a["incoming_links"].as_array().unwrap();
    // b.md links [[a]], so a.md has one incoming link from b.md.
    assert!(
        links.iter().any(|l| l["source_path"] == "b.md"),
        "expected incoming link from b.md: {a}"
    );
}

#[test]
fn col_mixed_bare_field_and_facet() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            "title,.headings",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    // Bare `title` narrows frontmatter to just that key.
    assert_eq!(a["frontmatter"]["title"], "Alpha");
    assert!(a["frontmatter"].get("type").is_none());
    // `.headings` facet still appears.
    assert_eq!(a["headings"].as_array().unwrap().len(), 1);
}

#[test]
fn col_records_facet_renders_labeled_field() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".headings",
            "--format",
            "records",
        ],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("headings"), "expected headings label: {text}");
    assert!(
        text.contains("# A heading"),
        "expected heading text: {text}"
    );
}

#[test]
fn default_no_col_is_frontmatter_only_no_facets() {
    let tmp = synth_vault();
    let v = json_out(&tmp, &["find", "--eq", "title:Alpha", "--format", "json"]);
    let a = doc_a(&v);
    // Whole frontmatter block, no facet keys.
    assert_eq!(a["frontmatter"]["type"], "note");
    assert_eq!(a["frontmatter"]["title"], "Alpha");
    assert!(a.get("headings").is_none(), "no facet keys on default: {a}");
    assert!(a.get("outgoing_links").is_none());
    assert!(a.get("incoming_links").is_none());
    assert!(a.get("body").is_none());
}

#[test]
fn removed_raw_facet_warns_and_emits_no_json_key() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".raw",
            "--format",
            "json",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown --col facet '.raw'"),
        "expected unknown-facet warning: {stderr}"
    );
    assert!(
        stderr.contains("bare names select frontmatter fields"),
        "expected vocabulary hint: {stderr}"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let a = doc_a(&v);
    assert!(
        a.get("raw").is_none(),
        "removed facet must not emit a raw JSON key: {a}"
    );
}

#[test]
fn absent_bare_field_warns_with_find_wording() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            "nope",
            "--format",
            "json",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--col field `nope` not present in any matching document"),
        "expected find's absent-field wording: {stderr}"
    );
}

#[test]
fn all_cols_expands_to_full_dump() {
    // `--all-cols` dumps whole frontmatter + every cache-served facet + body,
    // a superset of the frontmatter-only default.
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--all-cols",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    assert_eq!(a["frontmatter"]["title"], "Alpha");
    assert!(a.get("headings").is_some(), "headings present: {a}");
    assert!(a.get("outgoing_links").is_some(), "outgoing present: {a}");
    assert!(
        a.get("unresolved_links").is_some(),
        "unresolved present: {a}"
    );
    assert!(a.get("incoming_links").is_some(), "incoming present: {a}");
    assert!(
        a["body"].as_str().unwrap().contains("alpha body"),
        "body present: {a}"
    );
    assert!(a.get("raw").is_none(), "--all-cols must omit raw: {a}");
}

#[test]
fn default_omits_facets_that_all_cols_adds() {
    // Baseline: the default emits only path + frontmatter, none of the facets
    // that `--all-cols` adds.
    let tmp = synth_vault();
    let v = json_out(&tmp, &["find", "--eq", "title:Alpha", "--format", "json"]);
    let a = doc_a(&v);
    assert!(a.get("headings").is_none());
    assert!(a.get("incoming_links").is_none());
    assert!(a.get("body").is_none());
}

/// blake3 hex of a file's full bytes — the canonical `document_hash` convention
/// (matches `set::synth` / the applier / `edit --expected-hash`).
fn blake3_of_file(path: &std::path::Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap())
        .to_hex()
        .to_string()
}

/// NRN-105: `find --col .document_hash` emits the document's full-content blake3
/// hex — the value `edit --expected-hash` wants, obtainable through norn.
#[test]
fn col_document_hash_facet_emits_content_hash() {
    let tmp = synth_vault();
    let v = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--col",
            ".document_hash",
            "--format",
            "json",
        ],
    );
    let a = doc_a(&v);
    let expected = blake3_of_file(&tmp.path().join("vault").join("a.md"));
    assert_eq!(
        a["document_hash"], expected,
        "facet emits full-content blake3: {a}"
    );
    // Only the requested facet (+ identity) appears.
    assert!(a.get("body").is_none(), "body not requested: {a}");
    assert!(a.get("headings").is_none(), "headings not requested: {a}");
}

/// `.document_hash` is opt-in/identity-class: absent from the default dump AND
/// from `--all-cols`, so existing output stays byte-identical.
#[test]
fn document_hash_facet_is_opt_in_only() {
    let tmp = synth_vault();
    let d = json_out(&tmp, &["find", "--eq", "title:Alpha", "--format", "json"]);
    assert!(
        doc_a(&d).get("document_hash").is_none(),
        "default omits document_hash"
    );
    let a = json_out(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--all-cols",
            "--format",
            "json",
        ],
    );
    assert!(
        doc_a(&a).get("document_hash").is_none(),
        "--all-cols omits document_hash (opt-in only): {}",
        doc_a(&a)
    );
}

#[test]
fn all_cols_conflicts_with_col() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &[
            "find",
            "--eq",
            "title:Alpha",
            "--all-cols",
            "--col",
            "title",
        ],
    );
    assert!(!out.status.success(), "--all-cols + --col should conflict");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected conflict error; got: {stderr}"
    );
}
