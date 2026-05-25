//! Self-update subcommand: refreshes the running `vault` binary from the
//! latest GitHub release (or a pinned version).

#![allow(dead_code)]

pub mod download;
pub mod manifest;
pub mod receipt;
pub mod render;
pub mod resolve;

use serde::Serialize;

use self::resolve::Action;

/// JSON envelope for `vault self-update`. Independent of other report
/// schemas; `schema_version` bumps when this shape changes.
#[derive(Debug, Serialize)]
pub struct SelfUpdateReport {
    pub schema_version: u32,
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub target_version: String,
    pub target_triple: String,
    pub install_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_sha256: Option<String>,
    pub dry_run: bool,
    pub action: Action,
}

pub const SELF_UPDATE_SCHEMA_VERSION: u32 = 1;
