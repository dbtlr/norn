//! Incremental-equals-rebuild equivalence for the targeted overlay.
//!
//! [`overlay_changed_paths`](super::overlay_changed_paths) re-resolves only the
//! blast radius the reverse link index reports for the changed paths. That is a
//! correctness claim, not an optimization detail: the resulting graph must be
//! INDISTINGUISHABLE from a cold full walk of the same on-disk state. These
//! tests are the proof obligation.
//!
//! Whole-graph resolution semantics make the claim non-obvious in both
//! directions — a create can resolve a link that was dangling somewhere else in
//! the vault, and a delete can dangle one that resolved — so each change vector
//! (create, delete, rename/move, alias add/remove, stem collision against an
//! already-ambiguous target) gets its own case, and a randomized property test
//! sweeps single-document changes over a generated vault.

use camino::{Utf8Path, Utf8PathBuf};
use tempfile::TempDir;

use super::{build_index_with_options, overlay_changed_paths, IndexOptions};
use crate::domain::GraphIndex;
use crate::links::resolve_links;

fn vault() -> (TempDir, Utf8PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().join("vault")).unwrap();
    std::fs::create_dir(&root).unwrap();
    (tmp, root)
}

fn write(root: &Utf8Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent.as_std_path()).unwrap();
    }
    std::fs::write(path.as_std_path(), content).unwrap();
}

fn remove(root: &Utf8Path, rel: &str) {
    std::fs::remove_file(root.join(rel).as_std_path()).unwrap();
}

fn options() -> IndexOptions {
    IndexOptions {
        ignore: Vec::new(),
        alias_field: Some("aliases".to_string()),
    }
}

/// Everything the overlay is responsible for keeping true: the file table, the
/// document table, and every link's resolved state. `ignored_files` is
/// deliberately excluded — the overlay does not maintain it (a targeted refresh
/// never learns about paths it was not told changed), which is existing,
/// separately-tested behavior.
fn comparable(index: &GraphIndex) -> String {
    let mut out = String::new();
    for file in &index.files {
        out.push_str(&format!("file {} stem={}\n", file.path, file.stem));
    }
    for document in &index.documents {
        out.push_str(&format!(
            "doc {} stem={} aliases={:?} headings={:?} blocks={:?}\n",
            document.path,
            document.stem,
            document.aliases,
            document
                .headings
                .iter()
                .map(|heading| heading.slug.as_str())
                .collect::<Vec<_>>(),
            document.block_ids,
        ));
        for link in &document.links {
            out.push_str(&format!(
                "  link raw={} kind={:?} target={} status={:?} resolved={:?} reason={:?} \
                 candidates={:?}\n",
                link.raw,
                link.kind,
                link.target,
                link.status,
                link.resolved_path,
                link.unresolved_reason,
                link.candidates,
            ));
        }
    }
    out
}

/// Overlay `changed` onto a cold-built baseline and assert the result equals a
/// cold rebuild of the same on-disk state.
fn assert_overlay_equals_rebuild(
    root: &Utf8Path,
    baseline: GraphIndex,
    changed: &[&str],
    case: &str,
) {
    let changed: Vec<Utf8PathBuf> = changed.iter().map(Utf8PathBuf::from).collect();
    let mut incremental = baseline;
    overlay_changed_paths(&mut incremental, root, &changed, &options());
    let rebuilt = build_index_with_options(root, &options()).unwrap();

    assert_eq!(
        comparable(&incremental),
        comparable(&rebuilt),
        "{case}: incremental re-resolution diverged from a full rebuild"
    );
}

/// Overlay `changed` onto `baseline`, then assert the SCOPED re-resolution
/// agrees with a FULL `resolve_links` pass over the identical overlaid
/// document set (a clone of the overlay's own output, not a rebuild).
///
/// This isolates the scoping axis from the file-identity axis:
/// [`assert_overlay_equals_rebuild`] compares against a cold rebuild, so a
/// spelling variant of a changed path (e.g. `./probe.md`) can trip the
/// pre-existing case/spelling-collision retain bug tracked separately as
/// NRN-524, which has nothing to do with whether the SCOPE the overlay chose
/// to re-resolve was the right one. Here both sides see exactly the same
/// files and documents — the only question is whether re-resolving just the
/// blast radius produced the same answer whole-graph resolution would have,
/// for that fixed document set.
fn assert_overlay_scoping_is_sound(
    root: &Utf8Path,
    baseline: GraphIndex,
    changed: &[Utf8PathBuf],
    case: &str,
) {
    let mut incremental = baseline;
    overlay_changed_paths(&mut incremental, root, changed, &options());

    let mut fully_resolved = incremental.clone();
    resolve_links(&fully_resolved.files, &mut fully_resolved.documents);

    assert_eq!(
        comparable(&incremental),
        comparable(&fully_resolved),
        "{case}: scoped overlay diverged from a full resolve_links pass over the SAME \
         overlaid document set"
    );
}

