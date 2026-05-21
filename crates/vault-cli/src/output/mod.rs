//! CLI output module. New commands compose from `primitives` using `palette`
//! and `glyphs`. Unported commands import from `legacy` explicitly so that
//! "still needs porting" is greppable.

pub mod glyphs;
pub mod legacy;
pub mod pager;
pub mod palette;
pub mod primitives;
