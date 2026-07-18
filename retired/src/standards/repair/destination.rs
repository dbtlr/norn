//! Placeholder substitution for move_document destination strings.
//!
//! Supports {stem}, {filename}, and {frontmatter.<field>} substitution.
//! All substitution failures (missing frontmatter field, non-scalar value)
//! propagate as `SubstitutionError`, which the planner converts into a
//! skipped finding with `skip_reason: precondition_failed`.

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use crate::standards::config::DestinationSpec;

#[derive(Debug, Clone, thiserror::Error)]
pub enum SubstitutionError {
    #[error("placeholder substitution failed: unknown placeholder '{placeholder}'")]
    UnknownPlaceholder { placeholder: String },
    #[error("placeholder substitution failed: frontmatter field '{field}' is missing on source")]
    MissingFrontmatterField { field: String },
    #[error(
        "placeholder substitution failed: frontmatter field '{field}' is not a scalar (got {kind})"
    )]
    NonScalarFrontmatterField { field: String, kind: String },
    #[error("placeholder substitution failed: malformed placeholder syntax in '{template}'")]
    MalformedTemplate { template: String },
}

/// Resolves a `DestinationSpec` against a source file's path and frontmatter,
/// returning the substituted vault-relative destination path.
///
/// `source_path` is vault-relative (e.g., "Inbox/task.md").
/// `frontmatter` is the parsed JSON value of the source file's frontmatter
/// (None if the file has no frontmatter).
pub fn resolve_destination(
    spec: &DestinationSpec,
    source_path: &Utf8Path,
    frontmatter: Option<&Value>,
) -> Result<Utf8PathBuf, SubstitutionError> {
    let raw_template = spec.raw();
    let substituted = substitute(raw_template, source_path, frontmatter)?;
    let mut result = Utf8PathBuf::from(substituted);
    // For Directory variant, append the original filename to the substituted dir.
    if matches!(spec, DestinationSpec::Directory { .. }) {
        let filename =
            source_path
                .file_name()
                .ok_or_else(|| SubstitutionError::MalformedTemplate {
                    template: raw_template.to_string(),
                })?;
        result = result.join(filename);
    }
    Ok(result)
}

fn substitute(
    template: &str,
    source_path: &Utf8Path,
    frontmatter: Option<&Value>,
) -> Result<String, SubstitutionError> {
    let stem = source_path
        .file_stem()
        .ok_or_else(|| SubstitutionError::MalformedTemplate {
            template: template.to_string(),
        })?;
    let filename = source_path
        .file_name()
        .ok_or_else(|| SubstitutionError::MalformedTemplate {
            template: template.to_string(),
        })?;

    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut placeholder = String::new();
            let mut closed = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    closed = true;
                    break;
                }
                placeholder.push(inner);
            }
            if !closed {
                return Err(SubstitutionError::MalformedTemplate {
                    template: template.to_string(),
                });
            }
            let resolved = resolve_placeholder(&placeholder, stem, filename, frontmatter)?;
            result.push_str(&resolved);
        } else {
            result.push(c);
        }
    }
    Ok(result)
}

fn resolve_placeholder(
    placeholder: &str,
    stem: &str,
    filename: &str,
    frontmatter: Option<&Value>,
) -> Result<String, SubstitutionError> {
    match placeholder {
        "stem" => Ok(stem.to_string()),
        "filename" => Ok(filename.to_string()),
        other if other.starts_with("frontmatter.") => {
            let field = other.strip_prefix("frontmatter.").unwrap();
            let object = frontmatter.and_then(|v| v.as_object()).ok_or_else(|| {
                SubstitutionError::MissingFrontmatterField {
                    field: field.to_string(),
                }
            })?;
            let value =
                object
                    .get(field)
                    .ok_or_else(|| SubstitutionError::MissingFrontmatterField {
                        field: field.to_string(),
                    })?;
            scalar_to_string(value, field)
        }
        _ => Err(SubstitutionError::UnknownPlaceholder {
            placeholder: placeholder.to_string(),
        }),
    }
}

