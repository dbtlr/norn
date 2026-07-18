//! Deterministic word pools for the seeded expansion generator. Plain
//! `&[&str]` slices, iterated by index — no `HashMap`, no randomness beyond
//! `Rng`-driven index selection.

/// Folder-name pool (`garden/`-style). Long enough to cover the widest
/// folder fan-out any profile declares without repeating siblings.
pub const FOLDER_WORDS: &[&str] = &[
    "meadow", "grove", "thicket", "hollow", "glade", "copse", "fern", "moss", "willow", "cedar",
    "birch", "pine", "maple", "alder", "bramble", "reed", "hazel", "elm", "yew", "larch",
];

/// Document-stem word pool. Combined with a numeric suffix at generation
/// time to guarantee uniqueness across the whole expansion set.
pub const DOC_WORDS: &[&str] = &[
    "sprout", "acorn", "lantern", "compass", "ember", "harbor", "ridge", "brook", "meridian",
    "quartz", "cinder", "orchard", "beacon", "furrow", "hearth", "kestrel", "linden", "marrow",
    "nimbus", "opal", "pebble", "quill", "ripple", "sable", "tundra", "umber", "vale", "wisp",
    "yarrow", "zephyr",
];

/// Heading-phrase pool for section titles.
pub const HEADING_WORDS: &[&str] = &[
    "Overview",
    "Background",
    "Context",
    "Notes",
    "Details",
    "Findings",
    "Next Steps",
    "Open Questions",
    "References",
    "Summary",
];

/// Sentence-fragment pool. Joined with a trailing period to form fixed
/// sentences — no per-word grammar, just deterministic filler text.
pub const SENTENCE_WORDS: &[&str] = &[
    "The garden grows slowly but surely",
    "Every path leads back to the hollow",
    "A quiet observation worth recording",
    "Nothing here is left to chance",
    "The seed determines every branch",
    "Consistency matters more than speed",
    "Small folders compound into a forest",
    "This sentence exists to fill space deterministically",
    "The same input always yields the same tree",
    "Order is preserved from root to leaf",
];
