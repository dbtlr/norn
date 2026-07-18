---
title: "0001 — Documentation information architecture — four self-sufficient surfaces"
description: "Architectural decision establishing that norn's documentation surfaces (CLI --help, bundled agent SKILL, per-command docs, and README) are each written to be self-sufficient for their reader, with deliberate overlap managed by a release checklist rather than deduplication."
---

# 0001 — Documentation information architecture — four self-sufficient surfaces

norn's documentation lives on four surfaces, each written for a different reader in a different context: the CLI's own `--help` (deliberately deeper than clap defaults, with a `-h` short / `--help` long split), the bundled agent **SKILL.md** (installed by the CLI into a user's repo with no access to norn's source on disk), the **per-command docs** under `docs/commands/` (linked directly as Markdown from the README today, becoming the first draft of an Astro/Starlight site at norn.run), and the **README** front door.

**Decision:** each surface is written to be self-sufficient for its reader. Overlap between surfaces is deliberate and is *not* deduplicated. This is prose, not code — who is reading (human at a terminal, human on the web, an agent offline), with what tool, online or offline, changes what to say even about the same flag. Drift between surfaces is managed by a release-checklist step, not by architecture or generation.

## Considered options

- **Thin SKILL that points to the docs.** Rejected: the SKILL is installed into a foreign repo with no norn source on disk and no guaranteed network, so it must stand alone offline.
- **Defer flag enumeration to `--help` everywhere; keep docs/SKILL thin.** Rejected: it makes the docs non-self-sufficient and fights the norn.run website goal, where a command page is expected to be complete on its own.
- **Generate the SKILL and docs from a shared source (clap introspection or markdown partials).** Deferred, not rejected. Premature for a pre-1.0, single-consumer tool. Left as a visible future option once norn.run is real; the cost it removes (curation discipline) is currently cheap.

## Consequences

- Each surface owns the slice of truth it keeps truest. `--help` is compiled from clap, so it cannot drift from the binary — it is authoritative on flags, and is built deep on purpose (`-h` compact, `--help` full descriptions + examples + possible-values).
- Per-command pages are self-sufficient: purpose → worked examples → options → output/apply-model → recipes → see-also. They ship Starlight-ready (`title` + `description` frontmatter, relative links, plain CommonMark) so the README's relative links swing to norn.run URLs later with no rewrite of the pages themselves.
- The SKILL stays a full offline agent manual; its escape hatch is norn.run links plus the deep `--help`.
- The release checklist must re-check the SKILL and the per-command docs against the current CLI on every release. The SKILL drifted a full minor + patch behind the CLI (v0.36.0/v0.36.1) precisely because no such step existed — see *the SKILL-update-and-dogfood-loading note (internal design doc)*.
