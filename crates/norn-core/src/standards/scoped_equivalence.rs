//! Equivalence guard for the engine's document-scoped entry.
//!
//! `validate_document_with_compiled` exists to spare a single-document caller
//! the whole-vault walk, which is only sound if it answers EXACTLY what
//! `validate_with_compiled` answers for that document. This drives both entries
//! over generated vault shapes — seeded frontmatter, link, and path variation
//! across a fixed skeleton — and asserts, for every document in every generated
//! index, that the scoped finding list equals the whole-vault list filtered to
//! that path, field for field and in order. A coverage assertion keeps the
//! corpus honest: the run must actually have produced every finding family the
//! engine can emit, so an equivalence that holds only because nothing fired is
//! a failure.

use std::collections::BTreeSet;

use camino::Utf8Path;

use crate::graph::build_index;
use crate::standards::config::parse_config_compiled;
use crate::standards::engine::{validate_document_with_compiled, validate_with_compiled};
use crate::standards::findings::Finding;

/// Exercises every rule-selector shape (whole-vault `required_frontmatter`, a
/// path-globbed rule, a `path_not`-excluded rule, a frontmatter-matched rule)
/// and every check the engine runs (required/forbidden fields, field types,
/// scalar and element-wise `allowed_values`, `allowed_paths`, typed references).
const CONFIG: &str = r#"
validate:
  ignore:
    - "Archive/**"
  required_frontmatter:
    - title
  rules:
    - name: every-document
      match:
        path: "**/*.md"
      field_types:
        title: string

    - name: work-item
      match:
        frontmatter:
          type: [task, phase]
      required_frontmatter:
        - status
      allowed_values:
        status: [backlog, active]
        tags: [alpha, beta]
      allowed_paths:
        - "tasks/**"
        - "phases/**"
      field_references:
        parent:
          target_type: [phase]

    - name: notes
      match:
        path: "notes/**"
        path_not: "notes/keep/**"
      required_frontmatter:
        - kind
      forbidden_frontmatter:
        - legacy
"#;

/// A seeded xorshift picker — deterministic shape selection without a
/// dependency, so a failing seed reproduces exactly.
struct Shapes(u64);

impl Shapes {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point of xorshift; offset past it.
        Shapes(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Pick one option; `""` means "omit this slot" at the call sites that use it.
    fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
        options[(self.next() % options.len() as u64) as usize]
    }
}

fn write(root: &Utf8Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap().as_std_path()).unwrap();
    std::fs::write(path.as_std_path(), contents).unwrap();
}

/// One optional `key: value` frontmatter line — empty when the slot is omitted,
/// so the document is genuinely missing the field rather than carrying a blank.
fn line(key: &str, value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!("{key}: {value}\n")
    }
}

/// Build a vault whose fixed skeleton carries seeded per-document variation.
/// The skeleton pins the cross-document shapes an index-wide pass sees — an
/// ambiguous stem, an ignored reference target, a type-less reference target, a
/// folder-note pair, a misrouted document, an unparseable frontmatter block, a
/// non-portable path segment — while the seeded slots vary types, values, and
/// link targets across runs.
fn generate_vault(root: &Utf8Path, seed: u64) {
    let mut shapes = Shapes::new(seed);

    for i in 0..4 {
        let type_value = shapes.pick(&["task", "phase", "note", "widget", ""]);
        let status = shapes.pick(&["backlog", "active", "retired", "7", ""]);
        let tags = shapes.pick(&["[alpha]", "[alpha, gamma]", "[]", "[beta, [alpha]]", ""]);
        let title = shapes.pick(&["Work item", "7", ""]);
        let parent = shapes.pick(&[
            "\"[[p1]]\"",
            "\"[[n1]]\"",
            "\"[[ghost]]\"",
            "\"[[dup]]\"",
            "\"[[old]]\"",
            "\"[[untyped]]\"",
            "",
        ]);
        let body = shapes.pick(&["[[t0]]", "[[ghost]]", "[[dup]]", "plain body"]);
        write(
            root,
            &format!("tasks/t{i}.md"),
            &format!(
                "---\n{}{}{}{}{}---\n{body}\n",
                line("type", type_value),
                line("status", status),
                line("tags", tags),
                line("title", title),
                line("parent", parent),
            ),
        );
    }

    write(
        root,
        "phases/p1.md",
        "---\ntype: phase\nstatus: backlog\ntitle: Phase one\n---\n[[t0]]\n",
    );

    let kind = shapes.pick(&["log", ""]);
    let legacy = shapes.pick(&["true", ""]);
    write(
        root,
        "notes/n1.md",
        &format!(
            "---\ntype: note\ntitle: Note one\n{}{}---\n[[ghost]]\n",
            line("kind", kind),
            line("legacy", legacy),
        ),
    );
    write(
        root,
        "notes/keep/excluded.md",
        "---\ntype: note\ntitle: Kept\nlegacy: true\n---\nbody\n",
    );

    // Two documents share the `dup` stem, so every link to it is ambiguous.
    write(root, "notes/dup.md", "---\ntitle: Dup A\nkind: log\n---\n");
    write(root, "other/dup.md", "---\ntitle: Dup B\n---\n");

    // A validated, resolvable reference target with NO `type` field. Guards the
    // arm the ignored target does not: a `field_references` lookup that finds
    // the target present-but-type-less reports `(missing)`, where a lookup that
    // misses the target entirely (the ignored document below) skips it. Drop
    // type-less targets from the scoped lookup and only this document's
    // referrers diverge.
    write(root, "phases/untyped.md", "---\ntitle: Untyped\n---\n");

    // The folder-note layout: `Projects.md` sorts BEFORE `Projects/a.md` in
    // byte order and AFTER it under path-component order, so the pair pins the
    // ordering the index lookup searches under.
    write(root, "Projects.md", "---\ntitle: Projects\n---\n[[a]]\n");
    write(root, "Projects/a.md", "---\ntitle: Project A\n---\n");

    // Outside the validation contract: never validated itself, and never judged
    // as a reference target.
    write(root, "Archive/old.md", "---\ntype: note\n---\nold\n");

    // A work item outside `allowed_paths`.
    write(
        root,
        "stray/t9.md",
        "---\ntype: task\nstatus: backlog\ntitle: Stray\nparent: \"[[p1]]\"\n---\n",
    );

    // An unparseable frontmatter block yields a graph diagnostic.
    write(root, "broken.md", "---\ntitle: [unterminated\n---\nbody\n");

    // A path segment with a trailing space is not portable.
    write(
        root,
        "odd dir /oddity.md",
        "---\ntitle: Oddity\n---\n[[t1]]\n",
    );
}

