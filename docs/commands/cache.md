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
| `norn cache prune` | Evict dead, aged, and over-cap cache entries across all vaults. |

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

## Pruning

`norn cache prune` garbage-collects the *global* trees, not just the current vault: the cache tree (`~/.cache/norn/`, one `<sha256-of-canonical-root>/` entry per vault) and the state tree (`~/.local/state/norn/`).

A cache entry is evicted when any of these holds:

- its recorded vault root no longer exists;
- the entry is unreadable or empty (no content besides norn's own lock files);
- it is older than the retention window (default 90d, measured by the newest file mtime in the entry);
- the cache tree as a whole exceeds its internal 1 GiB cap — oldest entries first, until under.

Additionally, an entry's dev-channel database (`<hash>/dev/cache.db`) is evicted on its own 48h TTL, independent of the entry's overall freshness — only the `dev/` subtree is removed; the live database, the shared lock, and the entry itself stay. A single entry can therefore emit a `dev-stale` row plus a terminal row (e.g. `empty`, once the dev eviction leaves it with nothing else) in the same sweep — per-hash uniqueness of the evicted list is not a contract.

State entries are different: they hold the mutation event stream — the record of what norn changed, not a rebuildable acceleration structure — so they are evicted only when their vault root no longer exists (or the entry is empty), never by age or size. The current vault's entries are never evicted, in either tree.

Flags:

- `--dry-run` — report what would be evicted without deleting anything.
- `--retention <dur>` — age-eviction window override (`90d`-style: `<n>w`, `<n>d`, `<n>h`, `<n>m`). Precedence: this flag, then `cache.retention` in `.norn/config.yaml`, then the 90d default.
- `--format text|json` — stdout format.

Exit code is `0` on success, including when there is nothing to prune.

```bash
norn cache prune --dry-run
```

```text
cache  2 entries would be evicted, 410.2 MiB would be freed
state  1 entry would be evicted, 12.4 KiB would be freed

would be evicted:
  [cache] /home/user/old-vault  dead root  398.0 MiB
  [cache] /home/user/scratch-vault  aged 132d  12.2 MiB
  [state] /home/user/old-vault  dead root  12.4 KiB
```

`--format json` emits a stable object — `{dry_run, cache: {scanned, evicted: [{root, hash, reason, age_days, bytes}], skipped_locked, kept_unknown, bytes_freed}, state: {…}, total_bytes_freed}` — with kebab-case reasons (`dead-root`, `unreadable`, `empty`, `aged`, `over-cap`, `dev-stale`).

### Lazy sweep

You rarely need to run `prune` by hand: every norn invocation runs the same sweep at most once per 24h, throttled by the `~/.cache/norn/.last-prune` marker file. This means norn now deletes stale entries it previously kept forever. Opt a vault out with `cache: { prune: manual }` in `.norn/config.yaml`; the explicit command always works regardless. A real (non-`--dry-run`) `norn cache prune` refreshes the marker, resetting the 24h window.

## See also

- [Cache](../cache.md) — how the cache is keyed and stored.
- Run `norn cache --help` for the full subcommand list.
