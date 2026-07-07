---
title: completions
description: Install shell completions or emit a completion script.
---

# norn completions

Install tab-completions into the user's shell, or emit a completion script to stdout.

## Subcommands

| Command | Purpose |
|---|---|
| `norn completions install [SHELL]` | Install completions into the shell config. Auto-detects `$SHELL` if omitted. |
| `norn completions init <SHELL>` | Emit a completion script to stdout. |

Supported shells: bash, zsh, fish, powershell, elvish, nushell.

## Examples

```bash
norn completions install
# detect the shell and wire completions into its config (idempotent)

norn completions install zsh --print
# preview what would be written; change nothing

norn completions init fish > ~/.config/fish/completions/norn.fish
# emit a script and place it yourself
```

`completions install` is idempotent — re-running matches the existing install marker and no-ops; `--force` overwrites the marker block.

## See also

- Run `norn completions --help` for the full subcommand list.
