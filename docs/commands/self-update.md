---
title: self-update
description: Update norn to the latest GitHub release.
---

# norn self-update

Update norn to the latest GitHub release. Works only when norn was installed via the official GitHub install script — for a `cargo install`, Homebrew, or source build, use that tool's update mechanism instead.

If a `norn service` unit is loaded, a successful update restarts it automatically so the daemon picks up the new binary — see [Version and build skew](../service.md#version-and-build-skew).

## Examples

```bash
norn self-update
# update to the latest release

norn self-update --dry-run
# resolve the target version and print the plan; change nothing

norn self-update --dry-run --format json
# scriptable "is an update available?" check

norn self-update --version 0.30.0
# install a specific version (downgrades allowed)
```

## Options

| Flag | Effect |
|---|---|
| `--version <X.Y.Z>` | Install a specific version. Defaults to the latest release. Downgrades allowed. |
| `--dry-run` | Resolve the target and print the plan without downloading or modifying anything. |
| `--format text\|json` | Output shape. `text` on a TTY, `json` when piped. |

## See also

- Run `norn self-update --help` for the full flag reference.
