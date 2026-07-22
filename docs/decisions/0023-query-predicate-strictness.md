---
title: "0023 — query predicate input strictness"
description: "Architectural decision that a query date-operator value validates as a real ISO 8601 date or refuses, a --path glob parses or refuses, and a silently-empty result set produced from unparseable predicate input is a defect class — not the input forgiveness of ADR 0010."
---

# 0023 — query predicate input strictness

A read query's predicate input is trusted to mean what the user typed.
**Decision:** a date-operator value (`--before` / `--on` / `--after`) is either
the literal `today` or a valid ISO 8601 date (`YYYY-MM-DD`) / datetime, else the
invocation refuses (exit 2) with a
message naming the operator, the value, and the accepted forms. A `--path` glob
parses through `PathPattern` or the invocation refuses (exit 2) naming the bad
pattern and why. Both refusals surface on the same user-error path as any other
malformed predicate — one seam (`build_document_query`) covering `find`,
`count`, and `describe`, the direct CLI run and the owner-routed run
identically.

## The rule exists because

Both inputs previously produced a **silently-wrong result at exit 0**, which is
indistinguishable from an honest no-match — the exact trap the query family is
built to avoid (accurate queries are a link in the trust chain). A date value
substituted only the literal `today`; everything else passed verbatim into a
TEXT lexical compare, so `--before due:yesterday` compiled to `due < 'yesterday'`
(matching essentially every stored ISO date) and `--on created:2026-13-45`
compared an impossible date as a literal string. A `--path` glob was never
validated: `PathPattern::parse` errors were `.ok()`-discarded in the per-document
post-pass, so `{unclosed` filtered out every document — zero results, exit 0. An
agent reads "no results" where it should read "you typed something unparseable."

## Context

The NRN-424 seam audit surfaced both as carried behavior of the pinned 0.48.1
oracle. Help text already promised ISO 8601 for the date operators, so the value
grammar was documented but unenforced. The fix is fail-safe: refuse rather than
guess, at the single point where raw predicate strings become the typed query.

## Invariants

- **Date values validate or refuse.** Accepted: `today`, an ISO 8601 date
  (`YYYY-MM-DD`), or an ISO 8601 datetime at **minute, second, or fractional-second
precision** —
  naive (`YYYY-MM-DDThh:mm` / `YYYY-MM-DDThh:mm:ss`) or offset-bearing
  (`YYYY-MM-DDThh:mm±hh:mm` / `YYYY-MM-DDThh:mm:ss±hh:mm`, and `Z` at second
  precision). Minute precision is a valid ISO 8601 reduced-precision form and the
  dominant stored-frontmatter shape, so it must validate. chrono rejects
  impossible dates and garbage.
  This validates the predicate VALUE the user typed; it does **not** change how a
  valid ISO value compares against stored frontmatter — the lexical compare of
  valid ISO strings is unchanged (that comparison model is NRN-110, out of
  scope).
- **Glob patterns parse or refuse.** Every user-facing `--path` glob is parsed up
  front; an unparseable pattern is a refusal, never an empty result set.
- **Silently-empty from unparseable input is a defect, not forgiveness.** ADR
  0010 forgives *variant spellings of valid intent* (separator variance, aliases,
  vault-gated dynamic fields) and normalizes them to canonical. It never blesses
  a result silently computed from a value that cannot be parsed at all. An
  unparseable value has no valid intent to normalize toward — it refuses.
- **One seam, one class.** Both checks live in `build_document_query`; the CLI
  and the daemon-routed path share it, so both verbs and both transports refuse
  with the same message and the same exit code.

## Consequences

- The pinned 0.48.1 oracle accepts both bad inputs silently, so these fixes
  create deliberate divergence on bad-input cases. The divergences are ledgered
  against this decision (`docs/parity-ledger.toml`), reason `decided-better`.
- Valid inputs behave identically — `today` substitution and valid-ISO lexical
  comparison are byte-for-byte unchanged; a well-formed glob still runs.
- Scope boundary: this is the query predicate INPUT surface only. How valid
  stored dates compare (NRN-110) and the broader glob/date grammar work
  (NRN-426) are separate and untouched. Other operators' exit-code alignment is
  NRN-108's scope; these two refusals simply land in the already-correct
  bad-invocation class (exit 2).

## Amendment (2026-07-22): predicate value typing (NRN-426)

The original decision covered date-operator values and `--path` globs. The same
"a silently-wrong result at exit 0 is a defect" principle extends to the
VALUE-comparison operators (`--eq` / `--not-eq` / `--in` / `--not-in`). The
pinned oracle eagerly types every parseable predicate token (`true`/`false`,
i64, f64) before SQL, and SQLite never equates an INTEGER/REAL with TEXT. So
`--eq zip:07030` against a stored `zip: "07030"` returned zero matches silently,
and — the corrupting direction — `--not-eq zip:07030` RETURNED the very document
the user meant to exclude. It bit quoted zips, phones, versions, and zero-padded
ids.

**Decision (rules):**

1. **Schema is the type authority where declared.** When compiling a
   value-comparison predicate on field F, if EVERY validate rule declaring F
   agrees on one type vault-wide, the predicate compiles as that type.
   Disagreement between rules, or no declaration at all, falls back to (2). The
   declarable vocabulary carries no numeric or boolean type, so a declared field
   is string-shaped (`string`/`text`/`list_of_strings`/`wikilink`/
   `wikilink_or_list`) or temporal (`date`/`datetime`).
2. **Dual-type comparison fallback** for undeclared/disagreeing fields: a
   numeric-/bool-looking token matches EITHER representation — `--eq zip:07030`
   matches the numeric `7030` AND the string `"07030"`; `--not-eq` excludes a
   document matching either (De Morgan). The leading-zero overmatch (`07030`
   parses to `7030`) is accepted — strictly better than the silent miss.
3. **Declared-type-unparseable values refuse (exit 2)** naming the field, the
   declared type, and the value. A declared `date`/`datetime` field reuses the
   NRN-427 `is_iso_date_or_datetime` grammar (one refusal shape — no second date
   parser), so `--eq due:someday` on a declared-date `due` refuses.
4. **`--in` CSV** types each element independently by the same rules; a fallback
   element fans both representations into the one membership value set.
5. **Bools under fallback**: `--eq draft:true` matches YAML `true` AND the string
   `"true"`.
6. **No new CLI syntax** (no force-string quoting) and no zero-match warning.

**Compilation.** A fallback dual predicate compiles to the existing array-aware
membership shape — `--eq x:07030` ⇒ `x IN ["07030", 7030]`, and `--not-eq` to
`x NOT IN [...]` — so no new SQL is introduced and the EAV planner-guard
invariant (a single SCAN/SEARCH, no per-row subquery) holds. Type resolution is
computed once per query build from the loaded schema (owner side), never per row.

**Consequences.** The pinned 0.48.1 oracle keeps the old eager-coercion, so
matching-semantics cases diverge by design; ledgered `decided-better` under
`PD-124`. The read verbs (`find`/`count`/`describe`) now thread the vault schema
into `build_document_query`; the apply owner-set precondition path deliberately
keeps the historical eager coercion (it is a mutation gate, not the read query
surface).