fn scalar_to_string(value: &Value, field: &str) -> Result<String, SubstitutionError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err(SubstitutionError::NonScalarFrontmatterField {
            field: field.to_string(),
            kind: "null".to_string(),
        }),
        Value::Array(_) => Err(SubstitutionError::NonScalarFrontmatterField {
            field: field.to_string(),
            kind: "array".to_string(),
        }),
        Value::Object(_) => Err(SubstitutionError::NonScalarFrontmatterField {
            field: field.to_string(),
            kind: "object".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_directory_appends_filename() {
        let spec = DestinationSpec::Directory {
            to_directory: "Workspaces/demo/tasks/".into(),
        };
        let result = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap();
        assert_eq!(result, Utf8PathBuf::from("Workspaces/demo/tasks/task.md"));
    }

    #[test]
    fn to_directory_with_frontmatter_substitution() {
        let spec = DestinationSpec::Directory {
            to_directory: "Workspaces/{frontmatter.workspace}/tasks/".into(),
        };
        let fm = json!({"workspace": "demo"});
        let result = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), Some(&fm)).unwrap();
        assert_eq!(result, Utf8PathBuf::from("Workspaces/demo/tasks/task.md"));
    }

    #[test]
    fn to_path_substitutes_stem_and_filename() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/demo/tasks/{stem}.md".into(),
        };
        let result = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap();
        assert_eq!(result, Utf8PathBuf::from("Workspaces/demo/tasks/task.md"));
    }

    #[test]
    fn to_path_handles_rename() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/demo/notes/next-task.md".into(),
        };
        let result = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap();
        assert_eq!(
            result,
            Utf8PathBuf::from("Workspaces/demo/notes/next-task.md")
        );
    }

    #[test]
    fn missing_frontmatter_field_errors() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/{frontmatter.workspace}/{stem}.md".into(),
        };
        let fm = json!({"other": "x"});
        let err =
            resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), Some(&fm)).unwrap_err();
        assert!(matches!(
            err,
            SubstitutionError::MissingFrontmatterField { .. }
        ));
    }

    #[test]
    fn missing_frontmatter_object_errors() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/{frontmatter.workspace}/{stem}.md".into(),
        };
        let err = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap_err();
        assert!(matches!(
            err,
            SubstitutionError::MissingFrontmatterField { .. }
        ));
    }

    #[test]
    fn non_scalar_frontmatter_field_errors() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/{frontmatter.tags}/{stem}.md".into(),
        };
        let fm = json!({"tags": ["a", "b"]});
        let err =
            resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), Some(&fm)).unwrap_err();
        assert!(matches!(
            err,
            SubstitutionError::NonScalarFrontmatterField { kind, .. } if kind == "array"
        ));
    }

    #[test]
    fn unknown_placeholder_errors() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/{nonexistent}/{stem}.md".into(),
        };
        let err = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap_err();
        assert!(matches!(err, SubstitutionError::UnknownPlaceholder { .. }));
    }

    #[test]
    fn malformed_template_unclosed_brace_errors() {
        let spec = DestinationSpec::Path {
            to_path: "Workspaces/{stem".into(),
        };
        let err = resolve_destination(&spec, Utf8Path::new("Inbox/task.md"), None).unwrap_err();
        assert!(matches!(err, SubstitutionError::MalformedTemplate { .. }));
    }

    #[test]
    fn numeric_scalar_substitutes() {
        let spec = DestinationSpec::Path {
            to_path: "Y{frontmatter.year}/{stem}.md".into(),
        };
        let fm = json!({"year": 2026});
        let result = resolve_destination(&spec, Utf8Path::new("Inbox/note.md"), Some(&fm)).unwrap();
        assert_eq!(result, Utf8PathBuf::from("Y2026/note.md"));
    }
}
