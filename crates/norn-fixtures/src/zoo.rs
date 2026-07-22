//! The fixed-content vault "zoo" — a hand-curated set of documents with
//! fixed paths and byte-identical content across every generation, seed
//! notwithstanding. Two tiers:
//!
//! - `valid_docs()` / `binary_docs()` — always emitted; every markdown doc
//!   here satisfies the emitted config cleanly (zero validate findings).
//! - `violation_docs()` — emitted only when `Profile::violations` is set;
//!   each doc trips at least one specific, named finding code (carried as
//!   data on the [`ViolationDoc`], the single source the oracle-smoke test
//!   reads), and together they cover every constraint kind in the config plus
//!   every `repair.rules` match predicate.

use crate::Tier;

/// A fixed markdown document in the always-emitted zoo.
pub struct ZooDoc {
    /// Vault-relative path.
    pub path: &'static str,
    /// File content.
    pub content: &'static str,
    /// Which validation tier this doc exercises.
    pub tier: Tier,
    /// Eligible as an expansion link target. False for the deliberately
    /// ambiguous `duplicate` pair and for the files-ignored doc, either of
    /// which would inject an unintended finding into a clean profile.
    pub linkable: bool,
}

/// A fixed markdown document in the violation zoo, tagged with the finding
/// codes the oracle is expected to report for it.
pub struct ViolationDoc {
    /// Vault-relative path.
    pub path: &'static str,
    /// File content.
    pub content: &'static str,
    /// Finding codes the oracle reports against this doc.
    pub codes: &'static [&'static str],
}

const fn valid(path: &'static str, content: &'static str) -> ZooDoc {
    ZooDoc {
        path,
        content,
        tier: Tier::Valid,
        linkable: true,
    }
}

/// Valid-tier doc excluded from the expansion link-target pool (ambiguous stem).
const fn valid_unlinkable(path: &'static str, content: &'static str) -> ZooDoc {
    ZooDoc {
        path,
        content,
        tier: Tier::Valid,
        linkable: false,
    }
}

const fn validate_ignored(path: &'static str, content: &'static str) -> ZooDoc {
    ZooDoc {
        path,
        content,
        tier: Tier::ValidateIgnored,
        linkable: true,
    }
}

const fn files_ignored(path: &'static str, content: &'static str) -> ZooDoc {
    ZooDoc {
        path,
        content,
        tier: Tier::FilesIgnored,
        linkable: false,
    }
}

const fn viol(
    path: &'static str,
    content: &'static str,
    codes: &'static [&'static str],
) -> ViolationDoc {
    ViolationDoc {
        path,
        content,
        codes,
    }
}

/// The always-valid zoo, in fixed emission order.
pub fn valid_docs() -> Vec<ZooDoc> {
    vec![
        valid("index.md", INDEX),
        valid("notes/alpha.md", ALPHA),
        valid("notes/beta.md", BETA),
        valid("notes/keep/kept.md", KEPT),
        valid("notes/gamma.md", GAMMA),
        valid("phases/phase-one.md", PHASE_ONE),
        valid("tasks/task-001.md", TASK_001),
        valid("tasks/task-002.md", TASK_002),
        valid("notes/cycle-a.md", CYCLE_A),
        valid("notes/cycle-b.md", CYCLE_B),
        valid("notes/cycle-c.md", CYCLE_C),
        valid("notes/orphan.md", ORPHAN),
        valid_unlinkable("notes/duplicate.md", DUPLICATE_ONE),
        valid_unlinkable("archive2/duplicate.md", DUPLICATE_TWO),
        valid("notes/ambi.md", AMBI),
        valid("Über Notiz.md", UBER_NOTIZ),
        valid("Wide Open Spaces.md", WIDE_OPEN_SPACES),
        valid("logs/2025-01-15.md", LOG_DATED),
        valid("logs/scratch/rough.md", LOG_SCRATCH),
        validate_ignored("templates/broken-template.md", BROKEN_TEMPLATE),
        validate_ignored("drafts/a/draft-note.md", DRAFT_NOTE),
        files_ignored("ignored/hidden-away.md", HIDDEN_AWAY),
        valid("shapes/no-body.md", NO_BODY),
    ]
}

/// `(vault-relative path, bytes)` pairs for non-markdown fixed assets.
pub fn binary_docs() -> Vec<(&'static str, &'static [u8])> {
    vec![("Assets/pic.png", MINIMAL_PNG)]
}

