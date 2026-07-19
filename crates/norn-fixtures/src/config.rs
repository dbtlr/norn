//! `.norn/config.yaml` emitter. Content is fixed — independent of profile
//! and seed — and exercises the full v0.48.0 config vocabulary documented in
//! `docs/configuration.md` and `docs/rule-shape.md`.
//!
//! The directory names, path globs, and status vocabulary are not literals
//! here: they are `%%TOKEN%%` placeholders substituted from [`crate::contract`]
//! so the emitted rules and the documents the emitters place under those rules
//! share one source of truth. The template stays a single readable YAML block
//! (the literal `{{now}}` / `{ type: … }` forms would be corrupted by a
//! `format!`), and every substituted value passes through [`crate::yaml::scalar`]
//! or is a fixed glob, so the emitted bytes are unchanged from a hand-written
//! block.
//!
//! Deviation from the literal per-rule field lists in the NRN-319 spec: the
//! `typed-note` rule additionally declares `field_references: { parent: {
//! target_type: [phase] } }`. The spec's `typed-note` field list (search
//! `NRN-319` in the design doc) does not mention `field_references`, but the
//! spec's own violation-zoo entry for `notes/bad-parent.md` requires exactly
//! this behavior on a `type: note` document — `bad-parent.md` never matches
//! `task-rule`'s selector (`type: [task, phase]`), so without this addition
//! the described `frontmatter-reference-type` finding could never fire. This
//! is the smallest change that resolves the internal inconsistency while
//! keeping every other literal requirement intact.
//!
//! Second deviation, empirically discovered against the oracle: alias
//! resolution (`notes/beta.md`'s `aliases: [bee]`, exercised by
//! `notes/gamma.md`'s `[[bee]]` link per the spec's zoo description) is
//! opt-in via a `links.alias_field` config key in the installed v0.48.0
//! binary — confirmed by reading `retired/src/graph/build.rs` and
//! `retired/src/config_loader.rs` (the pre-0018 tree the 0.48.0 release was
//! built from) and verified directly against the oracle: `[[bee]]` reports
//! `link-target-missing` without this key, resolves cleanly with it.
//! `docs/configuration.md` does not document a `links:` top-level section at
//! all — a real doc gap — but the key is necessary for a feature the spec
//! explicitly exercises, so it is added here rather than dropping the
//! alias-resolution fixture.

use crate::contract::{
    DRAFTS_DIR, IGNORED_DIR, LOGS_DIR, NOTES_DIR, PHASES_GLOB, STATUS_VALUES, TASKS_DIR,
    TASKS_GLOB, TEMPLATES_DIR,
};
use crate::yaml;

/// Emit `.norn/config.yaml` with the contract values substituted in.
pub fn config_yaml() -> String {
    let status = STATUS_VALUES
        .iter()
        .map(|s| yaml::scalar(s))
        .collect::<Vec<_>>()
        .join(", ");
    CONFIG_TEMPLATE
        .replace("%%IGNORED%%", IGNORED_DIR)
        .replace("%%TEMPLATES%%", TEMPLATES_DIR)
        .replace("%%DRAFTS%%", DRAFTS_DIR)
        .replace("%%STATUS%%", &status)
        .replace("%%TASKS_GLOB%%", TASKS_GLOB)
        .replace("%%PHASES_GLOB%%", PHASES_GLOB)
        .replace("%%NOTES%%", NOTES_DIR)
        .replace("%%LOGS%%", LOGS_DIR)
        .replace("%%TASKS_DIR%%", TASKS_DIR)
}

/// A deliberately-invalid `.norn/config.yaml`: valid YAML that carries one
/// unknown top-level key, so a strict (`deny_unknown_fields`) config load fails
/// with a stable "unknown field" error. Drives the malformed-config error-surface
/// parity case (NRN-361) — the vault warms into a config rejection, and the CLI's
/// diagnostic surface (the `norn:` prefix) diverges from the oracle's bare line.
/// Content is fixed (profile/seed-independent) so the error text is deterministic.
pub fn malformed_config_yaml() -> String {
    "not_a_real_section: true\n".to_string()
}

const CONFIG_TEMPLATE: &str = r#"links:
  alias_field: aliases

files:
  ignore:
    - "%%IGNORED%%/**"
    - "**/*.tmp"

validate:
  ignore:
    - "%%TEMPLATES%%/**"
    - "%%DRAFTS%%/{a,b}/**"
  required_frontmatter:
    - title
  rules:
    - name: typed-note
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind
      field_types:
        created: datetime
        modified: datetime
        due: date
        tags: list_of_strings
        parent: wikilink
        related: wikilink_or_list
        project: { type: string, max_length: 32, indexed: true }
        summary: { type: text }
        internal: { indexed: true }
      field_references:
        parent:
          target_type: [phase]
      frontmatter_defaults:
        kind: note
        created: "{{now}}"
        title: "{{title|titlecase}}"

    - name: task-rule
      match:
        frontmatter:
          type: [task, phase]
      required_frontmatter:
        - status
      allowed_values:
        status: [%%STATUS%%]
      allowed_paths:
        - "%%TASKS_GLOB%%"
        - "%%PHASES_GLOB%%"
      field_references:
        parent:
          target_type: [phase]

    - name: no-legacy
      match:
        path: "%%NOTES%%/**"
      exclude:
        path: "%%NOTES%%/keep/**"
      forbidden_frontmatter:
        - legacy

    - name: dated-log
      match:
        path: "%%LOGS%%/**"
        path_not: "%%LOGS%%/scratch/**"
      field_types:
        when: date

    - name: unindexed
      match:
        path: "**/*.md"
      field_types:
        scratch: { type: string, indexed: false }

repair:
  rules:
    - name: fix-task-status
      match:
        code: value-not-allowed
        rule: task-rule
        field: status
        actual_value: legacy
      set_frontmatter:
        field: status
        value: backlog

    - name: remove-legacy
      match:
        code: frontmatter-forbidden-field
        field: legacy
      remove_frontmatter:
        field: legacy

    - name: add-kind
      match:
        code: frontmatter-required-field-missing
        rule: typed-note
        field: kind
      add_frontmatter:
        field: kind
        value: note

    - name: route-tasks
      match:
        code: document-misrouted
        rule: task-rule
      move_document:
        to_directory: "%%TASKS_DIR%%/"

index:
  auto: true

cache:
  retention: 30d
  prune: manual

templates:
  date_format: "YYYY-MM-DD"
  time_format: "HH:mm"
"#;
