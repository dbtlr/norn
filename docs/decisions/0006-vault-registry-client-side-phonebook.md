---
title: "0006 — The vault registry is a client-side phonebook"
description: "Architectural decision reintroducing a client-side vault name-to-path registry for norn, scoped strictly as a phonebook separate from vault configuration and resolved only at the CLI/MCP entry point. Amended 2026-07-17 (ADR 0017): promoted from phonebook to norn's central config and the authoritative source of vault identity."
---

# 0006 — The vault registry is a client-side phonebook

> **Amended by [0017](./0017-registered-vaults-summoned-owners.md) (2026-07-17):** the registry is promoted from name→path phonebook to norn's central config — the authoritative source of vault identity and the gate for durable artifacts; "registry-blind" becomes "non-registered tolerant." The phonebook scoping below is historical. See the dated amendment at the end.

**Decision:** norn reintroduces a machine-local **vault registry** — a mapping of short names → vault paths — and constrains it to exactly that: **a phonebook, not a config surface.** Everything that defines a vault (schema, rules, index options) stays in the vault's own `.norn/`; the registry holds name → path and nothing else. Names are resolved **client-side**, at the CLI/MCP entry point: the resolver turns a name into a canonical path before any routing happens, so **the wire speaks canonical paths only** and `norn-service` never consults the registry. A registry that is stale, missing, or absent entirely leaves daemon behavior unaffected; a stale entry fails loudly at the client ("no vault at the path 'docs' points to"), never by quietly serving the wrong data.

## Context

An earlier registry was deleted deliberately: the risk was that it would grow into a second config surface — hidden global state that agents and git repos cannot see. The question it answered ("how do you target a vault when not inside one?") was then narrowed to "path/`-C` targeting needs no registry," which held while routing derived per-vault socket paths from `vault_identity()`.

Two things changed (2026-07-04). First, the service topology moved to a single host daemon (ADR 0005 amendment), so socket-path derivation no longer carries the multi-vault routing story — the canonical path travels in the connection preamble. Second, and decisively, the real forcing function was named: **`norn -C /long/absolute/path <command>` is a bad agent surface.** Every invocation from a source repo carries the full vault path, every repo's MCP server config duplicates it, and a forgotten flag silently targets the wrong place.

The phonebook unlocks, in order of value:

1. **Repo binding.** A source repo commits a small pointer file carrying the vault's *name* (not its path); any `norn` invocation walks up, finds the binding, resolves the name through the local registry, and targets the vault — plain `norn find …` just works inside a bound repo. The name indirection is what makes the file committable: the repo says "docs," each machine's registry says where docs lives *there*. (Same pattern as a per-repo tool-config pointer file.)
2. **Short-name targeting anywhere** — a name flag from any directory.
3. **Zero-arg `norn mcp` from a bound repo** — identical MCP config across repos.
4. **Daemon niceties** — `norn serve --status` can render names instead of 64-hex hashes; optional pre-warm of registered vaults. Garnish, not needs.

**Alternatives rejected:**

- *No registry (status quo)* — leaves the agent surface as absolute paths everywhere; repo binding files would have to commit machine-specific paths, which don't survive a second machine.
- *Registry as config* — the shape that got the first registry deleted. Vault doctrine drifting into a hidden global file breaks the "config travels with the vault" property that makes vaults portable and agent-legible.
- *Daemon-side name resolution* — would make routing depend on mutable global state; a stale registry could route a request to the wrong vault's data. Client-side resolution keeps routing pure derivation and makes staleness fail loudly at the edge.

## Consequences

- **Resolution order** at the CLI entry becomes: explicit path (`-C`) → explicit name → repo binding file → `NORN_ROOT` → cwd. The exact flag/sigil surface and the binding filename are design work for the registry initiative, not fixed here.
- **The registry is per-machine mutable state** with a narrow contract; it needs its own verbs (add/remove/list) and nothing more.
- **`norn-service` is registry-blind by construction** — the daemon's contract (canonical paths on the wire) is frozen independent of anything the registry does later.
- **Repo binding files commit names**, so they are portable across machines and safe to check in.
## Amendment — 2026-07-17: from phonebook to central norn config; "registry-blind" becomes "non-registered tolerant"

[0017](./0017-registered-vaults-summoned-owners.md) promotes the registry from a name→path phonebook to **norn's central config**: a machine-local map of `name → { vault_root, vault_config, vault_cache, vault_logs, … }`, extensible with authorization keys (the off-network HTTP MCP tier) and with centrally-located per-vault config for consumers that keep nothing in the vault. It is the authoritative source of vault identity and the gate for all durable artifacts, and supports **reverse lookup** — a CWD inside a registered root resolves to that registered vault, making path-based invocation sugar over the registry.

The original "norn-service is registry-blind by construction" pin is amended to **non-registered tolerant**: the daemon may consult the central config (it must, to serve named/HTTP requests and to house per-vault artifact locations), but it never *requires* registration — an unregistered vault is served by an ephemeral-tier owner with tmp-homed everything. Fail-loud on stale/missing entries carries forward. Resolution order (explicit path → explicit name → repo binding → NORN_ROOT → cwd) carries forward with reverse lookup inserted at the cwd step.