/// The text-layer edge zoo — emitted only when `Profile::text_edge` is set.
/// Each doc isolates one decided divergence from the pinned oracle (NRN-349 /
/// NRN-350) so a dedicated parity case can pin it without perturbing any
/// shared zoo/clean case:
///
/// - `edge/bom-doc.md` — a leading UTF-8 BOM before the fence. The oracle reads
///   it as frontmatter-less; the rewrite recognizes the block (NRN-349).
/// - `edge/code-anchor.md` — defines a `^blkincode` block-id *inside* a fenced
///   code block, and `edge/code-linker.md` references it via
///   `[[code-anchor#^blkincode]]`. The oracle registers the code-fenced id and
///   resolves the link; the rewrite treats code as opaque (ADR 0019), so the
///   link is unresolved.
/// - `edge/url-decode-linker.md` — Markdown-link destinations exercising the
///   split-then-decode + block-ref reclassification (NRN-356). The oracle
///   decodes before splitting (so `note%23draft.md` becomes `note` + anchor
///   `draft.md`) and slugifies `#^blk1` as a heading anchor; the rewrite splits
///   the raw reference first (one `note#draft.md` segment) and classifies
///   `#^blk1` as a block ref that resolves against `edge/url-block-target.md`.
/// - `edge/url-scheme-linker.md` — Markdown-link destinations exercising generic
///   external-vs-local classification (NRN-357). The oracle's hand-rolled
///   lowercase prefix list treats `tel:` / `file:` / `//` / drive-letter /
///   mixed-case `hTTp:` as local (unresolved) links; the rewrite classifies each
///   external by URL rules and drops them from the link universe.
pub fn text_edge_docs() -> Vec<ZooDoc> {
    vec![
        valid_unlinkable("edge/bom-doc.md", BOM_DOC),
        valid_unlinkable("edge/code-anchor.md", CODE_ANCHOR),
        valid_unlinkable("edge/code-linker.md", CODE_LINKER),
        valid_unlinkable("edge/url-decode-linker.md", URL_DECODE_LINKER),
        valid_unlinkable("edge/url-block-target.md", URL_BLOCK_TARGET),
        valid_unlinkable("edge/url-scheme-linker.md", URL_SCHEME_LINKER),
    ]
}

/// The mutation-edge zoo — emitted only when `Profile::mutate_edge` is set.
/// Two documents whose frontmatter block is a non-mapping YAML node that a
/// field op must nonetheless be able to initialize (NRN-371):
///
/// - `shapes/null-block.md` — a bare `null` scalar frontmatter block. The
///   pinned oracle refuses a `set` against it (`frontmatter is not a top-level
///   mapping`, exit 2); the rewrite promotes the null block to an empty mapping
///   and splices the field, so a valid input yields a valid output.
/// - `shapes/comment-block.md` — a comment-only frontmatter block (parses to a
///   null mapping, same as above). The oracle refuses identically; the rewrite
///   preserves the comment and appends the field after it.
///
/// Isolated on a dedicated profile (mirroring `text_edge_docs`) so the decided
/// NRN-371 divergence never perturbs a shared zoo/clean case.
pub fn mutate_edge_docs() -> Vec<ZooDoc> {
    vec![
        valid_unlinkable("shapes/null-block.md", NULL_BLOCK),
        valid_unlinkable("shapes/comment-block.md", COMMENT_BLOCK),
    ]
}

/// The section-edge zoo — emitted only when `Profile::section_edge` is set. Two
/// documents whose BODY carries a heading shape that the NRN-437 section-op fix
/// corrects, isolated on a dedicated profile (the text-edge / mutate-edge
/// discipline) so the decided divergence never perturbs a shared zoo/clean case:
///
/// - `shapes/setext.md` — a SETEXT heading (`Alpha` underlined by `-----`). A
///   section op that derives the body start from a manual "byte after the first
///   newline" scan lands ON the underline and corrupts the heading (a
///   `replace_section` consumes the underline, demoting the heading to a
///   paragraph; an `insert_after_heading` pushes text between the title and its
///   underline). The oracle does this; the rewrite derives the body start from
///   the heading construct's end, keeping the underline welded to its title.
/// - `shapes/eof-heading.md` — a heading at EOF with NO trailing newline. An op
///   inserting body content must supply the missing line terminator first; the
///   oracle welds the content onto the marker (`## Tail- item`), the rewrite
///   separates it (`## Tail\n- item`).
pub fn section_edge_docs() -> Vec<ZooDoc> {
    vec![
        valid_unlinkable("shapes/setext.md", SETEXT_DOC),
        valid_unlinkable("shapes/eof-heading.md", EOF_HEADING_DOC),
    ]
}

