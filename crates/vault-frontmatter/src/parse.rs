use std::ops::Range;

use vault_core::Diagnostic;

pub fn extract_frontmatter<'a>(
    content: &'a str,
    diagnostics: &mut Vec<Diagnostic>,
) -> (
    Option<serde_json::Value>,
    Option<Range<usize>>,
    &'a str,
    usize,
) {
    let Some(after_open) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (None, None, content, 0);
    };

    let mut offset = content.len() - after_open.len();
    let yaml_start = offset;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let yaml = &content[yaml_start..offset];
            let yaml_range = yaml_start..offset;
            let body_start = offset + line.len();
            let body = &content[body_start..];
            return match serde_yaml::from_str::<serde_yaml::Value>(yaml) {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(value) => (Some(value), Some(yaml_range), body, body_start),
                    Err(error) => {
                        diagnostics.push(
                            Diagnostic::warning(
                                "frontmatter-json-conversion-failed",
                                "frontmatter could not be converted to JSON",
                            )
                            .with_detail(error.to_string()),
                        );
                        (None, Some(yaml_range), body, body_start)
                    }
                },
                Err(error) => {
                    diagnostics.push(
                        Diagnostic::warning(
                            "frontmatter-parse-failed",
                            "frontmatter could not be parsed",
                        )
                        .with_detail(error.to_string()),
                    );
                    (None, Some(yaml_range), body, body_start)
                }
            };
        }
        offset += line.len();
    }

    diagnostics.push(Diagnostic::warning(
        "frontmatter-unclosed",
        "frontmatter opening delimiter has no closing delimiter",
    ));
    (None, None, content, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_unchanged_body() {
        let mut diagnostics = Vec::new();
        let (value, range, body, body_start) = extract_frontmatter("# heading\n", &mut diagnostics);
        assert!(value.is_none());
        assert!(range.is_none());
        assert_eq!(body, "# heading\n");
        assert_eq!(body_start, 0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn well_formed_frontmatter_lf() {
        let mut diagnostics = Vec::new();
        let content = "---\ntitle: hello\n---\n# heading\n";
        let (value, range, body, body_start) = extract_frontmatter(content, &mut diagnostics);
        assert!(value.is_some());
        assert_eq!(range, Some(4..17));
        assert_eq!(body, "# heading\n");
        assert_eq!(body_start, 21);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn well_formed_frontmatter_crlf() {
        let mut diagnostics = Vec::new();
        let content = "---\r\ntitle: hello\r\n---\r\n# heading\r\n";
        let (value, _range, body, body_start) = extract_frontmatter(content, &mut diagnostics);
        assert!(value.is_some());
        assert!(body.starts_with("# heading"));
        assert!(body_start > 0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn unclosed_frontmatter_emits_diagnostic_and_returns_no_value() {
        let mut diagnostics = Vec::new();
        let content = "---\ntitle: hello\n# heading (no close)\n";
        let (value, range, body, body_start) = extract_frontmatter(content, &mut diagnostics);
        assert!(value.is_none());
        assert!(range.is_none());
        assert_eq!(body, content);
        assert_eq!(body_start, 0);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "frontmatter-unclosed");
    }

    #[test]
    fn malformed_yaml_emits_parse_failed_diagnostic_with_range() {
        let mut diagnostics = Vec::new();
        let content = "---\ntitle: : :\n---\n# body\n";
        let (value, range, _, _) = extract_frontmatter(content, &mut diagnostics);
        assert!(value.is_none());
        assert!(range.is_some());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "frontmatter-parse-failed");
    }

    #[test]
    fn empty_frontmatter_block_returns_some_with_empty_value() {
        let mut diagnostics = Vec::new();
        let content = "---\n---\n# body\n";
        let (value, range, body, body_start) = extract_frontmatter(content, &mut diagnostics);
        // empty YAML parses to null, which becomes JSON null
        assert!(value.is_some());
        assert_eq!(range, Some(4..4));
        assert_eq!(body, "# body\n");
        assert_eq!(body_start, 8);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn frontmatter_with_non_mapping_root_still_parses() {
        let mut diagnostics = Vec::new();
        let content = "---\n- one\n- two\n---\n# body\n";
        let (value, range, _, _) = extract_frontmatter(content, &mut diagnostics);
        assert!(value.is_some());
        assert!(range.is_some());
        // A YAML sequence at the root produces a JSON array, not a mapping.
        // Downstream consumers handle this via .as_object() / .as_array() — pin the current behavior.
        assert!(value.as_ref().unwrap().is_array());
        assert!(diagnostics.is_empty());
    }
}
