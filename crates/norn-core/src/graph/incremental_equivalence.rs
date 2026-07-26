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
/// ONE file on macOS, which makes "the vault contains both" untestable there;
/// case sensitivity in RESOLUTION is still exercised, via link targets that
/// differ in case from the document they name.
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
        let mut body = String::from("---\naliases:\n  - ");
        body.push_str(NAMES[rng.below(NAMES.len())]);
        body.push_str("\n---\n\n# Heading ");
        body.push_str(NAMES[rng.below(NAMES.len())]);
        body.push_str("\n\n");
        for _ in 0..rng.below(4) + 1 {
            let target = NAMES[rng.below(NAMES.len())];
            match rng.below(4) {
                0 => body.push_str(&format!("[[{target}]] ")),
                1 => body.push_str(&format!(
                    "[[{}{target}]] ",
                    FOLDERS[rng.below(FOLDERS.len())]
                )),
                2 => body.push_str(&format!("[link]({target}.md) ")),
                _ => body.push_str(&format!("[[{target}#Heading {target}]] ")),
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
