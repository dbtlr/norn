---
title: cache
description: Manage the SQLite-backed vault graph cache.
---

# norn cache

Manage the SQLite-backed vault graph cache — a per-vault, disposable read-acceleration store. Query commands (`find`, `get`, `count`, `validate`, `repair`) open and refresh it transparently, so you rarely need these subcommands. Reach for them to inspect the cache or force a clean rebuild.

## Subcommands

| Command | Purpose |
|---|---|
| `norn cache index` | Update the cache incrementally (the default refresh). |
| `norn cache rebuild` | Rebuild the cache from scratch. |
| `norn cache clear` | Delete the cache database; the next command rebuilds it. |
| `norn cache status` | Show cache path, size, document and link counts, and schema version. |

`cache index` takes `--rebuild` (full rebuild instead of incremental) and `--force-hash` (hash every file, skipping the mtime+size cheap-check).

## Examples

```bash
norn cache status
# where the cache lives and what it holds

norn cache rebuild
# force a clean rebuild after unexpected results

norn cache index --force-hash
# re-hash every file when mtimes are unreliable
```

## Refresh model

Query commands refresh the cache implicitly before reading. Pass the global `--no-cache-refresh` to skip that step for a single command. The cache is disposable — a missing or corrupt cache rebuilds silently, so treat cache errors as bugs to report, not states to program around.

## See also

- [Cache](../cache.md) — how the cache is keyed and stored.
- Run `norn cache --help` for the full subcommand list.
