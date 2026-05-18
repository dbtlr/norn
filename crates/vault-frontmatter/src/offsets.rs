use std::ops::Range;

pub struct FrontmatterPropertyString<'a> {
    pub property: String,
    pub text: &'a str,
    pub offset: Option<usize>,
}

pub fn frontmatter_property_strings<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    content: &str,
    frontmatter_range: Option<Range<usize>>,
) -> Vec<FrontmatterPropertyString<'a>> {
    let mut strings = Vec::new();

    for (property, value) in object {
        match value {
            serde_json::Value::String(text) => strings.push(FrontmatterPropertyString {
                property: property.to_string(),
                text,
                offset: frontmatter_scalar_offset(
                    content,
                    frontmatter_range.clone(),
                    property,
                    text,
                ),
            }),
            serde_json::Value::Array(values) => {
                for text in values.iter().filter_map(|value| value.as_str()) {
                    strings.push(FrontmatterPropertyString {
                        property: property.to_string(),
                        text,
                        offset: frontmatter_list_item_offset(
                            content,
                            frontmatter_range.clone(),
                            property,
                            text,
                        ),
                    });
                }
            }
            _ => {}
        }
    }

    strings
}

fn frontmatter_scalar_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) && trimmed.starts_with(&property_prefix) {
            return line.find(text).map(|offset| line_start + offset);
        }
        line_start += line.len();
    }

    None
}

fn frontmatter_list_item_offset(
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    property: &str,
    text: &str,
) -> Option<usize> {
    let range = frontmatter_range?;
    let yaml = &content[range.clone()];
    let property_prefix = format!("{property}:");
    let mut in_property = false;
    let mut line_start = range.start;

    for line in yaml.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !line.starts_with([' ', '\t']) {
            in_property = trimmed.starts_with(&property_prefix);
            line_start += line.len();
            continue;
        }

        if in_property && trimmed.starts_with('-') {
            if let Some(offset) = line.find(text) {
                return Some(line_start + offset);
            }
        }

        line_start += line.len();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(content: &str, range: Range<usize>) -> Vec<FrontmatterPropertyString<'_>> {
        let yaml = &content[range.clone()];
        let value: serde_json::Value = serde_yaml::from_str(yaml).unwrap();
        let value: serde_json::Value = serde_json::to_value(value).unwrap();
        let object = value.as_object().unwrap().clone();
        // Property strings need ownership of the map for the &'a borrow.
        // Box::leak is fine in a test for simplicity.
        let object: &'static serde_json::Map<String, serde_json::Value> =
            Box::leak(Box::new(object));
        frontmatter_property_strings(object, content, Some(range))
    }

    #[test]
    fn scalar_offset_is_returned_inside_property_value_range() {
        let content = "---\ntitle: hello world\n---\n# body\n";
        let strings = props(content, 4..23);
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].property, "title");
        assert_eq!(strings[0].text, "hello world");
        let offset = strings[0].offset.unwrap();
        assert!(content[offset..].starts_with("hello world"));
    }

    #[test]
    fn list_item_offsets_are_returned_for_top_level_list_of_strings() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let strings = props(content, 4..29);
        assert_eq!(strings.len(), 2);
        for s in &strings {
            assert_eq!(s.property, "aliases");
            let offset = s.offset.unwrap();
            assert!(content[offset..].starts_with(s.text));
        }
    }

    #[test]
    fn nested_yaml_objects_are_skipped() {
        // Current behavior: nested mappings are not surfaced as property strings.
        let content = "---\nmeta:\n  inner: value\n---\n";
        let strings = props(content, 4..25);
        // `meta` is an object, neither string nor list-of-strings, so no property strings.
        assert!(strings.is_empty());
    }

    #[test]
    fn property_value_containing_another_property_name_as_substring_is_pinned() {
        // Documents the known fragility: line.find(text) inside frontmatter_scalar_offset
        // may collide if a property's value text appears on another property's line first.
        // The current impl scans line-by-line and matches the first property prefix, so
        // this case is actually OK — but the test pins the behavior so Slice 2's
        // minimal-edit replacement can either preserve or deliberately fix it.
        let content = "---\nname: foo\nalias: name\n---\n";
        let strings = props(content, 4..26);
        // Both name and alias should produce property strings.
        let alias = strings.iter().find(|s| s.property == "alias").unwrap();
        let alias_offset = alias.offset.unwrap();
        // alias's offset should point into the second line, not the first.
        // The second line starts at content[14..] = "alias: name\n"; "name" is at offset 14 + "alias: ".len() = 21.
        assert_eq!(alias_offset, 21);
    }

    #[test]
    fn same_line_comment_inside_value_pins_current_behavior() {
        // line.find(text) does not consider comments. With "title: hello # comment", finding "hello"
        // returns the offset of the actual value text. The current implementation does NOT
        // distinguish a comment from value text. This test documents that — Slice 2's minimal-edit
        // work will likely need to handle this case more carefully.
        let content = "---\ntitle: hello # comment\n---\n";
        let strings = props(content, 4..26);
        let title = strings.iter().find(|s| s.property == "title").unwrap();
        // serde_yaml drops the comment when producing the JSON value; the text is "hello".
        assert_eq!(title.text, "hello");
        let offset = title.offset.unwrap();
        assert!(content[offset..].starts_with("hello"));
    }
}
