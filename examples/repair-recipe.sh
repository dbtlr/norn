#!/usr/bin/env bash
# Generic detect -> plan -> dry-run -> apply -> verify recipe.
#
# Run from a vault directory (or pass it via VAULT_DIR=/path/to/vault).
# The script is intentionally simple and shellcheck-clean — it's a starting
# point you copy and adapt to your own workflow, not a load-bearing tool.
#
# Usage:
#   ./examples/repair-recipe.sh                       # against $PWD
#   VAULT_DIR=/path/to/vault ./examples/repair-recipe.sh

set -euo pipefail

VAULT_DIR="${VAULT_DIR:-$PWD}"
PLAN_FILE="${PLAN_FILE:-repair.json}"

echo "norn repair recipe"
echo "  vault dir: $VAULT_DIR"
echo "  plan file: $PLAN_FILE"
echo

# 1. Snapshot the current state with a git tag for easy rollback.
#    Skip silently if the vault isn't a git repo.
if git -C "$VAULT_DIR" rev-parse --git-dir >/dev/null 2>&1; then
  SNAPSHOT_TAG="snapshot/vault-repair-$(date +%Y%m%d-%H%M%S)"
  git -C "$VAULT_DIR" tag "$SNAPSHOT_TAG"
  echo "snapshot tag: $SNAPSHOT_TAG"
  echo
fi

# 2. Detect: how much drift is there?
echo "step 1: validate --summary"
norn -C "$VAULT_DIR" validate --summary --format json
echo

# 3. Plan: write a repair plan artifact for review.
echo "step 2: repair --plan --out $PLAN_FILE"
norn -C "$VAULT_DIR" repair --plan --out "$PLAN_FILE"
echo "plan written"
echo

# 4. Dry-run: confirm the plan applies cleanly without writing.
echo "step 3: migrate $PLAN_FILE --dry-run"
norn -C "$VAULT_DIR" migrate "$PLAN_FILE" --dry-run --format json
echo

# 5. Apply. Every frontmatter write is checked against its intended value
#    before apply reports success (no separate verify flag to pass).
echo "step 4: migrate $PLAN_FILE"
norn -C "$VAULT_DIR" migrate "$PLAN_FILE" --format json
echo

# 6. Verify: re-run validate as the post-hoc check across the whole vault.
echo "step 5: validate --summary"
norn -C "$VAULT_DIR" validate --summary --format json
echo

# 7. Show what changed in git, if applicable.
if git -C "$VAULT_DIR" rev-parse --git-dir >/dev/null 2>&1; then
  echo "step 6: git diff summary"
  git -C "$VAULT_DIR" diff --stat
  echo
  echo "to roll back: git -C $VAULT_DIR reset --hard $SNAPSHOT_TAG"
fi

echo "done"
