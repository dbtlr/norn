//! Plan warnings — informational, non-blocking.
//! Stub — full stem-collision detection in a later task (Task 7).

#![allow(dead_code)]

use camino::Utf8PathBuf;
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