/// The wikilink-edge zoo — emitted only when `Profile::wikilink_edge` is set.
/// Backlink probes for the three wikilink-rewriter corruptions (NRN-424 /
/// NRN-431/432/433), isolated on a dedicated profile (the text-edge /
/// mutate-edge / section-edge discipline) so the decided divergences never
/// perturb a shared zoo/clean case. Each source doc holds a hand-authored
/// backlink to its target; the move / rewrite-wikilink cases exercise the
/// cascade and the verb rewriters:
///
/// - `wl/embed-target.md` + `wl/embed-src.md` — an embed backlink
///   `![[embed-target|Display]]`. On a move, the oracle's cascade collapses it
///   to `[[embed-moved]]` (a `strip_prefix("[[")` drops the `!` and the alias,
///   NRN-431); the rewrite preserves `![[embed-moved|Display]]`.
/// - `wl/fence-target.md` + `wl/fence-src.md` — a `[[fence-target]]` shadowed
///   inside a code fence ABOVE the real prose backlink. The oracle rewrites the
///   fenced sample (the move cascade's first-occurrence `replacen` hits it and
///   leaves the prose link dangling; the rewrite-wikilink verb's whole-file scan
///   rewrites BOTH), NRN-432; the rewrite is code-opaque and touches only prose.
/// - `wl/a^b.md` + `wl/caret-src.md` — a `[[a^b]]` backlink whose target stem
///   carries a bare `^`. The oracle splits the target on `^`, so the rewrite
///   never matches and the file is left untouched while success is reported
///   (NRN-433); the rewrite splits on `#` only and rewrites `[[a^b]]`.
/// - `wl/redirect-target.md` — the `delete --rewrite-to` redirect target for
///   the PD-116 delete variant (same embed-marker mechanism as the move case).
/// - `wl/spaced-target.md` + `wl/spaced-alias-src.md` — a
///   `[[spaced-target | Display Name]]` backlink with interior whitespace around
///   the pipe. On rewrite the reconstruction canonicalizes it: the oracle keeps
///   the alias-side space (`[[moved| Display Name]]`), the rewrite trims it
///   (`[[moved|Display Name]]`) — PD-119, decided-better.
/// - `wl/padded-target.md` + `wl/padded-src.md` — a `[[ padded-target ]]`
///   backlink with leading/trailing whitespace inside the brackets. The oracle's
///   untrimmed `bare_target` defeats its own match, so `rewrite-wikilink`
///   phantom-no-ops; the rewrite matches on the parser-trimmed target and
///   rewrites it — PD-119, decided-better (the spaced-target match is a fix).
pub fn wikilink_edge_docs() -> Vec<ZooDoc> {
    vec![
        valid_unlinkable("wl/embed-target.md", WL_EMBED_TARGET),
        valid_unlinkable("wl/embed-src.md", WL_EMBED_SRC),
        valid_unlinkable("wl/fence-target.md", WL_FENCE_TARGET),
        valid_unlinkable("wl/fence-src.md", WL_FENCE_SRC),
        valid_unlinkable("wl/a^b.md", WL_CARET_TARGET),
        valid_unlinkable("wl/caret-src.md", WL_CARET_SRC),
        valid_unlinkable("wl/redirect-target.md", WL_REDIRECT_TARGET),
        valid_unlinkable("wl/spaced-target.md", WL_SPACED_TARGET),
        valid_unlinkable("wl/spaced-alias-src.md", WL_SPACED_ALIAS_SRC),
        valid_unlinkable("wl/padded-target.md", WL_PADDED_TARGET),
        valid_unlinkable("wl/padded-src.md", WL_PADDED_SRC),
        valid_unlinkable("wl/unrepr-target.md", WL_UNREPR_TARGET),
        valid_unlinkable("wl/unrepr-src.md", WL_UNREPR_SRC),
    ]
}

