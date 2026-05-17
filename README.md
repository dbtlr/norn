# vault-cli

Experimental Rust CLI for deterministic Markdown vault graph indexing and
drift healing.

The current binary name is `vault`.

## Product Direction

`vault` is being shaped as a deterministic drift-healing surface for Markdown
vaults. The product loop is:

1. Detect drift with graph facts and configured validation rules.
2. Plan supported repairs as explicit, inspectable artifacts.
3. Apply supported plans only through an explicit apply command.
4. Verify the vault after changes and report what remains.

The current `validate` command is the detection layer for this loop. It is
read-only and intentionally does not guess repairs or mutate files. Future
repair commands should preserve that boundary: no hidden writes, no
LLM-required fixes inside the CLI, and no apply behavior without an explicit
plan/apply step.

Machine-facing workflows should prefer stable JSON and JSONL contracts. Human
review workflows should prefer table, path-list, or Markdown-style output where
available, while preserving the machine-readable schemas that agents and
scripts consume.

When `--format` is omitted, commands with human renderers use table output on a
terminal and JSON output when stdout is piped or captured. Pass `--format json`
or `--format jsonl` explicitly for stable machine-readable contracts, and pass
`--format paths` when a command supports path-list output.

The core workflow should not depend on QMD, embeddings, or semantic retrieval.
Those can remain useful adjacent layers, but `vault` should provide deterministic
path, frontmatter, graph, validation, search, repair-plan, and apply surfaces
directly. Atlas-specific doctrine belongs in `.vault/config.yaml`, docs, and
recipes rather than hardcoded generic CLI behavior.

## Build

Install repo tools with `mise`:

```bash
mise install
```

```bash
cargo build -p vault-cli
```

The debug binary is written to:

```bash
target/debug/vault
```

To install it onto your Cargo bin path:

```bash
cargo install --path crates/vault-cli
```

With `just` installed, common commands are available as:

```bash
just build
just test
just verify
just run -C fixtures/basic docs list --format jsonl
```

## v0 Scope

```bash
vault docs list --format jsonl
vault docs list --format table
vault docs list --format paths
vault docs list --filter status:draft --format jsonl
vault docs list --path "Workspaces/**/tasks/*.md" --has workspace --format jsonl
vault docs summary --count-by status --format json
vault cache build --cache .vault/cache --format json
vault links list --format jsonl
vault files --format jsonl
vault links unresolved --format jsonl
vault links backlinks <path-or-stem-or-file> --format jsonl
vault docs inspect <path-or-stem> --format json
vault validate --format jsonl
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --rule note-base --path "Workspaces/**" --summary --format json
vault validate --summary --format json
vault validate --summary --format table
vault -C <path> validate --summary --format json
```

Commands run against the current directory by default. Use global `-C, --cwd
<dir>` to run against another vault directory. When `--config` is omitted,
`vault` discovers `<cwd>/.vault/config.yaml` if it exists; missing discovered
config is fine and uses defaults. Explicit relative `--config` paths and
relative `--cache` paths resolve against the effective cwd.

All commands accept global `--config <path>` for explicit YAML configuration.
The current config shape is:

```yaml
files:
  ignore:
    - "**/__pycache__/**"
    - "**/*.pyc"
validate:
  ignore:
    - "Archive/**"
    - "System/Templates/**"
  required_frontmatter:
    - title
  rules:
    - name: workspace-notes
      match:
        path: "Workspaces/**/notes/*.md"
      required_frontmatter:
        - type
        - kind
        - workspace
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
        aliases: list_of_strings
        workspace: wikilink
    - name: task-status
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      required_frontmatter:
        - status
      allowed_values:
        status:
          - backlog
          - in_progress
          - completed
          - wont_do
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
    - name: agent-artifact
      match:
        path: "**/*.md"
        frontmatter:
          type: agent-artifact
      forbidden_frontmatter:
        - kind
      allowed_paths:
        - "Workspaces/**/agent-artifacts/*.md"
```

For the conceptual model of validate rules, see [docs/rule-shape.md](docs/rule-shape.md).

Configured ignores are applied before file inventory and document parsing. With no config, the graph remains a raw filesystem view except for hidden files/directories.

Ignored targets remain outside the graph. If an indexed Markdown document links to an ignored file, that link is reported as unresolved rather than hidden.

The current graph and validation passes are stateless and read-only. They walk
Markdown files, parse generic frontmatter, extract headings, extract Markdown
links and wikilinks, and resolve links against vault-relative paths or unique
note stems. Exact path lookup is case-sensitive; stem lookup is
case-insensitive.

The raw graph aims to follow Obsidian-style internal link behavior before applying any future standards-pack semantics. It includes body wikilinks, embeds, frontmatter/property wikilinks, URL-decoded Markdown internal links, extensionless Markdown note links, same-note heading/block references, Markdown image links to local files, and existing non-Markdown attachment targets.

Frontmatter link extraction is intentionally shallow in v0.x: it scans top-level scalar string properties and top-level lists of strings. Nested YAML object/list leaves are not graph links until the schema layer or a real vault need makes that boundary worth expanding.

Use `source_context.area` and `source_context.property` to distinguish body links from frontmatter/property links. Frontmatter links now include `source_span` for the shallow extraction cases. `vault files` emits the file inventory, and `vault links backlinks <exact-file-path>` can query incoming links to non-Markdown attachment targets.

