# Examples

Generic, copy-pasteable starter files for norn. Everything here uses generic Markdown vault terminology — adapt the field names, status vocabularies, and path globs to match your own vault's doctrine.

## config-minimal.yaml

Minimum viable `.norn/config.yaml` — just `files.ignore` patterns. Use this as a starting point when you want `norn` to walk a vault without enforcing any standards yet.

```bash
cp examples/config-minimal.yaml /path/to/vault/.norn/config.yaml
norn -C /path/to/vault validate --summary
```

## config-typed-notes.yaml

A worked config showing the full shape of `validate.rules` and `repair.rules`:

- Documents with `type: note` must have a `kind`, and a few common fields must match their expected shape when present.
- Documents with `type: task` must have a `status` from a fixed vocabulary and must live under `tasks/`.
- A legacy `status: someday` value repairs to `status: backlog`.

```bash
cp examples/config-typed-notes.yaml /path/to/vault/.norn/config.yaml
norn -C /path/to/vault validate --summary
norn -C /path/to/vault repair --plan --out repair.json
```

## repair-recipe.sh

Executable detect -> plan -> dry-run -> apply -> verify shell recipe. Tags a git snapshot first if the vault is a git repo, so rollback is one command.

```bash
./examples/repair-recipe.sh                       # against the current directory
VAULT_DIR=/path/to/vault ./examples/repair-recipe.sh
```

Read the script before running it against a real vault — it's small enough to skim end-to-end.

## See also

- [Configuration guide](../docs/configuration.md) — the `.norn/config.yaml` schema.
- [Validate rule shape](../docs/rule-shape.md) — the selector + constraint conceptual model.
- [Validation and repair](../docs/validation.md) — finding codes, the apply contract, and more recipes.