/// A vault where `hub.md` points at every interesting target, including ones
/// that do not exist yet.
fn seeded_vault() -> (TempDir, Utf8PathBuf) {
    let (tmp, root) = vault();
    write(
        &root,
        "hub.md",
        "---\naliases:\n  - Hub\n---\n# Hub\n\n[[alpha]], [[beta]], [[gamma]], \
         [[dupe]], [alpha link](alpha.md), [[notes/alpha]]\n",
    );
    write(&root, "alpha.md", "---\n---\n# Alpha\n\nsee [[hub]]\n");
    write(&root, "notes/dupe.md", "---\n---\n# Dupe one\n");
    write(&root, "other/dupe.md", "---\n---\n# Dupe two\n");
    (tmp, root)
}

#[test]
fn creating_a_document_resolves_a_link_that_dangled_elsewhere() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    // Pre-state witness: `hub.md`'s `[[beta]]` is dangling before the create.
    let hub = baseline
        .documents
        .iter()
        .find(|d| d.path == "hub.md")
        .unwrap();
    assert!(hub
        .links
        .iter()
        .any(|link| link.target == "beta" && link.resolved_path.is_none()));

    write(&root, "beta.md", "---\n---\n# Beta\n");
    assert_overlay_equals_rebuild(&root, baseline, &["beta.md"], "create");
}

#[test]
fn deleting_a_document_dangles_the_links_that_pointed_at_it() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    remove(&root, "alpha.md");
    assert_overlay_equals_rebuild(&root, baseline, &["alpha.md"], "delete");
}

#[test]
fn renaming_a_document_moves_every_link_that_pointed_at_it() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    let body = std::fs::read_to_string(root.join("alpha.md").as_std_path()).unwrap();
    remove(&root, "alpha.md");
    write(&root, "notes/alpha.md", &body);
    assert_overlay_equals_rebuild(
        &root,
        baseline,
        &["alpha.md", "notes/alpha.md"],
        "rename/move",
    );
}

#[test]
fn adding_an_alias_changes_nothing_a_rebuild_would_not_change() {
    // Aliases do NOT participate in resolution (NRN-455), so the graph must be
    // identical to a rebuild — including the derived `aliases` set itself.
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    write(
        &root,
        "alpha.md",
        "---\naliases:\n  - Beta\n  - The First\n---\n# Alpha\n\nsee [[hub]]\n",
    );
    assert_overlay_equals_rebuild(&root, baseline, &["alpha.md"], "alias add");
}

#[test]
fn removing_an_alias_changes_nothing_a_rebuild_would_not_change() {
    let (_tmp, root) = seeded_vault();
    write(
        &root,
        "alpha.md",
        "---\naliases:\n  - Beta\n---\n# Alpha\n\nsee [[hub]]\n",
    );
    let baseline = build_index_with_options(&root, &options()).unwrap();
    write(&root, "alpha.md", "---\n---\n# Alpha\n\nsee [[hub]]\n");
    assert_overlay_equals_rebuild(&root, baseline, &["alpha.md"], "alias remove");
}

#[test]
fn a_new_stem_collision_makes_an_already_resolved_link_ambiguous() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    let hub = baseline
        .documents
        .iter()
        .find(|d| d.path == "hub.md")
        .unwrap();
    assert!(
        hub.links
            .iter()
            .any(|link| link.target == "alpha" && link.resolved_path.is_some()),
        "the pre-state must resolve [[alpha]] uniquely for the collision to bite"
    );

    write(&root, "archive/alpha.md", "---\n---\n# Alpha archived\n");
    assert_overlay_equals_rebuild(&root, baseline, &["archive/alpha.md"], "stem collision");
}

#[test]
fn a_third_document_joins_an_already_ambiguous_stem() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    let hub = baseline
        .documents
        .iter()
        .find(|d| d.path == "hub.md")
        .unwrap();
    assert_eq!(
        hub.links
            .iter()
            .find(|link| link.target == "dupe")
            .unwrap()
            .candidates
            .len(),
        2,
        "the pre-state must already be ambiguous"
    );

    write(&root, "third/dupe.md", "---\n---\n# Dupe three\n");
    assert_overlay_equals_rebuild(&root, baseline, &["third/dupe.md"], "ambiguity grows");
}

