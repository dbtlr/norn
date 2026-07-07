---
title: config
description: Show, validate, migrate, or edit the per-vault .norn/config.yaml.
---

# norn config

Manage the per-vault `.norn/config.yaml` — the file that declares your rules and config. `config` inspects and maintains that file; [`norn init`](init.md) creates it.

## Subcommands

| Command | Purpose |
|---|---|
| `norn config show` | Show the effective config: resolved paths and counts. |
| `norn config validate` | Validate the config file itself (not the vault). |
| `norn config migrate` | Migrate the config to the current schema version. |
| `norn config edit` | Open the config in `$VISUAL` or `$EDITOR`. |

## Examples

```bash
norn config show
# effective paths, counts, and where the cache and logs live

norn config show --format json
# machine-readable snapshot for pipelines

norn config validate
# check the config file for errors before relying on it

norn config validate --format json
# machine-readable validation findings

norn config edit
# open .norn/config.yaml in your editor

norn config migrate
# upgrade an older config to the current schema
```

## Options

| Flag | Subcommand | Effect |
|---|---|---|
| `--format records\|json\|jsonl` | `show`, `validate` | Output format. Defaults to `records` regardless of TTY/pipe. |
| `--no-pager` | `show` | Bypass the pager even for TTY `records` output. |
| `--no-validate` | `edit` | Skip auto-validation after the editor exits. |

## See also

- [Configuration](../configuration.md) — every config key and rule shape.
- [`init`](init.md) — scaffold a new config.
- [`validate`](validate.md) — validate the vault, vs `config validate`, which checks the config file.
- Run `norn config --help` for the full subcommand list.
