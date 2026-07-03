# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it ships v1.0. Pre-1.0 versions may include breaking changes in minor releases.

## [Unreleased]

Entries here have landed on `main` but have not yet been cut into a tagged release. When a release is cut, this section is promoted to `## v0.X.0 - YYYY-MM-DD` and a fresh `## [Unreleased]` header is added above it.

### Added

- **Rule targets can carry a `{{seq}}` token for auto-incrementing document ids.** A creatable rule whose `target` includes `{{seq}}` — e.g. `target: "tasks/MMR-{{seq}}.md"` — allocates the next integer id at apply time, computed as filesystem max+1 over existing sibling files sharing the resolved prefix (per-prefix counters: `MMR-*` is independent of `NRN-*`, and the first id is `1`). Allocation runs under the per-vault mutation lock, so two concurrent `norn new` / `vault.new` creations get distinct sequential ids with no collision — the caller no longer needs its own `max+1`/retry. `--dry-run` (CLI and MCP) shows the honest unresolved target plus a non-binding `predicted_path` field; the real id is allocated at apply and can differ under concurrency. CLI `norn new --as <rule>` and MCP `vault.new` are at parity.

### Changed

- **Date operators (`--before`/`--after`/`--on`) now answer from the `document_fields` index on indexed fields.** The last scan-only frontmatter predicate class joins the index route via the same two-branch compile the string operators use: text-typed index rows range-compare directly (a genuine index range on `--before`/`--after`), and any non-text value falls through to the exact scan-path expression, so routed and scanned results stay identical for every value shape. With this, every frontmatter predicate routes when its fields are indexed. Query-plan only — results, output, and exit codes are unchanged. One visible side effect: a date query on an *unindexed* field now gets the standard past-1,000-documents cold-scan warning, where previously date ops fell back silently as a router limitation.
- **Membership lists mixing string and number/bool values now route through the `document_fields` index too.** The string-strictness fix below made mixed lists provably index-servable, lifting the carve-out shipped earlier in this cycle. Query-plan only.
- **Number/boolean `--eq`/`--not-eq`/`--in`/`--not-in` on indexed fields now answer from the `document_fields` index.** With the array-aware semantics below unifying the scan path, typed values became provably index-servable (the index already stores one typed row per array element); previously any non-string value forced the whole query back to the scan path. Query-plan only — results, output, and exit codes are unchanged relative to the fixed semantics.

### Fixed

- **String query values no longer match numeric or boolean storage — and indexed vs unindexed queries now agree on it.** Previously `--eq n:[[5]]` (the string `"5"` after bracket-stripping; a bare `5` parses as a number) matched a stored number `5` on an unindexed field but not on an indexed one — the unindexed comparison inherited an accidental text cast from SQLite's `replace()`, while the index compared typed. The typed comparison is now canonical everywhere: values compare within their type, strings additionally collapse `[[wikilink]]` brackets, and a string value matches text-typed storage only (plus structured values — objects and nested arrays — by their bracket-stripped JSON text, the sole query interface to those shapes; unchanged). Numeric/boolean storage counts as *unequal* to any string under `--not-eq`/`--not-in`; missing and null-valued fields stay excluded from negations as before. The anchored string operators (`--starts-with`/`--ends-with`/`--contains`) are unaffected — they are textual by design and keep comparing non-string values by JSON text rendering, as do the date operators. Result sets can change on queries pairing string values with numeric/boolean-typed fields.
- **`--eq`/`--not-eq`/`--in`/`--not-in` with number and boolean values are now array-aware, matching the string behavior.** `norn find --eq n:5` now matches `n: [5]` and `n: [5, 6]` (and `5.0`, numerically), `--eq flag:true` matches `flag: [true]`, and the negated forms exclude a document when *any* element matches. A mixed `--in` list applies each value's own rules (strings by bracket-stripped text, numbers/booleans typed). Previously typed values compared only against scalar fields — `--eq n:5` silently never matched `n: [5]` while the equivalent string query `--eq s:x` did match `s: [x]`, an asymmetry with no design rationale. Result sets can change on queries that touch array-valued numeric/boolean fields.
- **`--before`/`--after`/`--on` now compare per element on array-valued date fields, and collapse `[[wikilink]]` brackets on both sides.** Previously an array-valued field compared the *whole array's JSON-encoded text* lexically — `'["2020-01-01"]'` sorts after every plain date string, so `--before` could never match an array-valued field and `--after` always did, regardless of the dates inside. Now any element satisfying the comparison matches (an empty array never does). The bracket collapse means Obsidian daily-note links compare as their dates: `--on created:2026-01-01` matches a stored `created: "[[2026-01-01]]"`. Edge behavior change riding along: a non-string stored value now compares by its text rendering (`--on created:20260101` matches a stored bare `20260101`) instead of SQLite storage-class ordering, under which a number could never equal any date string.

## v0.41.0 - 2026-07-02

The frontmatter-index release (Wave 2 of the relational-engine enablement arc). Frontmatter-field queries stop degrading with vault size: declared fields shred into a derived cache index that `find`/`count` route through with byte-identical results, staying flat from 1k to 50k documents. The schema vocabulary gains bounded `string` and unbounded `text` types, and indexing is automatic for every bounded-typed field. The cache schema moves to v4: this binary rebuilds any older cache silently on first use, while a v0.40.0-or-older binary that encounters a v4 cache refuses cleanly with an "upgrade norn" message — upgrading is the fix; there is nothing to clear or repair by hand.

### Breaking changes

- **`list_of_strings` elements are now bounded (default 64 chars, same as `string`).** Previously a `list_of_strings` field accepted elements of any length; each element is now checked against the field's `max_length` (declared or the 64-char default). An over-length element reports a dedicated `frontmatter-exceeds-max-length` finding (carrying the field, the effective bound, and the actual length) rather than the generic `frontmatter-invalid-type`. Raise the bound per field with the extended `field_types` form (`tags: { type: list_of_strings, max_length: 200 }`, up to the 256-char ceiling) if a vault's existing values need more room.

### Added

