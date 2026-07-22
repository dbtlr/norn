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
  (`YYYY-MM-DD`), or an ISO 8601 datetime at **minute or second precision** —
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
