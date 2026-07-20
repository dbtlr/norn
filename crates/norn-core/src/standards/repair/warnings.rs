//! Plan warnings — informational, non-blocking.
//!
//! Currently surfaces stem-collision warnings: a rename whose new stem
//! already exists elsewhere in the vault inventory introduces wikilink
//! ambiguity. The operator decides whether to revise the rule.

use crate::domain::Document;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanWarning {
    StemCollisionAfterMove {
        new_stem: String,
        new_path: Utf8PathBuf,
        collides_with: Vec<Utf8PathBuf>,
    },
}

/// Detects a stem collision: a rename (stem change) where the new stem already
/// exists elsewhere in the vault inventory. Returns None when the stem doesn't
/// change OR when the new stem is unique.
pub fn detect_stem_collision(
    old_path: &Utf8Path,
    new_path: &Utf8Path,
    documents: &[Document],
) -> Option<PlanWarning> {
    let old_stem = old_path.file_stem()?;
    let new_stem = new_path.file_stem()?;
    if old_stem == new_stem {
        return None;
    }

    let new_stem_lower = new_stem.to_lowercase();
    let mut collisions: Vec<Utf8PathBuf> = documents
        .iter()
        .filter(|doc| doc.path != old_path)
        .filter(|doc| doc.stem.to_lowercase() == new_stem_lower)
        .map(|doc| doc.path.clone())
        .collect();

    if collisions.is_empty() {
        return None;
    }
    collisions.sort();

    Some(PlanWarning::StemCollisionAfterMove {
        new_stem: new_stem.to_string(),
        new_path: new_path.to_path_buf(),
        collides_with: collisions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_doc(path: &str) -> Document {
        Document {
            path: path.into(),
            stem: Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: String::new(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    #[test]
    fn no_warning_when_stem_unchanged() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Workspaces/x/tasks/task.md");
        let documents = vec![make_doc("Inbox/task.md"), make_doc("Other/notes/task.md")];
        let warning = detect_stem_collision(&old, &new, &documents);
        assert!(warning.is_none());
    }

    #[test]
    fn warning_when_rename_introduces_collision() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Inbox/next-task.md");
        let documents = vec![
            make_doc("Inbox/task.md"),
            make_doc("Workspaces/y/notes/next-task.md"),
        ];
        let warning = detect_stem_collision(&old, &new, &documents).unwrap();
        match warning {
            PlanWarning::StemCollisionAfterMove {
                new_stem,
                new_path,
                collides_with,
            } => {
                assert_eq!(new_stem, "next-task");
                assert_eq!(new_path, Utf8PathBuf::from("Inbox/next-task.md"));
                assert_eq!(
                    collides_with,
                    vec![Utf8PathBuf::from("Workspaces/y/notes/next-task.md")]
                );
            }
        }
    }

    #[test]
    fn no_warning_when_new_stem_is_unique() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Inbox/unique-name.md");
        let documents = vec![make_doc("Inbox/task.md"), make_doc("Other/elsewhere.md")];
        let warning = detect_stem_collision(&old, &new, &documents);
        assert!(warning.is_none());
    }

    #[test]
    fn warning_excludes_self_when_old_doc_listed() {
        let old = Utf8PathBuf::from("Inbox/task.md");
        let new = Utf8PathBuf::from("Inbox/task.md");
        let documents = vec![make_doc("Inbox/task.md")];
        let warning = detect_stem_collision(&old, &new, &documents);
        assert!(warning.is_none());
    }
}