- **`string` and `text` frontmatter field types.** `field_types` gains two new scalar types: `string` (bounded, default max length 64, raisable to a 256-char ceiling via `max_length`) and `text` (unbounded, no `max_length` allowed). Both apply everywhere `field_types` is enforced — `validate`, `norn set`, and `norn new`'s schema-aware coercion. `validate` reports an over-length value via the dedicated `frontmatter-exceeds-max-length` finding; `norn set`/`norn new` refuse the write with the bound in the error message. A `max_length` declared outside 1–256 is a config load error.
- **Extended `field_types` declaration form.** Alongside the existing bare `field: type_name` shorthand, a field can now declare `field: { type: type_name, max_length: N, indexed: BOOL }`. `max_length` is only valid on `string`/`list_of_strings`; `indexed` is valid on any type and is consumed by the (upcoming) derived frontmatter index to force a field in or out of the index regardless of its type. `type` may be omitted when the entry declares only `indexed` (`field: { indexed: BOOL }`) — this type-less shape votes on the index only and declares no type for validation or coercion (the field stays otherwise undeclared). Unknown keys in the object form, `max_length` on a type that doesn't carry a length bound, or a type-less entry that also declares `max_length`, are config load errors.
- **`index.auto` config toggle.** A new top-level `index:` config section gains `auto` (default `true`), the global switch for automatic indexing of bounded-type frontmatter fields in the derived frontmatter index (the `document_fields` table and query routing below).
- **Derived `document_fields` cache index table (schema version 2 → 4).** The cache gains an internal EAV table shredded from the declared/bounded frontmatter field set — one row per (document, indexed field), typed and canonicalized to match query semantics, with a sentinel row for fields absent or null. Object/array values are canonicalized the same way the scan path reads them: bracket-stripped as a whole, so a `[[wikilink]]` embedded inside a nested field (e.g. `custom: {name: "[[Alice]]"}`) matches identically whether the query routes through the index or scans. It's kept current by every rebuild/incremental index run and by a self-healing re-shred (regenerated from the cached frontmatter, no filesystem re-parse) whenever the resolved index set changes. The schema bump means every existing cache silently rebuilds once on first use after upgrading, exactly like any other schema-version bump.
- **`find`/`count` route qualifying queries through the `document_fields` index.** `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, and now `--starts-with`/`--ends-with`/`--contains` on a declared-`indexed` field answer from `document_fields` instead of scanning `frontmatter_json` via `json_extract` — same results, faster on large vaults (`norn count` shares the same query builder as `find`, so it routes identically). The string operators compile as a two-branch union per predicate: text-typed rows read straight off the index (`--starts-with` as an index range scan), and any row holding a non-text value for that field (booleans, numbers — the shapes that render differently between the index and `frontmatter_json`) re-evaluates the exact scan-path expression, so routed and scanned results stay byte-identical for every value shape. Routing is all-or-nothing per query: it only engages when every predicate present is one of these kinds with string-shaped values and every field they touch is indexed; a date predicate (`--before`/`--after`/`--on`) or a non-string `--eq`/`--in` value falls back to the unchanged scan path for the whole query (a provable-equivalence gap in those forms, not a defect — see `src/cache/scan_semantics_probe.rs`). Falling back because a referenced field simply isn't indexed additionally warns once on stderr, but only past 1,000 documents: `scanned N documents on unindexed field(s) 'x', 'y'; declare indexed: true (or a bounded type) to accelerate`. No output, exit-code, or ordering changes on any query — routing is purely an internal query-plan choice.

## v0.40.0 - 2026-07-02

The query-and-rule-parity release (Wave 1 of the relational-engine enablement arc). The `find`/`count` filter grammar gains anchored string operators and multi-key grouped counts; the rules vocabulary gains any-of selectors and typed-reference constraints — together the four parity primitives an external work-state consumer (Mimir) asked for, each shipped CLI+MCP isomorphic. Also folds in rule-targeted creation, the `NORN_ROOT` default root, and a security dep bump.

### Added

- **`NORN_ROOT` environment variable — a default for the vault root.** When `-C/--cwd` is omitted, `norn` now falls back to `$NORN_ROOT` before the process cwd, so exporting `NORN_ROOT=/path/to/vault` lets every invocation target that vault from any directory without repeating `-C`. Precedence is `-C/--cwd` > `$NORN_ROOT` > the process cwd; a relative `$NORN_ROOT` resolves against the process cwd (like a relative `-C`), and an empty/whitespace value is ignored. Applies uniformly to the CLI and `norn mcp` (both resolve the root through the same path).
- **Anchored string field operators — `--starts-with` / `--ends-with` / `--contains` on `find` and `count`.** Frontmatter fields gain prefix/suffix/substring predicates (`FIELD:VALUE` tokens, repeatable, AND-composed like `--eq`), enabling namespace enumeration over tag conventions — `norn find --starts-with tags:release:` returns every document carrying a `release:*` tag. The operators are case-sensitive, treat `%`/`_` and every other character literally (no wildcards or regex), apply to scalar fields and array elements alike, and collapse `[[wikilink]]` brackets on both sides (`--starts-with depends_on:NRN-` matches a stored `"[[NRN-123]]"`). Non-string stored values compare by their JSON text rendering (booleans as `true`/`false`, numbers in canonical form), and needle whitespace is preserved. The `vault.find` and `vault.count` MCP tools gain the matching `starts_with`/`ends_with`/`contains` params. `--contains` is frontmatter-scoped; body substring search remains `--text`.
- **Multi-key grouped counts — `count --by` accepts a comma-separated field list.** `norn count --by project,lifecycle` returns every project's lifecycle distribution in one call (the portfolio-rollup fast path): `groups` nests one map level per field with counts at the leaves, and `by` becomes the key list. The single-key shape is unchanged — `by` stays a plain string and `groups` a flat value→count map, byte-identical for existing consumers. Text output renders nested records-style blocks (indented per level, leaf columns aligned per sibling group); documents missing a field bucket under `(missing)` at that level, mirroring single-key behavior. The `vault.count` MCP tool accepts the same comma token in its `by` param and mirrors the CLI JSON exactly (string+flat for one key, array+nested for several). Field names are trimmed; empty and duplicate names are rejected, as are more than 16 fields. Two behavior changes ride along: `--by ""` (previously a degenerate exit-0 grouping with everything under `(missing)`) now errors, and a frontmatter key whose *name* literally contains a comma can no longer be grouped on (the token is always split — same convention as `--col`).
- **Any-of rule selectors — `match.frontmatter` accepts a list of candidate values.** A rule can now declare `match.frontmatter: { type: [task, phase, initiative] }` and fire on a document whose field equals *any* listed scalar, so one base rule carries constraints shared across several types and per-type rules refine on top — no duplicated constraints, no synthetic discriminator field. A scalar value keeps today's exact-equality semantics; the list enumerates candidate values (it is not containment over an array-valued field). Applies everywhere rules match: `validate`, write-time enforcement on `set`/`new`, and `frontmatter_defaults` gating, on both the CLI and MCP surfaces. Config validation rejects an empty list and non-scalar list elements (exit 2), and the defaults-conflict guard now treats two rules as overlapping when their value sets intersect.
- **Typed-reference constraints — `field_references` validates the *type* of what a field points at.** A rule can now declare `field_references: { parent: { target_type: [phase, initiative] } }` (scalar or any-of list, like `match.frontmatter`): when a frontmatter wikilink in that field resolves to a document whose `type` is outside the set, `validate` emits a new `frontmatter-reference-type` warning carrying `field`, `reference`, `target`, `actual_type`, and `allowed_types`. This is the typed half of referential integrity — link validation checks a reference *resolves*; this checks it points at the right *kind* of document (e.g. a task’s `parent` must be a container, `depends_on` must name tasks, not artifacts). Only resolved links are judged (broken references remain `link-*` findings); a resolved target without a `type` field is outside every set and reports as `(missing)`; array-valued fields check each element. Applies on both the CLI and MCP validate surfaces; not auto-repairable — `repair --plan` lists it as skipped with repoint/retype next-actions.
- **Rule-targeted document creation (`norn new --as <rule>` + `vault.new {rule}`).** A validation rule that declares a `target:` path template and (optionally) an inline `body:` scaffold becomes *creatable*: `norn new --as task --title "…" [--var k=v]` derives the path, applies the rule's `frontmatter_defaults`, and seeds the body — no hand-built path. The rule `name` is the handle; the validation matcher is derived from `target` so a generated doc always satisfies its own rule. A standalone `inbox:` config block (`inbox.path`) is the default target for unrouted creates (`norn new --title "…"`). `vault.new` gains `rule`/`title`/`vars` (mutually exclusive with `path`); `vault.describe` now lists each creatable rule (name, target, required vars, frontmatter defaults, body) plus `inbox`, so an off-filesystem MCP agent can place a fully-formed doc with no path guessing. New `{{var.X}}` template namespace mirrors `{{path.X}}`. Refusals (exit 2): unknown/non-creatable rule, missing required `--var`, missing `--title` where the template needs it.

### Security

- Bumped `anyhow` 1.0.102 → 1.0.103 to clear **RUSTSEC-2026-0190** (unsoundness in `Error::downcast_mut()` after `Error::context`). norn does not call `downcast_mut`, so the advisory was never reachable here — the bump keeps the `cargo deny` advisory gate green.

## v0.39.0 - 2026-06-23

The audit-reader release. norn gains a native read surface over its append-only mutation event stream — the source of truth for every confirmed mutation, previously inspectable only by `cat`-ing files under the state dir. `norn audit` and the capability-isomorphic `vault.audit` MCP tool close the last off-filesystem read gap, so an MCP-only agent can finally read back the audit log it could never reach. Also folds in four runtime dependency bumps.

### Added

- **`norn audit` and the `vault.audit` MCP tool — a native reader over the per-vault mutation event stream.** The append-only JSONL stream (the source of truth for every confirmed mutation) was previously inspectable only by `cat`-ing files under the state dir — impossible for an off-filesystem/MCP-only client. `norn audit` reads it back newest-first with AND-combined filters: `--trace` (one invocation), `--status applied|skipped|failed`, `--target` (matches a move's source or destination), `--since`/`--until` (`YYYY-MM-DD` or RFC-3339), and `--limit` (default 20). Output is a flattened norn-native projection (hot fields `trace`/`status`/`target` promoted to top-level, the rest in a generic `attributes` bag) or, with `--raw`, the stored OTEL Logs objects verbatim; `--format records|json`. The `vault.audit` MCP tool carries the identical filter surface, is capability-isomorphic with the CLI, and — being read-only — is available even under `norn mcp --read-only`. Only confirmed mutations are recorded (dry-runs and reads are not); an empty or absent stream returns `[]` with exit 0.

### Changed

- Bumped `camino` 1.2.2 → 1.2.3 (patch).
- Bumped `chrono` 0.4.44 → 0.4.45 (patch).
- Bumped `regex` 1.12.3 → 1.12.4 (patch; pulls `regex-syntax` 0.8.10 → 0.8.11).
- Bumped `rusqlite` 0.39.0 → 0.40.1 (still `features = ["bundled"]`; ships newer bundled SQLite). No source changes: 0.40's breaking changes are confined to the virtual-table API, which norn does not use, and 0.40.1's SAVEPOINT-name SQL-injection fix does not apply (norn issues no SAVEPOINTs). Cache schema and on-disk format unchanged; Atlas dogfood reads held at ~32ms. Pulls `libsqlite3-sys` 0.37 → 0.38 and `hashlink` 0.11 → 0.12 (the latter adds a second `foldhash` version transitively — a cargo-deny `multiple-versions` warning, not an error).

## v0.38.0 - 2026-06-16

The sub-document-edit release. `norn edit` and the `vault.edit` MCP tool add atomic, content-anchored partial edits to a document body — closing the last vault-mutation gap with no filesystem-free path, so an agent driving a remote/off-filesystem MCP vault can now make surgical body edits (not just whole-document operations). The CLI and MCP surfaces are capability-isomorphic, and the feature rides the existing safe-mutation machinery (dry-run/confirm, per-vault lock, event-stream audit).

### Added

- **`norn edit <DOC>` and the `vault.edit` MCP tool — atomic, content-anchored sub-document body edits.** Surgical alternative to `set --body-from-stdin`'s wholesale body replacement: apply an ordered JSON array of ops, all-or-nothing, each against the result of the prior. Ops: `str_replace {old, new, replace_all?}` (literal replace, unique-match-or-refuse unless `replace_all`), `replace_section {heading, content}` (swap a section's body, heading kept), `append_to_section {heading, content}`, `delete_section {heading}` (heading + body), and `insert_before_heading` / `insert_after_heading {heading, content}` (positional insert). Sections are addressed by exact heading text; a duplicated heading refuses as ambiguous (exit 2), and any anchor failure refuses the whole batch and writes nothing. CLI input via `--edits-json` or stdin, with the same `--dry-run`/`--yes`/`--format records|json` safety convention as `norn set` (dry-run by default in a non-TTY; exit `0` success/dry-run, `1` operator-cancelled, `2` refusal); apply is audited to the event stream with a `trace_id`. The `vault.edit` MCP tool carries the identical op array — dry-run by default, `confirm: true` to write, and refused under `norn mcp --read-only`. Decision rule: `set --body-from-stdin` for a wholesale body rewrite, `edit` for surgical changes; frontmatter stays with `set`.

## v0.37.0 - 2026-06-14

The agent-integration release. norn becomes an **MCP server** and the global cache/state trees gain **cross-vault garbage collection**. `norn mcp` exposes the vault to coding-agent harnesses as 12 structured tools (6 read, 6 mutation) over stdio — mutations are dry-run by default with event-stream auditing, a `--read-only` flag yields a query-only surface, and `vault.describe` lets off-filesystem clients place new documents. `norn cache prune` (plus a throttled lazy sweep) reclaims the per-vault cache/state dirs norn previously kept forever.

### Added

- **`norn mcp` — a stdio Model Context Protocol server exposing the vault as 12 tools.** The same deterministic primitives behind the CLI, now reachable by any MCP client (Claude Code, Codex, or a generic harness) over JSON-RPC on stdio. One vault per server, resolved from `-C`/`--cwd`; the cache is held warm across calls (with the CLI's per-invocation freshness check on each call). The tool catalog:
  - **Read (6):** `vault.find` (full-text + metadata search with sort/limit/paging), `vault.count` (total or grouped), `vault.get` (one or more docs: frontmatter, headings, links, optional body), `vault.validate` (structured findings), `vault.repair_plan` (a deterministic `MigrationPlan` without applying it), and `vault.describe`.
  - **Mutation (6):** `vault.new`, `vault.set`, `vault.move`, `vault.delete`, `vault.rewrite_wikilink`, and `vault.apply_plan` (apply a `MigrationPlan`, e.g. one returned by `vault.repair_plan`, inline).
  - **`vault.describe` — the placement aid for off-filesystem clients.** Returns the folder tree, the declared path rules (which glob gets which frontmatter defaults — i.e. where each kind of doc lives), and the frontmatter schema (field types, allowed values, required fields). An agent that can't `ls` the vault uses it to construct a new document's path, then calls `vault.new`.
  - **Mutation-safety contract: every mutation tool is DRY-RUN by default.** A call without `confirm: true` returns the planned change and writes nothing; `confirm: true` acquires the per-vault mutation lock, applies via the same shared applier as the CLI, and audits the mutation to the append-only event stream (the same records `norn set`/`migrate` write — `trace_id` included).
  - **`norn mcp --read-only` — a query-only surface.** The 6 mutation tools are dropped from `tools/list` (never registered) and refused at runtime (defense in depth), leaving only the 6 read tools.
  - In-process calls are serialized for safety (NRN-55): the server guards the cold-start cache-open window so concurrent calls can't race into "database is locked".
  - **Explicitly out of scope for this v1** (tracked under the ongoing MCP initiative): no sub-document/prose-body edit tool (`vault.set` replaces a whole body; surgical edits await `norn edit`); no HTTP/remote transport (stdio/local only); no declared doc-kind placement templates (use `vault.describe` + construct the path). For heavy use, ensure the cache is built first (`norn cache rebuild` or any prior CLI run) — a cold concurrent start otherwise serializes behind the first call's rebuild.
- **`norn cache prune` — cross-vault GC for the global cache and state trees.** Scans the cache tree (`~/.cache/norn/`, one entry per vault) and the state tree (`~/.local/state/norn/`). Cache entries are evicted when their vault root no longer exists, they are unreadable or empty (lock-file-only entries count as empty), they exceed the retention window (default 90d, newest-file-mtime metric), or they push the cache tree over its internal 1 GiB cap (oldest first); state entries (the mutation event stream — a record, not a rebuildable cache) are evicted only when their vault root is gone (or the entry is empty), never by age or size. The current vault's entries are never evicted. Flags: `--dry-run` (report, delete nothing), `--retention <dur>` (overrides config and the 90d default), `--format text|json` (the JSON report is a stable `{dry_run, cache, state, total_bytes_freed}` object with kebab-case eviction reasons: `dead-root`, `unreadable`, `empty`, `aged`, `over-cap`). Exit 0 includes nothing-to-prune.
- **Lazy cache GC: every norn invocation now runs the prune sweep at most once per 24h** (throttled by the `~/.cache/norn/.last-prune` marker file). **norn now deletes stale entries under `~/.cache/norn/` and `~/.local/state/norn/` that it previously left in place forever.** The sweep is the same GC as `norn cache prune`: the current vault's entries are always exempt, and state entries are removed only when their vault root is gone (or the entry is empty). Opt out per vault with `cache: { prune: manual }` in `.norn/config.yaml` — the explicit `norn cache prune` always works regardless.
- **New `cache:` config block** with `retention` (a `90d`-style duration string — `<n>w|d|h|m`; a malformed value falls back to the 90d default, best-effort) and `prune` (`lazy` default | `manual`), mirroring the `telemetry:` block's shape and best-effort parsing.

### Changed

- **norn invocations now garbage-collect the global cache and state trees** (at most once per 24h) — see the lazy-GC entry under Added for the full eviction rules and the per-vault opt-out.

## v0.36.1 - 2026-05-31

A maintenance release: a `norn self-update` fix plus the find/get unification follow-ups and a rename-leftover cleanup. The headline is self-update — its archive extractor still looked for the pre-rename `vault` binary, so it couldn't unpack any post-rename release. (Because a broken self-update can't repair itself, upgrading to this release needs one manual installer re-run; self-updates work normally after.) Also completes find/get parity with the `.stem` facet, single-sources the shared sort/paging flags, and moves the cache directory from the leftover `~/.cache/vault/` to `~/.cache/norn/`.

### Fixed

- **`norn self-update` no longer fails to extract the new binary from the release archive.** The archive extractor searched each tarball entry for a file named `vault` — the pre-v0.34 binary name — so against any post-rename release it found no match and aborted with `archive … did not contain a norn binary`. The binary cargo-dist ships has been `norn` since v0.34.0. This is the third rename-leftover of the same class (after the help literals and the v0.35.2 receipt-path fix), and it stayed hidden because the unit test fixture used the same stale `vault` name and because same-version self-updates short-circuit before downloading. The extractor (and the temp-file fallback name) now key on the bin target name via `CARGO_BIN_NAME`, so a future rename can't desync them again; the test fixtures use the real cargo-dist layout (`norn-run-<target>/norn`) with a regression guard that the stale `vault` name is *not* matched. **As with the v0.35.2 fix, an already-broken install can't repair itself** — upgrading to a release carrying this fix needs one manual installer re-run; self-updates work normally thereafter.

### Added

- **`.stem` facet on `--col` for both `norn find` and `norn get`, and `get --sort stem`.** The filename stem (the document name without its `.md` extension) is now addressable as the `.stem` structural facet — `find --col .stem` / `get --col .stem` emit it as a labeled `stem` row (records) or `"stem"` key (json/jsonl) alongside the `path` identity. Like `.path`, it is **identity-class**: opt-in by name only, never included in the default view or `--all-cols`, so existing dumps are byte-unchanged. `get` also gains `--sort stem` to match `find`, which already sorted by stem — completing stem parity across the shared read contract.

### Changed

- **The cache directory moved from `~/.cache/vault/` to `~/.cache/norn/`.** The cache-identity path still carried the pre-v0.34 product name `vault`, while the sibling state directory (mutation lock, event stream) already used `~/.local/state/norn/` — an inconsistency and a leftover from the rename. `cache_dir_for` now resolves `<XDG_CACHE_HOME>/norn/<hash>/` (honoring `$XDG_CACHE_HOME`), matching the state dir and what `norn config show` reports. The cache is a disposable read-acceleration store, so the only effect is a one-time transparent rebuild on the next run; any old `~/.cache/vault/` directory is now orphaned and can be deleted.
- **`find` and `get` now share one `--sort` / `--limit` / `--no-limit` / `--starts-at` definition (internal single-sourcing).** Behavior is unchanged — `find` still defaults `--limit` to 10 and sorts in SQL; `get` still returns every named target by default and sorts in-memory — but the flags are now defined once and flattened into both commands so the surface can't drift. The only visible effect is lightly reworded `--help` text (e.g. the `--limit` help names both commands' defaults).

## v0.36.0 - 2026-05-30

The unified `find` / `get` output contract — plus the warning/mutation-channel fixes accumulated since v0.35.2. `find` (predicate selection, broad) and `get` (identity selection, targeted) now differ only in *how they pick documents*; everything downstream is one shared, identical contract: `--col` projection (bare names = frontmatter fields, dot-prefixed = structural facets), the `records` / `paths` / `json` / `jsonl` formats, `--all-cols`, and sort / limit / paging. Facets render identically on both, single-sourced so the two can't drift again. The one principled divergence is `get --format markdown` (a single byte-faithful document — selection-bound, so get-only). Most of the breaking surface is `get` catching up to `find`'s output model.

### Breaking changes

- **`norn get` now offers the same output formats as `norn find`: `records`, `paths`, `json`, `jsonl`.** Previously `get` had only `text` and `json`. The `text` format is **renamed to `records`** (it always *was* the vertical key-value record-block primitive `find` calls `records`), and `get` gains `paths` (one document path per line; `--col` is inert and warns, exactly as on `find`) and `jsonl` (one JSON record object per line — the streaming sibling of the `json` array). Migration: `get --format text` → `get --format records`. The default is now `records`.
- **`norn get`'s default `records` view renders each frontmatter field as its own labeled line, matching `norn find`.** Previously `get`'s default emitted frontmatter as a single consolidated `frontmatter` block (all keys inline under one label); `find`'s `records` emitted each key as its own labeled line. Under one shared contract a bare field is a column, so both now render frontmatter fields as individual labeled lines at rest. Migration: the consolidated block is no longer the default — request it explicitly with `get --col .frontmatter` (which still emits the whole block). `json`/`jsonl` are unaffected (frontmatter was already an object under the `frontmatter` key in both).
- **`norn get --col <section>` now requires a leading dot for structural facets.** `get --col` accepted only a fixed set of structural section names (`frontmatter`, `headings`, `body`, the link sets) and rejected anything else — so `get <doc> --col status` warned and showed nothing, even though `find --col status` projects that field. The two commands now share one `--col` model: **bare names are frontmatter fields** (`get <doc> --col status,title` projects just those, identical to `find`), and the structural facets are **dot-prefixed** — `.path`, `.frontmatter` (the whole block), `.headings`, `.body`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`. The leading dot keeps a frontmatter key named e.g. `body` from colliding with the body facet. Migration: existing `get --col <section>` calls must add the dot — `get --col body` → `get --col .body`, `get --col frontmatter` → `get --col .frontmatter`, etc. (The nested/dotted *frontmatter*-key half of the addressing model — `--col schema.version` — remains future work; this ships the top-level field projection + the facet split.)
- **The `norn get --body` flag is removed.** Body content is now reached through the unified `--col` vocabulary that `get` shares with `find`: `--all-cols` for the full structured dump (which self-loads the body) or `--col .body` for the body on its own. The standalone `--body` boolean was a pre-unification artifact — it predated `.body` being a first-class facet, and once `.body` self-loads on request the flag is redundant. Migration: `get <doc> --body` → `get <doc> --all-cols` or `get <doc> --col .body`; `get --body` now errors as an unknown flag.

### Added