/// Every field of a finding, so the comparison cannot pass on a partial match.
fn render(finding: &Finding) -> String {
    format!("{finding:?}")
}

#[test]
fn scoped_findings_equal_the_whole_vault_pass_per_document() {
    let (config, compiled) = parse_config_compiled(CONFIG, Utf8Path::new("norn.yaml")).unwrap();
    let mut codes_seen: BTreeSet<String> = BTreeSet::new();
    let mut saw_element_finding = false;
    let mut saw_type_less_reference = false;
    let mut saw_clean_document = false;

    for seed in 0..12u64 {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap().join("vault");
        generate_vault(&root, seed);
        let index = build_index(&root).unwrap();

        let full = validate_with_compiled(&index, &config.validate, &compiled);
        for finding in &full {
            codes_seen.insert(finding.code.clone());
            saw_element_finding |= finding.message.contains("(element:");
            saw_type_less_reference |= finding.message.contains("(type: (missing))");
        }

        // The folder-note layout reached the index, so the equality loop below
        // really does search a pair whose byte order and path-component order
        // disagree.
        for expected_path in ["Projects.md", "Projects/a.md"] {
            assert!(
                index
                    .documents
                    .iter()
                    .any(|doc| doc.path.as_str() == expected_path),
                "seed {seed}: the corpus must carry {expected_path}"
            );
        }

        for document in &index.documents {
            let expected: Vec<String> = full
                .iter()
                .filter(|finding| finding.path == document.path)
                .map(render)
                .collect();
            let scoped: Vec<String> = validate_document_with_compiled(
                &index,
                &config.validate,
                &compiled,
                &document.path,
            )
            .iter()
            .map(render)
            .collect();
            assert_eq!(
                scoped, expected,
                "seed {seed}: scoped findings diverge for {}",
                document.path
            );
            // An ignored document is clean for free — it is never validated at
            // all — so only a document inside the validation contract counts
            // as evidence that a clean document round-trips.
            let ignored = document.path.as_str().starts_with("Archive/");
            saw_clean_document |= expected.is_empty() && !ignored;
        }

        // A path the index does not carry is not a document, so it has no
        // findings — the same answer the whole-vault pass gives by never
        // visiting it.
        assert!(
            validate_document_with_compiled(
                &index,
                &config.validate,
                &compiled,
                Utf8Path::new("nowhere/absent.md"),
            )
            .is_empty(),
            "seed {seed}: an absent path must yield no findings"
        );
    }

    // Non-vacuity: equivalence over a corpus that fired nothing proves nothing.
    for code in [
        "frontmatter-required-field-missing",
        "field-type-invalid",
        "frontmatter-forbidden-field",
        "value-not-allowed",
        "document-misrouted",
        "frontmatter-reference-type",
        "nonportable-filename",
        "link-target-missing",
        "link-ambiguous",
    ] {
        assert!(
            codes_seen.contains(code),
            "the generated corpus never produced a {code} finding; codes seen: {codes_seen:?}"
        );
    }
    // The graph-diagnostic family is whatever the parser codes its failures as,
    // so assert on the count of families instead of a literal code: the corpus
    // must carry at least one finding beyond the nine named above.
    assert!(
        codes_seen.len() > 9,
        "the generated corpus never produced a graph-diagnostic finding; \
         codes seen: {codes_seen:?}"
    );
    assert!(
        saw_element_finding,
        "the generated corpus never produced an element-wise allowed-values finding"
    );
    assert!(
        saw_type_less_reference,
        "the generated corpus never referenced a validated, type-less target — the arm that \
         separates a present-but-type-less lookup from a missing one is unguarded"
    );
    assert!(
        saw_clean_document,
        "the generated corpus never produced a finding-free, validated document"
    );
}