`vault validate` is read-only. It reports unresolved links, ambiguous links,
document diagnostics, configured missing frontmatter fields, invalid
frontmatter field types, forbidden frontmatter fields, path-location
violations, and configured disallowed frontmatter values without mutating
files. Global `validate.required_frontmatter` applies to every document that is
not skipped by `validate.ignore`. Scoped `validate.rules` apply additional
requirements only to documents matched by `match.path`, `match.path_not`, and
`match.frontmatter`; findings include `rule` when a scoped rule produced them.
Rule-level `exclude.path` can remove a path subset from a specific rule without
removing those files from the graph.

Use `vault validate --summary` to emit grouped finding counts instead of raw
findings. Summary output includes total findings plus counts by `code`,
`severity`, `rule`, frontmatter `field`, disallowed field value, and top-level
path prefix.

`vault validate` supports triage filters for cleanup queues: `--code`,
`--severity`, `--field`, `--rule`, `--path`, `--target`, and `--reason`.
Comma-separated values are ORed within a filter dimension, and different
dimensions are ANDed. Filters apply before both raw output and `--summary`, and
filtered summaries keep the same JSON schema while counting only the filtered
finding set.

For human inspection, use table output:

```bash
vault docs list
vault docs list --format paths
vault validate --summary
```

For agents and scripts, request the JSON contract explicitly:

```bash
vault docs list --path "Workspaces/**/tasks/*.md" --format json
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --summary --format json
```

## Validation Recipes

Use filtered summaries to size a cleanup queue before reading raw findings:

```bash
vault validate --summary --code frontmatter-invalid-type --field created --format table
vault validate --summary --code frontmatter-disallowed-value --field status --format json
vault validate --summary --path "Workspaces/**/tasks/*.md" --field description --format json
```

Use raw JSONL when an agent or script needs one finding per line:

```bash
vault validate --code frontmatter-invalid-type --field created --format jsonl
vault validate --code frontmatter-disallowed-value --field status --format jsonl
vault validate --rule note-description --path "Workspaces/**/notes/*.md" --format jsonl
```

Use link filters to split link cleanup by failure mode:

```bash
vault validate --code link-unresolved --reason target-missing --format jsonl
vault validate --code link-unresolved --reason anchor-missing,block-ref-missing --format jsonl
vault validate --code link-ambiguous --summary --format table
```

`--target` matches the raw parsed link target string, not a fuzzy note stem, a
resolved path, or a normalized candidate. For example, filtering with
`--target "duplicate"` matches findings whose link target text is exactly
`duplicate`.

Filters are applied before raw output and before `--summary`, so the same filter
set can produce either a scoped planning count or the concrete cleanup queue:

```bash
vault validate --code frontmatter-invalid-type --field modified --summary --format json
vault validate --code frontmatter-invalid-type --field modified --format jsonl
```

These recipes are intentionally just command combinations. Config-defined saved
filters or presets may be worth adding later, but the current surface avoids a
new query language while repair planning is still being designed.

`vault docs list` supports small inventory filters: `--path <glob>` for
vault-relative paths, repeatable `--filter field:value` for frontmatter scalar
or list values, comma-separated value sets such as `status:backlog,completed`,
and `--has <field>` / `--missing <field>` for field presence. Repeated filters
are ANDed; comma-separated values within one `--filter` are ORed. `vault docs
summary --count-by <field>` emits grouped document counts for one frontmatter
field.

## Glob Matching

Config path patterns, `docs list --path`, and `validate --path` patterns are
matched against vault-relative paths using path-segment semantics:

- `*` matches within one path segment only.
- `**` matches zero or more complete path segments.
- `Workspaces/*/*.md` matches `Workspaces/app/root.md`, but not
  `Workspaces/app/notes/note.md`.
- `Workspaces/**/*.md` matches markdown files at any depth under `Workspaces`.
- `Workspaces/**/notes/*.md` matches files directly inside a `notes` directory,
  including nested workspace paths, but not files in subdirectories below
  `notes`.

## Validate Rule Matching

Scoped validate rules support path and frontmatter predicates. All predicates are
ANDed. `match.path_not` excludes matching paths from a rule. Missing frontmatter
fields do not match. Frontmatter predicates use exact, type-sensitive equality
for strings, booleans, and numbers.
Rules can also constrain allowed scalar field values with `allowed_values`.
Allowed-value checks are exact and type-sensitive.
Rules can validate present field shapes with `field_types`, forbid fields with
`forbidden_frontmatter`, and report folder-routing violations with
`allowed_paths`.

```yaml
validate:
  rules:
    - name: note-kind
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind

    - name: task-status
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      required_frontmatter:
        - status
      allowed_values:
        status:
          - backlog
          - in_progress
          - completed
          - wont_do
      allowed_paths:
        - "Workspaces/**/tasks/*.md"

    - name: agent-artifact
      match:
        path: "**/*.md"
        frontmatter:
          type: agent-artifact
      forbidden_frontmatter:
        - kind
      allowed_paths:
        - "Workspaces/**/agent-artifacts/*.md"
```

Supported `field_types` are `datetime`, `date`, `list_of_strings`, `wikilink`,
and `wikilink_or_list`. Field type checks only run when the field is present;
use `required_frontmatter` when presence is also required. `datetime` accepts
common ISO/YAML forms with optional seconds, fractional seconds, `Z`, numeric
timezone offsets, or a space separator. `date` accepts plain `YYYY-MM-DD`
values and YAML-normalized midnight datetime strings.