- **`--all-cols` on both `norn find` and `norn get` — the full structured dump in one flag.** Where the default `find` view is frontmatter-only and the default `get` view is frontmatter + headings + links (no body), `--all-cols` emits **every cache-served facet at once**: every frontmatter field plus `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`, and `.body`. It **self-loads the body** so you don't have to ask for it separately, and it renders identically on both commands (per-field frontmatter rows + facet blocks in `records`; the whole frontmatter object plus facet keys in `json`/`jsonl`). It deliberately **excludes `.raw`**: `.raw` is the byte-faithful disk-read representation, always requested by name, and keeping `--all-cols` cache-only means a broad `find --all-cols` never silently fans out to N file reads (`--all-cols` on `find` does imply the per-match deep fetch for headings/links, but zero disk reads). `--all-cols` and `--col` are **mutually exclusive** — passing both is a clean usage error rather than a silent precedence rule. This completes the find/get shared output contract: the same dump-everything escape hatch over both selection front-ends.
- **`norn get` gains `--sort` / `--desc` / `--limit` / `--no-limit` / `--starts-at`.** `get` resolves the named targets, then sorts/limits/pages the resulting record set in memory — the same sort-and-paging surface `find` exposes, so the shared output contract now covers ordering too. Two intentional divergences from `find`: (1) `get`'s **default limit is none** (you named the targets — truncating them to 10 the way a broad query does would surprise), so `--limit N` is opt-in and `--no-limit`/absent returns every named target; (2) `get`'s sort is a **simple in-memory display-string field compare** (a frontmatter key or the `path` identity; records missing the field sort last), not `find`'s SQL collation — an acceptable divergence for an already-resolved, targeted record set. `--starts-at` applies the 1-indexed offset before `--limit`. Irrelevant for `--format markdown` (which still returns a single byte-faithful document and errors on >1 selected); sort/limit are skipped there so they can't mask that error.
- **`norn get --format markdown` — the single selected document, byte-faithful, straight to stdout.** The one place the contract is *allowed* to differ between the two commands, because it is selection-bound rather than a projection difference: "give me **this** document, verbatim" only means anything for a single document, and a single named document is exactly what `get`'s identity selector is. So `markdown` is `get`-only (`find`, a set-returning broad query, does not offer it — the broad/multi-doc raw case is served by the `.raw` facet in `json`/`jsonl`). It prints the source file exactly as on disk (no count line, no record block, no trailing-newline fixup) — the "cat this one doc" / "feed this file to another tool" case without a `jq` extraction. **Limit-1 is enforced, not assumed:** `get a.md b.md --format markdown` (or a target that resolves to more than one document) errors with `--format markdown returns a single document; N selected — use --format json --col .raw for multiple` and exits non-zero, so the never-safe multi-document raw-concatenation problem can't arise. `--col` is inert (you get the whole faithful document) and warns, as with `paths`.
- **New `.raw` facet on both `norn find --col` and `norn get --col` — the byte-faithful whole source file.** Where `.body` is the *parsed* body served from the cache (post-frontmatter, trimmed), `.raw` is the **entire source file exactly as it sits on disk**: the frontmatter block, the body, original whitespace, YAML comments, and key order all preserved. It is the **one facet that reads from the filesystem at query time** — the cache stores only parsed data, so `.raw` does a disk read per document. Coherence relies on a fresh cache: in a row mixing cache-served facets with `.raw`, the two are momentarily inconsistent if the file changed since the last cache build (the same assumption every read makes; the pre-query refresh normally closes it). Because it touches disk, a broad `find --col .raw` is N disk reads — `.raw` is therefore **excluded from the default dump** and is only emitted when requested by name. `find` reads `.raw` per match only when `.raw` is in `--col`, so the default path stays at zero disk reads. Renders identically on both commands (a `raw` field in `records`, a `"raw"` string key in `json`/`jsonl`), placed last as the heaviest facet; the key/field is omitted when the file is unreadable.
- **`norn get --col .body` / `.raw` now load what they request without needing a separate flag.** A requested heavy facet self-loads: `get <doc> --col .body` used to show nothing because the body only populated when the (now-removed) `--body` flag was passed — it now loads the body whenever `.body` is requested (or `--all-cols`, the full dump), and `.raw` does its disk read whenever `.raw` is requested. Both stay opt-in: neither appears in the default (no-`--col`, no-`--all-cols`) view.
- **`norn find --col` now accepts the structural facets `get` already supports** — `.frontmatter`, `.body`, `.headings`, `.outgoing_links`, `.unresolved_links`, `.incoming_links`. Previously `find --col` understood only bare frontmatter field names; a dot-prefixed token like `.headings` was misread as a (nonexistent) frontmatter field named `.headings` and silently projected nothing. The two commands now share one `--col` vocabulary end-to-end: **bare names select frontmatter fields** (`find --eq type:note --col status,title`), and the **dot-prefixed tokens are the fixed structural facets** — the leading dot keeps a frontmatter key named e.g. `body` from colliding with the `.body` facet. `find` and `get` render every facet identically (the per-facet leaf rendering is single-sourced), so `find --col .headings` and `get <doc> --col .headings` produce the same heading block / JSON shape. The join-backed facets (`.headings` and the three link sets) require a per-document lookup, so they are fetched **only when one of those facets is requested** — the default frontmatter-only `find` path and the cheap facets (`.frontmatter`, `.body`, both already carried on every match) cost zero extra queries. `find` keeps its own output envelope (the leading count line in `records`, the `{total, returned, starts_at, documents:[…]}` wrapper in `json`, the truncation note on `paths`/`jsonl`); only the per-facet rendering is shared. An unknown facet (`--col .bogus`) warns with the valid-facet list and a bare-name-vs-dot hint; a bare field absent from every match warns as before.

### Changed

- **`norn new` now distinguishes `ambiguous-wikilink` from `unresolved-wikilink`.** When a wikilink field value's stem resolved to *multiple* documents, `norn new` previously reported it as `unresolved-wikilink` (the "no candidates" kind) — the spec's fifth warning kind was deferred in v1. It now emits a distinct `ambiguous-wikilink` warning carrying the `candidates` list (the matching document paths), so an operator/agent can tell "no such doc" (fix by creating it or correcting a typo) from "multiple docs share that stem" (fix by disambiguating with a path-qualified `[[dir/doc]]` link). Still warn-only; `norn new` never refuses on link resolution. Consistent with how `norn validate` already separates `link-unresolved` from `link-ambiguous`.
- **`norn set` now refuses a value outside a field's `allowed_values` set.** Previously `set` keyed its schema validation entirely off `field_types`, so `norn set <doc> --field status=bogus` would write `bogus` without complaint even when the schema declared `allowed_values: [backlog, in_progress, completed]` for `status` (only `norn validate` caught it, after the fact). `set` now refuses a disallowed value with a clear error listing the allowed set and exits non-zero (2), mirroring how it already refuses `datetime`/`wikilink` type mismatches. `--force` writes the value verbatim and emits a `--force bypass` warning. Enforcement applies to the scalar set paths (`--field`, `--field-json`); list-element (`--push`/`--pop`) enforcement is not included.

### Fixed

- **`norn set` no longer prints a spurious `unknown field` warning for `allowed_values`/required-only fields.** `set` treated a field as "unknown to the schema" whenever it lacked a `field_types` entry — so a field declared via `allowed_values` and/or `required_frontmatter` but with no special coercion type (e.g. `status`) drew a false `warning: unknown field: status` on every write. "Known to the schema" is now the union of `field_types`, `allowed_values`, and `required_frontmatter`; the warning fires only when the field is declared by none of them. (The "is this field known?" check is now a dedicated `is_known_field` oracle, split out of the `lookup_field_type` coercion lookup that was overloaded to answer both questions.)
- **Concurrent cache opens no longer spuriously fail with `database is locked`.** Two `norn` invocations opening the same vault cache at the same moment could race on the brief write locks taken during schema setup (fresh open) and the journal/integrity pragmas (inspecting open), with the loser returning `SQLITE_BUSY` immediately because SQLite's default lock-wait is zero. Every cache connection now sets a 5s `busy_timeout` right after open — matching the 5s advisory flock that `rebuild` already holds — so these brief collisions are absorbed instead of erroring. (Surfaced as an intermittent `two_simultaneous_rebuilds_serialize` failure on macOS CI.)

## v0.35.2 - 2026-05-29

Patch release. Fixes `norn self-update`, which has been broken for every GitHub-installer install since the v0.34 rename. **One-time upgrade step:** because the broken version can't update itself, re-run the installer once to land on v0.35.2 — `self-update` works normally thereafter.

### Fixed

- **`norn self-update` now finds the install receipt again.** Since the v0.34 `vault-cli → norn` rename, `norn self-update` reported `this binary does not have an install receipt` for *every* GitHub-installer install, because it looked for the receipt under the binary name (`~/.config/norn/install-receipt.json`) while cargo-dist writes it under the package name as `~/.config/norn-run/norn-run-receipt.json`. The path is now derived from `CARGO_PKG_NAME` (`<app>/<app>-receipt.json`), matching what the installer writes and staying correct across future renames. Additionally, the receipt parser no longer requires a top-level `target` field (absent in current cargo-dist receipts) — both `target` and `version` are now optional, since the update target triple comes from the compiled binary and the version from the binary itself. **Note:** because the broken detection shipped in v0.34.0–v0.35.1, upgrading *to* the fixed version requires one manual installer re-run (`curl … norn-run-installer.sh | sh`); `self-update` works normally from the fixed version onward.

## v0.35.1 - 2026-05-29

Patch release. Two `frontmatter_defaults` rule-matching fixes surfaced by dogfooding norn against a real vault config — they let a schema use per-folder defaults (e.g. `tasks/ → type: task` alongside `notes/ → type: note`) and make `norn new`'s provenance honest. No schema, CLI-surface, or behavior changes beyond the fixes below.

### Fixed

- **`frontmatter_defaults` conflict detection is now path-aware.** `norn config validate` (and config load) previously rejected any two rules that set the same `frontmatter_defaults` field to different values, even when their `match` predicates were provably disjoint and could never apply to the same document. This blocked the natural per-folder pattern of `tasks/ → type: task` alongside `notes/ → type: note`. The check now only flags a conflict when the two rules can actually co-apply — it treats rules as disjoint when their concrete path globs diverge on a literal segment (before any `**`) or when their `match.frontmatter` predicates demand incompatible values. Genuine conflicts between rules that can overlap (e.g. two `**/*.md` rules) still error as before.
- **`norn new` now credits each schema default to the rule that actually governs the document.** The per-field provenance shown by `norn new` (`(schema-default, <rule>)`) previously named the first rule in config order that *declared* the field, regardless of whether that rule's `match` applied to the target path — so a document under `tasks/` could be attributed to a `notes/`-only rule that merely declared the same default earlier. Attribution is now restricted to the rules that match the document's path and resolved frontmatter. Resolved values were always correct; only the displayed source rule was wrong.

## v0.35.0 - 2026-05-29

The safe-mutation release. The mutation surface that landed piecemeal in v0.32–v0.33 is now unified and made trustworthy. Three arcs ship together: (1) **mutation unification** — `move`, `delete`, `rewrite-wikilink`, and the new `migrate` command all build a `MigrationPlan` (schema v1), expand it through one shared planner, and apply it through one shared applier, emitting a single `ApplyReport` (schema v2); `norn repair --plan` generates plans that `norn migrate` applies, retiring `repair apply`. (2) **safe-mutation guarantees** — an exclusive per-vault mutation lock guards every mutating command; the backlink cascade goes from atomic-abort to best-effort collect-and-continue with a bounded retry pass and explicit failure reporting; and an always-on, append-only OTEL-Logs-Data-Model JSONL event stream in the XDG state dir becomes the authoritative record of every mutation, with each command's `--format json` summary folded from the same in-memory event stream (a summary can't disagree with its log). (3) **query reach** — `norn find` (and `norn count`) gain link-relationship predicates (`--links-to`, `--unresolved-links`), closing the last 2026-05-27 migration-dogfood tool gap. Pre-1.0, so the unification carries breaking JSON/CLI changes with no migration shims; regenerate any persisted plans and update scripts that parsed the old report envelopes.

### Breaking changes

- **`norn repair apply <plan>` removed.** Use `norn migrate <plan>` instead — it applies any migration plan, including repair-generated ones. The canonical auto-fix pipeline is now `norn repair --plan --format json | norn migrate -`. Invoking `norn repair apply` exits non-zero (the subcommand no longer exists). No migration shim.
- **`norn repair plan` subcommand replaced by the `norn repair --plan` flag.** `norn repair --plan` generates a plan; bare `norn repair` prints a findings summary. Every prior `repair plan` flag carries over to `repair --plan`: `--format` (report/json/paths), `--out`, `--confidence`, `--skip-reason`, and the triage filters `--code` / `--severity` / `--field` / `--rule` / `--path` / `--target` / `--reason`.
- **`MoveReport`, `DeleteReport`, and `RepairApplyReport` JSON envelopes replaced by a single `ApplyReport` (schema_version 2).** `norn move`, `norn delete`, `norn rewrite-wikilink`, and `norn migrate` all emit `ApplyReport` for `--format json`: `{ schema_version, plan_hash, vault_root, trace_id, dry_run, applied, skipped, failed, remaining, operations: [{ op_id, kind, status, from?, summary, error?, footnote?, cascade? }], warnings }`. Scripts that parsed the old `source` / `destination` / `link_rewrites` / `target` / `incoming_links` keys must read `operations[]` instead (the move/delete backlink count moves to `operations[].cascade`, see Added). No migration shim.
- **`ApplyReport`, `SetReport`, and `NewReport` carry a `trace_id`; all three schema versions are `2`.** `trace_id` is the mutation event stream's per-invocation id (see Added) — present on every `--format json` report from a real apply (empty on dry-run/preview). `SetReport` (`norn set`) and `NewReport` (`norn new`) keep their existing actuals-based shapes otherwise. No migration shim.
- **`norn repair --plan` emits a `MigrationPlan` (schema_version 1), not a `RepairPlan` (schema_version 9).** The shape changes from `{ schema_version: 9, changes: [{operation, …}], skipped_findings, summary, source_filters }` to `{ schema_version: 1, generator: "norn-repair", generated_at, operations: [{kind, fields, footnote?}], skipped }`. Feed the new plan to `norn migrate` (the `repair apply` consumer is gone). No migration shim — regenerate any persisted plans with `norn repair --plan`.

### Added