#[test]
fn removing_one_of_two_ambiguous_targets_resolves_the_link() {
    let (_tmp, root) = seeded_vault();
    let baseline = build_index_with_options(&root, &options()).unwrap();
    remove(&root, "other/dupe.md");
    assert_overlay_equals_rebuild(&root, baseline, &["other/dupe.md"], "ambiguity shrinks");
}

#[test]
fn changing_a_heading_revalidates_the_anchors_that_pointed_at_it() {
    let (_tmp, root) = vault();
    write(&root, "target.md", "---\n---\n# One\n\n## Section A\n");
    write(
        &root,
        "source.md",
        "---\n---\n[[target#Section A]] and [[target#Section B]] and [[target#^blk]]\n",
    );
    let baseline = build_index_with_options(&root, &options()).unwrap();
    write(
        &root,
        "target.md",
        "---\n---\n# One\n\n## Section B\n\nparagraph ^blk\n",
    );
    assert_overlay_equals_rebuild(&root, baseline, &["target.md"], "heading/block churn");
}

#[test]
fn a_non_markdown_file_appearing_captures_a_markdown_link() {
    let (_tmp, root) = vault();
    write(&root, "note.md", "---\n---\n![shot](assets/shot.png)\n");
    let baseline = build_index_with_options(&root, &options()).unwrap();
    write(&root, "assets/shot.png", "binary-ish");
    assert_overlay_equals_rebuild(&root, baseline, &["assets/shot.png"], "asset create");
}

#[test]
fn embedding_a_document_created_at_the_root_fires_the_root_rung_not_the_base_rung() {
    // `dir/a.md` embeds `![[shared]]`. `link_lookup_keys`'s `Embed` arm records
    // THREE rungs for that one link: base-relative (`dir/shared.md`),
    // root-relative (`shared.md`), and the stem bucket. `shared.md` is created
    // at the vault ROOT, not under `dir/`, so only the root-relative and stem
    // rungs can hit — this pins that the root rung fires on its own, distinct
    // from the base rung it sits alongside.
    let (_tmp, root) = vault();
    write(&root, "dir/a.md", "---\n---\n![[shared]]\n");
    let baseline = build_index_with_options(&root, &options()).unwrap();
    let a = baseline
        .documents
        .iter()
        .find(|d| d.path == "dir/a.md")
        .unwrap();
    assert!(
        a.links
            .iter()
            .any(|link| link.target == "shared" && link.resolved_path.is_none()),
        "the pre-state must have a dangling embed for the create to bite"
    );

    write(&root, "shared.md", "---\n---\n# Shared\n");
    assert_overlay_equals_rebuild(&root, baseline, &["shared.md"], "root-rung embed create");
}

#[test]
fn heading_churn_revalidates_a_documents_self_anchor_link() {
    // A document that links `[[#Heading]]` to one of its OWN headings takes the
    // self-reference branch of the resolution ladder (empty target, an anchor)
    // — `link_lookup_keys` records only the document's own source-path key for
    // it, not a stem/path key derived from a target. Churning the heading the
    // self-link names must still revalidate it.
    let (_tmp, root) = vault();
    write(
        &root,
        "self.md",
        "---\n---\n# Section A\n\nSee [[#Section A]] above.\n",
    );
    let baseline = build_index_with_options(&root, &options()).unwrap();
    let doc = baseline
        .documents
        .iter()
        .find(|d| d.path == "self.md")
        .unwrap();
    assert!(
        doc.links
            .iter()
            .any(|link| link.target.is_empty() && link.resolved_path.is_some()),
        "the pre-state self-anchor link must already resolve"
    );

    write(
        &root,
        "self.md",
        "---\n---\n# Section B\n\nSee [[#Section A]] above.\n",
    );
    assert_overlay_equals_rebuild(&root, baseline, &["self.md"], "self-anchor heading churn");
}

// ── Randomized single-change property ───────────────────────────────────────

/// Deterministic xorshift64* — a seeded generator so a failure reproduces from
/// the printed seed without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Names the generator draws both document paths and link targets from, so the
/// vault has real hits, real misses, real stem collisions across folders, and
/// real case variation.
const NAMES: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "Alpha", "one.two", "x", "y",
];
const FOLDERS: [&str; 4] = ["", "notes/", "notes/deep/", "archive/"];

