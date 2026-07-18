//! Render a PLAN OPERATION example block for inclusion in clap `--help` output.
//!
//! Used by every intent-verb's args struct so the CLI ⇄ plan parity is
//! visible at the point of consumption. Same renderer can be reused for
//! future `norn apply explain` or plan-visualization surfaces.

/// Render a YAML block showing the same invocation as a MigrationPlan op.
///
/// Output shape (for `("move_document", &[("src", "old.md"), ("dst", "new.md")])`):
///
/// ```text
///
/// PLAN OPERATION:
///   - kind: move_document
///     fields:
///       src: old.md
///       dst: new.md
/// ```
///
/// The leading blank line and indentation match clap's `after_help` rendering
/// (which appends the block after the standard help body with a single blank
/// line of separation).
pub fn render_plan_example(kind: &str, fields: &[(&str, &str)]) -> String {
    let mut s = String::from("\nPLAN OPERATION:\n  - kind: ");
    s.push_str(kind);
    if !fields.is_empty() {
        s.push_str("\n    fields:");
        for (k, v) in fields {
            s.push_str("\n      ");
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
        }
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_move_document_example() {
        let yaml = render_plan_example("move_document", &[("src", "old.md"), ("dst", "new.md")]);
        assert!(yaml.contains("PLAN OPERATION"));
        assert!(yaml.contains("kind: move_document"));
        assert!(yaml.contains("src: old.md"));
        assert!(yaml.contains("dst: new.md"));
    }

    #[test]
    fn render_rewrite_wikilink_example() {
        let yaml = render_plan_example(
            "rewrite_wikilink",
            &[("old", "old-target"), ("new", "new-target")],
        );
        assert!(yaml.contains("kind: rewrite_wikilink"));
        assert!(yaml.contains("old: old-target"));
        assert!(yaml.contains("new: new-target"));
    }

    #[test]
    fn render_with_no_fields_only_has_kind() {
        let yaml = render_plan_example("ping", &[]);
        assert!(yaml.contains("kind: ping"));
        // Should still have the PLAN OPERATION header for consistency.
        assert!(yaml.contains("PLAN OPERATION"));
    }
}
