//! Link risk classification for move_document changes.
//! Stub — full implementation in a later task (Task 6).

#![allow(dead_code)]

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use vault_core::LinkKind;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct LinkRisk {
    pub stem_changed: bool,
    pub directory_changed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stem_links: Vec<AffectedLink>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub path_qualified_wikilinks: Vec<AffectedLink>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub markdown_links: Vec<AffectedLink>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AffectedLink {
    pub source_path: Utf8PathBuf,
    pub raw: String,
    pub kind: LinkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_span: Option<vault_core::SourceSpan>,
    pub rewritten: String,
}
