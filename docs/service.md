---
title: Warm host daemon
description: Run norn serve, the warm host daemon that holds one verified vault cache per vault on the host and serves the full MCP toolset over a Unix socket — trust model, lifecycle, the `norn service` launchd supervisor, and the CLI's routing contract.
---

# Warm host daemon (`norn serve`)

`norn serve` runs one persistent foreground process that serves the full MCP toolset for *any* vault on the host, over a single well-known Unix-domain socket. Instead of one `norn mcp` process per vault re-verifying its cache on every call, the daemon holds each vault's cache warm and verified across many calls.

```sh
norn serve
```

There is no `--cwd` for data — vaults are named per-connection (see [Wire preamble](#wire-preamble) below) — and no `--config`; each vault always loads its own default `.norn/config.yaml`.

Two things build on top of this daemon: [`norn service`](#the-supervisor-norn-service) supervises it as a launchd unit so it survives logout and reboot, and the `norn` CLI itself routes its own reads and mutations through a live daemon — see [How the CLI routes to the daemon](#how-the-cli-routes-to-the-daemon).

## One host daemon, lazy per-vault contexts

There is exactly one daemon per host (per user, enforced by a lifetime advisory lock — see [Lifecycle](#lifecycle)), independent of how many vaults it ends up serving. It does not pre-open anything: a vault's warm context is opened lazily on its first `hello`, then held in an in-process map keyed by the vault's identity hash. A second connection naming an already-open vault shares the existing warm context; a connection for a vault the daemon has never seen pays one open (config parse now, integrity check + index build on the first query) and is warm from then on.

## Trust model

The daemon does not weaken norn's trust guarantee to gain speed. The same invariant that governs the CLI's routing seam (ADR 0005) governs the daemon itself: trust is **inherited from a live authority or re-established locally, never skipped**. Concretely:

- **Verify once per vault.** The first touch of a vault runs the same `PRAGMA integrity_check` a one-shot `norn` invocation pays. A warm context does not repeat it on every call — that would defeat the point of staying warm — but it never skips it either; it happens exactly once, at open.
- **Per-request freshness, not a stale lease.** Every warm query still runs a cheap self-heal pipeline: it re-checks that the vault root is still there, that `.norn/config.yaml` hasn't changed (re-parsing and hot-swapping the config, or fully reopening if the change affects what's indexed), that the cache file on disk is still the one the daemon opened (catching an out-of-band `norn cache clear`/`prune`/`rm` while a connection is held), and performs a stat-sweep freshness probe. A stale probe coalesces onto the same incremental refresh a one-shot CLI invocation gets; a fresh probe avoids a writer-queue trip. So a warm read reflects edits made between calls exactly like a fresh `norn` invocation would — it just skips re-paying the integrity check, which is the one thing proven safe to skip once trust is established.
- **Published cache snapshots are atomic.** Post-mutation cache maintenance stages document rows and the complete resolved-link set privately in connection-local TEMP tables, yielding to freshness work between bounded chunks. Readers see the full previous snapshot until one terminal transaction publishes the complete replacement. A newer refresh or publication revokes older staged work before it can publish.
- **A vanished root fails the request, not the daemon.** If a vault's root disappears, requests against it error individually; the daemon keeps running and the stale context self-heals on the next `hello` for that vault.

## Lifecycle

`norn serve` is a **foreground process** — it does not detach, fork, or write a PID file for you. Run it under your own process supervisor (launchd, systemd, tmux, whatever you already use), or use [`norn service`](#the-supervisor-norn-service) below, which wraps it in a launchd LaunchAgent on macOS.

- **Single instance per user.** On startup the daemon acquires an exclusive advisory lock next to the socket and holds it for its entire life. A second `norn serve` on the same host refuses immediately with the incumbent's pid rather than racing it for the socket.
- **Startup order:** run directory → lock → signal handlers → socket bind. Registering the shutdown signal handlers before binding the socket means a signal-registration failure never leaves a bound socket behind for a later probe to mistake for a live daemon.
- **Shutdown:** SIGINT or SIGTERM stops the accept loop, unlinks the socket, and exits 0. In-flight connections may be dropped; a client on the other end falls back to a direct, self-verified open exactly as it would if the daemon had never been running.

## Socket path and wire preamble

The daemon listens at the same well-known path the CLI's routing probe targets: `<XDG_CACHE_HOME>/norn/run/norn.sock` (typically `~/.cache/norn/run/norn.sock`), fixed regardless of how many vaults are open — there is no per-vault derivation to overflow a platform's socket-path length limit.

Every accepted connection starts with one newline-delimited JSON control frame before anything else happens:

- **`ping`** → the daemon answers one `pong` (protocol version, daemon version, pid, uptime) and closes. The ordinary host-global routing ping is an O(1) liveness probe that touches no vault and takes no map lock. A status ping carrying `vault_root` additionally performs one bounded context-map lookup and coherent progress snapshot; it does not open the vault or touch its filesystem.
- **`hello`** → names the vault this connection is for (`vault_root`, a path). The daemon derives the vault's identity itself (never trusting a client-supplied hash), resolves or opens its warm context, and answers `ready`. From that point the rest of the connection is a normal MCP session — the daemon hands the (possibly already-pipelined) remaining bytes straight to the MCP server for that vault.
- Anything else, or a protocol-version mismatch, gets a one-line `error` frame and the connection closes.

A connection error never crashes the daemon — each connection runs in its own task, and a failure is logged as a single stderr line.

## Multi-vault behavior

Because vaults are named per-connection rather than baked into the socket path, one daemon serves as many vaults as clients ask for, each with its own independently warm, independently verified context. Two connections naming the same vault share one context; concurrent first-touches of the *same* vault open it exactly once (the second waits on the first rather than racing a duplicate open). Contexts for different vaults never contend with each other.

## The supervisor (`norn service`)

`norn service` supervises `norn serve` as a launchd user agent on macOS, so the daemon comes up at login and stays up (`KeepAlive` + `RunAtLoad`) instead of needing a terminal tab or a hand-rolled supervisor. There is exactly one managed unit — the serve daemon — so there is no unit selector, only verbs:

```sh
norn service install     # render the plist and load it (idempotent)
norn service uninstall   # unload and remove the plist; config and logs are kept
norn service start       # load an installed-but-stopped daemon
norn service stop        # unload the daemon (an honest stop — see below)
norn service restart     # kill and rerun the loaded daemon
norn service status      # host launchd state + a live control-ping
norn service status --vault PATH  # also report one vault's serving/writer state
```

Every verb accepts `--format text|json` (default `text`); `json` always emits a machine-readable object, even on failure. `norn service` is macOS-only today — on any other host every verb refuses with:

```
norn service requires macOS (launchd)
  run `norn serve` under your supervisor of choice; systemd support is planned
```

### install / uninstall

`install` resolves the running binary's own path (absolutized, but **not** symlink-resolved — a Homebrew symlink is the stable launcher path; the versioned `Cellar` target it points at dies on the next upgrade), renders a LaunchAgent plist naming that binary, creates the plist's and the log's parent directories if needed, writes the plist, and bootstraps the unit:

```
$ norn service install
serve installed
  binary /opt/homebrew/bin/norn
  socket ~/.cache/norn/run/norn.sock
  plist  ~/Library/LaunchAgents/com.dbtlr.norn.serve.plist
  log    ~/.cache/norn/log/serve.log
```

`--format json` emits `{"action":"install","ok":true,"binary":…,"plist":…,"log":…,"socket":…}`. `install` is idempotent (bootout-first), so re-running it after a binary upgrade re-renders the plist against the new path and reloads.

`uninstall` unloads the unit (if loaded) and removes the plist; **config and logs are kept**. Text output is `serve uninstalled (config and logs kept)`, or `serve: not installed (nothing to remove)` when there was nothing to tear down; `--format json` emits `{"action":"uninstall","ok":true,"was_present":bool,"removed_plist":bool}`.

### start / stop / restart

The three lifecycle verbs act **only on an installed unit** and follow one rule: exit 0 iff the daemon ended in the requested state through this call (or `start` found it already running); a no-op on a unit that was never installed, or already in the requested state for `stop`/`restart`, is a *reported* no-op at exit 1 — so a deploy chain doesn't silently proceed past a daemon that was never there.

| Outcome | Text | JSON | Exit |
|---|---|---|---|
| Acted (start/stop/restart ran) | `serve started` / `serve stopped` / `serve restarted` | `{"action":"start","ok":true}` | 0 |
| `start` on an already-running unit | `serve already running` | `{"action":"start","ok":true,"reason":"already running"}` | 0 |
| `stop`/`restart` on a loaded-but-stopped unit | `serve: not running — nothing to stop` (stderr) | `{"action":"stop","ok":false,"reason":"not running"}` | 1 |
| any verb with nothing installed | `serve: not installed — nothing to start` (stderr) | `{"action":"start","ok":false,"reason":"not installed"}` | 1 |

`start` on a unit that is loaded but not currently running (e.g. crash-throttled between `KeepAlive` respawns) kickstarts it rather than reporting "already running" — that state is a genuine transition, not a no-op. `stop` is an **honest stop**: it unloads the unit rather than just killing the pid, because `KeepAlive` would otherwise respawn a killed process immediately.

### status

`status` reports what it knows rather than failing: a `launchctl` probe failure renders as `launchd state unavailable` (carrying the probe's error text) instead of aborting, and if the live control socket still answers, the running version/build/uptime are shown regardless — a daemon that answers the socket never reads as dead just because `launchctl` hiccuped. `status`'s exit code is a health-gate signal distinct from the acting verbs: it is `0` for every *known* state (running, stopped, not installed) and `1` only when the launchd state itself is unknown, so `norn service status || alert` fires on genuinely unknown supervision state, not on a healthy stopped daemon.

Plain `norn service status` stays host-level. Add `--vault PATH` when diagnosing one vault: the client canonicalizes the path and asks the daemon for that vault's `serving` state (`cold`, `opening`, or `ready`) plus `writer_progress` (`busy` and an opaque monotonic `sequence`). A cold status observation does not open the vault. The sequence is not a timestamp or work count; compare it with a later observation only. It belongs to that vault for the daemon lifetime, so evicting and later reopening a warm context does not reset it. A busy writer whose sequence advances is making progress, while stall timing/classification is owned by the client wait policy rather than this status command.

A real run against a daemon that predates a local rebuild:

```
$ norn service status
serve: loaded, running (pid 73414)
  running v0.46.0 · on-disk v0.46.0 — restart pending (rebuilt)
  uptime 17h42m
  socket ~/.cache/norn/run/norn.sock
  plist  ~/Library/LaunchAgents/com.dbtlr.norn.serve.plist
  log    ~/.cache/norn/log/serve.log
```

```
$ norn service status --format json
{
  "loaded": true,
  "running": true,
  "launchd_error": null,
  "pid": 73414,
  "running_version": "0.46.0",
  "on_disk_version": "0.46.0",
  "running_build": null,
  "on_disk_build": "4bc0446af25a63572e1e7ac3782c4e512ecc4d15c55a145ce8ff474123b4447f",
  "restart_pending": true,
  "uptime_secs": 63771,
  "plist": "~/Library/LaunchAgents/com.dbtlr.norn.serve.plist",
  "log": "~/.cache/norn/log/serve.log",
  "socket": "~/.cache/norn/run/norn.sock"
}
```

A scoped text report adds three lines without changing the host-level fields:

```text
$ norn service status --vault ~/vaults/atlas
serve: loaded, running (pid 73414)
  running v0.48.0 · on-disk v0.48.0
  uptime 3m12s
  vault  /Users/example/vaults/atlas
  serving ready
  writer  idle · sequence 17
  socket ~/.cache/norn/run/norn.sock
  plist  ~/Library/LaunchAgents/com.dbtlr.norn.serve.plist
  log    ~/.cache/norn/log/serve.log
```

In JSON, those values are grouped under an additive `vault` object:

```json
{
  "vault": {
    "root": "/Users/example/vaults/atlas",
    "serving": "ready",
    "writer_progress": {
      "busy": false,
      "sequence": 17
    }
  }
}
```

If the daemon does not answer with scoped state (for example, it is stopped or is an older build), `serving` and `writer_progress` are `null` and the text report renders them as unavailable. The canonical root is still shown so the diagnostic scope is explicit.

The second line is the running-vs-on-disk reconciliation, and it renders differently depending on what's out of sync:

| Condition | Second line |
|---|---|
| running and on-disk match (version *and* build) | `running v0.45.1 · on-disk v0.45.1` |
| running daemon is an older **version** | `running v0.44.0 · on-disk v0.45.1 — restart pending` |
| running daemon is the same **version** but a different **build** (a local rebuild) | `running v0.45.1 · on-disk v0.45.1 — restart pending (rebuilt)` |
| daemon loaded but not answering the control socket | `on-disk v0.45.1 · no answer on the control socket` (no `uptime` line) |
| unit not loaded at all | first line reads `serve: not loaded` |

`restart_pending` is `true` whenever a daemon answered the socket and its self-reported version *or* build fingerprint differs from the on-disk binary the plist would launch — including a daemon whose pong predates the build fingerprint field at all, which can't match and so always pends. Either way the fix is the same: `norn service restart`.

## How the CLI routes to the daemon

Every one of the `norn` CLI's reads (`count`, `find`, `get`, `repair --plan`) and mutations (`set`, `edit`, `new`, `move`, `delete`, `rewrite-wikilink`, `apply`) is also a client of the daemon above. On each invocation the CLI decides, before doing anything else, whether a live warm daemon can serve the request or whether to run the same direct, integrity-verified path it always has. This section is the operator contract for that decision — what an operator observes, not the implementation.

### When routing happens

The probe is a well-known-socket check, O(1) in the common case: no socket file means no daemon, so the cost is one `stat` and every command runs the exact same direct path as if the daemon had never existed. Only when a socket is present does the CLI pay a short handshake (bounded at 250ms by default) to prove the daemon is alive and requires it to report the **exact same released version and the exact same build fingerprint** as this CLI binary before routing. Anything short of that — no socket, a timeout, a version or build mismatch, a protocol mismatch — resolves to Direct.

### What never routes

A few shapes are deliberately excluded from routing, by design, regardless of whether a daemon is live:

- **`--config` / `--no-cache-refresh`.** The wire speaks canonical vault roots only, never a config path — the daemon always loads each vault's own default `.norn/config.yaml` and always serves from a freshly self-healed cache. Either flag forces the direct path so its meaning is honored exactly rather than silently ignored.
- **Interactive TTY confirm prompts.** An unconfirmed, interactive mutation (no `--yes`, no `--format json`, running at a terminal) previews, prompts, and applies as one conversation that holds the mutation lock across the prompt — the daemon can't drive your terminal, so this whole flow stays Direct. (Most mutations detect "interactive" on stdin being a terminal; `norn new` keys off stdout instead.) `--dry-run`, `--yes`, `--format json` (implicit preview), and non-interactive invocations all route normally.
- **`--body-from-stdin`.** There is no wire-faithful way to forward stdin to an MCP tool call, so a wholesale body replacement from stdin always runs Direct.
- Two narrower per-command exceptions with no wire-faithful analogue: structured `get --section` (the wire's keyed section object cannot preserve request order for the records renderer), and bare `repair` with no `--plan` (the wire tool only ever returns a `MigrationPlan`, so repair's read-only summary mode has none). `get --format markdown` does route even when `--section` is present, because that flag is inert for Markdown: `vault.get` reads the resolved source file at request time and returns its exact content in a format-specific envelope, while the client preserves the ignored-section warning.

### Reads

A routed call has **no overall call timeout**. While its request socket is waiting for a response, the client heartbeats the vault-scoped control plane and interprets `writer_progress` on its own monotonic clock:

- A responsive idle writer is healthy. Its sequence does not need to change.
- A busy writer whose sequence advances is making progress, so the client waits indefinitely. A long chunked operation may outlive five seconds in total as long as it keeps publishing progress.
- No compatible scoped pong for five seconds, or a busy writer whose sequence is unchanged for five seconds, classifies the service as stalled. This is the one service-level stall budget; sequence changes reset it. An indivisible writer step that publishes no transition inside the budget is therefore classified as stalled even if it is merely slow.

Non-writer tool-body progress is deliberately outside this signal. If scoped pongs stay healthy and the writer stays idle, the client continues waiting even though the tool body itself has no separate progress stamp.

A read (`count`/`find`/`get`/`repair --plan`) still falls back to Direct on any daemon-side failure because it is safe to retry. Ordinary transport failures stay silent unless `--verbose`; a heartbeat-classified stall is always actionable and prints:

```text
norn: service stopped responding or making writer progress — run `norn service restart`; using direct execution
```

The command then pays for a verified Direct open instead of trusting a stalled daemon.

### Mutations: the send-commit policy

A mutation can't retry as freely as a read, because retrying after the daemon may have already written would risk applying it twice. The CLI splits on **whether the request reached the daemon**:

- **Before the daemon call is sent** (forced-Direct flags, no live daemon, a failed handshake, or a heartbeat-classified stall while waiting for `ready`/`initialize`) — falls back to a direct re-run, exactly like a read. The mutation never ran anywhere. A heartbeat-classified stall prints the restart note above.
- **After the request is sent, for a committing apply** (`--yes` — the only shape that commits over the wire; a non-interactive invocation without `--yes` is an implicit *preview*, covered below) — any failure, including a heartbeat-classified stall, does **not** retry Direct. The CLI surfaces `post-send-uncertain` (exit 1, "verify vault state before retrying") because the daemon may already have applied the change; re-running blind could double-apply it. See [Error and outcome contract](errors.md) for the full exit-code and error-code contract.
- **A clean daemon-side refusal** (a coded precondition failure — stale hash, unknown path, a schema refusal, a lock timeout, and so on) comes back as a normal, coded refusal and renders **exactly like the same refusal would on the direct path** — exit 2, nothing written. This is not the uncertainty case: a refusal is proof the daemon didn't write anything.
- **Dry-runs and previews** (`--dry-run`, `--format json` without `--yes`, the non-TTY implicit-preview path) write nothing either way, so they route with the same full silent-fallback behavior as a read.

### Version and build skew

A daemon that answers but reports a different released version, *or* a different build of the same version (two builds of `0.x` can carry different wire schemas even with an identical version string), never serves a request — it fails the handshake gate and the CLI falls back to Direct. This is the one routing failure worth telling you about: it's actionable, and staying silent would leave the CLI quietly running Direct forever after an upgrade with no visible cause. You'll see exactly one stderr line, once per invocation:

```
norn: service is v0.44.0, client is v0.45.1 — restart the norn serve daemon
```

or, for a same-version rebuild:

```
norn: service is a different build of v0.45.1 — restart the norn serve daemon
```

`norn service status` shows the same condition as `restart pending` / `restart pending (rebuilt)` (see [status](#status) above). Plainly stated: after you upgrade or rebuild `norn`, the still-running daemon stops serving requests — silently, on every command — until you run `norn service restart`. Nothing breaks in the meantime; every command just runs the direct path it always could.

`norn self-update` closes this gap for itself: after it swaps in a new binary, it restarts a loaded `serve` unit automatically (`kickstart -k`), so the daemon picks up the update without a manual `norn service restart`. If the restart itself fails, `self-update` still exits successfully — the binary is updated either way — and prints a stderr warning pointing back at `norn service restart` as the fallback.

### Byte-identity promise

Successful routed and direct execution remain byte-for-byte identical: stdout, stderr, exit code, and the on-disk result of a mutation — the telemetry `trace_id` aside, which is non-deterministic on the direct path too. Routing failures may add an operator-actionable stderr notice for version/build skew or a heartbeat-classified stall before falling back to the same trust-verified Direct path. A committing mutation that fails after send remains the deliberate `post-send-uncertain` exception described above.

## Relationship to `norn mcp`

[`norn mcp`](mcp-server.md) is the one-shot stdio server: one process per vault, launched and torn down by the client's own process lifecycle, re-verifying its cache on every call. `norn serve` is the *same* MCP server and the *same* tool catalog and mutation-safety contract, made persistent and shared across vaults — the difference is entirely in how the cache is held open and verified, not in what the tools do. An MCP client that wants the warm daemon connects to it directly over the socket described above; a client that just wants a simple one-shot server per vault keeps using `norn mcp`.

The `norn` CLI is itself one such client: every read and every mutation it supports routes through a live, version- and build-matched `norn serve` daemon when one is reachable, byte-identically to running direct — see [How the CLI routes to the daemon](#how-the-cli-routes-to-the-daemon) above.

## See also

- [MCP server](mcp-server.md) — the tool catalog and mutation-safety contract the daemon shares with `norn mcp`.
- [Cache](cache.md) — the per-vault cache the daemon holds warm.
- [Error and outcome contract](errors.md) — the exit-code and error-code contract a routed mutation's `post-send-uncertain` and coded refusals draw on.
