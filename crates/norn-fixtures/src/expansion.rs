//! Seeded expansion: procedurally generates `profile.expansion_docs`
//! documents under a `garden/`-rooted folder tree, plus `tasks/**` and
//! `phases/**` placements for typed docs. Every choice — folder, doc name,
//! type, dates, body shape, links, and (round-robin) violation injection —
//! is driven by `Rng`, so `(profile, seed)` fully determines the output.

use crate::contract::{PHASES_DIR, STATUS_VALUES, TASKS_DIR};
use crate::dates::from_base_plus_minutes;
use crate::rng::Rng;
use crate::words::{DOC_WORDS, FOLDER_WORDS, HEADING_WORDS, SENTENCE_WORDS};
use crate::yaml;
use crate::Profile;

pub struct ExpansionDoc {
    pub path: String,
    pub content: String,
    /// Finding codes the injected violation (if any) makes the oracle report;
    /// empty for a clean expansion doc.
    pub codes: &'static [&'static str],
}

/// A previously emitted document, tracked so later docs can link to it.
/// `link_stem` is the bare filename stem used for stem-form wikilinks;
/// `link_path` is the vault-relative path without extension, used for
/// path-qualified wikilinks.
pub struct KnownDoc {
    pub link_stem: String,
    pub link_path: String,
}

impl KnownDoc {
    /// Build a link target from a stem and a vault-relative path. The single
    /// owner of the `.md`-trim rule for `link_path` (deduped from lib.rs).
    pub fn new(link_stem: String, path: &str) -> Self {
        KnownDoc {
            link_stem,
            link_path: path.trim_end_matches(".md").to_string(),
        }
    }
}

/// The base document shape drawn per expansion doc. Mirrors `ViolationKind`:
/// an explicit enum rather than a bare `usize`, so the match arms are
/// exhaustive and self-documenting.
#[derive(Clone, Copy)]
enum DocKind {
    Note,
    Task,
    Phase,
    Plain,
}

/// Index-addressable in the same 0..4 order the rng draw expects, so the draw
/// keeps consuming exactly one value and the stream shape is unchanged.
const DOC_KINDS: [DocKind; 4] = [DocKind::Note, DocKind::Task, DocKind::Phase, DocKind::Plain];

/// Round-robin violation classes injected into expansion docs. A small,
/// deliberately-reduced subset of the full violation-zoo repertoire (see
/// the module doc on why): each class is generically applicable to a
/// procedurally-placed document without needing bespoke path plumbing.
#[derive(Clone, Copy)]
enum ViolationKind {
    MissingKind,
    BadStatus,
    FieldTypeInvalid,
    Misrouted,
    DeadLink,
}

const VIOLATION_KINDS: [ViolationKind; 5] = [
    ViolationKind::MissingKind,
    ViolationKind::BadStatus,
    ViolationKind::FieldTypeInvalid,
    ViolationKind::Misrouted,
    ViolationKind::DeadLink,
];

impl ViolationKind {
    /// The finding code(s) the oracle reports for a doc carrying this class.
    fn codes(self) -> &'static [&'static str] {
        match self {
            ViolationKind::MissingKind => &["frontmatter-required-field-missing"],
            ViolationKind::BadStatus => &["value-not-allowed"],
            ViolationKind::FieldTypeInvalid => &["field-type-invalid"],
            ViolationKind::Misrouted => &["document-misrouted"],
            ViolationKind::DeadLink => &["link-target-missing"],
        }
    }
}

/// Build the deterministic `garden/`-rooted folder tree: `garden` itself,
/// plus `folder_width` children per parent for `folder_depth` levels. No
/// `Rng` involved — folder identity comes from position, not chance.
fn build_folders(depth: usize, width: usize) -> Vec<String> {
    let mut folders = vec!["garden".to_string()];
    if depth == 0 || width == 0 {
        return folders;
    }
    let mut frontier = vec!["garden".to_string()];
    for _level in 0..depth {
        let mut next_frontier = Vec::new();
        for parent in &frontier {
            for w in 0..width {
                let word = FOLDER_WORDS[w % FOLDER_WORDS.len()];
                let child = format!("{parent}/{word}");
                folders.push(child.clone());
                next_frontier.push(child);
            }
        }
        frontier = next_frontier;
    }
    folders
}

