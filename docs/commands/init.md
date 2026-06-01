---
title: init
description: Scaffold a .norn/config.yaml in the current vault.
---

# norn init

Scaffold a `.norn/config.yaml` in the current directory — the file that declares your vault's rules and config. Run it once per vault to start enforcing standards.

## Examples

```bash
norn init
# scaffold .norn/config.yaml in the current directory

norn -C /path/to/vault init
# scaffold in another directory

norn init --force
# overwrite an existing config
```

## Behavior

`init` writes a starter `.norn/config.yaml` with commented examples of the common rule shapes. It refuses to overwrite an existing config unless `--force`. After scaffolding, edit the file (or run [`norn config edit`](config.md)) to declare your rules, then run [`norn validate`](validate.md) to check the vault against them.

## See also

- [Configuration](../configuration.md) — every config key.
- [`config`](config.md) — show, validate, migrate, or edit the config.
- [`validate`](validate.md) — run the rules `init` scaffolds.
- Run `norn init --help` for the full flag reference.