- **`norn find --links-to <TARGET>`** — link-relationship predicate: match documents whose outgoing links resolve to TARGET. TARGET accepts the same shapes as `norn get` (vault-relative path, case-insensitive stem, or `[[wikilink]]`). Resolved-only: TARGET must resolve to an existing document (dangling links that merely name TARGET's stem are not matched — use `--unresolved-links` for the "has broken links" job). Repeatable; multiple `--links-to` are AND'd (a document must link to every named target). A target that resolves to no document, or ambiguously to several, errors (exit 1, matching `norn get`'s missing-target behavior). Composes with every existing predicate, e.g. `norn find --eq type:task --links-to norn --format paths`. Closes the last 2026-05-27 migration-dogfood gap (enumerating "every doc that links to X" no longer needs `norn get … --col incoming_links` + jq).
- **`norn find --unresolved-links`** — link-relationship predicate: match documents with at least one link whose status is `unresolved`. (Ambiguous-status links are not matched — they are a distinct state with their own `validate` codes.) Composes with other predicates and with `--format paths`, e.g. `norn find --unresolved-links --eq type:task --format paths`.
- Both link predicates are part of the shared filter surface, so they apply to **`norn count`** as well (e.g. `norn count --links-to norn`, `norn count --unresolved-links --by type`). No cache schema change — the existing `links` table columns and `idx_links_resolved` index back the queries.
- **`norn migrate <plan>`** — apply any migration plan. Reads a JSON or YAML file, or `-` for stdin (stdin defaults to JSON; override with `--input-format yaml`). Validates `schema_version == 1`, expands high-level ops via the shared planner, applies through the shared applier, and emits an `ApplyReport`. Standard `--dry-run` / `--yes` / `--format` / `--out` flags. Pre-flight failures (parse error, unsupported schema version, unresolvable op) exit 2; mid-flight failures exit 1.
- **`norn rewrite-wikilink OLD NEW`** — graph-aware wikilink retargeting across body and frontmatter, without requiring a file move to trigger the cascade. Resolves `OLD` by stem, path, or alias; rewrites every matching body wikilink and every frontmatter wikilink field. Refuses with exit 2 when `OLD` resolves to no document. Standard `--dry-run` / `--yes` / `--format` flags.
- **`norn move --parents` / `-p`** — creates missing destination parent directories before moving (mirrors `norn new --parents`). Without it, a move into a non-existent directory refuses with exit 2.
- **`norn move --recursive` / `-r`** — folder-recursive move: relocates every `.md` file under a source directory to a destination directory, preserving subdirectory structure, with a single backlink-cascade pass and empty-source-directory cleanup. A directory source is auto-detected even without `--recursive`.
- **Bare `norn repair`** — prints a findings summary (count + by-code breakdown). Placeholder for a future interactive repair walkthrough.
- **`MigrationPlan` schema (v1)** — the unified plan artifact for both repair-generated and hand-authored plans. Two high-level intent op kinds (`move_folder`, `rewrite_wikilink`) expand into low-level ops at apply time; the eight existing low-level op kinds (`move_document`, `delete_document`, `set_frontmatter`, `add_frontmatter`, `remove_frontmatter`, `rewrite_link`, `replace_body`, `create_document`) apply directly. Crafted plans accept JSON or YAML; optional per-op `footnote` and plan-level `generator` / `generated_at` metadata distinguish generated plans from hand-authored ones.
- **PLAN OPERATION blocks in `--help`** — `norn move`, `norn delete`, `norn set`, `norn new`, and `norn rewrite-wikilink` each show the equivalent migration-plan op in their `--help` output, so the CLI doubles as the plan-authoring reference.
- **Per-op backlink cascade summary in `ApplyReport`.** `norn move`, `norn delete`, and `norn migrate` now report the backlink cascade each move/delete triggers as a per-op `cascade: { planned, applied, skipped, failed, files }` object in `--format json`, sourced from what actually landed on disk rather than the plan forecast: `applied` and `files` are actuals, `planned` is intent, `skipped` counts planned rewrites that didn't apply, and `failed` counts rewrites that hit a real filesystem error (see "Backlink cascade is now best-effort" under Changed). (A move with no backlinks, or a plain `norn delete` without `--rewrite-to`, reports an all-zero cascade; `delete`'s cascade is non-trivial only when `--rewrite-to` redirects backlinks.) A rewrite skipped because the on-disk link text drifted between plan and apply is recorded with a reason (`drifted`) instead of being silently dropped. `--verbose` adds the per-entry `rewrites: [{file,from,to}]` and `skips: [{file,from,to,reason}]` lists — counts are always present, lists only under `--verbose`. Restores the blast-radius visibility JSON/agent consumers lost when `MoveReport`/`DeleteReport` were folded into `ApplyReport` (the TTY render already showed it).
- **Vault mutation lock**: exclusive per-vault `flock` guard held for the duration of any mutating command (`migrate`, `move`, `delete`, `rewrite-wikilink`, `set`, `new`). Prevents concurrent vault mutations from racing on vault files. On contention, the blocking command exits 2 with `another norn mutation is in progress against this vault`. Dry-run invocations and read-only commands (`validate`, `find`, `get`, `count`) are never affected.
- **Stdin plan rescue on contention**: when `norn migrate -` (stdin source) is blocked by the mutation lock, the consumed plan is automatically saved to `<state_dir>/pending/<hash>.plan.json` and a `retry with: norn migrate <path>` message is printed. Pending files delete themselves on successful retry and are swept after 7 days.
- **State directory**: per-vault XDG state directory at `<XDG_STATE_HOME>/norn/<vault-hash>/` (default `~/.local/state/norn/<hash>/`) used for the mutation lock file and pending plan storage. Independent of the cache dir — persists across `norn cache clear`.
- **Backlink-cascade failure reporting.** When a backlink rewrite hits a real filesystem error during `norn move` / `delete --rewrite-to` / `migrate`, the per-op `cascade` now reports a `failed` count plus a `failures: [{ file, from, to, reason, detail }]` list in `--format json` (`reason` is `read_failed` or `write_failed`; `detail` is the underlying OS error). Unlike the verbose-gated `rewrites`/`skips` lists, `failures` is always present when non-empty. A loud stderr warning naming each dangling backlink (and its error detail) is printed in every output format, since the command still exits 0 in this case (see Changed).
- **Mutation event stream.** Every real `move` / `delete` / `migrate` / `rewrite-wikilink` / `set` / `new` now appends OTEL-Logs-Data-Model-shaped JSONL events to a per-vault, daily-rotated log under the XDG state dir (`<XDG_STATE_HOME>/norn/<vault-hash>/events/events-YYYY-MM-DD.jsonl`). The log is the **authoritative record**: each command's `--format json` summary (`ApplyReport` op statuses and the per-op `cascade` counts) is folded from the same in-memory event stream that is written to disk, so a summary can never disagree with its log. Events are emitted per action as the apply proceeds and flushed immediately, so a crash mid-`migrate` leaves the completed ops' events on disk. Always-on and best-effort — a telemetry write failure degrades to in-memory and never aborts the mutation. Configurable via a `telemetry:` block in `.norn/config.yaml`: `location` relocates the log, `retention` (e.g. `90d`, `12h`) bounds history (defaults: the state dir, 90 days), with an internal total-size guardrail that drops the oldest daily files. Dry-runs generate events in memory for the summary but never persist. `norn config show` now reports the resolved events-log directory. Each real apply's `--format json` report carries a `trace_id` and the records/TTY output prints a `trace: <id>` footer, for correlating a run to its log lines (`grep <trace_id> <log>`). No native query command yet — the log is plain JSONL you read with your own tools.

### Changed

- **All document-mutation commands now share one planner, one applier, and one report envelope.** `norn move`, `norn delete`, `norn rewrite-wikilink`, and `norn migrate` build a migration plan, expand it through the shared planner, and apply it through the shared applier — replacing the per-command bespoke apply paths. `norn new` and `norn set` keep their existing output envelopes pending a follow-up conversion.
- Plan-staleness errors (schema mismatch, document-hash drift, expected-old-value mismatch) and the missing-default fix hint now say `regenerate with \`norn repair --plan\`` (was `norn repair plan`).
- The `norn move` / `norn delete` human (records) output now reports the backlink-rewrite count from what actually landed (the cascade actuals), matching the `--format json` `cascade`. Previously the count came from the plan forecast; the two differ only when a backlink's on-disk text drifted between plan and apply (the drifted rewrite is now excluded from the "rewrote N" line and recorded as a skip).
- **Backlink cascade is now best-effort, not atomic-abort.** A real filesystem error while rewriting one backlink no longer aborts the whole `move`/`delete`/`migrate` — the primary operation stands, the remaining backlinks are still rewritten, and the failed one is reported (`failed` count + `failures` list + stderr warning) rather than discarding the partial result with a hard exit. Before giving up, a cleanup pass retries each failed rewrite up to 3 rounds with backoff (~1.3 s worst case) so a transient lock or busy file self-corrects; clean applies incur no retry cost. A backlinker that was deleted or moved away mid-run is reported as a `source_missing` skip (not a failure, not retried). Because the primary operation succeeded, the command still exits 0 when only backlinks failed; the exit code continues to reflect failed *operations* (`report.failed`), not cascade backlinks.

### Fixed

- `norn move` no longer aborts with `read backlinker failed: No such file or directory` (exit 1) when the moved doc contains a wikilink to itself. The Pass 3 cascade now reads each affected file from its post-move location, rewriting self-references in place and exiting 0. Surfaced by the 2026-05-27 atlas migration dogfood, which observed 5 of 212 backlinks silently missed when the moved doc's own body referenced its old stem — every entry iterated after the self-reference was skipped.
- `norn move --dry-run --format json` and `norn delete --dry-run --format json` now emit the documented JSON envelope (the `ApplyReport`, per Breaking changes above). Previously the dry-run branch short-circuited the format check and unconditionally rendered the human-records format, breaking scripts that piped dry-run output into other tooling.
- `norn move` on a doc containing a sibling-relative CommonMark self-link (e.g. `[label](file.md)` referring to the moved doc itself) now rewrites the link relative to the destination so it stays self-stable. The previous rewrite computed the relative path from the old source directory, producing a dangling cross-directory traversal after the move.

### Known limitations

- **`norn migrate` uses pass-based op ordering, not per-op sequencing.** Ops execute grouped by kind (frontmatter mutations → link rewrites → deletes → body replaces → creates → moves → move-cascades), and within a group in plan order. For a plan whose ops have cross-pass dependencies (e.g., a `delete_document` and a `move_document` of the same path where relative order matters), split into two sequential `norn migrate` invocations. The schema reserves per-op `id` + `requires` fields for future ordering constraints; the v1 applier ignores them.
- **`norn rewrite-wikilink` does not rewrite CommonMark `[label](target.md)` links** — only wikilinks (`[[target]]`) and frontmatter wikilink fields. Markdown links resolve by relative path rather than stem, so a stem-rename does not apply to them; they are left untouched without a warning.
- **`norn migrate` applies all-or-nothing per op with stop-on-first-error and no rollback.** A failure mid-plan leaves preceding ops applied and reports the failing op plus the remaining un-run ops in the `ApplyReport`. Recovery is the operator's responsibility (inspect the report, fix the cause, re-run with an adjusted plan).

## v0.34.0 - 2026-05-27

The Norn rename release. Renames the project identity end-to-end from `vault-cli` to Norn: binary `vault` → `norn`, crate `vault-cli` → `norn-run` (published on crates.io as `norn-run`), config directory `.vault/` → `.norn/`, repository at `dbtlr/norn`. Pre-existing `v0.x` GitHub Releases and tags are deleted; this is the first Norn release with `norn-run-*` cargo-dist asset names. Domain-noun uses of "vault" (the user's Markdown directory; Rust `VaultGraph` / `vault_root` identifiers; README prose like "your Markdown vault") are preserved per the vault-word policy: product = norn, data = vault. Also bundled: the long-deferred 7-crate workspace collapse (commit `38fa0cb`, no user-visible behavior change) and the plan-staleness error-message polish that surfaced post-v0.33.0 grooming. 1,136 tests passing across 21 suites.

### Breaking changes

- **Project renamed from `vault-cli` to Norn.** The binary is now `norn` (was `vault`); the published crate on crates.io is `norn-run`; the repository lives at `https://github.com/dbtlr/norn`. To migrate an existing installation: uninstall the old `vault` binary (`cargo uninstall vault-cli` if installed via cargo, or remove the old install per your installer), install `norn` via the new installer at https://github.com/dbtlr/norn/releases/latest, and run `mv .vault .norn` in any vault that had a `.vault/` config directory. There is no transitional fallback — Norn reads `.norn/config.yaml` only.
- **Shell-completion fence markers renamed.** `norn completions install` writes `# >>> norn completions ... # <<< norn completions <<<` fences into shell rc files. Existing `# >>> vault completions ...` blocks from the prior binary are not detected by the new uninstaller — remove them manually one time after installing the new completions. The generated completion script filenames also change: `vault.bash` → `norn.bash`, `_vault` → `_norn`, `vault.fish` → `norn.fish`, `vault.1` → `norn.1`.
- **Pre-existing v0.x GitHub Releases deleted.** Releases `v0.15` through `v0.33.0` are removed from `dbtlr/norn`. The first Norn release is `v0.34.0` with `norn-run-*` cargo-dist asset names (e.g., `norn-run-installer.sh`, `norn-run-aarch64-apple-darwin.tar.xz`). Anyone with an outstanding pin to a deleted tag must repin.

### Changed

- `norn repair apply`'s plan-staleness errors (schema mismatch, document-hash drift, expected-old-value mismatch) now end with `; regenerate with \`norn repair plan\`` so operators see the next step inline instead of having to consult the CHANGELOG.
- Internal: collapsed the 7-crate workspace into a single `norn-run` crate.
  No user-visible behavior changes. Enables crates.io publishing without
  registering 6 internal libraries that were never meant as public API.

## v0.33.0 - 2026-05-26

The mutation-surface completion + self-update release. Closes the CRUD-ish document-mutation arc started in v0.32: `vault set` adds the update verb (frontmatter mutation with `--push` / `--pop` / `--remove` and wholesale body replacement via `--body-from-stdin`), and `vault new` adds the create verb (schema-aware scaffold from path with `frontmatter_defaults` declared per rule, Obsidian-core-compatible substitution language, and seven chainable pipe transforms). `vault edit` (partial-edit primitive) remains the one deferred verb. Alongside the mutation work: `vault self-update` lands as the housekeeping primitive — refresh the running binary against GitHub Releases; pair `--dry-run` with `--format json` for scriptable "is there an update?" checks. `vault-standards` config gains `frontmatter_defaults`, named path captures (`{{name}}` declared in `match.path`, `{{path.name}}` referenced in defaults), and an optional `templates: { date_format, time_format }` block — all additive. Three runtime dependency bumps ship: `serde_json` patch, `rusqlite` minor (bundled SQLite update), and `sha2` 0.10 → 0.11 (cache-identity hashing migrated to explicit byte iteration; existing cache directories continue to resolve). `repair_plan_schema_version` bumps twice (7 → 8 for `replace_body`, 8 → 9 for `create_document`). 865 → 1136 tests.

### Breaking changes

- **vault-standards path patterns**: `?` and `{a,b,c}` are now interpreted as glob
  wildcards (single-char and alternation respectively), matching standard glob
  semantics. Previously both were treated as literal characters by the legacy
  matcher. Atlas config does not exercise either shape today; user configs that
  deliberately used a literal `?` or literal `{` must escape via regex-free
  equivalents.
- **`RepairPlan` schema version 7 → 9** (two additive bumps in one release).
  7 → 8 adds the `replace_body` op variant (used by `vault set --body-from-stdin`);
  8 → 9 adds the `create_document` op variant (used by `vault new`). Plans
  emitted at schema 7 or 8 still parse; new plans emit version 9.

### Added

- **`vault new <PATH>`** — create verb of the document mutation surface. Schema-aware
  scaffold from path with `frontmatter_defaults` declared per rule. Substitution
  language is Obsidian-core-compatible (`{{title}}`, `{{date}}`, `{{time}}` +
  formatted `{{date:fmt}}`/`{{time:fmt}}`) plus Norn extensions (`{{now}}`,
  `{{path.X}}`, and pipe transforms `titlecase` / `sentencecase` / `lower` /
  `upper` / `unsep` / `strip_date_prefix` / `slugify`). New `-p` / `--parents`
  flag for mkdir-style parent-dir auto-create. Apply model mirrors `vault set` /
  `move` / `delete` (TTY confirm default, non-TTY implicit dry-run, `--yes`,
  `--dry-run`, `--format records|json`). Post-create `vault validate` hook
  surfaces findings as envelope warnings.
- **`vault set <DOC>`** — frontmatter mutation (`--field` set, `--push`/`--pop` for arrays, `--remove` to drop a key) plus wholesale body replacement via `--body-from-stdin`. Schema-aware value validation with `--force` opt-out. Wikilink-typed fields auto-wrap on write (`--field workspace=foo` → `[[foo]]`) and surface unresolved / ambiguous targets as warnings. Combined ops apply atomically. Safe-by-default apply model matching `vault move` / `vault delete`: TTY confirms, non-TTY implicit dry-run, `--yes` to mutate, `--dry-run` to preview, `--format json` for scripting.
- **`vault self-update`** — refresh the running `vault` binary to the latest (or a pinned) GitHub release. `--dry-run` resolves the target and prints the plan (with `update_available`, current/latest/target versions, asset URL, sha256) without downloading anything; pair with `--format json` for scriptable "is there an update?" checks. `--version X.Y.Z` pins a specific release (downgrades allowed). Hidden from `vault --help` and exits 2 with installer instructions when the running binary was not installed via the official GitHub install script (detected by the presence of cargo-dist's install receipt at `~/.config/vault-cli/install-receipt.json`).

### Changed

- `vault-standards` config gains optional `frontmatter_defaults` field on
  validate rules, named path variables (`{{name}}` in `match.path` declared
  bare, referenced as `{{path.name}}` in defaults), and an optional top-level
  `templates: { date_format, time_format }` block. Pre-existing configs work
  unchanged — all additions default to empty/none.
- `vault repair apply` (and the underlying `vault-frontmatter` minimal-edit writer) now supports array-valued frontmatter operations. Previously, only scalar `set_frontmatter` / `add_frontmatter` ops applied; array values were rejected at apply time. New `vault set` push/pop and any future repair-action that writes arrays now work end-to-end.
- Bumped `serde_json` 1.0.149 → 1.0.150 (patch).
- Bumped `rusqlite` 0.32.1 → 0.39.0 (still `features = ["bundled"]`; ships with a newer bundled SQLite). No source changes required; cache schema and on-disk format unchanged.
- Bumped `sha2` 0.10.9 → 0.11.0. The digest type lost its `LowerHex` impl in 0.11; cache-identity hashing in `vault-cache` migrated from `format!("{:x}", …)` to explicit byte-iteration. Hash output is byte-identical to the previous formulation, so existing cache directories continue to resolve.

## v0.32.0 - 2026-05-25

The renderer-port + mutation-surface release. Two coordinated arcs ship together. (1) The `output::legacy` cleanup that began at v0.27 closes: `vault validate`, `vault repair plan`, and `vault repair apply` all port onto `output::primitives` with new TTY-as-summary shapes, JSON envelopes (`{ total, findings }`, `{ ... + applied }`), and orthogonal `--out` / `--format` streams; the `legacy.rs` module itself goes away with the last renderer. (2) A new document mutation surface lands — `vault get` (renamed from `vault show`), `vault move`, and `vault delete` — replacing `vault repair links` and giving operators a CRUD-shaped surface for working with vault documents without touching the filesystem directly. Both `move` and `delete` are safe-by-default (TTY interactive confirm; non-TTY implicit dry-run; `--yes` to mutate; `--dry-run` to preview-and-exit; `--format json` is non-interactive). Along the way: `vault files` retired (orphan command with no operator use case), `repair_plan_schema_version` bumps twice (5 → 6 for SkipReason taxonomy + plan/apply orthogonality; 6 → 7 for the `force` field on `PlannedChange`), and the long-running pre-port `output::legacy` arc closes. 812 → 865 tests.

### Breaking changes

- **`vault repair links` removed.** Move/delete impact analysis now lives in `vault move <SRC> <DST> --dry-run` and `vault delete <DOC> --dry-run`. Global broken-link/ambiguous-link enumeration is in `vault validate --code 'link-*'` (since v0.30). Duplicate-stem and path-style-Markdown-link reports retired with no replacement.
- **`vault show` renamed to `vault get`.** No alias; pre-1.0 break. Anchors the CRUD-shaped mutation surface (`get` / `move` / `delete`).
- **`vault files` removed.** No documented user story; "Files" demoted from a first-class graph concept to an internal walker step. Broken attachment references continue to surface via `vault validate`'s `link-target-missing` finding.
- **`vault validate` records output now follows the norn-cli-output spec.** Status headline → severity tally → grouped tallies (`--summary`) or per-finding blocks with fix hints (default). `--format table` is no longer supported; use `--format records` (default on a TTY) or `--format json`/`jsonl`/`paths` for machine consumers. Default piped format is now `jsonl` (validate has no natural `paths` representation).
- **`vault validate --format json` output is now wrapped in `{"total": N, "findings": [...]}`** (matches norn-cli-output §5.3). Consumers reading the old bare-array shape must navigate to `.findings`.
- **`vault repair plan --format jsonl` removed.** Previously broken (emitted the entire envelope as a single line). Use `--format json` for piping to `vault repair apply`. The command now emits an explicit migration error when `--format jsonl` is supplied.
- **`vault repair plan --format table` removed.** The new TTY default is `--format report`: a decision-support summary with counts, skip tally, top affected files, and inline apply guidance. `--format table` rejected with a migration error.
- **`SkipReason` enum extended with fine-grained variants.** The coarse `Unsupported` and `Ambiguous` variants are replaced by `MissingDefault`, `LinkDecisionNeeded`, `NoRuleMatched`, `AliasShadowed`, `GraphDiagnostic`, and `AmbiguousTarget`. JSON consumers see snake_case identifiers in `skip_reason` (`missing_default`, `link_decision_needed`, `no_rule_matched`, `alias_shadowed`, `graph_diagnostic`, `ambiguous_target`, `missing_hash`, `precondition_failed`). A parallel `reason_code` field on each skipped finding carries the kebab-case stable identifier (`missing-default`, ...) — agents typically want `reason_code` since it matches the `--skip-reason` flag input.
- **`SkippedSummary` reshaped** from named per-variant fields (`unsupported`, `ambiguous`, ...) to `{ by_reason: { code: count, ... }, total }`. Zero-count buckets are omitted.

  Migration:
  - `summary.skipped.unsupported` → fans out into one or more of `summary.skipped.by_reason["missing-default"]`, `["link-decision-needed"]`, `["no-rule-matched"]`, `["alias-shadowed"]`, `["graph-diagnostic"]` depending on the finding kind.
  - `summary.skipped.ambiguous` → `summary.skipped.by_reason["ambiguous-target"]`
  - `summary.skipped.missing_hash` → `summary.skipped.by_reason["missing-hash"]`
  - `summary.skipped.precondition_failed` → `summary.skipped.by_reason["precondition-failed"]`
  - Zero-count buckets are omitted from `by_reason` (use `.get("<code>").unwrap_or(&0)` for safe access).
- **`repair_plan_schema_version` bumped 5 → 6** covering all of the above.
- **`vault repair apply --format jsonl` and `--format table` removed.** Use `--format report` (TTY summary) or `--format json` (full envelope). Both retired values are rejected with explicit migration messages.
- **`vault repair plan --out` and `--format` are now orthogonal streams.** Previously `--out` short-circuited stdout entirely (any `--format` was silently ignored when `--out` was set). New behavior: `--out` always writes JSON to the file; `--format` independently governs stdout. When only `--out` is set, stdout stays silent (matches the prior default). When both are set, both streams are honored.
- **`vault repair apply <PLAN>` positional is now optional.** When absent or `-`, the plan is read from stdin. Enables the pipeline form `vault repair plan --format json | vault repair apply`.
- **`repair_plan_schema_version` bumped 6 → 7** (additive). New `force: bool` field on `PlannedChange` carries the `vault move --force` semantic through the orchestrator. Existing v6 plans still apply correctly — `force` defaults to `false` and skips serialization when false.

### Added

- `vault move <SRC> <DST>` — rename/move a document with cascading backlink rewrites. Safe-by-default: interactive confirm in TTY, `--yes` to skip the prompt and apply, `--dry-run` to preview and exit, `--force` to overwrite an existing destination, `--no-link-rewrite` to skip the backlink cascade. Output is a thin `MoveReport` (records-shaped TTY by default; `--format json` emits the structured envelope and is implicitly non-interactive).
- `vault delete <DOC>` — delete a document. Refuses if incoming links exist unless `--allow-broken-links` (leaves them broken; surfaced via `vault validate`) or `--rewrite-to <ALT_DOC>` (redirects backlinks to an alternate doc). Same safe-by-default apply model as `vault move`.
- New `cli::ValidateFormat { Records, Json, Jsonl, Paths }`; default honors `isatty` (Records on TTY, Jsonl piped).
- `vault repair plan --skip-reason <PATTERN>` filter narrows `skipped_findings` by stable reason code (`missing-default`, `link-decision-needed`, `no-rule-matched`, `alias-shadowed`, `graph-diagnostic`, `ambiguous-target`, `missing-hash`, `precondition-failed`). Glob patterns accepted (`'link-*'`). Does NOT narrow planned changes — skip-reason is a skip-tier filter only.
- `vault repair plan --format paths` emits affected document paths (one per line, sorted, deduplicated) for `xargs`-style pipelines. Respects all filters.
- `vault repair plan --format report` (new TTY default) produces a decision-support summary with counts, confidence breakdown, skipped tally, top-5 affected files, and filter-aware apply guidance.
- `reason_code` field on each `skipped_findings[]` entry in JSON output, derived from `SkipReason::code()`.
- `skip_reason` array in JSON `source_filters` echoes the operator's `--skip-reason` input.
- `vault repair apply --out <PATH>` — write the JSON apply report to file.
- `vault repair apply` reads the plan from stdin when no positional or `-` is given. Pipeline form: `vault repair plan --format json | vault repair apply` is now supported.
- `vault repair apply --format report` (new TTY default) — human summary composed from `output::primitives`.
- `vault repair apply --format paths` — sorted dedup of changed files, one per line.

### Changed

- `vault validate --format paths` continues to emit unique sorted paths of documents that have findings.
- `vault repair plan` piped default flips from JSON-via-default-clap-value to TTY-detected: report when stdout is a terminal, JSON when piped.
- `vault repair apply` TTY output reshaped from key-value metric table to summary (count line + severity tally + by-operation tally + optional warnings sub-block + footer). Same data, new shape.

### Fixed

- `NO_COLOR` now correctly overrides `--color always` per [no-color.org](https://no-color.org/). Previously, an explicit `--color always` would still emit ANSI even when `NO_COLOR` was set. Affects every command using the shared palette (`vault find`, `vault config show`, `vault show`, `vault validate`).

### Removed (internal)

- `crates/vault-cli/src/link_repair.rs` deleted. `LinkRepairReport` and `plan_link_repairs` went with `vault repair links`.
- `crates/vault-cli/src/output/legacy.rs` module deleted. The long-running pre-port cleanup arc (started v0.27) is now closed. `is_broken_pipe` relocated to `output::primitives`.

## v0.31.0 - 2026-05-23

The link-health release. Three coordinated cuts ship together: the docs/links namespace cleanup retires `vault docs` and `vault links` in favor of `vault count`, `vault show`, and `vault validate --code 'link-*'`; alias-aware wikilink resolution lets a configured frontmatter field serve as a fallback link target; and the Gap 1 + Gap 4 cut splits `link-unresolved` into three specific codes (`link-target-missing`, `link-anchor-missing`, `link-block-missing`), adds glob matching to `--code`, and gives `vault repair plan` the ability to propose closest-match rewrites for broken targets with a confidence band carried in a new `footnotes` layer that `repair apply` ignores entirely. `vault repair apply` learns a new `rewrite_link` operation that preserves display text, anchors, and block-ref suffixes. Plan schema bumps to v5 (additive). On top of all this, `--help` output gets its own overhaul: a custom renderer with canned EXAMPLES, vault-derived LIVE EXAMPLES, conceptual prose sections, and pagination via `$PAGER`; `-h` stays a one-screen orientation summary.

### Breaking changes

- **`vault docs` namespace removed** (`docs summary`, `docs inspect`). Replaced by `vault count` and `vault show`.
- **`vault links` namespace removed** (`links backlinks`, `links unresolved`; `links list` was removed in v0.29). Use `vault show <doc> --col incoming_links` and `vault validate --code 'link-*'`.
- `link-unresolved` finding code retired. Replaced by `link-target-missing`,
  `link-anchor-missing`, and `link-block-missing` — each maps 1:1 to the
  existing `unresolved_reason` taxonomy and is now filterable via `--code`
  directly. Scripts filtering on `--code link-unresolved` should migrate to
  `--code 'link-*'` (now globbable) or list the three codes explicitly.

### Changed

- Unresolved-link inventory now respects `.vault/config.yaml`'s `validate.ignore` patterns by default. The old `vault links unresolved` walked every indexed document; the migration path `vault validate --code 'link-*'` respects the same ignore config that `vault validate` already honored. Vaults with ignored paths will see fewer finding rows than before.
- **BREAKING:** `vault --help` and `vault -h` (and the same flags on every subcommand) now render through a custom layout instead of clap's default. Two forms with different jobs: `-h` is a one-screen orientation summary; `--help` is the deep reference with hanging-indent flag prose and pagination via `$PAGER`. Pager mirrors `vault find` (`less -FRX` default, honored `$PAGER`, TTY+height gate). Set `PAGER=cat` or pipe through `cat` to bypass. `GLOBAL OPTIONS` is shown in full on every subcommand. Phase 1 ships the structural skeleton; canned examples, live examples, and conceptual sections layer on in later phases.
- Cache identity now tracks `links.alias_field`. Changing the config (enabling, disabling, or renaming the field) triggers a silent cache rebuild on the next vault-cli invocation.

### Added

- `--code` filter accepts glob patterns: `--code 'link-*'` matches the four
  link finding codes; `--code 'frontmatter-alias-*'` matches the alias family.
  Exact-string matching unchanged when the value contains no glob meta.
- **`vault repair plan` proposes closest-match link rewrites** for
  `link-target-missing` findings. Slug-normalize identity (case, whitespace,
  hyphen-vs-underscore variants) emits `high`-confidence proposals; small
  residual edit distance (Levenshtein ratio ≥ 0.7) emits `medium`-confidence
  proposals. Multi-candidate ties skip with `SkipReason::Ambiguous` and
  populate the candidate list — the algorithm surfaces ambiguity rather than
  guessing through it. Atlas dogfood: 559 findings → 100 high + 24 medium
  proposals, 1 legitimate tie, 434 unsupported.
- **`footnotes` array on repair plan output.** Read-only commentary that
  carries per-change confidence + structured details (`original_target`,
  `normalized_target`, `candidate_stem`, `normalized_distance`,
  `slug_normalized_identity`). `repair apply` ignores footnotes entirely —
  the plan stays a purely executable contract. LLM/operator consumers read
  footnotes to reason about which proposals to trust.
- **`vault repair plan --confidence high`** filters the plan to only
  high-confidence proposals (drops medium proposals and their footnotes).
  Default emits all bands per the dump-everything principle.
- **`vault repair apply` learns the `rewrite_link` operation.** Mutates
  matching wikilinks in source docs; preserves display text (`[[X|label]]`),
  anchor (`[[X#section]]`), and block-ref (`[[X^block-id]]`) suffixes. All
  matching occurrences in the source are rewritten. Hash check enforced
  before write; `--dry-run` honors the check but skips the write. Known
  limitation: the rewrite parser does not skip code-fenced content; if the
  same target appears both in prose and inside `\`\`\` ... \`\`\``, both will
  be rewritten.

### Changed

- **`repair-plan-schema` bumped from `4` to `5`.** Additive: `footnotes`
  array on `RepairPlan`, `change_id` field on each `PlannedChange`
  (deterministic SHA-256 of path + finding-code + expected-old-value +
  occurrence-index, first 16 hex chars). `repair apply` rejects plans with
  `schema_version != 5` — re-run `repair plan` to regenerate.
- **`vault count`** — native grouped counting. `--by <field>` groups by a frontmatter field; without it, emits total only. Shares the full filter flag surface with `vault find` (`--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`). Replaces `vault docs summary --count-by`.
- **`vault show <doc>...`** — unified single-doc detail. Accepts vault-relative paths, case-insensitive stems, and wikilink-shaped inputs (brackets stripped before resolution). Default fields: path, frontmatter, headings, outgoing_links, unresolved_links, incoming_links. `--body` adds content; `--col` narrows; multi-target supported. Replaces `vault docs inspect` and `vault links backlinks`.
- vault-cli: `--help` now includes canned EXAMPLES on most commands. Examples are hand-authored, vault-independent, and concentrated on multi-shape commands (`find`, `validate`, `repair plan`, top-level `vault`) where the flag block alone leaves invocation patterns unclear. `-h` short form unchanged.
- `vault find --help` now emits a `LIVE EXAMPLES` block with a real, runnable
  query generated from your vault's cached index. The block appears only when
  a vault is in scope and the cache loads, and only when your vault has at
  least one enum-like frontmatter field (small set of values, ≥ 3 docs at the
  top value, ≥ 10% field coverage). Outside a vault, on `-h`, or in vaults
  without enum-shaped frontmatter, the block is omitted silently.
- `vault validate --help`, `vault repair plan --help`, and `vault repair apply --help` now carry conceptual prose sections explaining how each workflow fits together. `HOW VALIDATION WORKS` covers finding shapes, severity, exit codes, and how triage filters compose. `THE PLAN/APPLY BOUNDARY` walks through what plan emits vs. what apply consumes, with sample JSON for a planned change and a skipped finding. `HOW APPLY WRITES` numbers the precondition checks and write sequence apply runs. Conceptual sections render only on `--help`, positioned after EXAMPLES and LIVE EXAMPLES. Commands without workflow-shaped operation skip the section silently.
- **Alias-aware wikilink resolution.** Opt-in via `.vault/config.yaml`'s new `links.alias_field` key (typically `aliases`). When set, wikilinks that don't resolve via filename stem fall back to matching alias values from the named frontmatter field. Fallback-only — stem resolution always wins, so enabling never turns previously-resolved links into unresolved or ambiguous. Alias values are case-insensitive and tolerate any YAML scalar (string, number, boolean).
- **`vault show` addressing accepts aliases.** When `links.alias_field` is configured, `vault show "[[Some Alias]]"` (or bare `vault show "some alias"`) resolves to the doc whose alias matches, after stem resolution fails.
- Three new validate finding codes for alias frontmatter drift: `frontmatter-alias-shadowed-by-stem` (an alias matches another doc's stem in fallback resolution), `frontmatter-alias-duplicate-across-docs` (two or more docs claim the same alias), `frontmatter-alias-malformed` (the alias field contains a non-scalar map or nested sequence value). All three are warnings, fire only when `links.alias_field` is configured, and respect `validate.ignore` patterns.
- `vault validate --help` gains a `FINDING CODES` section listing all ten codes with one-line explanations.
- `vault init` scaffold now emits a commented `links.alias_field` hint block explaining the opt-in feature.

### Notes

- **Internal dependency bumps** (no user-visible behavior change): `thiserror` 1.0 → 2.0, `pulldown-cmark` 0.10 → 0.13, `clap_mangen` 0.2 → 0.3. All three exercised against the full 713-test workspace before merge.
- **Release CI:** `cargo-dist` upgraded 0.30.2 → 0.32.0, regenerating `release.yml` with Node.js 24-ready GitHub Action versions (`actions/checkout@v6`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`). Addresses the 2026-06-02 deprecation deadline for Node.js 20 actions and the `attest-build-provenance@v3` → `attest@v4` migration.

## v0.29.0 - 2026-05-20

A foundation release. Three large arcs land together: (1) the SQLite cache becomes the read path for query commands; (2) a new `vault find` consolidates search and metadata filtering into a single composable command, retiring `vault search` and `vault docs query`; (3) the `vault init` + `vault config` cluster bootstraps and inspects per-vault configuration. Layered on top: a shared `output/` primitives module that implements the new CLI output spec — bone-bold record headers, dim-gray labels via ANSI 256 instead of SGR 2, cell-shaped value wrapping that force-breaks long unbreakable tokens, count lines that lead query output, severity tallies with fix-hint blocks. The `--eq`, `--not-eq`, `--in`, `--not-in` predicates are now array-aware and bracket-tolerant for string values, so `vault find --eq workspace:vault-cli` matches both scalar `"[[vault-cli]]"` and `["[[vault-cli]]"]` shapes without users escaping brackets or knowing the field's underlying type.

Pre-release-shaped breaking changes: `vault search`, `vault docs query`, `vault registry`, and `--vault NAME` are removed (use `vault find` and `--cwd PATH`); `vault find` requires a predicate or the new `--all` flag (a bare invocation prints help); JSON wrapper key is now `documents` not `matches`; config commands use `ConfigFormat` (Records/Json/Jsonl) — `--format table` is gone. No migration shims; vault-cli is still pre-1.0.

### Breaking changes

- **`vault search` removed.** Replaced by `vault find`. No alias. Scripts and skills that called `vault search --text X` should call `vault find --text X`. Pre-release; no migration shim.
- **`vault docs query` (the `docs query` List subcommand) removed.** Replaced by `vault find`. `vault docs summary` and `vault docs inspect` are unaffected.
- **`vault registry` removed.** The `add`, `list`, and `remove` subcommands are gone. Vault targeting now uses `--cwd PATH` exclusively.
- **`--vault NAME` global flag removed.** Use `--cwd PATH`.
- **`vault find --format json` wrapper key renamed `matches` → `documents`.** Now `{ total, returned, starts_at, documents }`. The `truncated` and `sort` keys are removed (derivable from `returned < total`; `sort` echoed the request). Per the CLI output spec §5.3.
- **`vault config show | validate --format table` removed.** Replaced by `--format records` (the new TTY default). `ConfigFormat` (Records/Json/Jsonl) replaces `OutputFormat` for the config family; the `paths` variant is also gone (config commands have no path-list shape).
- **`vault find` records output reshaped.** Count line leads (`23 documents · showing 1–10`); field rows are 2-indent with thread-highlighted sort field; separator between records is `─` × min(term, 60) with no blank-line padding; trailing `… X of Y` footer removed. Scripts parsing records output were not expected to work before (records is not a stable contract per spec); only flagged as breaking out of caution.
- **`vault find` with no predicate now prints help and exits 2.** A bare `vault find` previously dumped the entire vault; that's almost always a mistake. Pass at least one of `--text`, `--eq`, `--not-eq`, `--in`, `--not-in`, `--has`, `--missing`, `--before`, `--after`, `--on`, `--path`, or the new `--all` escape hatch to opt into a full-vault dump.

### Added

- **Shared `output/` primitives module** for CLI rendering — `palette` (brand-token Style consts honoring `--color`/`NO_COLOR`/`CLICOLOR_FORCE`), `glyphs` (Glyph enum with UTF + ASCII fallback via `NORN_ASCII` env), and `primitives` (composable `status_headline`, `count_line`, `severity_tally`, `record_block`, `separator`, `note_line`). Future command redesigns compose from these instead of hand-rolling renderers.
- **Global `--color {auto|always|never}` flag** on `vault` (was per-command on `find` only). Honors `NO_COLOR` and `CLICOLOR_FORCE` everywhere.
- **`vault config validate` records output** now matches the per-finding shape from the CLI output spec §6.1: status headline (`validating .vault/config.yaml…`), severity tally (collapses to single `✓ … pass` when clean), per-finding blocks with 2-indent path / 4-indent message / optional 4-indent `fix:` hint. Hardcoded fix hints for `config-parse-error` and `unknown-schema-version`.
- **`vault find --not-eq FIELD:VALUE`** — inequality predicate, the negation of `--eq`. Same array-aware + bracket-tolerant handling for string values: matches scalar fields where the value differs and array fields where no element matches.
- **`vault find --all`** — escape hatch for the no-predicate gate. Returns every document; required when no other predicate is set.
- **Empty-record placeholders in `vault find` records output.** A document with no frontmatter renders as `  (no frontmatter)` (dim) under its path header; with `--col` active but no matching fields, the line reads `  (no matching fields)`. Previously these records collapsed to just a path + separator with no visible body.
- **`vault find`** — unified find/search command. Consolidates full-text search + metadata filtering with sort, limit, paging, column selection, and four output formats. Replaces `vault search` and `vault docs query`. Operators: `--text "needle"` (case-insensitive substring); `--eq field:value`, `--in field:v1,v2,...`, `--not-in field:v1,v2,...`, `--has field`, `--missing field`; `--before/--after/--on field:date` (ISO 8601; `today` keyword); `--path GLOB`. Composition: ALL-of across flags; ANY-of inside `--in`. `--sort field [--desc]`. `--limit N` default 10, `--no-limit` for everything. `--starts-at N` (1-indexed) for paging. `--col field,field` narrows visible frontmatter fields. `--format paths|records|json|jsonl`; auto-detects TTY (records) vs. piped (paths). `--color always|auto|never`; honors `NO_COLOR` and `CLICOLOR_FORCE`. Records mode pipes through `$PAGER` (default `less -FRX`) on TTY when output exceeds screen height; `--no-pager` bypass.
- **`vault init`** scaffolds `.vault/config.yaml` with empty stubs for all top-level keys, pre-filled common ignores (`.obsidian/`, `.git/`, `.trash/`, `node_modules/`), and observed-frontmatter-field hints from a vault scan. Refuses if config exists (`--force` overwrites). Cache lives at `~/.cache/vault/<hash>/cache.db` (not inside the vault), so no `.gitignore` is written.
- **`vault config show`** prints discovery paths + section counts as a single records doc (TTY default) or flat JSON object (pipe default). Inherits `vault find`'s display contract: anstyle colors, `$PAGER` auto-pager for records on overflow, `--no-pager` bypass. No `--col` flag (`show` is a bearings-check command, not a high-traffic query). Errors with exit 1 + `vault init` hint when no config is discovered.
- **`vault config validate`** parses the config file and emits findings shaped like `vault validate`'s output (`{code, severity, path, message}`). Recognises `config-parse-error` (parse failures, including the deprecated `graph:` key) and `unknown-schema-version` (version != current). Exit codes 0 clean / 1 warning / 2 error / 3 unreadable.
- **`vault config migrate`** reserves the verb for future schema migrations. In v1 with schema v1 it prints `"Config is on schema v1 (current). Nothing to migrate."` and exits 0.
- **`vault config edit`** opens the discovered config file in `$VISUAL` (then `$EDITOR`); auto-runs `vault config validate` on save. `--no-validate` skips the post-edit check. Validate's exit code takes precedence over editor's 0 (errors matter more than a successful editor exit). Editor failure short-circuits (validate not run).
- **Cache query API extensions** (in `vault_cache`): `DocumentQuery` gains `frontmatter_in`, `frontmatter_not_in`, `date_before`, `date_after`, `date_on`, `body_text_contains` fields. New types `FindQuery`, `SortClause`, `SortDirection`, `FindResult`. New method `Cache::find_documents(&FindQuery) -> Result<FindResult, CacheError>` with ORDER BY / LIMIT / OFFSET / COUNT support and accurate total-count signaling.
- **SQLite cache as the read path for query commands** (v1). Reintroduces a cache surface that was removed in v0.26.0 — this time, query commands actually consume it. Closes the "every command does a full filesystem rescan" performance gap.
  - Cache lives at `~/.cache/vault/<sha256(canonical-vault-root)>/cache.db` (honors `$XDG_CACHE_HOME`). Directory is `0700`, database file is `0600`. Identity hash uses the canonical vault-root path so symlinked roots, `--vault registry-name`, and direct cwd-discovery all resolve to the same cache.
  - New `vault cache` subcommand: `index` (incremental, default), `rebuild` (full from scratch), `clear` (delete the cache file), `status` (path, size, doc/file/link counts, schema version, last full rebuild). `index --rebuild` and `index --force-hash` flags; `status --format json|text`. New global flag `--no-cache-refresh` skips the implicit refresh before query commands.
  - Query commands (`validate`, `docs`, `files`, `links`, `repair plan/apply`, `search`) now load from the cache via a shared helper. The cache is refreshed transparently before each command unless `--no-cache-refresh` is set. Lock contention during refresh downgrades to a stderr notice and reads stale; never errors.
  - Cache schema starts at `v2` (bumped during implementation to persist `Link.unresolved_reason`, `Link.candidates`, and a new `diagnostics` table; the v1 → v2 jump was internal and never released).
  - Self-heal triggers on `Cache::open`: missing file, schema older than the binary, identity drift (vault root path changed), and SQLite corruption (failed `PRAGMA integrity_check`) all auto-rebuild silently with a one-line stderr message. Only a *newer*-than-known schema hard-errors (interpreting unknown future fields is the destructive risk).
  - Incremental updates use a `(mtime, size)` cheap-check then blake3 hash-verify on mismatch. `--force-hash` skips the cheap-check for filesystems where mtime is unreliable (NFS, Docker bind-mounts on macOS, rsync-restored vaults, post-`git-restore-mtime` workflows).
  - Aggressive invalidation: any change to a file drops that doc's rows + every incoming link targeting it; added files re-resolve every link whose target could now match.
  - Concurrency: SQLite WAL mode for parallel reads; advisory `flock(2)` write lock at `<cache_dir>/.lock` with a 5-second timeout for `cache index|rebuild|clear`. Read commands never block.
  - Performance: cold rebuild on a 1k-doc fixture clocks under 200 ms in our perf regression test (target was < 2s); warm reads on an unchanged vault stay near steady-state. The `vault-cache` crate ships its own property test (`incremental == from-scratch` for random op sequences) and a `#[ignore]`-gated perf gate.
  - New `vault-cache` workspace crate. SQL-direct queries and FTS5 are designed-for but not implemented; tracked separately as v3 (command migrations) and v4 (FTS5) in the atlas vault.
  - `body_text` field added to `vault_core::Document`, populated during parse. Pre-positions FTS5 to land later without a full vault re-scan.
- New `--no-cache-refresh` global flag.
- **Cache query API foundations** (v2 — internal plumbing for the upcoming v3 command migrations). New typed methods on `vault_cache::Cache` that query the persisted schema directly instead of reconstructing a full `GraphIndex`:
  - `documents_matching(&DocumentQuery) -> Vec<DocumentSummary>` — SQL-narrowed scan. Frontmatter predicates push into SQL via `json_extract` with the JSON path bound as a parameter (closes the SQL-injection vector and supports any character in frontmatter keys, including hyphens and dots). Path globs apply via a Rust post-pass using the same `vault_graph::pattern_matches_path` matcher v1's `filter_documents` uses.
  - `document_by_path(&Utf8Path) -> Option<Document>` — single-doc fetch with full joined data (headings, block_ids, links, diagnostics). For `docs inspect`-style use.
  - `files() -> Vec<VaultFile>` — non-markdown inventory.
  - `links() / links_unresolved() / backlinks_to(&Utf8Path) -> Vec<Link>` — link queries; all share a private row-decoder.
  - `diagnostics() -> Vec<(Utf8PathBuf, Diagnostic)>` — per-doc diagnostic rows.
  - `has_diagnostic_errors() -> bool` — single `SELECT EXISTS` primitive for exit-code derivation on cache-direct command paths.
- `vault_core::DocumentSummary` — lean projection type (`path`, `stem`, `hash`, `frontmatter`, `body_text`) returned by `documents_matching`. `Document` minus the joined tables. Implements `From<Document>` and `From<&Document>` for use by helpers that don't need the joined data.
- `vault_standards::validate_rule(&ValidateRule, &[DocumentSummary]) -> Vec<Finding>` — per-rule validate entrypoint that accepts a pre-narrowed scope. The existing `validate(&GraphIndex, &ValidateConfig)` keeps working unchanged.

### Changed

- **`vault config show` records output** uses the new `record_block` primitive: config file path becomes the record header; remaining keys are 2-indent fields. No grid/box-drawing.
- **`vault init` output** uses voice-spec lowercase, em-dash separator, proper singular/plural in observation counts, and a `tip:` note line for the next-step suggestion (replaces the old `next:` prefix).
- **`vault config migrate`** no-op message uses voice-spec lowercase + em-dash: `config is on schema v1 — nothing to migrate`.
- **`vault find` stderr warnings** use voice-spec severity prefix: `warning:` for `--col` mismatches; `note:` for paths/jsonl truncation signals. Field names in backticks.
- **`--eq`, `--not-eq`, `--in`, `--not-in` are array-aware and bracket-tolerant for string values.** `--eq workspace:vault-cli` matches both scalar `workspace: "[[vault-cli]]"` and scalar `workspace: vault-cli`. `--eq source_notes:seed` matches any element of an array field like `source_notes: ["[[seed]]", "[[other]]"]`. Both sides have Obsidian `[[…]]` wrappers stripped at query time, so users never have to escape brackets in the shell or know whether the underlying field is a scalar or array. Non-string predicates (bool/number) keep their original scalar SQL — typed comparisons are unambiguous and don't benefit from the array/strip handling.
- **`dim` palette token ships as ANSI 256 color #244** (`#808080` medium gray) instead of SGR 2 ("faint"). Several terminals (macOS Terminal.app default profile, some tmux configs) silently ignore SGR 2 and render dim text identically to the terminal default, defeating the label-vs-value visual distinction. The explicit gray renders consistently across every 256-color terminal.
- **Record-block header rendered bone-bold** per the CLI output spec §4.3 (was plain bone, relying on terminal default fg which doesn't always read as bone-white).
- **`record_block` force-breaks long unbreakable values** (UUIDs, paths without spaces, URLs) at the value-column boundary. Previously such values overflowed the terminal width and got soft-wrapped to column 0, visually spilling into the key column on the next line.
- **`vault config show` and `vault config validate` emit a leading blank line** for breathing room from the shell prompt. `vault find` does not — its count line acts as the visual separator and a leading blank doubled the spacing.
- Query commands no longer rebuild the in-memory `GraphIndex` from a full filesystem scan on every invocation. They open the cache, optionally refresh it, and reconstruct `GraphIndex` from rows. Existing query logic is unchanged — only the source of the index moved from "filesystem walk" to "cache read".
- Content hashing in the cache uses blake3 throughout (matches `vault_graph`'s existing hash; the implementation plan's SHA-256 specification was corrected to blake3 during execution to avoid mismatches with parser-emitted hashes).
- `vault_cli::filter::DocumentSummary` renamed to `DocsSummaryReport`. The renamed struct is the aggregation report from `summarize_documents` — the new name reflects what it actually is, and frees the `DocumentSummary` name for the new `vault_core` projection. Internal rename; no command behavior change.
- **`VaultConfig` schema gains a `version: u32` field** defaulting to `1`. Existing configs without an explicit `version:` continue to parse and are treated as v1. `vault_cache::cache_dir_for` is now re-exported at the `vault-cache` crate root (was previously under `vault_cache::identity`).

### Performance

- Atlas dogfood (780 docs, 2673 links): `vault find --eq type:note` < 10ms wall; `vault find --text "typescript" --limit 10` < 10ms wall; `vault find --eq type:note --sort created --desc --limit 10` < 10ms wall. Cache rebuild 198ms. Well under the 50ms spec target. `EXPLAIN QUERY PLAN` confirms `find_documents` runs as a single SCAN/SEARCH against the `documents` table.

### Internal

- v1 text-substring implementation uses `LOWER(body_text) LIKE '%...%'`. FTS5 native search is tracked as a separate task and will swap the SQL clause without changing `DocumentQuery::body_text_contains` or the `--text` CLI surface.
- Nested frontmatter keys (`schema.version` as a dotted JSON path) are not supported in v1 of find. Tracked as a backlog task; v3 surface treats all `--col` / `--eq` / etc. field arguments as flat keys.
- New workspace dependencies: `terminal_size 0.4` (TTY column-width detection), `chrono 0.4` (date keyword resolution for `--on today`). `anstyle 1` added as a direct dep (was transitive via clap).
- `find/` module split by concern: `query.rs` (args → FindQuery), `render.rs` (paths/records/json/jsonl renderers + records wrapping), `pager.rs` (subprocess), `color.rs` (anstyle palette).

### Notes

- No version bump for this work. The cache is at v1 + v2 (foundations); we'll dogfood on `main` and bundle the version bump with v3 (query command migrations) and v4 (FTS5 native search) once that work lands.
- v2's planned command migrations (`docs query`, `docs summary`, `links list`, `search`, `validate`, `repair plan`) are **deferred to v3** pending a command-by-command user-story re-evaluation. Some commands may be redesigned, consolidated, or removed rather than directly migrated. The cache query API above is the foundation v3 will build on.
- Operator documentation: `docs/cache.md`.
- Follow-ups already tracked in the atlas vault: `cache-telemetry-and-doctor` (observability for slow-query debugging), `repair-plan-schema-self-heal` (apply the same self-heal posture to repair-plan v3→v4), `self-heal-audit` (broader CLI surface review).

## v0.28.0 - 2026-05-18

Closes the validate → plan → apply → verify loop for `frontmatter-required-field-missing` and `document-misrouted` findings by adding two new repair actions. Bumps the repair plan JSON schema to v4.

### Breaking changes

- **Repair plan JSON schema bumps from v3 to v4.** `vault repair apply` rejects v3 plans with `unsupported repair plan schema version: expected 4, got 3`. No migration shim. Regenerate any persisted plans with `vault repair plan` against v0.28.0+.
- **`PlannedChange` adds new top-level fields** (`destination`, `link_risk`, `warnings`). The `field` field changes from `String` to `Option<String>` (`None` for `move_document` changes). Any tooling consuming the plan JSON must handle the new fields (or ignore them) and the optional field shape; strict-schema consumers will need updates.
- **`vault repair apply`'s default blast radius is wider when a plan contains `move_document` actions.** Apply now writes to every file containing a classifiable backlink to a moved file, not just the moved file itself. Agents and scripts that assumed `repair apply` only touches files named in `changes[].path` should now also expect writes to the files named in each move change's `link_risk.*.source_path` entries. The apply output's `moved_files` and `rewritten_links` reflect all written paths.

### Added

- `add_frontmatter` repair action — inserts a missing frontmatter field with a literal value. Refuses if the field already exists (use `set_frontmatter` for replacement). Same minimal-edit YAML preservation as set/remove.
- `move_document` repair action — moves or renames a file. Accepts `to_directory` (sugar: file moves into the directory, filename preserved) or `to_path` (full destination including filename for rename or move+rename). Both support `{stem}`, `{filename}`, `{frontmatter.<field>}` placeholder substitution.
- Automatic backlink rewriting on move. `vault repair apply` rewrites all classifiable backlinks (path-qualified wikilinks, Markdown links, and stem-only wikilinks when the stem changes) alongside the move. No flag required.
- `PlanWarning::StemCollisionAfterMove` — informational warning attached to a planned move when the new stem already exists elsewhere in the vault. Non-blocking; reported in both plan and apply output.
- `vault repair links --move-to <path>` — read-only analysis showing the link risk and warnings a proposed move would produce, without authoring a repair rule.
- New `ApplyError` variants: `FieldAlreadyPresent`, `MoveDestinationExists`, `MoveSourceMissing`, `MoveSourceIsSymlink`.

### Changed

- The validate → plan → apply → verify loop now closes for `frontmatter-required-field-missing` and `document-misrouted` findings when a matching repair rule supplies the deterministic detail.
- Substitution failures (missing frontmatter field, non-scalar value) skip the finding with `skip_reason: precondition_failed` and a specific reason string surfaced in the plan's `skipped_findings`.

### Known limitations

- When a backlinking file contains multiple identical link occurrences pointing at a moved file, `repair apply` rewrites only the first occurrence. Subsequent occurrences will flag as unresolved on the next `vault validate`. To be addressed in a follow-up by adopting byte-span-precise edits.

## v0.27.0 - 2026-05-18

Shell completion install UX.

### Added

- `vault completions install [shell]` writes the right shell-integration line (or script file, for fish and nushell) to the user's shell config. Auto-detects from `$SHELL` if no shell argument given. Idempotent via a marker comment block; `--force` replaces an existing install; `--print` previews without writing. Supported shells: bash, zsh, fish, powershell, elvish, nushell.
- `vault completions init <shell>` emits the completion script to stdout (was `vault completions <shell>`).
- `clap_complete_nushell` dependency for nushell completion script generation. `build.rs` now also emits `target/completions/vault.nu` alongside the bash/zsh/fish scripts.

### Changed

- The hidden `vault completions <shell>` subcommand has been replaced by the visible `vault completions init <shell>` subcommand. Scripts or agent skills that invoked the old form must update.
- The `completions` command group is now visible in `vault --help` (was hidden in v0.26.x).
- `manpage` remains hidden in `vault --help`. (A future `vault manpage install` task is tracked separately.)

### Removed

- The hidden `vault completions <SHELL>` subcommand syntax (use `vault completions init <SHELL>`).

## v0.26.2 - 2026-05-18

Release-pipeline patch on top of v0.26.1. The v0.26.1 tag was pushed but the release workflow failed before publishing artifacts; this version supersedes it. No user-facing CLI behavior changes.

### Fixed

- `build.rs` now generates `target/completions/{vault.bash,_vault,vault.fish}` and `target/man/vault.1` as a side effect of every `cargo build`. Previously the cargo-dist release workflow listed those paths in `dist-workspace.toml`'s `include` directive but never invoked the `just completions` / `just manpage` recipes that produced them, so packaging failed. The hidden `vault completions <shell>` and `vault manpage` subcommands are unchanged; the Justfile recipes remain as convenience wrappers.
- Dropped `rust-version = "1.95"` from `crates/vault-cli/Cargo.toml`. Cargo-dist's `aarch64-unknown-linux-musl` builder ships rustc 1.93.1 and rejected the build with "rustc 1.93.1 is not supported". vault-cli does not yet publish to crates.io, so the field was informational only. CI continues to track latest stable via `dtolnay/rust-toolchain@stable` and `mise.toml`. The MSRV policy in `docs/development.md` is updated to explain the omission.

### Changed

- `InspectOutput` moved from `crates/vault-cli/src/cli.rs` to `crates/vault-cli/src/target.rs`. This keeps `cli.rs` free of intra-crate dependencies so `build.rs` can include it via `#[path = "src/cli.rs"]` without pulling in the full graph crates as build-dependencies. Internal refactor only; no public surface change.

## v0.26.1 - 2026-05-18

First installer-backed release. GitHub-readiness work: standard repo files, public Cargo metadata, README/docs reorganization, generic agent skill template, CI workflow with quality gates, shell completions / man page, and hosted shell installer via cargo-dist. No code behavior changes beyond the new hidden `completions` and `manpage` subcommands.

### Added

- `LICENSE` (MIT, 2026 Drew Butler).
- `CONTRIBUTING.md`, `SECURITY.md` (security reports to `hi@dbtlr.com`).
- `.github/dependabot.yml` (weekly Cargo + GitHub Actions updates), `.github/ISSUE_TEMPLATE/bug.md` and `feature.md`, `.github/PULL_REQUEST_TEMPLATE.md`, `.github/CODEOWNERS`.
- Public package metadata on `crates/vault-cli/Cargo.toml`: `authors`, `description`, `repository`, `homepage`, `readme`, `categories`, `keywords`, `rust-version = "1.95"` (latest stable policy).
- Concise `README.md` landing page; dense reference material moved into focused `docs/` pages (`installation.md`, `quickstart.md`, `concepts.md`, `commands.md`, `configuration.md`, `validation.md`, `agent-workflows.md`, `development.md`, `releases.md`). `AGENTS.md` slimmed to a ~30-line agent contract pointing at `docs/agent-workflows.md`.
- `integrations/agent-skill/SKILL.md` — single harness-independent agent skill. `integrations/agent-skill/README.md` documents the two install paths: `.claude/skills/` for Claude Code, `.agents/skills/` for every other coding agent (Codex, Open Code, OpenClaw, Hermes, PI).
- `examples/config-minimal.yaml`, `examples/config-typed-notes.yaml`, `examples/repair-recipe.sh` (executable), `examples/README.md` — generic Markdown vault content.
- `.github/workflows/ci.yml` — matrix CI on ubuntu-latest and macos-latest. Runs `cargo fmt --check`, `cargo test --workspace --locked`, `cargo build -p vault-cli --release --locked`, `cargo install --path crates/vault-cli --locked`, `vault --help`, fixture validation, repo self-validation, `cargo audit`, `cargo deny check`, and `shellcheck examples/repair-recipe.sh`. Triggers: push to main, pull_request, weekly cron.
- `deny.toml` — conservative supply-chain policy. Permissive license allow list (MIT, Apache-2.0, BSD-2/3-Clause, ISC, Unicode-DFS-2016, Unicode-3.0, Zlib); denies yanked advisories, unknown registries, unknown git sources; warns on wildcards and unmaintained crates.
- `.vault/config.yaml` — repo-root dogfood config requiring `title` + `description` frontmatter on `docs/**/*.md`. CI asserts zero findings.
- Hidden `vault completions <shell>` and `vault manpage` subcommands that emit shell completions (bash, zsh, fish) and a roff-formatted man page to stdout. Wired into `Justfile` (`just completions`, `just manpage`) and smoke-tested in CI.
- `dist-workspace.toml` configures cargo-dist 0.30.2 to ship `vault` binaries for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, and `aarch64-unknown-linux-musl`. The hosted shell installer at `https://github.com/dbtlr/vault-cli/releases/latest/download/vault-cli-installer.sh` is the recommended install path.
- Shell completions for bash, zsh, and fish, plus a man page, ship alongside the binary in every release archive (`vault-cli-<target>.tar.xz`).
- `.github/workflows/release.yml` (generated by `cargo dist init`) runs the release pipeline on tag push (`v*.*.*`). Pull requests trigger a dry-run plan that asserts the configuration without uploading artifacts.
- `docs/installation.md` documents both pipe-to-sh and download-then-run install forms plus a verification recipe.
- `Justfile` recipes: `dist-plan`, `dist-build-local`, `release version` (existing `release` recipe renamed to `build-release`).
- Keep a Changelog format header at the top of this file.

### Changed

- `Justfile` — removed the stale `fixture-build-cache` recipe (the `vault cache build` command was removed in v0.26.0).
- `docs/rule-shape.md` — added `title` and `description` frontmatter so the repo self-validation passes against the pre-existing page.
- Root `Cargo.toml` — added `[profile.dist]` (inherits release, `lto = "thin"`) used by cargo-dist's release builds.

## v0.26.0 - 2026-05-18

Structural cleanup release: apply migration to vault-standards, minimal-edit YAML preservation, repair plan schema v3 with SkipReason taxonomy, config schema serde rewrite, SQLite cache deletion, foundation tests across pure-parsing crates, and bundled CLI polish. Four-slice cleanup, four merged branches, 193 tests passing.

### Breaking changes

- **Repair plan JSON schema bumps from v2 to v3.**
  - `RepairPlan` drops the separate `unsupported_findings` and `ambiguous_findings` top-level arrays. Use `skipped_findings` (single canonical list) and filter by `skip_reason` (`unsupported` | `ambiguous` | `missing_hash` | `precondition_failed`).
  - `RepairPlanSummary` drops flat `skipped_findings` / `unsupported_findings` / `ambiguous_findings` count fields. Use `summary.skipped.{unsupported,ambiguous,missing_hash,precondition_failed,total}`.
  - `RepairApplyReport.plan_context` restructures from three flat counts to nest a `skipped` object matching the plan's summary shape.
  - `vault repair apply` rejects v2 plans with `unsupported repair plan schema version: expected 3, got 2`.
  - Migration:
    ```text
    old: plan.unsupported_findings.length
    new: plan.skipped_findings.filter(f => f.skip_reason === "unsupported").length

    old: plan.summary.unsupported_findings
    new: plan.summary.skipped.unsupported

    old: apply.plan_context.unsupported_findings
    new: apply.plan_context.skipped.unsupported
    ```
- **`vault cache build` command removed.** The SQLite cache was write-only with no consumer; future warm-state needs (LSP/MCP/daemon) will be designed against memory-resident indexes instead.
- **`VaultFile.hash` is now `Option<String>`.** Non-Markdown attachment files no longer carry BLAKE3 hashes — only Markdown documents (which need them for repair preconditions). Path identity remains stable; backlink queries by exact attachment path are unaffected.

### Added

- **Apply migration to `vault-standards`.** New `vault-standards::apply` module with `ApplyError`, `RepairApplyReport`, `validate_plan_for_apply`, `changes_by_path`, `apply_file_changes`. Apply contract logic now lives in the engine crate, not the binary crate. CLI `repair_apply.rs` shrinks from ~230 lines to ~75 of orchestration.
- **Minimal-edit YAML preservation.** `vault repair apply` no longer rewrites the entire frontmatter mapping through `serde_yaml::to_string`. YAML lines untouched by a repair are preserved byte-for-byte (comments, quote style, key ordering). Touched values preserve the original quote style when the new value can be expressed in it; double-quoted stays double-quoted, single-quoted stays single-quoted. Closes the v0.24 Atlas quote-churn issue.
- **`vault-frontmatter::top_level_property_spans`** returning `PropertySpan { name, line_range, value_range, style }` for byte-range YAML editing.
- **`vault-frontmatter::serialize_value_preserving_style`** with upgrade-only quote-style rules.
- **`SkipReason` taxonomy.** Each `SkippedFinding` carries a tagged `skip_reason` (`unsupported` | `ambiguous` | `missing_hash` | `precondition_failed`) — replaces the prior string-heuristic `is_ambiguous_skipped` and the three overlapping result vectors. `MissingHash` produces a clearer message ("document hash not present in index — file may have been removed or renamed") instead of the previous misleading "inspect the repair rule".
- **`vault-standards::config::parse_config`** as the single config entry point. Replaces the prior split between `serde_yaml::from_str` (CLI) and `validate_config_yaml` (engine).
- **`vault_core::display`** module exposing `link_kind_str`, `link_status_str`, `severity_str`, `unresolved_reason_str` — one source of truth for enum-to-string mappings previously duplicated across three CLI files.
- **Foundation tests** in `vault-core`, `vault-frontmatter`, `vault-links`, and `vault-standards` covering parsing rules, link resolution, frontmatter offsets, repair classification, and validate engine smoke paths. Slice 1 added ~53 tests; Slices 2 and 4 stack ~33 more. Total ~193 tests passing across the workspace.
- **`--format paths` dedupe** for `vault links list`, `links unresolved`, `links backlinks`. Multiple links from the same source path now contribute one path-list row, matching `vault validate --format paths` behavior.

### Changed

- **Config schema rewritten with serde.** `crates/vault-standards/src/config.rs` uses `#[serde(deny_unknown_fields)]` typed structs plus a focused ~80-line `post_validate` for what serde can't express (field type whitelist, allowed_values scalar-only, repair rule action exclusivity, deprecated `graph.ignore` rename). The 519-line hand-rolled `config_schema.rs` is deleted.
- **`RepairRule.action()` method** replaces the field-flattened `RepairAction` on the struct, computed from the present-exactly-one of `set_frontmatter` / `remove_frontmatter` enforced by `post_validate`.
- **`cli.rs` argument-struct duplication removed** via `#[command(flatten)]`. `FrontmatterFilterArgs` is shared by `docs list`, `docs summary`, and `search`. `ValidateTriageArgs` is shared by `validate` and `repair plan`.
- **Subcommand `--help` groups options** under "Global options", "Filter options", and "Triage filters" via clap `help_heading`. Positional arguments on `repair apply`, `registry add`, and `registry remove` now have descriptions.
- **Wikilink and block-id regexes** now compile once per process via `std::sync::LazyLock` instead of once per call.

### Removed

- `crates/vault-graph/src/cache.rs` (283 lines).
- `crates/vault-standards/src/config_schema.rs` (519 lines).
- `rusqlite` dependency from `vault-graph` and `vault-cli` dev-deps.
- BLAKE3 hashing of non-Markdown files — `(stat::size, stat::mtime)` identity is sufficient for path-based backlink queries against attachments.
- `is_ambiguous_skipped` string heuristic.
- `RepairPlanFinding` struct (replaced by `SkippedFinding`).

### Internal

- `planned_change` returns `Result<PlannedChange, SkipReason>` instead of `Option<PlannedChange>`.
- `REPAIR_PLAN_SCHEMA_VERSION` is now a single `pub const` in `vault-standards::repair`; `vault-standards::apply::validate_plan_for_apply` references it.
- `crates/vault-cli/src/repair_apply.rs` reduced from ~230 lines to ~75 lines of orchestration. All apply contract checks moved to `vault-standards::apply`.

## v0.25.1 - 2026-05-18

Repair workflow documentation polish.

### Changed

- Clarified the `repair plan --out` unsupported format error.
- Documented when to use temp paths versus committed repair plan artifacts.

## v0.25.0 - 2026-05-18

Repair workflow ergonomics release.

### Added

- Added `vault repair plan --out <path>` to write JSON repair plan artifacts directly without shell redirection.
- Documented a live repair maintenance recipe with snapshot, dry-run apply, verified apply, and diff inspection.

### Changed

- Documented YAML frontmatter style normalization during repair apply.
- Updated README link/path repair wording to use ambiguous/skipped repair fallout vocabulary.

## v0.24.0 - 2026-05-18

Repair workflow polish release.

### Added

- Added `plan_context` counts to `vault repair apply` output for skipped, unsupported, and ambiguous findings from the source plan.
- Documented the stable repair workflow loop: validate summary, repair plan, dry-run apply, verified apply.

### Changed

- Updated `vault repair plan` and `vault repair apply` help text to match the applyable-plan contract.
- Changed `vault repair links` decision wording to use `skipped:` reasons instead of manual-decision language.

## v0.23.0 - 2026-05-18

Repair plan usability release.

### Added

- Added `skipped_findings` repair plan fallout with reasons, candidates, and next actions for findings that cannot be planned deterministically.
- Added row-oriented table output for `vault repair plan --format table` and `vault repair links --format table`.

### Changed

- `vault repair plan` now keeps executable `changes` applyable by moving non-executable findings into skipped/unsupported/ambiguous fallout.
- `vault repair apply` now applies deterministic changes from broad plans without rejecting skipped fallout.
- `repair plan` source filters now record normalized comma-separated values.
- Repair commands now expose only supported `json`, `jsonl`, and `table` formats.

## v0.22.0 - 2026-05-17

Safe apply and link/path planning release.

### Added

- Added `vault repair apply <plan>` for frontmatter-only repair plans with document hash preconditions, expected-old-value checks, dry-run support, changed-file manifests, and optional post-apply validation.
- Added read-only `vault repair links` reports for unresolved links, ambiguous links, path-style Markdown links, duplicate-stem risks, affected files, and target move/delete risk.

## v0.21.0 - 2026-05-17

Repair planning MVP release.

### Added

- Added read-only `vault repair plan` with schema-versioned JSON plans, configured frontmatter repair rules, unsupported finding reporting, and manual-decision reporting.
- Added `repair.rules` config for deterministic frontmatter repairs using `set_frontmatter` and `remove_frontmatter` actions.
- Added documented detect -> plan agent workflows for frontmatter drift healing.

## v0.20.0 - 2026-05-17

Native retrieval and vault targeting release.

### Added

- Added top-level `vault search` with document path filters, frontmatter filters, field presence filters, literal text filters, and JSON/JSONL/table/paths output.
- Added `vault registry add/list/remove` and global `--vault <name>` targeting using an XDG-style registry file.

## v0.19.0 - 2026-05-17

Direction, human output, and workflow recipe release.

### Added

- Added `table` and `paths` output formats for document inventory inspection.
- Added `table` output for validation summaries.
- Commands with human renderers now default to table output on terminals and JSON output when stdout is piped or captured.
- Documented validation cleanup recipes for filtered summaries, JSONL queues, link failure modes, and raw `--target` matching.

### Changed

- Documented deterministic drift healing as the product direction: detect, plan, apply, and verify.

## v0.18.0 - 2026-05-17

Validation triage ergonomics release.

### Added

- Added `vault validate` filters: `--code`, `--severity`, `--field`, `--rule`, `--path`, `--target`, and `--reason`.
- Added comma-separated value sets for validate filters, such as `--code link-unresolved,link-ambiguous`.
- Filtered `vault validate --summary` now summarizes the filtered finding set while preserving the existing summary schema.

### Changed

- Unknown `docs list --has` / `--missing` warnings now name the operator instead of using generic filter wording.

## v0.17.1 - 2026-05-17

Date/datetime validation polish release.

### Changed

- `datetime` field types now accept common ISO/YAML forms including fractional seconds, `Z`, numeric timezone offsets, and space-separated YAML datetime strings.
- `date` field types now accept plain dates plus YAML-normalized midnight datetime strings such as `2026-03-20 00:00:00+00:00`.
- Date validation now checks real month/day bounds, including leap years.

## v0.17.0 - 2026-05-17

Document query ergonomics release.

### Added

- Added `vault docs list --path <glob>` using the same path-segment glob semantics as config paths.
- Added `vault docs list --has <field>` and `--missing <field>` for frontmatter field presence filters.
- Added comma-separated value sets for `vault docs list --filter`, such as `status:backlog,completed`.
- Added `vault docs summary --count-by <field>` for grouped document inventory counts.
- Added warnings for unknown `--has`, `--missing`, and `--count-by` fields.

### Changed

- `vault docs inspect` now defaults to `--format json` because it emits one logical object.

## v0.16.0 - 2026-05-17

CLI surface regroup release. **Breaking command paths and config key rename.**

### Changed

- Replaced the `vault graph` umbrella with top-level command groups:
  - `vault docs list`
  - `vault docs inspect <target>`
  - `vault files`
  - `vault links list`
  - `vault links unresolved`
  - `vault links backlinks <target>`
  - `vault cache build`
- Promoted `--config` and `--verbose` to global flags.
- Renamed config key `graph.ignore` to `files.ignore`.
- Renamed the CLI integration test file from `graph_output.rs` to `cli_output.rs`.

### Added

- `vault docs list --filter <field:value>` now warns on stderr when a filter field is not present in any document frontmatter.
- Legacy `graph.ignore` configs now fail with a clear v0.16 rename error.

### Removed

- Removed `vault graph ...` command paths.
- Removed `vault graph diagnostics`; use `vault validate --format jsonl` to surface graph diagnostics as validation findings.

## v0.15.0 - 2026-05-17

Internal restructure release. **Breaking JSONL output schema for validate findings.**

### Added

- `vault-frontmatter` crate: YAML extraction and shallow property/offset utilities.
- `vault-links` crate: CommonMark link parsing, wikilink parsing, block IDs, anchor helpers, and link resolution.
- `vault-standards` crate: validate engine, `Finding` / `FindingBody` types, summary, predicates, YAML config-schema validator.
- `Finding` sum type replacing the 12-field `ValidateFinding` god-struct. Variant-specific fields only appear on findings that carry them; no more `null` defaults.
- `docs/rule-shape.md`: canonical conceptual model for validate rules (selectors + constraints).
- `Summary.invalid_types` grouping for `frontmatter-invalid-type` findings by field and expected type.

### Changed

- Renamed `vault-index` crate to `vault-graph` (matches the original modular-architecture spec and the command surface name).
- `vault-cli/src/main.rs` reduced from ~1376 lines to ~150 lines of dispatch; per-concern modules (`cli`, `config`, `output`, `filter`, `target`) carry the rest.
- `validate` (formerly `validate_findings`) is now a ~55-line orchestrator in `vault-standards::engine` dispatching to seven per-check functions, each under 40 lines.

### Renamed finding codes (breaking)

| Old | New |
|---|---|
| `path-not-allowed` | `document-misrouted` |
| `frontmatter-field-value-not-allowed` | `frontmatter-disallowed-value` |
| `frontmatter-field-type-invalid` | `frontmatter-invalid-type` |
| `frontmatter-field-forbidden` | `frontmatter-forbidden-field` |

Any scripts or agent skills filtering on these codes need to update.

### Output schema (breaking)

`vault validate --format jsonl` rows are still flat JSON objects keyed by `code`, but variant-specific fields (`field`, `actual_value`, `allowed_values`, `expected_type`, `allowed_paths`, `link`, `diagnostic`) only appear on findings that carry them, rather than being emitted as `null` everywhere.

### Unchanged

- CLI command paths (`vault graph documents`, `vault graph backlinks`, `vault validate`, etc.) are identical to v0.14. The CLI surface regroup ships in v0.16.
- Config YAML keys (`graph.ignore`, `validate.required_frontmatter`, etc.) are unchanged. The `graph.ignore` rename ships in v0.16.

## v0.14.0 - 2026-05-17

- Added validation-only `validate.ignore` patterns so files can remain graph-visible while being skipped by standards checks.
- Added scoped rule path exclusions with `match.path_not` and `exclude.path`.
- Added `validate.rules[].field_types` checks for `datetime`, `date`, `list_of_strings`, `wikilink`, and `wikilink_or_list`.
- Added `validate.rules[].forbidden_frontmatter` for absent-field constraints.
- Added `validate.rules[].allowed_paths` for read-only folder-routing validation.
- Added finding context for expected field types and allowed path patterns.

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
