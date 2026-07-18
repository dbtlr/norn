---
title: "0010 — CLI grammar: one canonical form, forgiving inputs"
description: "Architectural decision that every CLI grammar has one canonical spelling while input parsing forgives evidence-mined variants — separator variance, common aliases, vault-gated dynamic field predicates, set positionals, and edit single-op sugar — accepted but never taught."
---

# 0010 — CLI grammar: one canonical form, forgiving inputs

**Decision:** Every norn CLI grammar has exactly **one canonical spelling** — the form docs teach, help shows, and errors echo. Input parsing is deliberately **forgiving**: it accepts predictable variants and silently normalizes them to canonical. Forgiveness is **evidence-earned** — a variant is admitted only when session-mining shows agents actually guess it, never speculatively — and **accepted, never taught**: no forgiven form appears in primary documentation. Outputs, wire contracts, and docs render canonical only; forgiveness is an input-side property (companion to the three-form-identity rule).

## Context

The 2026-07-06 documentation audit mined real agent sessions and found the #1 friction was not semantics but *spelling*: agents hold norn's mental model correctly and render it in a plausible-but-wrong grammar. Measured: ~15 cross-applications of the `--eq field:value` / `--field key=value` separator split (both directions), 7+ bare `k=v` positional guesses on `set`, 6 distinct flag-per-op guesses on `edit` (including norn's own integration test), 3+ independent `--type` guesses, plus `--where`, `--filter`, `--group-by`, `--ops-file`. Every miss costs a round-trip, and reducing agent turns is the mission's mechanism. The design question per guess class: absorb it (forgive), adopt it (make canonical), or teach at the failure point (error). This ADR settles the doctrine: **keep one canon, absorb the predictable variance**.

## The forgiveness rules

1. **Separator variance.** Predicates stay canonical `field:value` (search idiom); assignments stay canonical `key=value` (env/git-config idiom). Both families parse either separator: the separator is the **first `:` or `=` in the token, whichever comes first** — deterministic because keys contain neither, and value-embedded colons (datetimes, URLs) parse correctly.
2. **Common aliases** (hidden from help): `--where`/`--filter` → `--eq`; `--group-by` → `--by`; `count --all` accepted as a no-op for `find` symmetry; `--ops-file F` → read the `edit` ops array from a file. (`--content` on `--str-replace` is *not* a hidden alias — it is the canonical, visible payload flag shared by the section ops, accepted on `--str-replace` too as the anticipated guess; see rule 5.)
3. **Dynamic field predicates** (query family only — every `FilterArgs` surface: `find`/`count`/`describe`). An unknown `--key value` is interpreted as `--eq key:value`, under two mandatory guardrails:
   - **Reserved flags always win.** Built-in flags are never reinterpreted; a vault field named `format` is reachable only via canonical `--eq format:x`.
   - **The field-universe gate.** The key must resolve against **this vault's** known fields (schema-declared ∪ observed frontmatter keys), else hard error with did-you-mean across both flags and fields. This is deliberately vault-specific: without the gate, every typo of a real flag becomes a valid empty query — an agent reads "no results" where it should read "you typo'd" (the silent-empty trap the same audit flagged in sibling tooling).
   - Equality only (the other operators keep canonical flags); repetition desugars to `--in` any-of; a value is required (no bare-flag booleans); never on the mutate family, where `--field k=v` owns assignment.
4. **`set` trailing positionals.** `norn set <doc> [key=value ...]` desugars exactly to `--field` (same coercion, same repeat-accumulates-to-array). First positional is always DOC; later positionals must contain a separator or hard-error. Plain assignment only — `--push`/`--pop`/`--remove`/`--field-json` stay flags.
5. **`edit` single-op sugar** via a generative rule: **flag = op name** (kebab of the op vocabulary), **flag value = the op's anchor**, **companion flags = payload fields named exactly as the JSON fields** (`--new`, `--content`, `--replace-all`). E.g. `--str-replace OLD --new NEW`, `--append-to-section H --content C`, `--delete-section H`. Exactly one op flag per invocation, mutually exclusive with `--edits-json`/stdin, desugaring 1:1 to a one-op array so preview/confirm/exit codes are inherited. The JSON ops array remains the canonical batch form.
6. **Cross-family teaching errors** for flag-name misses that forgiveness can't safely absorb: `set --eq …` errors with a pointer to `--field key=value`; the query family's unknown-key error points at `--eq`.

All rules are additive and non-breaking; nothing here carries a release-timing constraint.

## Alternatives rejected

- **Converge on a single separator everywhere** (breaking, release-timed with the naming convergence). Rejected: sacrifices each family's home idiom to buy spelling purity, when input leniency captures the full turn-savings without a break.
- **Strict parsing + did-you-mean errors only.** Rejected: every guess still costs a round-trip, and the round-trip is the metric.
- **A `--type` special-case flag.** Rejected: privileges one convention field in a schema-agnostic core; subsumed by dynamic field predicates, which answer the same guess generically.
- **Bare positional predicate tokens on `find`** (`norn find type:note`). Rejected: a new canonical surface rather than forgiveness, and it spends the positional slot wanted for doc/stem addressing.
- **Gate-free dynamic predicates with a zero-match warning.** Rejected: a typo'd query still masquerades as "no results"; accurate queries are a link in the trust chain, so unknown intent must refuse, not guess.