/// Draw a document path that does not collide with an existing one even on a
/// case-insensitive filesystem. Two differently-cased spellings of one path name
/// ONE file on macOS, which makes "the vault contains both" untestable there —
/// but even where the filesystem tolerates it, `resolve_links`'s `by_path_lower`
/// table has only one entry for two paths differing only in case, a known,
/// separately pinned defect (`case_only_duplicate_filenames_silently_overwrite_in_path_lower`
/// in `links::resolve`; tracked as NRN-524). Generating a case-only collision
/// here would make an overlay-vs-rebuild mismatch ambiguous between "the
/// scoping logic under test is wrong" and "the pre-existing table-collision
/// defect fired," so this sweep dodges the collision entirely rather than
/// exercising it. Case sensitivity in RESOLUTION is still exercised, via link
/// targets that differ in case from the document they name.
fn distinct_path(rng: &mut Rng, taken: &[String]) -> Option<String> {
    let rel = format!(
        "{}{}.md",
        FOLDERS[rng.below(FOLDERS.len())],
        NAMES[rng.below(NAMES.len())]
    );
    taken
        .iter()
        .all(|existing| !existing.eq_ignore_ascii_case(&rel))
        .then_some(rel)
}

fn generated_vault(rng: &mut Rng, root: &Utf8Path) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for _ in 0..24 {
        let Some(rel) = distinct_path(rng, &paths) else {
            continue;
        };
        let own_heading = NAMES[rng.below(NAMES.len())];
        let mut body = String::from("---\naliases:\n  - ");
        body.push_str(NAMES[rng.below(NAMES.len())]);
        body.push_str("\n---\n\n# Heading ");
        body.push_str(own_heading);
        body.push_str("\n\n");
        for _ in 0..rng.below(6) + 1 {
            let target = NAMES[rng.below(NAMES.len())];
            match rng.below(6) {
                0 => body.push_str(&format!("[[{target}]] ")),
                1 => body.push_str(&format!(
                    "[[{}{target}]] ",
                    FOLDERS[rng.below(FOLDERS.len())]
                )),
                2 => body.push_str(&format!("[link]({target}.md) ")),
                3 => body.push_str(&format!("[[{target}#Heading {target}]] ")),
                // Wikilink-embed: same target resolution as a wikilink, but the
                // `!`-prefixed embed form, which walks a different rung set
                // (base-relative, root-relative, and stem — see
                // `link_lookup_keys`'s `LinkKind::Embed` arm).
                4 => body.push_str(&format!("![[{target}]] ")),
                // Self-anchor: an empty-target wikilink pointing at this
                // document's OWN heading — the self-reference branch that owns
                // only its own source-path key, not a stem/path key derived
                // from `target`.
                _ => body.push_str(&format!("[[#Heading {own_heading}]] ")),
            }
        }
        body.push_str("\n\nparagraph ^blk\n");
        write(root, &rel, &body);
        paths.push(rel);
    }
    paths
}

#[test]
fn incremental_matches_a_full_rebuild_for_a_random_single_document_change() {
    for seed in 1u64..=40 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (_tmp, root) = vault();
        let paths = generated_vault(&mut rng, &root);
        let baseline = build_index_with_options(&root, &options()).unwrap();

        let victim = paths[rng.below(paths.len())].clone();
        let changed: Vec<String> = match rng.below(4) {
            // Create a brand-new document that may collide on stem.
            0 => {
                let Some(rel) = distinct_path(&mut rng, &paths) else {
                    continue;
                };
                write(&root, &rel, "---\n---\n# Fresh\n\nparagraph ^blk\n");
                vec![rel]
            }
            // Delete an existing document.
            1 => {
                remove(&root, &victim);
                vec![victim]
            }
            // Rename an existing document into another folder.
            2 => {
                let body = std::fs::read_to_string(root.join(&victim).as_std_path()).unwrap();
                let stem = Utf8Path::new(&victim).file_stem().unwrap().to_string();
                let moved = format!("{}{stem}.md", FOLDERS[rng.below(FOLDERS.len())]);
                if paths.iter().any(|taken| taken.eq_ignore_ascii_case(&moved)) {
                    continue;
                }
                remove(&root, &victim);
                write(&root, &moved, &body);
                vec![victim, moved]
            }
            // Rewrite an existing document's frontmatter, headings, and links.
            _ => {
                let target = NAMES[rng.below(NAMES.len())];
                write(
                    &root,
                    &victim,
                    &format!(
                        "---\naliases:\n  - {target}\n---\n\n# Rewritten {target}\n\n[[{target}]] \
                         [[{target}#Rewritten {target}]]\n\nparagraph ^other\n"
                    ),
                );
                vec![victim]
            }
        };

        let changed: Vec<Utf8PathBuf> = changed.iter().map(Utf8PathBuf::from).collect();
        let mut incremental = baseline;
        overlay_changed_paths(&mut incremental, &root, &changed, &options());
        let rebuilt = build_index_with_options(&root, &options()).unwrap();
        assert_eq!(
            comparable(&incremental),
            comparable(&rebuilt),
            "seed {seed}: incremental re-resolution diverged from a full rebuild for \
             changed={changed:?}"
        );
    }
}

