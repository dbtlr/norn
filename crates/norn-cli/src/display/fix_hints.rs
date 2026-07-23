//! Hardcoded fix-hint lookup keyed by finding code. Not every code needs a hint
//! (the message + code
//! are sometimes enough to point an operator at the fix); the `validate
//! --format records` full view appends one under each finding when present.

pub fn fix_hint_for(code: &str) -> Option<&'static str> {
    match code {
        "frontmatter-required-field-missing" => Some(
            "add the field to the document's frontmatter, or scope the rule to exclude this path",
        ),
        "value-not-allowed" => Some(
            "change the value to one of the allowed values, or relax the rule's allowed-values list",
        ),
        "field-type-invalid" => {
            Some("coerce the value to the expected type, or relax the rule's field-type constraint")
        }
        "frontmatter-exceeds-max-length" => {
            Some("shorten the value to fit the bound, or raise the rule's max_length")
        }
        "frontmatter-forbidden-field" => {
            Some("remove the field from the document, or relax the rule's forbidden-fields list")
        }
        "document-misrouted" => Some(
            "move the document under one of the allowed paths, or relax the rule's path constraint",
        ),
        "frontmatter-alias-malformed" => {
            Some("remove or fix the non-scalar entries in the alias field")
        }
        "frontmatter-alias-shadowed-by-stem" => {
            Some("remove the alias (it never resolves) or rename the shadowing document")
        }
        "frontmatter-alias-duplicate-across-docs" => {
            Some("remove the alias from all but one document so resolution is unambiguous")
        }
        "link-target-missing" => {
            Some("fix the link target, or run `norn repair --plan` for a closest-match proposal")
        }
        "link-anchor-missing" => Some(
            "fix the anchor (the heading may have been renamed), or remove the anchor from the link",
        ),
        "link-block-missing" => Some("fix the block-ref, or remove the block suffix from the link"),
        "link-ambiguous" => {
            Some("qualify the link target with a directory prefix to pick one of the candidates")
        }
        "frontmatter-reference-type" => {
            Some("repoint the field at a document of an allowed type, or change the target's type")
        }
        "frontmatter-parse-failed" => {
            Some("fix the YAML syntax in the document's frontmatter, then re-run validate")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_return_hint() {
        assert!(fix_hint_for("frontmatter-required-field-missing").is_some());
        assert!(fix_hint_for("link-target-missing").is_some());
        assert!(fix_hint_for("value-not-allowed").is_some());
        assert!(fix_hint_for("field-type-invalid").is_some());
    }

    #[test]
    fn unknown_codes_return_none() {
        assert!(fix_hint_for("not-a-real-code").is_none());
    }
}
