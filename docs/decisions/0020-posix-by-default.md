---
title: "0020 — the CLI follows POSIX (and common GNU) conventions by default"
description: "Architectural decision that POSIX plus the common GNU CLI conventions are norn's default contract for env-var semantics, exit codes, argument conventions, and diagnostic format — every deviation needs a strong, documented reason recorded here or in a sibling ADR."
---

# 0020 — the CLI follows POSIX (and common GNU) conventions by default

norn's command-line surface conforms to POSIX — and the common GNU conventions layered on it (long options, `--` end-of-options, the no-color.org / CLICOLOR color signals) — **by default**. **Decision:** POSIX + common-GNU behavior is the contract for four surfaces — environment-variable semantics, process exit codes, argument conventions, and diagnostic format — and any deviation requires a strong, explicitly documented reason, recorded either in its own ADR or in the amendment list at the foot of this one. Conformance is the default an agent or pipeline may assume without reading norn's source; a surprise is a bug unless it is written down here as a deliberate, reasoned exception.

The rule exists because norn is driven far more by agents and pipelines than by hands at a prompt, and those callers already model "a well-behaved Unix CLI." Every gratuitous deviation is a fact the caller must special-case — an error that need not exist. Conforming by default is the cheapest possible interface.

## Context

Several conforming behaviors already landed piecemeal; this ADR states the governing rule once so future surfaces inherit it rather than re-deciding it, and records the sweep (NRN-363) that brought the existing surface into line.

**Precedent already in force:**

- **Locale precedence** (`norn-cli` glyph selection): `LC_ALL` overrides `LC_CTYPE` overrides `LANG`, and a variable set to the empty string is treated as *unset* — POSIX's rule that an empty value selects no locale. (NRN-336.)
- **Tool-prefixed diagnostics**: every user-facing error on stderr leads with a stable `norn:` program-name prefix, the conventional `progname: message` shape a pipeline can grep. (NRN-361.)
- **Stream discipline**: stdout carries only the payload, in every format; stderr carries the conversation (diagnostics and report annotations). A consumer can always `2>/dev/null` and keep a clean payload.

### Environment-variable semantics

The default, applied across every variable norn reads: **an empty value is treated as unset** (POSIX semantics for behavior-selecting variables), and an **invalid value is never silently wrong** — it either errors loudly or fails safe to a documented default, never produces a quietly-wrong result. The decided per-variable behavior is recorded in a doc comment at each read site. The presence-toggle class (`NO_COLOR`, `NORN_ASCII`, and the base of `CLICOLOR_FORCE`) is funnelled through one `env_flag` helper in `norn-cli` — a variable is "on" only when present **and** non-empty — so the empty-is-unset rule is enforced in one place rather than re-implemented per site.

| Variable | Empty | Invalid | Notes |
|---|---|---|---|
| `NO_COLOR` | unset (color allowed) | n/a (presence toggle) | no-color.org: honored when present and non-empty; takes precedence over `--color always`. Internal, tty-sensitive, unpinnable by parity. Added/repaired in this sweep. |
| `CLICOLOR_FORCE` | unset | `0` → does not force | CLICOLOR spec (bixense.com/clicolors): forces color when present, non-empty, and not `0`. |
| `NORN_ASCII` | unset (locale decides) | n/a (presence toggle) | forces ASCII glyph fallback when present and non-empty. |
| `LC_ALL` / `LC_CTYPE` / `LANG` | unset (fall through) | n/a | POSIX precedence, first non-empty wins; all-unset → ASCII fallback. |
| `NORN_ROOT` | unset (trimmed) | n/a | direct root path; whitespace-only is treated as unset by the resolver. |
| `NORN_CONFIG_DIR` | unset | relative → loud error | absolute-path override for the config home; a relative value fails loud (a cwd-dependent config location is non-deterministic). |
| `XDG_CONFIG_HOME` | unset | relative → ignored per XDG spec | config-home base. |
| `HOME` | unset | relative → loud error | last-resort config-home base. |
| `XDG_RUNTIME_DIR` | unset (fall to `TMPDIR`) | n/a | owner-socket runtime dir base. |
| `TMPDIR` | unset (fall to system temp) | n/a | owner-socket runtime dir base; empty now falls through rather than erroring (repaired in this sweep). |
| `NORN_EPHEMERAL_TTL_SECS` | unset (default 120s) | fail-safe to default | resource knob (idle-owner lifetime only, never vault correctness); an unparseable value falls back to the default rather than aborting a command. |
| `NORN_OWNER_WARMUP_DELAY_MS` | no delay | no delay | internal test/debug seam. |
| cache tuning knobs (`norn-core`) | debug-only | debug-only | compiled out of release builds entirely (`#[cfg(debug_assertions)]`). |

