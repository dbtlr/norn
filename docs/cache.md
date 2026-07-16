---
title: Vault cache
description: The SQLite-backed cache that accelerates norn query commands — where it lives, when it auto-rebuilds, what's stored, and how to tune it.
---

# Vault cache

`norn` uses a SQLite-backed cache to accelerate query commands. The cache is the read path for `norn validate`, `norn find`, `norn count`, `norn get`, and `norn repair` — these commands open the cache, refresh it incrementally if needed, and load the graph in-memory before running their existing logic.

## Where it lives

```text
~/.cache/norn/<sha256-of-canonical-vault-root>/v<schema>/cache.db
```

Honors `$XDG_CACHE_HOME` when set. The directory is created at `0700` and the database file at `0600` — explicitly tightened (not relying on umask) to protect frontmatter values on shared hosts.

The database is namespaced by **schema version** (`v5` for the current schema) as well as by channel (below): the schema version is part of the database's on-disk identity, so a binary only ever opens the db in its own `v<schema>` directory. Mixed norn versions therefore coexist — each builds and uses its own cache — and a version downgrade self-heals instead of locking the binary out (see [When the cache rebuilds automatically](#when-the-cache-rebuilds-automatically)).

The cache identity is derived from the canonical path of the vault root (symlinks resolved). Querying via the symlinked path or its resolved target hits the same cache.

### Channels: `live` vs `dev`

The database is namespaced by **channel** so a binary built from a cargo tree can never read, migrate, or overwrite the cache the installed binary uses — a stray dev build must not silently rebuild an installed client's cache to a newer schema and lock it out.

```text
~/.cache/norn/<hash>/v<schema>/cache.db        # live channel (installed binary)
~/.cache/norn/<hash>/dev/v<schema>/cache.db    # dev channel  (cargo build tree)
```