fn titlecase(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn build_body(rng: &mut Rng) -> String {
    let sections = 1 + rng.range(4);
    let mut body = String::new();
    for _ in 0..sections {
        let heading = rng.pick(HEADING_WORDS);
        body.push_str("## ");
        body.push_str(heading);
        body.push_str("\n\n");
        let sentence_count = 1 + rng.range(3);
        let mut sentences = Vec::with_capacity(sentence_count);
        for _ in 0..sentence_count {
            sentences.push(format!("{}.", rng.pick(SENTENCE_WORDS)));
        }
        body.push_str(&sentences.join(" "));
        body.push_str("\n\n");
    }
    body
}

fn build_links(rng: &mut Rng, profile: &Profile, known: &[KnownDoc], ghost_seed: usize) -> String {
    if known.is_empty() || profile.max_links_per_doc == 0 {
        return String::new();
    }
    let link_count = rng.range(profile.max_links_per_doc + 1);
    if link_count == 0 {
        return String::new();
    }
    let mut out = String::from("\nLinks:\n\n");
    for k in 0..link_count {
        let is_broken =
            profile.broken_link_per_mille > 0 && rng.chance(profile.broken_link_per_mille, 1000);
        if is_broken {
            out.push_str(&format!("- [[ghost-{ghost_seed}-{k}]]\n"));
            continue;
        }
        let target = rng.pick(known);
        let form = rng.range(3);
        let line = match form {
            0 => format!("- [[{}]]\n", target.link_stem),
            1 => format!("- [[{}]]\n", target.link_path),
            _ => {
                let display = rng.pick(SENTENCE_WORDS);
                let short: String = display
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("- [[{}|{}]]\n", target.link_stem, short)
            }
        };
        out.push_str(&line);
    }
    out
}

#[allow(clippy::too_many_lines)]
pub fn generate(profile: &Profile, seed: u64, seed_docs: Vec<KnownDoc>) -> Vec<ExpansionDoc> {
    let mut rng = Rng::new(seed ^ 0xE5C4_9A17_D02B_88F1);
    let folders = build_folders(profile.folder_depth, profile.folder_width);
    let mut known = seed_docs;
    let mut out = Vec::with_capacity(profile.expansion_docs);
    let mut violation_counter = 0usize;

    for i in 0..profile.expansion_docs {
        let word = DOC_WORDS[i % DOC_WORDS.len()];
        let stem = format!("{word}-{i:03}");
        let title = format!("{} {}", titlecase(word), i);

        let mut kind = DOC_KINDS[rng.range(4)];

        let inject_violation =
            profile.violation_per_mille > 0 && rng.chance(profile.violation_per_mille, 1000);
        let violation = if inject_violation {
            let v = VIOLATION_KINDS[violation_counter % VIOLATION_KINDS.len()];
            violation_counter += 1;
            Some(v)
        } else {
            None
        };

        // Violations that require a specific base type override the rng
        // choice; the rng call above still happened, keeping the stream
        // shape stable regardless of injection.
        if let Some(v) = violation {
            kind = match v {
                ViolationKind::MissingKind | ViolationKind::FieldTypeInvalid => DocKind::Note,
                ViolationKind::BadStatus | ViolationKind::Misrouted => DocKind::Task,
                ViolationKind::DeadLink => kind,
            };
        }

        let folder = rng.pick(&folders);
        let title = yaml::scalar(&title);

        let (path, mut frontmatter) = match kind {
            DocKind::Note => {
                let created_minutes = rng.range(2 * 365 * 24 * 60) as i64;
                let modified_minutes = created_minutes + rng.range(60 * 24 * 30) as i64;
                let created = from_base_plus_minutes(created_minutes).datetime_z();
                let modified = from_base_plus_minutes(modified_minutes).datetime_space();
                let created = match violation {
                    Some(ViolationKind::FieldTypeInvalid) => "not-a-date".to_string(),
                    _ => created,
                };
                let mut fm = format!("title: {title}\ntype: note\n");
                if !matches!(violation, Some(ViolationKind::MissingKind)) {
                    fm.push_str("kind: note\n");
                }
                fm.push_str(&format!("created: {created}\nmodified: {modified}\n"));
                (format!("{folder}/{stem}.md"), fm)
            }
            DocKind::Task => {
                let status = if matches!(violation, Some(ViolationKind::BadStatus)) {
                    "someday"
                } else {
                    rng.pick(STATUS_VALUES)
                };
                let fm = format!(
                    "title: {title}\ntype: task\nstatus: {status}\nparent: \"[[phase-one]]\"\n"
                );
                let path = if matches!(violation, Some(ViolationKind::Misrouted)) {
                    format!("{folder}/{stem}.md")
                } else {
                    format!("{TASKS_DIR}/{stem}.md")
                };
                (path, fm)
            }
            DocKind::Phase => {
                let status = rng.pick(STATUS_VALUES);
                let fm = format!("title: {title}\ntype: phase\nstatus: {status}\n");
                (format!("{PHASES_DIR}/{stem}.md"), fm)
            }
            DocKind::Plain => {
                let fm = format!("title: {title}\n");
                (format!("{folder}/{stem}.md"), fm)
            }
        };

        let body = build_body(&mut rng);
        let mut links = build_links(&mut rng, profile, &known, i);
        if matches!(violation, Some(ViolationKind::DeadLink)) {
            links.push_str(&format!("- [[ghost-expansion-{i}]]\n"));
        }

        frontmatter = format!("---\n{frontmatter}---\n\n");
        let content = format!("{frontmatter}{body}{links}");

        let codes: &'static [&'static str] = violation.map_or(&[], ViolationKind::codes);
        known.push(KnownDoc::new(stem, &path));
        out.push(ExpansionDoc {
            path,
            content,
            codes,
        });
    }

    out
}