### Exit codes

norn uses the conventional tri-state, defined in `docs/errors.md` and carried by the `EXIT_OK` / `EXIT_OPERATIONAL` / `EXIT_USAGE` constants:

- **`0`** — success (including `--help` / `--version`, per GNU convention, emitted by clap).
- **`1`** — operational failure: a well-formed invocation that could not be carried out (config error, an operation rejected, a value that parsed as a flag but was semantically invalid).
- **`2`** — usage error: an argv clap itself could not accept (unknown flag, unknown command, an operand where none is allowed).

One documented refinement rides on top for the mutation verbs (`apply` / `move` / `delete` / `rewrite-wikilink`): there, `2` is a *preflight refusal* (byte-identical vault) and `1` is a *partial-apply* failure, keyed on the single fact "did any write land." This is a deliberate, documented specialization of the operational/usage split for the surface where "did anything change" is the load-bearing question — see `docs/errors.md`. Non-mutating commands use the plain tri-state above.

### Argument conventions

Standard clap-derived behavior holds and is relied upon:

- **`--` ends option processing.** The forgiving-input normalizer (`normalize_argv`, ADR 0010) stops desugaring at a bare `--` and passes everything after it through verbatim; clap then treats those tokens as positional operands. `norn find -- --eq` does *not* desugar `--eq` into a predicate — it reaches clap as a positional and is rejected as an unexpected operand (`find` takes none), exit 2. The double-dash is honored end to end.
- **`-Cvalue` short-option-with-value** works the standard getopt way (`-C .` and `-C.` both bind the value).
- **`-` is not a magic stdin placeholder** anywhere today; it is an ordinary operand (an unresolved `-` target simply fails to resolve). norn asserts no stdin convention it does not implement.

### Diagnostic format

Two stderr streams, both off stdout:

1. **The diagnostic stream** — every user error, `norn:`-prefixed, with optional soft-landing `hint:` lines (NRN-361). This is the single path all error sites route through (`Presenter::present_diagnostic`).
2. **The report-annotation stream** — the pinned `note:` / `warning:` lines the oracle-parity surfaces emit (truncation notes, `--col` facet warnings). These are intentionally *not* `norn:`-prefixed: they are byte-matched against the pinned oracle report format, a deliberate split from the diagnostic stream. Both live on stderr; stdout stays payload-only.

Owner-subprocess diagnostics (`norn owner: …`) carry their own tool-scoped prefix and are conforming.

### Signals

The summoned owner installs SIGINT/SIGTERM handlers that drain in-flight work and clean up its socket and db before exiting (NRN-345) — the conforming behavior for a long-lived Unix daemon. No change; recorded here as compliant.

## Consequences

- **`env_flag` is the enforcement layer** for the presence-toggle class: `NO_COLOR`, `NORN_ASCII`, and the base of `CLICOLOR_FORCE` all read through it, so "empty is unset" cannot drift back in per site. `NO_COLOR` support, previously reading presence-only (empty counted as set), now conforms to no-color.org; `CLICOLOR_FORCE=0` correctly does not force; an empty `TMPDIR` now falls through to the system temp dir.
- **Every env read carries a doc comment** stating its decided empty/invalid behavior, so the table above is derivable from the source and stays honest.
- **Future surfaces inherit the rule.** A new env var, exit path, argument form, or diagnostic must conform or justify the exception here — the reviewer's default question becomes "does this match POSIX/GNU, and if not, where is that written down?"
- The color, glyph, and TTL behaviors are tty-sensitive or off the daily path and are **not pinnable by the parity harness** (it runs piped); they are covered by unit tests, not ledger entries.

## Sanctioned deviations

Deviations from the default contract, each with its standing rationale. Add to this list (or file a sibling ADR) rather than deviating silently.

- **Grammar-wide last-wins flag repetition (PD-110 / NRN-365).** A repeated scalar flag (`find --limit 5 --limit 1`) resolves last-wins (→ `1`, exit 0) instead of erroring. This is *more permissive* than GNU tools, which typically reject a repeated single-value option. Rationale: norn's flags follow merge-queue semantics — the last operation on a key wins — which is how agents and scripts compose argv incrementally; erroring on a repeat manufactures a failure that need not exist. Implemented as clap's `args_override_self` on the root, propagating to every subcommand. Genuine cross-flag conflicts (e.g. `delete --allow-broken-links` vs `--rewrite-to`) still error. Ledgered as a decided oracle divergence; see `docs/parity-ledger.toml`.

