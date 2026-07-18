//! Output vocabulary for the custom help renderer: the brand color palette
//! and the glyph set. Ported from the donor `src/output/` (retired tree) —
//! only the pieces the help renderer depends on (`palette`, `glyphs`). The
//! record-block primitives, projection, and pager port with the read verbs
//! (separate burn-down rows), not with the help surface.

pub mod glyphs;
pub mod palette;
