//! `.norn/config.yaml` emitter. Content is fixed — independent of profile
//! and seed — and exercises the full v0.48.0 config vocabulary documented in
//! `docs/configuration.md` and `docs/rule-shape.md`.
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

pub const CONFIG_YAML: &str = r#"links:
  alias_field: aliases

files:
  ignore:
    - "ignored/**"
    - "**/*.tmp"

validate:
  ignore:
    - "templates/**"
    - "drafts/{a,b}/**"
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
        status: [backlog, active, done]
      allowed_paths:
        - "tasks/**/*.md"
        - "phases/**/*.md"
      field_references:
        parent:
          target_type: [phase]

    - name: no-legacy
      match:
        path: "notes/**"
      exclude:
        path: "notes/keep/**"
      forbidden_frontmatter:
        - legacy

    - name: dated-log
      match:
        path: "logs/**"
        path_not: "logs/scratch/**"
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
        to_directory: "tasks/"

index:
  auto: true

cache:
  retention: 30d
  prune: manual

templates:
  date_format: "YYYY-MM-DD"
  time_format: "HH:mm"
"#;
