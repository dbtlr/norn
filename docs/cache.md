---
title: Vault cache
description: The SQLite-backed cache that accelerates vault query commands — where it lives, when it auto-rebuilds, what's stored, and how to tune it.
---

# Vault cache

`vault` uses a SQLite-backed cache to accelerate query commands. The cache is the read path for `vault validate`, `vault find`, `vault count`, `vault get`, and `vault repair` — these commands open the cache, refresh it incrementally if needed, and load the graph in-memory before running their existing logic.

## Where it lives

```text
~/.cache/vault/<sha256-of-canonical-vault-root>/cache.db
```

Honors `$XDG_CACHE_HOME` when set. The directory is created at `0700` and the database file at `0600` — explicitly tightened (not relying on umask) to protect frontmatter values on shared hosts.

The cache identity is derived from the canonical path of the vault root (symlinks resolved). Querying via the symlinked path or its resolved target hits the same cache.

## Surface

```text
vault cache index               # incremental update (default)
vault cache index --rebuild     # full rebuild from scratch
vault cache index --force-hash  # skip mtime cheap-check; hash every file
vault cache rebuild             # explicit alias for `index --rebuild`
vault cache clear               # delete the cache; next command rebuilds
vault cache status              # path, size, doc/link/file counts, schema version
```

Every cache subcommand accepts the global `-C` and `--config` flags; `status` accepts `--format json|table` like other query commands.

## When the cache rebuilds automatically

The cache is *disposable*. Any of the following triggers an automatic silent rebuild (one-line stderr message; exit code 0):

- Cache file missing (first run, or after `vault cache clear`).
- Cache schema version older than the binary expects.
- SQLite file corruption (open failure or `PRAGMA integrity_check` mismatch).
- Vault root identity drift (cache was built against a different canonical path).

A cache with a *newer* schema version than the binary supports is the one case that hard-errors — interpreting unknown future fields would be unsafe. Upgrade `vault` to read it.

The current `schema_version` is `2`. It is surfaced by `vault cache status` and stamped into the `meta` table on every rebuild.

## `--force-hash`

Skips the `(mtime, size)` cheap-check during change detection; reads and hashes every file. Use on filesystems where mtime is unreliable:

- NFS shared vaults (mtime can lag several seconds).
- Docker bind-mounts on macOS / WSL.
- Vaults restored from `rsync --times`, `tar -p`, or backup tools that copy mtime verbatim.
- Post-`git-restore-mtime` workflows that touch timestamps.

## `--no-cache-refresh`

Query commands implicitly refresh the cache before reading. Pass the global `--no-cache-refresh` flag to skip that step — useful when batching many commands in a CI pipeline that already ran `vault cache index` explicitly, or when investigating cache state without changing it.

```bash
vault cache index
vault --no-cache-refresh validate --summary --format json
vault --no-cache-refresh validate --code 'link-*' --format jsonl
```

## What's cached

Stored: document path, stem, content hash, frontmatter, body text, mtime, size; outgoing links with resolved targets (including the unresolved reason and candidate list for ambiguous links); headings; block IDs; non-Markdown file inventory.

Not stored: validation findings — they depend on `.vault/config.yaml`, which can change between runs. Findings always recompute fresh against the in-memory graph loaded from the cache.

## Performance targets

- Cold rebuild on a 1000-document vault: under 2 seconds.
- Warm read for `vault validate`: under 100 ms on a vault with no filesystem changes.
- `vault cache status`: under 50 ms.

If you're seeing significantly slower numbers, run `vault cache rebuild` to start from a clean slate.

## Schema evolution

The schema is versioned. Bumps trigger a silent auto-rebuild on next open. The current version is exposed via `vault cache status`.

Future evolution (planned, not in this release):

- Full-text search via SQLite FTS5 over body text and frontmatter values.
- SQL-direct query path (commands issue SQL instead of loading the in-memory `GraphIndex`).
- MCP server with a file watcher driving cache updates without explicit invocations.

## Concurrency

Writes are serialized by an advisory file lock (`fs2`). Two simultaneous `vault cache index` runs will queue rather than race; readers never block, because reads go through SQLite's WAL mode and the in-memory `GraphIndex` is rebuilt on each command.

## See also

- [Commands reference](commands.md) — the full `vault cache` subcommand table.
- [Configuration](configuration.md) — the `.vault/config.yaml` schema (validation findings are recomputed against this on every run).
