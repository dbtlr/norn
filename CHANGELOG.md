# Changelog

All notable changes to this project are documented here.

## v0.13.0 - 2026-05-17

- Handled closed stdout pipes gracefully so JSON/JSONL output can be piped into early-exit consumers such as `head` without panic text.
- Added `fields` counts to `vault validate --summary`.
- Added `disallowed_values` counts to `vault validate --summary` for configured allowed-value findings.
- Kept raw validation finding JSON/JSONL output unchanged.

## v0.12.0 - 2026-05-17

- Added global `-C, --cwd <dir>` and made commands default to the process current directory.
- Removed command-local `--root` arguments from graph and validate commands.
- Added default config discovery from `<cwd>/.vault/config.yaml` when `--config` is omitted.
- Resolved explicit relative config paths and relative cache paths against the effective cwd.
- Updated Justfile recipes and docs for the cwd-based command surface.

## v0.11.0 - 2026-05-17

- Added `validate.rules[].allowed_values` for type-sensitive scalar frontmatter value validation.
- Added `frontmatter-field-value-not-allowed` findings with `field`, `rule`, `actual_value`, and `allowed_values` context.
- Added config validation for malformed `allowed_values` maps and non-scalar allowed values.
- Changed validation summary root-file path prefix from `.` to `root`.
- Documented allowed-value validation examples.

## v0.10.0 - 2026-05-17

- Renamed the read-only standards checking command from `vault doctor` to `vault validate`.
- Renamed config rules from `doctor:` to `validate:`.
- Added `vault validate --summary` for grouped finding counts by code, severity, rule, and top-level path prefix.
- Kept raw validation finding JSON/JSONL output unchanged unless `--summary` is requested.

## v0.9.0 - 2026-05-17

- Added `doctor.rules[].match.frontmatter` predicates for top-level frontmatter equality matching.
- ANDed path and frontmatter predicates for scoped doctor rules.
- Added type-sensitive string, boolean, and number comparisons without coercion.
- Rejected unknown `match.*` keys and non-scalar frontmatter predicate values during config validation.
- Updated docs and examples for frontmatter-aware doctor rules and recursive ignore patterns.

## v0.8.0 - 2026-05-17

- Tightened config path glob semantics so `*` matches within one path segment and `**` matches recursive path segments.
- Added matcher tests for workspace root, recursive workspace, and nested notes patterns.
- Added config validation errors for malformed doctor rule shapes.
- Documented glob matching semantics for config path patterns.

## v0.7.0 - 2026-05-17

- Added scoped `doctor.rules` with `match.path`.
- Added scoped `required_frontmatter` checks.
- Preserved global `doctor.required_frontmatter` for simple configs.
- Added `rule` context to scoped missing-frontmatter doctor findings.
- Documented scoped doctor rule configuration.

## v0.6.0 - 2026-05-16

- Added read-only `vault doctor`.
- Added doctor findings for unresolved links, ambiguous links, and document diagnostics.
- Added config-driven required frontmatter checks via `doctor.required_frontmatter`.
- Documented `vault doctor` and the ignored-target policy: indexed documents linking to ignored files surface unresolved links rather than hiding the fact.

## v0.5.0 - 2026-05-16

- Added richer `vault graph --help` overview while keeping `vault graph -h` compact.
- Added explicit YAML config support with `--config`.
- Added `graph.ignore` patterns for exact paths, directory prefixes such as `__pycache__/**`, and simple `*` wildcards.
- Applied configured ignores before file inventory and document parsing.
- Added `ignored_files` to `graph build` summaries.
- Added `source_span` for shallow frontmatter/property wikilinks.
- Documented graph config, ignore behavior, and frontmatter spans.

## v0.4.0 - 2026-05-16

- Resolved same-note wikilink references such as `[[#Heading]]` and `[[#^block-id]]`.
- Added precise same-note missing-reference reasons: `anchor-missing` and `block-ref-missing`.
- Emitted local Markdown image links such as `![Alt](Assets/pic.png)` as `kind: "embed"` graph facts.
- Added `vault graph files` for file inventory output.
- Allowed `vault graph backlinks <exact-file-path>` for non-Markdown file targets.
- Added `unresolved_reason: "ambiguous"` to ambiguous link rows.
- Expanded long help and docs for graph semantics.

## v0.3.0 - 2026-05-16

- Parsed frontmatter/property wikilinks as graph links.
- Added `source_context` so graph consumers can distinguish body links from frontmatter/property links.
- Resolved percent-encoded Markdown internal links.
- Resolved extensionless Markdown note links.
- Resolved path-qualified wikilinks by path before stem fallback.
- Added file inventory-backed resolution for existing attachment links.
- Documented Obsidian-compatible raw graph semantics.

## v0.2.0 - 2026-05-16

- Polished graph CLI help and output contracts.
- Added `vault graph diagnostics`.
- Normalized document target lookup by exact path or case-insensitive unique stem.
- Ignored wikilinks inside inline code and fenced code blocks.
- Clarified SQLite cache path semantics.
- Documented frontmatter-only filter behavior and graph contract expectations.

## v0.1.0 - 2026-05-16

- Scaffolded the Rust workspace and `vault` CLI.
- Added read-only graph document output.
- Added graph link extraction with JSON/JSONL output contracts.
- Added `vault graph backlinks`.
- Added `vault graph inspect`.
- Enriched link facts with anchors, block refs, source spans, and resolution status.
- Added anchor and block-ref validation.
- Added frontmatter filters for `vault graph documents`.
- Added SQLite cache build support.
- Added initial README documentation, Justfile recipes, and mise toolchain config.
