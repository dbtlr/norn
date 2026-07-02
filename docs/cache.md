---
title: Vault cache
description: The SQLite-backed cache that accelerates norn query commands — where it lives, when it auto-rebuilds, what's stored, and how to tune it.
---

# Vault cache

`norn` uses a SQLite-backed cache to accelerate query commands. The cache is the read path for `norn validate`, `norn find`, `norn count`, `norn get`, and `norn repair` — these commands open the cache, refresh it incrementally if needed, and load the graph in-memory before running their existing logic.

## Where it lives

```text
~/.cache/norn/<sha256-of-canonical-vault-root>/cache.db
```

Honors `$XDG_CACHE_HOME` when set. The directory is created at `0700` and the database file at `0600` — explicitly tightened (not relying on umask) to protect frontmatter values on shared hosts.

The cache identity is derived from the canonical path of the vault root (symlinks resolved). Querying via the symlinked path or its resolved target hits the same cache.

## Surface

```text
norn cache index               # incremental update (default)
norn cache index --rebuild     # full rebuild from scratch
norn cache index --force-hash  # skip mtime cheap-check; hash every file
norn cache rebuild             # explicit alias for `index --rebuild`
norn cache clear               # delete the cache; next command rebuilds
norn cache status              # path, size, doc/link/file counts, schema version
norn cache prune               # cross-vault GC; --dry-run to preview
```

Every cache subcommand accepts the global `-C` and `--config` flags; `status` accepts `--format text|json` like other query commands.

## When the cache rebuilds automatically

The cache is *disposable*. Any of the following triggers an automatic silent rebuild (one-line stderr message; exit code 0):

- Cache file missing (first run, or after `norn cache clear`).
- Cache schema version older than the binary expects.
- SQLite file corruption (open failure or `PRAGMA integrity_check` mismatch).
- Vault root identity drift (cache was built against a different canonical path).

A cache with a *newer* schema version than the binary supports is the one case that hard-errors — interpreting unknown future fields would be unsafe. Upgrade `norn` to read it.

The current `schema_version` is `4`. It is surfaced by `norn cache status` and stamped into the `meta` table on every rebuild.

## Lifecycle

The cache tree is self-maintaining. Every `norn` invocation runs a cross-vault prune sweep at most once per 24h (throttled by the `~/.cache/norn/.last-prune` marker): cache entries whose vault root no longer exists, that are unreadable or empty, that are older than the retention window (default 90d), or that push the tree over its internal 1 GiB cap are deleted. The current vault's entries never are. State entries (`~/.local/state/norn/`, the mutation event stream) are removed only when their vault root is gone (or the entry is empty) — they are a record, not a rebuildable cache. Opt out per vault with `cache: { prune: manual }` in `.norn/config.yaml`; run the sweep on demand with [`norn cache prune`](commands/cache.md#pruning).

## `--force-hash`

Skips the `(mtime, size)` cheap-check during change detection; reads and hashes every file. Use on filesystems where mtime is unreliable:

- NFS shared vaults (mtime can lag several seconds).
- Docker bind-mounts on macOS / WSL.
- Vaults restored from `rsync --times`, `tar -p`, or backup tools that copy mtime verbatim.
- Post-`git-restore-mtime` workflows that touch timestamps.

## `--no-cache-refresh`

Query commands implicitly refresh the cache before reading. Pass the global `--no-cache-refresh` flag to skip that step — useful when batching many commands in a CI pipeline that already ran `norn cache index` explicitly, or when investigating cache state without changing it.

```bash
norn cache index
norn --no-cache-refresh validate --summary --format json
norn --no-cache-refresh validate --code 'link-*' --format jsonl
```

## What's cached

Stored: document path, stem, content hash, frontmatter, body text, mtime, size; outgoing links with resolved targets (including the unresolved reason and candidate list for ambiguous links); headings; block IDs; non-Markdown file inventory.

Not stored: validation findings — they depend on `.norn/config.yaml`, which can change between runs. Findings always recompute fresh against the in-memory graph loaded from the cache.

### `document_fields` — the derived frontmatter index

`document_fields` is a derived EAV (entity-attribute-value) table shredded from the declared, bounded frontmatter field set (the fields `field_types` bounds, plus any field explicitly marked `indexed`). Every document gets exactly one row per declared field: scalars are canonicalized the same way the query layer computes them; arrays expand to one row per element (an empty array, or an array whose elements are all null, gets a single row with a SQL `NULL` value, meaning "present, no scalar"); a field absent from frontmatter (or present but null, or whose frontmatter failed to parse) gets a reserved BLOB sentinel — every (document, declared field) pair always has at least one row. Fields outside the declared set get no rows.

The table is rebuilt automatically: a full `norn cache rebuild` or incremental `norn cache index` keeps it current, and opening the cache with a resolved index set that no longer matches the stamped `index_set_hash` meta row triggers a silent in-place re-shred straight from the cached `frontmatter_json` column — no filesystem re-parse, no user-facing output. Like the rest of the cache, it's disposable and self-healing; nothing about it needs manual maintenance.

## Performance targets

- Cold rebuild on a 1000-document vault: under 2 seconds.
- Warm read for `norn validate`: under 100 ms on a vault with no filesystem changes.
- `norn cache status`: under 50 ms.

If you're seeing significantly slower numbers, run `norn cache rebuild` to start from a clean slate.

## Schema evolution

The schema is versioned. Bumps trigger a silent auto-rebuild on next open. The current version is exposed via `norn cache status`.

Future evolution (planned, not in this release):

- Full-text search via SQLite FTS5 over body text and frontmatter values.
- SQL-direct query path (commands issue SQL instead of loading the in-memory `GraphIndex`).
- MCP server with a file watcher driving cache updates without explicit invocations.

## Concurrency

Writes are serialized by an advisory file lock (`fs2`). Two simultaneous `norn cache index` runs will queue rather than race; readers never block, because reads go through SQLite's WAL mode and the in-memory `GraphIndex` is rebuilt on each command.

## See also

- [Commands reference](commands.md) — the full `norn cache` subcommand table.
- [Configuration](configuration.md) — the `.norn/config.yaml` schema (validation findings are recomputed against this on every run).
