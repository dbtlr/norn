//! Target resolution and backlink lookup over a built graph.
//!
//! `resolve_target_path` turns a user-supplied target (an exact path or a stem)
//! into a single vault path, erroring on an ambiguous stem; `backlinks`
//! collects the links across the index that resolve to a given path. The
//! mutation appliers and `delete` use these to find what points at a document
//! before they move or remove it.
//!
//! This module also owns the WORDING of a failed resolution, not just its
//! codes: [`TargetResolution::or_refuse`] and the two message builders are the
//! single source for every "no document matched" / "ambiguous document stem"
//! string in the tree, so no verb spells its own.

use crate::domain::{GraphIndex, Link};
use anyhow::{anyhow, Result};
use camino::Utf8PathBuf;

pub fn backlinks<'a>(index: &'a GraphIndex, target_path: &Utf8PathBuf) -> Vec<&'a Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.resolved_path.as_ref() == Some(target_path))
        .collect()
}

/// The outcome of resolving a user-supplied target to a vault path: a unique
/// match, no match, or an ambiguous stem carrying the real candidate paths. The
/// `Ambiguous` arm keeps the candidates structured so a caller can name them in
/// a refusal message rather than reconstructing (or dropping) the set — see
/// [`resolve_target_path`] for the string-shaped wrapper the read verbs use.
pub enum TargetResolution {
    Resolved(Utf8PathBuf),
    NotFound,
    Ambiguous(Vec<Utf8PathBuf>),
}

impl TargetResolution {
    /// Collapse to the resolved path or the refusal the failure reports as —
    /// the shape every mutating verb wants, since both failure arms refuse the
    /// same way and only the slot's codes and prefix distinguish them.
    pub fn or_refuse(self, slot: TargetSlot, target: &str) -> Result<Utf8PathBuf, TargetRefusal> {
        match self {
            Self::Resolved(path) => Ok(path),
            Self::NotFound => Err(TargetRefusal {
                code: slot.not_found_code(),
                message: format!("{}{}", slot.prefix(), target_not_found_message(target)),
            }),
            Self::Ambiguous(candidates) => Err(TargetRefusal {
                code: slot.ambiguous_code(),
                message: format!(
                    "{}{}",
                    slot.prefix(),
                    target_ambiguous_message(target, &candidates)
                ),
            }),
        }
    }
}

/// Which addressable input a target refusal is about. A verb with a single
/// document argument (`set`, `edit`, `move`'s source, `delete`'s target) uses
/// [`TargetSlot::Target`]; `delete --rewrite-to` resolves a SECOND document, so
/// it carries its own codes and names the flag ahead of the shared prose,
/// telling a reader which of the two inputs failed.
#[derive(Clone, Copy)]
pub enum TargetSlot {
    Target,
    RewriteTo,
}

impl TargetSlot {
    fn not_found_code(self) -> &'static str {
        match self {
            Self::Target => "target-not-found",
            Self::RewriteTo => "rewrite-to-not-found",
        }
    }

    fn ambiguous_code(self) -> &'static str {
        match self {
            Self::Target => "target-ambiguous",
            Self::RewriteTo => "rewrite-to-ambiguous",
        }
    }

    /// What the slot prints ahead of the shared prose. The primary target adds
    /// nothing — it is what a verb is about — so only the redirect slot is
    /// qualified.
    fn prefix(self) -> &'static str {
        match self {
            Self::Target => "",
            Self::RewriteTo => "--rewrite-to: ",
        }
    }
}

/// A failed target resolution ready for the wire: the stable kebab `code` and
/// the operator-facing `message`. Every verb refuses with a pair built here, so
/// a miss reads the same on `set` as on `delete` and one ambiguous stem renders
/// its candidates identically everywhere.
pub struct TargetRefusal {
    pub code: &'static str,
    pub message: String,
}

/// The one not-found message: a target that matched nothing, naming the ladder
/// it was tried against (an exact path, then a case-insensitive stem — nothing
/// else resolves).
pub fn target_not_found_message(target: &str) -> String {
    format!("no document matched path or stem: {target}")
}

/// The one ambiguous message: the colliding paths as a readable comma-joined
/// list in resolver (lexical) order, never a `Debug` rendering of the vector.
pub fn target_ambiguous_message(target: &str, candidates: &[Utf8PathBuf]) -> String {
    format!(
        "ambiguous document stem: {target}; candidates: {}",
        join_candidates(candidates)
    )
}

