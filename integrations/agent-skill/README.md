# norn agent skill

A single harness-independent skill that teaches a coding agent how to drive the `norn` CLI safely. The skill body lives in [SKILL.md](SKILL.md). This README is the install-and-adapt guide.

## Two install paths

The coding-agent ecosystem has standardized on `.agents/skills/` for every harness except Claude Code, which uses `.claude/skills/`. That gives us exactly two install paths regardless of which agent you use:

| Harness | Install path |
|---|---|
| Claude Code | `.claude/skills/norn/SKILL.md` |
| Everything else (Codex, Open Code, OpenClaw, Hermes, PI, ...) | `.agents/skills/norn/SKILL.md` |

The skill body in `SKILL.md` is identical for both. Only the install location and (optionally) the frontmatter quirks differ.

## Claude Code

Copy `SKILL.md` into one of:

- **Personal:** `~/.claude/skills/norn/SKILL.md`
- **Plugin or project:** `<project>/.claude/skills/norn/SKILL.md`

Claude Code reads the frontmatter `name` and `description` fields to decide when to trigger the skill. The bundled frontmatter is already shaped correctly — see the top of [SKILL.md](SKILL.md) for the current `name`, `description`, and `version` rather than duplicating them here, where they'd drift out of sync.

Optional Claude-specific extension: add an `allowed-tools` field to the frontmatter to pre-permit the `Bash` tool for `norn *` invocations. Example:

```yaml
---
name: norn
description: ...
allowed-tools:
  - Bash
---
```

Restart Claude Code (or run `/refresh-skills` if your version supports it) after installing.

## All other coding agents

Copy `SKILL.md` into `<workspace-root>/.agents/skills/norn/SKILL.md`. Most harnesses pick up the skill on the next session.

Per-harness frontmatter quirks (none required; these are optional adaptations):

### Codex

No frontmatter additions needed. Codex reads `name` and `description` from the bundled frontmatter directly.

### Open Code

No frontmatter additions needed. Open Code's skill loader matches Codex.

### OpenClaw

OpenClaw supports an optional `metadata.openclaw.priority` integer field for ordering skills when multiple match. Add it to the frontmatter only if you have several skills competing for the same triggers.

### Hermes

Hermes supports an optional `metadata.hermes.tags` list field for cross-skill linking. Add it like this:

```yaml
---
name: norn
description: ...
metadata:
  hermes:
    tags:
      - markdown
      - vault
      - validation
---
```

### PI

No frontmatter additions needed. PI reads `name` and `description` from the bundled frontmatter.

## Adding a new harness

If you're using a coding agent not listed above, install to `<workspace-root>/.agents/skills/norn/SKILL.md` first and see whether it picks up the skill. Most harnesses do.

If your harness needs a frontmatter quirk to discover or trigger the skill, please open a PR adding a subsection under "All other coding agents" rather than introducing a new install path. We deliberately keep this to two paths total — Claude Code, and everything else.

## Verifying the install

After installing, ask your agent something like:

> Inspect the vault at ./my-notes with norn and tell me how many documents are missing a title field.

A well-installed skill should produce a `norn -C ./my-notes validate --summary --format json` invocation, parse the JSON, and answer with the count from `fields.title`.

If the skill doesn't trigger, check that:

1. The file lives at the exact install path above (case-sensitive on Linux).
2. The frontmatter is valid YAML with `---` delimiters on their own lines.
3. The agent has been restarted or had its skill cache refreshed.
4. The `norn` binary is on the agent's `PATH` (most harnesses inherit the user's `PATH`).

## See also

- [SKILL.md](SKILL.md) — the harness-independent skill body.
- [../../docs/agent-workflows.md](../../docs/agent-workflows.md) — the full agent-facing workflow guide.
- [../../README.md](../../README.md) — project landing page.
