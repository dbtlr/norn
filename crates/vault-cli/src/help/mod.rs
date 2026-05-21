//! Custom help renderer per the CLI Help Output v2 spec.
//!
//! This module owns rendering for both `-h` and `--help`. clap is used as the
//! argument parser and the source of arg metadata; it does not emit help text.

pub mod bin_name;
pub mod extract;
pub mod model;
pub mod render;

#[allow(unused_imports)]
pub use bin_name::BIN_NAME;
#[allow(unused_imports)]
pub use extract::build_model;
#[allow(unused_imports)]
pub use model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpForm, HelpModel};
#[allow(unused_imports)]
pub use render::{render_long, render_short};