/// Render candidate paths as the single list form every candidate-bearing
/// message uses.
pub fn join_candidates(candidates: &[Utf8PathBuf]) -> String {
    candidates
        .iter()
        .map(|path| path.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a target (exact path first, then a case-insensitive stem match) to a
/// structured outcome. The mutation refusal paths (`delete`) branch on this to
/// surface the real ambiguous candidates; [`resolve_target_path`] wraps it for
/// callers that only need the `anyhow` string error.
pub fn resolve_target(index: &GraphIndex, target: &str) -> TargetResolution {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return TargetResolution::Resolved(document.path.clone());
    }

    let mut matches = index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();
    // Lexical path order is the refusal contract; sorting here keeps it
    // independent of the index's own document ordering.
    matches.sort();

    match matches.as_slice() {
        [path] => TargetResolution::Resolved(path.clone()),
        [] => TargetResolution::NotFound,
        _ => TargetResolution::Ambiguous(matches),
    }
}

/// The `anyhow`-shaped wrapper the read verbs use to resolve a `--links-to`
/// operand. It carries the same prose as a mutating verb's refusal — one
/// resolution failure has one wording, whatever consumed the target.
pub fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    resolve_target(index, target)
        .or_refuse(TargetSlot::Target, target)
        .map_err(|refusal| anyhow!(refusal.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Document, GraphIndex, Link, LinkKind, LinkStatus};

    fn doc(path: &str, stem: &str) -> Document {
        Document {
            path: path.into(),
            stem: stem.into(),
            hash: "h".into(),
            frontmatter: None,
            head_text: String::new(),
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn index(documents: Vec<Document>) -> GraphIndex {
        GraphIndex {
            root: "/vault".into(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    fn link_to(source: &str, resolved: Option<&str>) -> Link {
        Link {
            source_path: source.into(),
            raw: "[[x]]".into(),
            kind: LinkKind::Wikilink,
            target: "x".into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: resolved.map(Utf8PathBuf::from),
            unresolved_reason: None,
            candidates: vec![],
            status: LinkStatus::Resolved,
        }
    }

    #[test]
    fn resolve_exact_path_wins() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        assert_eq!(
            resolve_target_path(&idx, "notes/a.md").unwrap(),
            Utf8PathBuf::from("notes/a.md")
        );
    }

    #[test]
    fn resolve_unique_stem_case_insensitive() {
        let idx = index(vec![doc("notes/Alpha.md", "Alpha")]);
        assert_eq!(
            resolve_target_path(&idx, "alpha").unwrap(),
            Utf8PathBuf::from("notes/Alpha.md")
        );
    }

    #[test]
    fn resolve_missing_target_errors() {
        let idx = index(vec![doc("notes/a.md", "a")]);
        let err = resolve_target_path(&idx, "missing").unwrap_err();
        assert!(err.to_string().contains("no document matched"), "{err}");
    }

    #[test]
    fn resolve_target_returns_structured_candidates_for_ambiguous_stem() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        match resolve_target(&idx, "a") {
            TargetResolution::Ambiguous(candidates) => assert_eq!(
                candidates,
                vec![
                    Utf8PathBuf::from("notes/a.md"),
                    Utf8PathBuf::from("tasks/a.md"),
                ]
            ),
            _ => panic!("expected Ambiguous with real candidates, got a different arm"),
        }
        assert!(matches!(
            resolve_target(&idx, "missing"),
            TargetResolution::NotFound
        ));
        assert!(matches!(
            resolve_target(&idx, "notes/a.md"),
            TargetResolution::Resolved(_)
        ));
    }

    #[test]
    fn resolve_ambiguous_stem_errors_with_candidates() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);
        let err = resolve_target_path(&idx, "a").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous document stem"), "{msg}");
        assert!(
            msg.contains("notes/a.md") && msg.contains("tasks/a.md"),
            "{msg}"
        );
    }

    /// The target slot picks the codes and the qualifier; the prose after it is
    /// the same string on both slots, which is the whole point of routing every
    /// verb through one constructor.
    #[test]
    fn or_refuse_gives_one_wording_per_family_and_slot_specific_codes() {
        let idx = index(vec![doc("notes/a.md", "a"), doc("tasks/a.md", "a")]);

        let miss = resolve_target(&idx, "missing")
            .or_refuse(TargetSlot::Target, "missing")
            .unwrap_err();
        assert_eq!(miss.code, "target-not-found");
        assert_eq!(miss.message, "no document matched path or stem: missing");

        let miss_redirect = resolve_target(&idx, "missing")
            .or_refuse(TargetSlot::RewriteTo, "missing")
            .unwrap_err();
        assert_eq!(miss_redirect.code, "rewrite-to-not-found");
        assert_eq!(
            miss_redirect.message,
            "--rewrite-to: no document matched path or stem: missing"
        );

        let ambiguous = resolve_target(&idx, "a")
            .or_refuse(TargetSlot::Target, "a")
            .unwrap_err();
        assert_eq!(ambiguous.code, "target-ambiguous");
        assert_eq!(
            ambiguous.message,
            "ambiguous document stem: a; candidates: notes/a.md, tasks/a.md"
        );

        let ambiguous_redirect = resolve_target(&idx, "a")
            .or_refuse(TargetSlot::RewriteTo, "a")
            .unwrap_err();
        assert_eq!(ambiguous_redirect.code, "rewrite-to-ambiguous");
        assert_eq!(
            ambiguous_redirect.message,
            "--rewrite-to: ambiguous document stem: a; candidates: notes/a.md, tasks/a.md"
        );
    }

    /// Candidates render as a readable joined list — never the `Debug` form of
    /// the vector, which leaks Rust quoting and brackets into operator prose.
    #[test]
    fn candidates_render_joined_not_debug() {
        let candidates = vec![
            Utf8PathBuf::from("archive2/duplicate.md"),
            Utf8PathBuf::from("notes/duplicate.md"),
        ];
        assert_eq!(
            join_candidates(&candidates),
            "archive2/duplicate.md, notes/duplicate.md"
        );
        let message = target_ambiguous_message("duplicate", &candidates);
        assert!(!message.contains('['), "{message}");
        assert!(!message.contains('"'), "{message}");
    }

    #[test]
    fn backlinks_collects_links_resolving_to_target() {
        let mut source = doc("notes/src.md", "src");
        source.links = vec![
            link_to("notes/src.md", Some("notes/target.md")),
            link_to("notes/src.md", Some("notes/other.md")),
        ];
        let idx = index(vec![source, doc("notes/target.md", "target")]);
        let found = backlinks(&idx, &Utf8PathBuf::from("notes/target.md"));
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].resolved_path.as_ref().unwrap(),
            &Utf8PathBuf::from("notes/target.md")
        );
    }

    #[test]
    fn backlinks_empty_when_nothing_points_at_target() {
        let idx = index(vec![doc("notes/target.md", "target")]);
        assert!(backlinks(&idx, &Utf8PathBuf::from("notes/target.md")).is_empty());
    }
}