/// The violation zoo, emitted only when `Profile::violations` is true. Each
/// doc's `codes` are exactly what the pinned oracle reports against it.
pub fn violation_docs() -> Vec<ViolationDoc> {
    vec![
        viol(
            "broken/parse-fail.md",
            PARSE_FAIL,
            &[
                "frontmatter-parse-failed",
                "frontmatter-required-field-missing",
            ],
        ),
        viol(
            "broken/no-frontmatter.md",
            NO_FRONTMATTER,
            &["frontmatter-required-field-missing"],
        ),
        viol(
            "notes/missing-kind.md",
            MISSING_KIND,
            &["frontmatter-required-field-missing"],
        ),
        viol(
            "notes/has-legacy.md",
            HAS_LEGACY,
            &["frontmatter-forbidden-field"],
        ),
        viol(
            "tasks/task-bad-status.md",
            TASK_BAD_STATUS,
            &["value-not-allowed"],
        ),
        viol("stray-task.md", STRAY_TASK, &["document-misrouted"]),
        viol(
            "notes/bad-types.md",
            BAD_TYPES,
            &["field-type-invalid", "frontmatter-exceeds-max-length"],
        ),
        viol(
            "notes/bad-parent.md",
            BAD_PARENT,
            &["frontmatter-reference-type"],
        ),
        viol(
            "notes/dangling-parent.md",
            DANGLING_PARENT,
            &["link-target-missing"],
        ),
        viol("notes/ambi-bare.md", AMBI_BARE, &["link-ambiguous"]),
        viol("notes/dead-end.md", DEAD_END, &["link-target-missing"]),
        viol(
            "notes/into-ignored.md",
            INTO_IGNORED,
            &["link-target-missing"],
        ),
        viol(
            "shapes/empty-block.md",
            EMPTY_BLOCK,
            &["frontmatter-required-field-missing"],
        ),
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
zip: "07030"
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

// ---- text-layer edge content ----------------------------------------------

/// A BOM-prefixed document (NRN-349). The `\u{feff}` sits before the fence, so
/// the oracle never recognizes the block and sees `type` as absent; the rewrite
/// skips the BOM and indexes the frontmatter.
const BOM_DOC: &str =
    "\u{feff}---\ntitle: BOM Doc\ntype: bomkind\n---\n\nThe oracle reads this as frontmatter-less.\n";

/// Defines a `^blkincode` block-id *inside* a fenced code block (NRN-350 / ADR
/// 0019). The oracle registers it as a real anchor; the rewrite does not.
const CODE_ANCHOR: &str = r#"---
title: Code Anchor
---

A block id that lives only inside a fenced code block:

```
^blkincode
```

Nothing outside the fence defines that id.
"#;

/// References the code-fenced block id (NRN-350). Under the oracle the link
/// resolves; under the rewrite it is unresolved (the anchor no longer exists).
const CODE_LINKER: &str = r#"---
title: Code Linker
---

Points at a code-fenced block id: [[code-anchor#^blkincode]].
"#;

/// Markdown-link destinations for the URL split-then-decode + block-ref probe
/// (NRN-356). `note%23draft.md`: the oracle decodes-then-splits into `note` +
/// anchor `draft.md`; the rewrite keeps one `note#draft.md` segment.
/// `url-block-target.md#^blk1`: the oracle slugifies `^blk1` as a heading anchor
/// (anchor-missing); the rewrite classifies it as a block ref and resolves it
/// against the sibling's `^blk1`.
const URL_DECODE_LINKER: &str = r#"---
title: URL Decode Linker
---

Percent-encoded hash in the path: [hashpath](note%23draft.md).

Block reference on a Markdown link: [blk](url-block-target.md#^blk1).
"#;

/// The block-ref resolution target for `url-decode-linker.md`: defines the
/// `^blk1` block-id in ordinary prose (outside any code fence).
const URL_BLOCK_TARGET: &str = r#"---
title: URL Block Target
---

A paragraph that carries a block id. ^blk1
"#;

/// Markdown-link destinations for the external-vs-local classification probe
/// (NRN-357). Every link here is external by URL rules — a URI scheme
/// (`tel:` / `file:` / mixed-case `hTTp:`), a protocol-relative `//`, or a
/// Windows drive-letter path — so the rewrite drops them all; the oracle's
/// lowercase prefix list mistakes each for an (unresolved) local link.
const URL_SCHEME_LINKER: &str = r#"---
title: URL Scheme Linker
---

Scheme links: [tel](tel:+15551234), [file](file:///etc/hosts),
[proto](//example.com/x), [drive](C:/Users/x/n.md), [mixed](hTTp://example.com).
"#;

// ---- mutation-edge content -------------------------------------------------

/// A bare `null` scalar frontmatter block (NRN-371). YAML parses the block to a
/// null node — not a mapping — so the oracle refuses a field `set` against it;
/// the rewrite promotes it to an empty mapping and splices the field.
const NULL_BLOCK: &str = "---\nnull\n---\n\nBody under a bare-null frontmatter block.\n";

/// A comment-only frontmatter block (NRN-371). The lone `#` line is a YAML
/// comment, so the block parses to a null mapping — same refusal on the oracle;
/// the rewrite preserves the comment and appends the field after it.
const COMMENT_BLOCK: &str =
    "---\n# only a comment, no keys\n---\n\nBody under a comment-only frontmatter block.\n";

// ---- section-edge content --------------------------------------------------

/// A SETEXT heading (`Alpha` underlined by `-----`) followed by an ordinary ATX
/// section (NRN-437). The underline is part of the heading; a section op must
/// keep it welded to its title line.
const SETEXT_DOC: &str =
    "---\ntitle: Setext Doc\n---\n\nAlpha\n-----\n\nBody under alpha.\n\n## Beta\n\nBody under beta.\n";

/// A heading at end-of-file with NO trailing newline (NRN-437). An op inserting
/// body content must supply the missing line terminator, else the content welds
/// onto the heading marker. Deliberately not newline-terminated.
const EOF_HEADING_DOC: &str = "---\ntitle: EOF Heading Doc\n---\n\n## Tail";

// ---- wikilink-edge content -------------------------------------------------

const WL_EMBED_TARGET: &str = "---\ntitle: Embed Target\n---\n\nEmbed target body.\n";
/// An embed backlink with an alias (NRN-431): the `!` and `|Display` must
/// survive a move-cascade rewrite.
const WL_EMBED_SRC: &str = "---\ntitle: Embed Src\n---\n\nSee ![[embed-target|Display]] here.\n";

const WL_FENCE_TARGET: &str = "---\ntitle: Fence Target\n---\n\nFence target body.\n";
/// A `[[fence-target]]` shadowed inside a code fence, ABOVE the real prose
/// backlink (NRN-432): the fenced sample is literal and must not be rewritten.
const WL_FENCE_SRC: &str =
    "---\ntitle: Fence Src\n---\n\n```\n[[fence-target]]\n```\n\nReal prose [[fence-target]] link.\n";

const WL_CARET_TARGET: &str = "---\ntitle: Caret Target\n---\n\nCaret target body.\n";
/// A `[[a^b]]` backlink whose target stem carries a bare `^` (NRN-433): the
/// caret is an ordinary filename character, not a block sigil.
const WL_CARET_SRC: &str = "---\ntitle: Caret Src\n---\n\nSee [[a^b]] here.\n";

const WL_REDIRECT_TARGET: &str = "---\ntitle: Redirect Target\n---\n\nRedirect target body.\n";

const WL_SPACED_TARGET: &str = "---\ntitle: Spaced Target\n---\n\nSpaced target body.\n";
/// A `[[spaced-target | Display Name]]` backlink with interior whitespace around
/// the pipe (PD-119): the reconstruction canonicalizes it to
/// `[[…|Display Name]]`, dropping the alias-side space the oracle keeps.
const WL_SPACED_ALIAS_SRC: &str =
    "---\ntitle: Spaced Alias Src\n---\n\nSee [[spaced-target | Display Name]] here.\n";

const WL_PADDED_TARGET: &str = "---\ntitle: Padded Target\n---\n\nPadded target body.\n";
/// A `[[ padded-target ]]` backlink with leading/trailing whitespace inside the
/// brackets (PD-119): the oracle's untrimmed match fails and phantom-no-ops; the
/// rewrite matches on the parser-trimmed target and rewrites it.
const WL_PADDED_SRC: &str = "---\ntitle: Padded Src\n---\n\nSee [[ padded-target ]] here.\n";

const WL_UNREPR_TARGET: &str =
    "---\ntitle: Unrepr Target\n---\n\nUnrepresentable-rename target body.\n";
/// A `[[unrepr-target]]` backlink whose rename destination carries a wikilink
/// delimiter (PD-120): the oracle emits `[[a|b]]` (which re-parses as a DIFFERENT
/// link — target `a`, alias `b`), corrupting the backlink; the rewrite refuses
/// (the `rewrite-wikilink` verb, exit 2) or skips (the move cascade), leaving the
/// link intact.
const WL_UNREPR_SRC: &str = "---\ntitle: Unrepr Src\n---\n\nSee [[unrepr-target]] here.\n";

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