/// Confound-free counterpart to
/// [`incremental_matches_a_full_rebuild_for_a_random_single_document_change`]:
/// that test compares against a cold REBUILD, which conflates two axes — did
/// the overlay choose the right SCOPE to re-resolve, and did it land on the
/// right DOCUMENT SET (file/duplicate handling, tracked separately as
/// NRN-524). A changed-path spelling variant or a phantom entry can trip the
/// second axis without the scoping logic under test being at fault at all.
///
/// This sweep isolates the first axis: [`assert_overlay_scoping_is_sound`]
/// compares the overlay's scoped re-resolution against a full `resolve_links`
/// pass over a CLONE of that same overlay's own document set, so there is no
/// second document set to disagree about. It also perturbs the changed-path
/// spelling (a bare rel vs. a `./`-prefixed rel) and adds a phantom entry (a
/// changed path that never existed on disk, as another layer might report) to
/// confirm neither perturbs the scoping proof.
#[test]
fn scoped_overlay_matches_full_resolve_over_the_same_document_set_for_random_changes() {
    for seed in 1u64..=300 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 5);
        let (_tmp, root) = vault();
        let paths = generated_vault(&mut rng, &root);
        let baseline = build_index_with_options(&root, &options()).unwrap();

        let victim = paths[rng.below(paths.len())].clone();
        let mut changed: Vec<String> = match rng.below(4) {
            // Create a brand-new document that may collide on stem.
            0 => {
                let Some(rel) = distinct_path(&mut rng, &paths) else {
                    continue;
                };
                write(&root, &rel, "---\n---\n# Fresh\n\nparagraph ^blk\n");
                vec![rel]
            }
            // Delete an existing document.
            1 => {
                remove(&root, &victim);
                vec![victim]
            }
            // Rename an existing document into another folder.
            2 => {
                let body = std::fs::read_to_string(root.join(&victim).as_std_path()).unwrap();
                let stem = Utf8Path::new(&victim).file_stem().unwrap().to_string();
                let moved = format!("{}{stem}.md", FOLDERS[rng.below(FOLDERS.len())]);
                if paths.iter().any(|taken| taken.eq_ignore_ascii_case(&moved)) {
                    continue;
                }
                remove(&root, &victim);
                write(&root, &moved, &body);
                vec![victim, moved]
            }
            // Rewrite an existing document's frontmatter, headings, and links.
            _ => {
                let target = NAMES[rng.below(NAMES.len())];
                write(
                    &root,
                    &victim,
                    &format!(
                        "---\naliases:\n  - {target}\n---\n\n# Rewritten {target}\n\n[[{target}]] \
                         [[{target}#Rewritten {target}]]\n\nparagraph ^other\n"
                    ),
                );
                vec![victim]
            }
        };

        // Spelling variant: a caller can report a changed path with a leading
        // `./`. Both sides of THIS comparison overlay the identical set either
        // way, so this can only exercise the scoping logic, never the
        // dedup/retain axis NRN-524 owns.
        if rng.below(2) == 0 {
            changed = changed.into_iter().map(|rel| format!("./{rel}")).collect();
        }
        // Phantom entry: a changed path that never existed before or after —
        // e.g. a rename another layer reported that this vault never actually
        // saw. `parse_graph_path` returns `None` for it and it drops out; the
        // reverse-index lookup for it is simply empty.
        if rng.below(3) == 0 {
            changed.push(format!(
                "{}ghost-{seed}.md",
                FOLDERS[rng.below(FOLDERS.len())]
            ));
        }

        let changed: Vec<Utf8PathBuf> = changed.iter().map(Utf8PathBuf::from).collect();
        assert_overlay_scoping_is_sound(&root, baseline, &changed, &format!("seed {seed}"));
    }
}