- `NO_COLOR` (present and non-empty) disables color even against an explicit `--color always` — the literal no-color.org reading, chosen over its FAQ's flags-win recommendation; the environment is treated as the operator's standing order.

## Amendment — 2026-07-23: stdin operand, the closed stderr-annotation vocabulary, input precedence, and selector precedence

Four corrections/additions, gathered from the current tree rather than this ADR's original text (some of which drifted since NRN-363 landed).

**`-` as a stdin operand (supersedes the "Argument conventions" bullet above).** The original claim — "`-` is not a magic stdin placeholder anywhere today" — was accurate when written but is false now: `norn apply -` reads the `MigrationPlan` from stdin (`read_plan_source`, `crates/norn-cli/src/commands/apply.rs`), defaulting the input format to JSON unless `--input-format` overrides it (`apply.rs`'s `PLAN` value-name help notes it). This is the sanctioned convention: a command whose sole positional is a single document/payload source may treat a bare `-` operand as "read this from stdin," the same way `apply` does. `edit`'s stdin fallback is a different shape — it reads stdin as the *implicit default* when neither `--edits-json` nor `--ops-file` is given, with no `-` operand involved — and is not this convention. Everywhere else, `-` is an ordinary operand with no special meaning: a `get`/`find`/`move` target literally named `-` simply fails to resolve like any other unresolved path.

**The closed stderr-annotation prefix set (extends the "Diagnostic format" section above).** NRN-407 (ADR 0022) gave the report-annotation stream a typed `Severity`, and the prefix vocabulary is now closed to exactly three: `note:` / `warning:` / `error:` (`Conversation::note` / `::warning` / `::error` and `severity_prefix`, `crates/norn-cli/src/display/conversation.rs`). `warning:` and `error:` render a report [`Note`]'s typed `Severity` (`Conversation::report_note`, never sniffing message text); `note:` is reserved for the CLI's own non-severity informational annotations (e.g. a truncation notice) and carries no typed `Severity` behind it — see ADR 0022's 2026-07-23 amendment, which records the wire `Severity` enum as deliberately closed to `warning`/`error` with no informational rung. Two verb annotations that previously spoke off-set prefixes (`get`'s ambiguous-stem `note:` and its unresolved-`--col` `warn:`) converged onto `warning:` in the same change; ledgered as PD-131 (`docs/parity-ledger.toml`). PD-132 (records-body `not-run` label) and PD-133 (MCP `vault.get` typed notes) are sibling NRN-407 divergences on the same decision, not stderr-prefix changes themselves.

**F3 — input-source precedence: explicit beats environmental, explicit-vs-explicit conflicts refuse.** `norn edit`'s op-source resolution (`resolve_ops`, `crates/norn-cli/src/commands/edit.rs`) orders its sources: single-op sugar (`--str-replace` and siblings) first, then `--edits-json`, then the hidden `--ops-file` alias, then stdin last as the implicit fallback. Sugar combined with `--edits-json` or `--ops-file` refuses (`"<flag> cannot be combined with --edits-json"` / `--ops-file`) rather than picking one silently — an explicit-vs-explicit conflict is a refusal, never a precedence pick. Stdin is reached only when no explicit source (sugar, `--edits-json`, `--ops-file`) is present at all — "environmental" in the sense that it carries no flag of its own. `norn apply`'s plan source has no sugar tier: it is file-path-or-`-`(stdin), with `--input-format` an explicit override of format-detection, not of source.

**H3b — selector precedence: explicit `-C` beats `--vault NAME`.** `Registry::resolve` (`crates/norn-config/src/resolve.rs`) orders vault-selection strictly: explicit path (`-C`/`--cwd`) first, explicit registered name (`--vault`) second, then repo binding, `NORN_ROOT`, cwd reverse lookup, and finally the unregistered-cwd (ephemeral) outcome — `explicit_path_wins_over_everything` pins this with both a name AND a stale `NORN_ROOT` set alongside a `-C` path. Today this precedence is silent: supplying both `-C` and `--vault` picks `-C` with no diagnostic. NRN-419 is planned to add a warning when both are given and disagree; this amendment records the precedence itself as the decided, standing contract independent of that warning.
