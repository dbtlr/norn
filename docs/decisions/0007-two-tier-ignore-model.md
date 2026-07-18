---
title: "0007 — Two-tier ignore: `files.ignore` excludes from the graph, `validate.ignore` exempts from validation"
description: "Architectural decision fixing norn's two orthogonal path-relaxation tiers — files.ignore (out of the graph entirely) and validate.ignore (indexed and in-graph but unvalidated) — and rejecting a third per-finding-code tier on the grounds that indexing is independent of validation."
---

# 0007 — Two-tier ignore: `files.ignore` excludes from the graph, `validate.ignore` exempts from validation

**Decision:** norn has exactly **two** ways to relax its treatment of a path, and they are orthogonal, not points on a spectrum:

- **`files.ignore` — out of norn entirely.** The parser never reads the file, so it is not indexed, not a graph member, not a resolvable link target, and not validated. Links *into* an ignored path become `link-target-missing` — the target is outside norn by definition. The tier for content norn should not know about (e.g. `Archive/**`, so archived stems stop shadowing live hubs).
- **`validate.ignore` — indexed, in-graph, unvalidated.** The path is fully indexed (its frontmatter is queryable via `find`/`get`), it is a resolvable link target, and its valid outbound links are edges — but it is exempt from validation: no frontmatter-schema findings and no broken-link findings. Broken links inside it are stored but never reported; its frontmatter need not satisfy the standards pack. **Full-file exemption only** — there is no per-finding-code path suppression. The tier for *frozen records* norn should track but not police (e.g. `artifacts/session-logs/**`, `artifacts/scratch/**`).

The load-bearing insight that makes two tiers sufficient: **indexing is independent of validation.** `validate.ignore` turns validation off while leaving indexing on, so a validation-exempt session-log stays queryable (`find type=session-log`) even though it is never policed. This is why no third, finer tier is needed.

## Context

A grooming pass over the frozen-records bugs surfaced a proposed "new primitive" that would partially disconnect a file from the graph and fully from validation. Reading the code showed it **already exists as `validate.ignore`** (`src/standards/engine.rs:237-254`): it skips the whole document in the validation loop while leaving `GraphIndex`/cache untouched, so the doc stays indexed and resolvable.

Meanwhile `files.ignore` was found effectively **inert**: `options.ignore` is hardcoded empty at both cache-write sites (`src/cache/writer.rs:50,121`), the compiled `files_ignore` field is dead code with a "no live consumer" comment (`src/standards/config.rs:506`), and the only enforcement is a read-time `retain()` that drops the doc from the returned list without retracting other docs' already-resolved links into it — so ignored docs stay link-resolvable. The config already had the intended two-tier shape; **one tier was broken and the other undocumented.**

**Alternatives rejected:**

- *"Ignore = graph membership, still addressable by path"* (first reframe proposed) — would let `get`/`set`/`new`/`delete`/`move` all operate on ignored files by direct path. Rejected: it muddies "ignored" into "half-present." Once `files.ignore` truly excludes at cache-build, the doc simply isn't in the cache, so every verb uniformly fails to resolve it — no special addressability rule is needed.
- *Per-finding-code path suppression* (abandoned) — keep the frontmatter contract on frozen records but drop only link findings. Rejected: because indexing ≠ validation, full exemption **already** preserves queryability — session-logs stay found by `find type=session-log` without keeping validation on. The finer granularity bought nothing the two tiers don't already give, at the cost of a larger config surface and precedence rules.

## Consequences

- **`files.ignore` must be wired into the cache build** so ignored files are never parsed/resolved (the keystone); the dead `files_ignore` field goes away. Live→ignored links legitimately become `link-target-missing`.
- **Frozen records norn should still track belong in `validate.ignore`, not `files.ignore`.** In particular `artifacts/scratch/**` moves `files.ignore` → `validate.ignore`, which also dissolves the `delete`/`move` verb asymmetry: once ignore excludes at cache-build, a truly-ignored path is addressable by *no* verb, and a validation-exempt path is addressable by *all* of them.
- **One clause remains to verify**: a genuinely malformed-YAML file is not indexed regardless of tier (its fields can't be parsed), so "need not have valid frontmatter" holds for *schema-invalid* frontmatter but not *parse-failed* frontmatter. If frozen records must tolerate parse-failed YAML too, that is a separate, narrower fix.
- **No per-code validation suppression exists or is planned.** If a real need for "keep schema, drop links on a path" reappears, it reopens this ADR rather than bolting on a third tier.