Detection is not purely path-based: a binary copied out of its build tree (e.g. `cp target/release/norn /tmp/norn-after`) carries none of its original path's ancestry, so a runtime-only walk would misresolve it to `live` and let it migrate the installed binary's cache (the incident this section's second and third layers close). Channel identity is resolved once per process, in order:

1. **`NORN_CACHE_CHANNEL`** — set it to exactly `live` or `dev` to force the channel. Any other non-empty value is a hard error; an empty value counts as unset. Note that forcing `live` from a build-tree binary deliberately re-enables cross-channel access — including the older-schema silent rebuild the channel split exists to prevent — and is an escape hatch for exactly that debugging purpose.
2. **The channel baked into the binary at compile time.** `build.rs` bakes `dev` iff the tree being built is a `.git` checkout (worktrees included) that is neither a `cargo install --git` checkout (built from a clone under `CARGO_HOME`) nor a CI release build (`CI` env set — cargo-dist's GitHub Actions release artifacts stay `live`). This is what makes channel identity travel *with the binary* rather than living in its path: a local dev build baked `dev` still resolves `dev` no matter where it's later run from. `NORN_BUILD_CHANNEL=live`/`=dev` overrides the auto rule at build time (any other non-empty value fails the build); `just install` sets `NORN_BUILD_CHANNEL=live` so `cargo install --path .` from a local checkout lands on `live` like a normal install.
3. **Runtime detection**, when nothing was baked (or an unrecognized baked value slipped through): `dev` when the running executable sits under a cargo build tree — detected by any ancestor directory containing a `CACHEDIR.TAG` file (cargo writes one into every target directory root, whatever its name, so custom `CARGO_TARGET_DIR` locations are covered) — **or** when the executable resides under a system temp location (`std::env::temp_dir()`, `/tmp`, `/private/tmp`, `/var/tmp`, `/var/folders`), which is what catches a binary copied out of its build tree when nothing was baked (e.g. a CI-built release binary, since layer 2's auto rule stays `live` in CI). If the executable's own path can't be resolved, norn fails toward `dev` — the safe direction is an isolated cache.
4. Otherwise **`live`**.

Only the database moves. The per-vault **write lock** (`<hash>/.lock`) and vault-level state stay shared across channels *and* schema versions, so a dev and a live binary (of any schema) mutating the same vault still serialize against each other. `norn cache status` prints a `channel:` line and the `dev/` and `v<schema>/` segments show up in the reported path; correct isolation is otherwise silent.

Every database *except the one a norn binary is actively using* reclaims itself: the prune sweep evicts any db location left idle for ~48h on its own clock — independent of the entry's overall freshness. That covers a `dev/` database of any schema, a live database of a non-current schema (`v<N>/` left behind by a newer or older binary), and a legacy bare `cache.db` at a channel root (the pre-schema-segment layout). This runs even inside the current vault's own entry (which is otherwise exempt from whole-entry eviction) — it protects only the invoking binary's own current database, so a single-vault user's leftover stale databases still age out. An abandoned dev build or an obsolete-schema database therefore never pins disk inside a still-active `live` entry.

## Surface

```text
norn cache index               # incremental update (default)
norn cache index --rebuild     # full rebuild from scratch
norn cache index --force-hash  # skip mtime cheap-check; hash every file
norn cache rebuild             # explicit alias for `index --rebuild`
norn cache clear               # delete the whole cache entry (all channels/schemas); next command rebuilds
norn cache status              # channel, path, size, doc/link/file counts, schema version
norn cache prune               # cross-vault GC; --dry-run to preview
```

Every cache subcommand accepts the global `-C` and `--config` flags; `status` accepts `--format text|json` like other query commands.

`cache clear` is the one exception to "every cache subcommand opens the cache": it deletes the vault's whole entry dir (every channel and schema database, plus legacy leftovers) purely from the vault's identity, without opening the database — the escape hatch that must work even when the cache is too broken to open at all. It refuses (exit `2`, nothing deleted) only while another process holds the entry lock. See [Clearing](commands/cache.md#clearing) for details.

## When the cache rebuilds automatically

The cache is *disposable*. Any of the following triggers an automatic silent rebuild (one-line stderr message; exit code 0):

- Cache file missing (first run, or after `norn cache clear`).
- SQLite file corruption (open failure or `PRAGMA integrity_check` mismatch), including a `schema_version` that disagrees with the binary inside its own `v<schema>` dir — the only way a foreign or damaged database can land there.
- Vault root identity drift (cache was built against a different canonical path).

There is **no** "schema newer than this binary supports; upgrade norn" hard error anymore (retired in NRN-286). A genuinely newer binary writes to *its own* `v<newer>/` directory that this binary never opens; a downgrade simply builds and uses the older schema's own directory. Mixed versions coexist, and the stale-schema database ages out via the 48h prune TTL. This retires the incident where a newer-schema binary upgraded a shared cache and locked an older installed binary out of every operation.

The current `schema_version` is `5`. Version 5 adds an atomically maintained graph fingerprint to `meta`; daemon mutation reservations compare that O(1) token before staging so a graph captured before a newer cache publication cannot overwrite it. Because databases are now stored per schema version, moving between versions builds a fresh database in the target version's own directory rather than migrating in place. The schema version is surfaced by `norn cache status` and stamped into the `meta` table on every rebuild.

## Lifecycle

The cache tree is self-maintaining. Every `norn` invocation runs a cross-vault prune sweep at most once per 24h (throttled by the `~/.cache/norn/.last-prune` marker): cache entries whose vault root no longer exists, that are unreadable or empty, that are older than the retention window (default 90d), or that push the tree over its internal 1 GiB cap are deleted. The current vault's entry is never deleted as a whole (though its stale non-current-schema databases still reclaim on the 48h TTL, sparing only the database in use). State entries (`~/.local/state/norn/`, the mutation event stream) are removed only when their vault root is gone (or the entry is empty) — they are a record, not a rebuildable cache. Opt out per vault with `cache: { prune: manual }` in `.norn/config.yaml`; run the sweep on demand with [`norn cache prune`](commands/cache.md#pruning).

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

`find`/`count`'s query router reads this table directly for `--eq`/`--not-eq`/`--in`/`--not-in`/`--has`/`--missing`/`--starts-with`/`--ends-with`/`--contains` predicates on a declared-indexed field, instead of scanning `frontmatter_json` via `json_extract` — same results, but the query plan is an index seek rather than a per-document scan (see [`find`](commands/find.md#filters)).

The table is rebuilt automatically: a full `norn cache rebuild` or incremental `norn cache index` keeps it current, and opening the cache with a resolved index set that no longer matches the stamped `index_set_hash` meta row triggers a silent in-place re-shred straight from the cached `frontmatter_json` column — no filesystem re-parse, no user-facing output. Like the rest of the cache, it's disposable and self-healing; nothing about it needs manual maintenance.

## Performance targets

- Cold rebuild on a 1000-document vault: under 2 seconds.
- Warm read for `norn validate`: under 100 ms on a vault with no filesystem changes.
- `norn cache status`: under 50 ms.

If you're seeing significantly slower numbers, run `norn cache rebuild` to start from a clean slate.

## Schema evolution

The schema is versioned. Bumps trigger a silent auto-rebuild on next open. Version 5's one-time rebuild from version 4 initializes the graph fingerprint used by atomic daemon mutation publication; rebuilds, incremental refreshes, and successful mutation publications then update that fingerprint in the same transaction as the graph rows. The current version is exposed via `norn cache status`.

Future evolution (planned, not in this release):

- Full-text search via SQLite FTS5 over body text and frontmatter values.
- SQL-direct query path (commands issue SQL instead of loading the in-memory `GraphIndex`).
- MCP server with a file watcher driving cache updates without explicit invocations.

## Concurrency

Writes are serialized by an advisory file lock (`fs2`). Two simultaneous `norn cache index` runs will queue rather than race; readers never block, because reads go through SQLite's WAL mode and the in-memory `GraphIndex` is rebuilt on each command.

## See also

- [Commands reference](commands.md) — the full `norn cache` subcommand table.
- [Configuration](configuration.md) — the `.norn/config.yaml` schema (validation findings are recomputed against this on every run).
