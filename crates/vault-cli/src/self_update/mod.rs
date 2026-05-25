//! Self-update subcommand: refreshes the running `vault` binary from the
//! latest GitHub release (or a pinned version).

#![allow(dead_code)]

pub mod manifest;
pub mod receipt;
pub mod resolve;
