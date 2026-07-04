---
title: Warm host daemon
description: Run norn serve, the warm host daemon that holds one verified vault cache per vault on the host and serves the full MCP toolset over a Unix socket — trust model, lifecycle, and wire preamble.
---

# Warm host daemon (`norn serve`)

`norn serve` runs one persistent foreground process that serves the full MCP toolset for *any* vault on the host, over a single well-known Unix-domain socket. Instead of one `norn mcp` process per vault re-verifying its cache on every call, the daemon holds each vault's cache warm and verified across many calls.

```sh
norn serve
```

There is no `--cwd` for data — vaults are named per-connection (see [Wire preamble](#wire-preamble) below) — and no `--config`; each vault always loads its own default `.norn/config.yaml`.

## One host daemon, lazy per-vault contexts

There is exactly one daemon per host (per user, enforced by a lifetime advisory lock — see [Lifecycle](#lifecycle)), independent of how many vaults it ends up serving. It does not pre-open anything: a vault's warm context is opened lazily on its first `hello`, then held in an in-process map keyed by the vault's identity hash. A second connection naming an already-open vault shares the existing warm context; a connection for a vault the daemon has never seen pays one open (config parse now, integrity check + index build on the first query) and is warm from then on.

## Trust model

The daemon does not weaken norn's trust guarantee to gain speed. The same invariant that governs the CLI's routing seam (ADR 0005) governs the daemon itself: trust is **inherited from a live authority or re-established locally, never skipped**. Concretely:

- **Verify once per vault.** The first touch of a vault runs the same `PRAGMA integrity_check` a one-shot `norn` invocation pays. A warm context does not repeat it on every call — that would defeat the point of staying warm — but it never skips it either; it happens exactly once, at open.
- **Per-request freshness, not a stale lease.** Every warm query still runs a cheap self-heal pipeline: it re-checks that the vault root is still there, that `.norn/config.yaml` hasn't changed (re-parsing and hot-swapping the config, or fully reopening if the change affects what's indexed), that the cache file on disk is still the one the daemon opened (catching an out-of-band `norn cache clear`/`prune`/`rm` while a connection is held), and runs the same incremental index refresh the CLI gets on every invocation. So a warm read reflects edits made between calls exactly like a fresh `norn` invocation would — it just skips re-paying the integrity check, which is the one thing proven safe to skip once trust is established.
- **A vanished root fails the request, not the daemon.** If a vault's root disappears, requests against it error individually; the daemon keeps running and the stale context self-heals on the next `hello` for that vault.

## Lifecycle

`norn serve` is a **foreground process** — it does not detach, fork, or write a PID file for you. Run it under your own process supervisor (launchd, systemd, tmux, whatever you already use) until `norn service` supervisor verbs ship as a separate layer; this doc does not describe those verbs because they don't exist yet.

- **Single instance per user.** On startup the daemon acquires an exclusive advisory lock next to the socket and holds it for its entire life. A second `norn serve` on the same host refuses immediately with the incumbent's pid rather than racing it for the socket.
- **Startup order:** run directory → lock → signal handlers → socket bind. Registering the shutdown signal handlers before binding the socket means a signal-registration failure never leaves a bound socket behind for a later probe to mistake for a live daemon.
- **Shutdown:** SIGINT or SIGTERM stops the accept loop, unlinks the socket, and exits 0. In-flight connections may be dropped; a client on the other end falls back to a direct, self-verified open exactly as it would if the daemon had never been running.

## Socket path and wire preamble

The daemon listens at the same well-known path the CLI's routing probe targets: `<XDG_CACHE_HOME>/norn/run/norn.sock` (typically `~/.cache/norn/run/norn.sock`), fixed regardless of how many vaults are open — there is no per-vault derivation to overflow a platform's socket-path length limit.

Every accepted connection starts with one newline-delimited JSON control frame before anything else happens:

- **`ping`** → the daemon answers one `pong` (protocol version, daemon version, pid, uptime) and closes. This is an O(1) liveness probe: it touches no vault and takes no lock, so it answers promptly even while the daemon is busy serving other vaults.
- **`hello`** → names the vault this connection is for (`vault_root`, a path). The daemon derives the vault's identity itself (never trusting a client-supplied hash), resolves or opens its warm context, and answers `ready`. From that point the rest of the connection is a normal MCP session — the daemon hands the (possibly already-pipelined) remaining bytes straight to the MCP server for that vault.
- Anything else, or a protocol-version mismatch, gets a one-line `error` frame and the connection closes.

A connection error never crashes the daemon — each connection runs in its own task, and a failure is logged as a single stderr line.

## Multi-vault behavior

Because vaults are named per-connection rather than baked into the socket path, one daemon serves as many vaults as clients ask for, each with its own independently warm, independently verified context. Two connections naming the same vault share one context; concurrent first-touches of the *same* vault open it exactly once (the second waits on the first rather than racing a duplicate open). Contexts for different vaults never contend with each other.

## Relationship to `norn mcp`

[`norn mcp`](mcp-server.md) is the one-shot stdio server: one process per vault, launched and torn down by the client's own process lifecycle, re-verifying its cache on every call. `norn serve` is the *same* MCP server and the *same* tool catalog and mutation-safety contract, made persistent and shared across vaults — the difference is entirely in how the cache is held open and verified, not in what the tools do. An MCP client that wants the warm daemon connects to it directly over the socket described above; a client that just wants a simple one-shot server per vault keeps using `norn mcp`.

The CLI does not yet route its own reads through `norn serve` — that lands separately. Today the daemon is reachable only by an MCP client that speaks the wire preamble above.

## See also

- [MCP server](mcp-server.md) — the tool catalog, mutation-safety contract, and read-only mode the daemon shares with `norn mcp`.
- [Cache](cache.md) — the per-vault cache the daemon holds warm.
