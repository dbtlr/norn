//! The fixed-content vault "zoo" — a hand-curated set of documents with
//! fixed paths and byte-identical content across every generation, seed
//! notwithstanding. Two tiers:
//!
//! - `valid_docs()` / `binary_docs()` — always emitted; every markdown doc
//!   here satisfies `config::CONFIG_YAML` cleanly (zero validate findings).
//! - `violation_docs()` — emitted only when `Profile::violations` is set;
//!   each doc trips at least one specific, named finding code, and together
//!   they cover every constraint kind in the config plus every `repair.rules`
//!   match predicate.

/// `(vault-relative path, file content)` pairs for the always-valid zoo.
pub fn valid_docs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("index.md", INDEX),
        ("notes/alpha.md", ALPHA),
        ("notes/beta.md", BETA),
        ("notes/keep/kept.md", KEPT),
        ("notes/gamma.md", GAMMA),
        ("phases/phase-one.md", PHASE_ONE),
        ("tasks/task-001.md", TASK_001),
        ("tasks/task-002.md", TASK_002),
        ("notes/cycle-a.md", CYCLE_A),
        ("notes/cycle-b.md", CYCLE_B),
        ("notes/cycle-c.md", CYCLE_C),
        ("notes/orphan.md", ORPHAN),
        ("notes/duplicate.md", DUPLICATE_ONE),
        ("archive2/duplicate.md", DUPLICATE_TWO),
        ("notes/ambi.md", AMBI),
        ("Über Notiz.md", UBER_NOTIZ),
        ("Wide Open Spaces.md", WIDE_OPEN_SPACES),
        ("logs/2025-01-15.md", LOG_DATED),
        ("logs/scratch/rough.md", LOG_SCRATCH),
        ("templates/broken-template.md", BROKEN_TEMPLATE),
        ("drafts/a/draft-note.md", DRAFT_NOTE),
        ("ignored/hidden-away.md", HIDDEN_AWAY),
        ("shapes/no-body.md", NO_BODY),
    ]
}

/// `(vault-relative path, bytes)` pairs for non-markdown fixed assets.
pub fn binary_docs() -> Vec<(&'static str, &'static [u8])> {
    vec![("Assets/pic.png", MINIMAL_PNG)]
}

/// `(vault-relative path, file content)` pairs for the violation zoo,
/// emitted only when `Profile::violations` is true.
pub fn violation_docs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("broken/parse-fail.md", PARSE_FAIL),
        ("broken/no-frontmatter.md", NO_FRONTMATTER),
        ("notes/missing-kind.md", MISSING_KIND),
        ("notes/has-legacy.md", HAS_LEGACY),
        ("tasks/task-bad-status.md", TASK_BAD_STATUS),
        ("stray-task.md", STRAY_TASK),
        ("notes/bad-types.md", BAD_TYPES),
        ("notes/bad-parent.md", BAD_PARENT),
        ("notes/dangling-parent.md", DANGLING_PARENT),
        ("notes/ambi-bare.md", AMBI_BARE),
        ("notes/dead-end.md", DEAD_END),
        ("notes/into-ignored.md", INTO_IGNORED),
        ("shapes/empty-block.md", EMPTY_BLOCK),
    ]
}

// ---- valid zoo content ---------------------------------------------------

const INDEX: &str = r#"---
title: Index
---

# Index

Hub linking to the zoo:

- [[alpha]]
- [[beta]]
- [[gamma]]
- [[phase-one]]
- [[task-001]]
- [[task-002]]
- [[Über Notiz]]
- [[Wide Open Spaces]]
"#;

const ALPHA: &str = r#"---
title: Alpha
type: note
kind: note
created: 2024-01-01T09:00:00Z
modified: 2024-02-01 10:30:00
due: 2024-03-01
tags: [alpha, sample]
parent: "[[phase-one]]"
related: "[[beta]]"
project: sample-project
summary: A short summary of alpha, unbounded in length.
internal: true
---

# Alpha

An introductory paragraph for the alpha fixture document.

## Section One

Content belonging to section one.

## Section Two

Content belonging to section two.

## Section Three

Content belonging to section three.
"#;

const BETA: &str = r#"---
title: Beta
type: note
kind: note
aliases: [bee]
---

# Beta

Every link shape, exercised once each:

- Stem: [[alpha]]
- Path-qualified: [[notes/alpha]]
- Aliased: [[alpha|the first note]]
- Heading anchor: [[alpha#Section One]]
- Markdown relative: [alpha](alpha.md)
- Attachment embed: ![pic](../Assets/pic.png)
"#;

const KEPT: &str = r#"---
title: Kept
type: note
kind: note
legacy: true
---

Carries `legacy: true` legally — inside the `notes/keep/**` carve-out.
"#;

const GAMMA: &str = r#"---
title: Gamma
type: note
kind: note
---

Alias resolution: [[beta]] and [[bee]] both resolve here. Self-link: [[gamma]].
"#;

const PHASE_ONE: &str = r#"---
title: Phase One
type: phase
status: active
---

The phase fixture, targeted by `field_references` and `allowed_paths`.
"#;

const TASK_001: &str = r#"---
title: Task One
type: task
status: backlog
parent: "[[phase-one]]"
---

A task with a valid `field_references` parent.
"#;

const TASK_002: &str = r#"---
title: Task Two
type: task
status: done
related: ["[[task-001]]"]
---

A task with `related` as a list of wikilinks.
"#;

const CYCLE_A: &str = r#"---
title: Cycle A
---

Points to [[cycle-b]].
"#;

const CYCLE_B: &str = r#"---
title: Cycle B
---

Points to [[cycle-c]].
"#;

const CYCLE_C: &str = r#"---
title: Cycle C
---

Points to [[cycle-a]], closing the cycle.
"#;

const ORPHAN: &str = r#"---
title: Orphan
---

No links in or out.
"#;

const DUPLICATE_ONE: &str = r#"---
title: Duplicate One
---

The first of two documents sharing the stem `duplicate`.
"#;

const DUPLICATE_TWO: &str = r#"---
title: Duplicate Two
---

The second of two documents sharing the stem `duplicate`.
"#;

const AMBI: &str = r#"---
title: Ambi
---

Path-qualified, unambiguous: [[notes/duplicate]].
"#;

const UBER_NOTIZ: &str = r#"---
title: Über Notiz
---

A document with a unicode filename.
"#;

const WIDE_OPEN_SPACES: &str = r#"---
title: Wide Open Spaces
---

A document with spaces in its filename.
"#;

const LOG_DATED: &str = r#"---
title: Log 2025-01-15
when: 2025-01-15
---

A dated log entry.
"#;

const LOG_SCRATCH: &str = r#"---
title: Rough Notes
---

Under the `logs/scratch/**` carve-out — `when` may be omitted here.
"#;

const BROKEN_TEMPLATE: &str = "---\ntitle: \"Unclosed template title\ntype: note\n---\n\nExempt via validate.ignore: templates/**.\n";

const DRAFT_NOTE: &str = r#"---
title: Draft Note
---

Under the `drafts/{a,b}/**` validate.ignore alternation.
"#;

const HIDDEN_AWAY: &str = r#"---
title: Hidden Away
---

Excluded from the graph entirely via files.ignore: ignored/**.
"#;

const NO_BODY: &str = r#"---
title: No Body
---
"#;

// ---- violation zoo content ------------------------------------------------

const PARSE_FAIL: &str =
    "---\ntitle: \"Unclosed quote starts here\ntype: note\nstatus: draft\n---\n\nMalformed frontmatter — never closes the opening quote.\n";

const NO_FRONTMATTER: &str = "# No Frontmatter\n\nThis document has no frontmatter block at all, so it fails the global `title` requirement.\n";

const MISSING_KIND: &str = r#"---
title: Missing Kind
type: note
---

`type: note` with no `kind` — frontmatter-required-field-missing.
"#;

const HAS_LEGACY: &str = r#"---
title: Has Legacy
legacy: old
---

Carries `legacy` outside the `notes/keep/**` carve-out — frontmatter-forbidden-field.
"#;

const TASK_BAD_STATUS: &str = r#"---
title: Task Bad Status
type: task
status: legacy
---

`status: legacy` is not an allowed value — value-not-allowed.
"#;

const STRAY_TASK: &str = r#"---
title: Stray Task
type: task
status: backlog
---

A task document at the vault root, outside `tasks/**` and `phases/**` — document-misrouted.
"#;

const BAD_TYPES: &str = r#"---
title: Bad Types
type: note
kind: note
created: not-a-date
due: 2025-13-45
tags: [ok, 7]
project: this-is-a-very-long-project-name-that-exceeds-the-limit
---

Several field-type-invalid findings plus one frontmatter-exceeds-max-length.
"#;

const BAD_PARENT: &str = r#"---
title: Bad Parent
type: note
kind: note
parent: "[[alpha]]"
---

`parent` resolves to a note, not a phase — frontmatter-reference-type.
"#;

const DANGLING_PARENT: &str = r#"---
title: Dangling Parent
type: note
kind: note
parent: "[[missing-phase]]"
---

`parent` does not resolve at all — stays a link-* finding, not reference-type.
"#;

const AMBI_BARE: &str = r#"---
title: Ambi Bare
type: note
kind: note
---

Bare, ambiguous: [[duplicate]].
"#;

const DEAD_END: &str = r#"---
title: Dead End
type: note
kind: note
---

Broken links: [[does-not-exist]] and [missing](nope.md).
"#;

const INTO_IGNORED: &str = r#"---
title: Into Ignored
type: note
kind: note
---

Links into a files.ignore'd target: [[hidden-away]].
"#;

const EMPTY_BLOCK: &str =
    "---\n---\n\nEmpty frontmatter block — fails the global `title` requirement.\n";

/// A minimal, valid 1x1 transparent PNG (67 bytes) — small enough to embed
/// as a byte literal, real enough to be a legitimate link target.
#[rustfmt::skip]
const MINIMAL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41,
    0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];
