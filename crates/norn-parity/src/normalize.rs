//! Output normalization applied to both sides before comparison. Plain
//! string operations only — no regex crate (ADR 0018 harness constraint).
//!
//! Kept as an enum, not a bare function, so later phases add normalization
//! steps deliberately (e.g. a `Timestamp` step lands when a ported surface
//! starts emitting wall-clock time) instead of silently widening what
//! "matches" means.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalization {
    /// Replace every occurrence of the vault root's absolute path with
    /// `<VAULT>`. The binary's own argv[0]-dependent output (e.g. usage
    /// lines naming the invoked binary) is deliberately NOT normalized —
    /// that is a real parity surface, not noise.
    VaultRoot,
    /// Mask the telemetry trace id from a CONFIRMED-apply mutation report, on
    /// both surfaces it appears: the records footer `trace: <hex>` and the JSON
    /// `"trace_id":"<hex>"` field. Keyed precisely to the donor's two spellings
    /// — the run of ascii-hexdigits immediately following each marker is
    /// removed, nothing else; the marker itself is left in place, so whether
    /// the line/field is PRESENT AT ALL stays a pinned, un-normalized part of
    /// the comparison. The pinned oracle mints a fresh random id on every apply
    /// (empirically non-deterministic: 3 runs of one op yield 3 distinct ids).
    /// The rewrite's own id varies by verb: `set`/`new`/`edit` carry an
    /// empty-until-real id (their verb paths don't yet route through
    /// `EventSink` — NRN-400 wires it), while the four cascade verbs
    /// (`apply`/`move`/`delete`/`rewrite-wikilink`) already emit a real 32-hex
    /// `EventSink`-derived id on a confirmed apply. Masking both sides' id text
    /// makes a confirmed apply that is otherwise byte-equal Match regardless of
    /// which of those the rewrite's id happens to be, without over-normalizing
    /// anything but the id run itself. Applied per-case, only on the
    /// confirmed-apply cases that need it — never a read case, and never a
    /// refusal/forecast (which carry no id on either side).
    TraceId,
    /// Strip the `plan_hash` hex from a cascade-verb `--format json` report (the
    /// pretty `"plan_hash": "<hex>"` field). `plan_hash` is
    /// `MigrationPlan::canonical_hash()`, which serializes the plan INCLUDING its
    /// absolute `vault_root` — so it is genuinely root-dependent and differs
    /// between the harness's two per-side vault copies (different roots), exactly
    /// as the sibling `vault_root` field does (already normalized). Same root →
    /// identical hash on both binaries (verified out-of-band + unit-pinned by the
    /// verbs' op-field-set tests), so normalizing the environment-dependent hash
    /// lets a `--format json` cascade case compare the rest of the report shape
    /// (operations, cascade, outcome) byte-exactly. Only for `--format json`
    /// cascade cases; records omits `plan_hash`.
    PlanHash,
}

/// The normalization steps applied to every case today.
pub const DEFAULT: &[Normalization] = &[Normalization::VaultRoot];

/// A normalized (stdout, stderr, exit code) triple, ready for byte-exact
/// comparison. Beyond the explicit substitutions in `steps`, nothing is
/// trimmed or reformatted — trailing whitespace and everything else is
/// preserved so comparison stays byte-exact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Normalize `text` under `steps`, replacing any of `vault_roots` with
/// `<VAULT>`. Multiple root spellings are accepted because a temp vault has
/// more than one valid absolute spelling on some platforms — notably macOS,
/// where `/var/folders/...` is a symlink alias of the canonical
/// `/private/var/folders/...` and `norn` may echo either. Longer spellings
/// are replaced first so a shorter alias (`/var/..`) never partially
/// rewrites a longer one (`/private/var/..`).
pub fn normalize_text(text: &str, vault_roots: &[&Path], steps: &[Normalization]) -> String {
    let mut out = text.to_string();
    for step in steps {
        match step {
            Normalization::VaultRoot => {
                let mut roots: Vec<String> = vault_roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                roots.sort_by_key(|s| std::cmp::Reverse(s.len()));
                for root in roots {
                    out = out.replace(&root, "<VAULT>");
                }
            }
            Normalization::TraceId => {
                out = strip_hex_run_after(&out, "trace: ");
                out = strip_hex_run_after(&out, "\"trace_id\":\"");
            }
            Normalization::PlanHash => {
                // The cascade verbs' `--format json` is PRETTY, so the field is
                // `"plan_hash": "<hex>"` (a space after the colon).
                out = strip_hex_run_after(&out, "\"plan_hash\": \"");
            }
        }
    }
    out
}

/// Remove the run of ascii-hexdigits immediately following each occurrence of
/// `marker` in `text`, leaving `marker` (and everything else) intact. The
/// applied trace id is a fixed-format hex run right after a literal marker on
/// both the records (`trace: `) and JSON (`"trace_id":"`) surfaces on EITHER
/// side, so this masks whatever id text follows the marker — the oracle's
/// random hex, the rewrite's own real `EventSink`-derived hex on the four
/// cascade verbs, or `set`/`new`/`edit`'s empty-until-real id — without
/// touching anything but the id run itself. A marker followed by no hexdigit
/// (an empty id) is a no-op.
fn strip_hex_run_after(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let after_marker = pos + marker.len();
        out.push_str(&rest[..after_marker]);
        let tail = &rest[after_marker..];
        let hex_end = tail
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(tail.len());
        rest = &tail[hex_end..];
    }
    out.push_str(rest);
    out
}

/// Why an output could not be normalized for comparison.
pub enum NormalizeError {
    /// The process was killed by a signal — there is no exit code to compare.
    Signaled,
    /// The named stream (`"stdout"`/`"stderr"`) is not valid UTF-8. Lossy
    /// conversion is forbidden in an exact parity gate: two DIFFERENT invalid
    /// byte sequences would both become U+FFFD and falsely compare equal.
    NonUtf8 { stream: &'static str },
}

pub fn normalize_output(
    raw: &crate::exec::RawOutput,
    vault_roots: &[&Path],
    steps: &[Normalization],
) -> Result<NormalizedOutput, NormalizeError> {
    let exit_code = raw.exit_code.ok_or(NormalizeError::Signaled)?;
    let stdout = std::str::from_utf8(&raw.stdout)
        .map_err(|_| NormalizeError::NonUtf8 { stream: "stdout" })?;
    let stderr = std::str::from_utf8(&raw.stderr)
        .map_err(|_| NormalizeError::NonUtf8 { stream: "stderr" })?;
    Ok(NormalizedOutput {
        stdout: normalize_text(stdout, vault_roots, steps),
        stderr: normalize_text(stderr, vault_roots, steps),
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &[Normalization] = &[Normalization::TraceId];

    #[test]
    fn trace_id_records_footer_hex_is_stripped_to_empty() {
        // The oracle's applied records footer carries a random hex id; strip it
        // to the rewrite's empty form so an otherwise byte-equal apply matches.
        let oracle = "set notes/a.md\n  x: 1 → 2\ntrace: 30b8c28c3eafd385c63aca6850a209fa\n";
        let rewrite = "set notes/a.md\n  x: 1 → 2\ntrace: \n";
        assert_eq!(
            normalize_text(oracle, &[], TRACE),
            normalize_text(rewrite, &[], TRACE),
            "oracle hex id and rewrite empty id normalize equal"
        );
        assert_eq!(normalize_text(oracle, &[], TRACE), rewrite);
    }

    #[test]
    fn trace_id_json_field_hex_is_stripped_to_empty() {
        let oracle = r#"{"trace_id":"e213f95a09a57088230ff09aeb0e4718","applied":true}"#;
        let rewrite = r#"{"trace_id":"","applied":true}"#;
        assert_eq!(normalize_text(oracle, &[], TRACE), rewrite);
        // The rewrite's already-empty id is a no-op (idempotent).
        assert_eq!(normalize_text(rewrite, &[], TRACE), rewrite);
    }

    #[test]
    fn trace_id_leaves_non_trace_hex_untouched() {
        // A hex-looking token that is not preceded by a trace marker is a real
        // parity surface and must survive normalization.
        let text = "target notes/deadbeef.md\nhash abcdef0123\n";
        assert_eq!(normalize_text(text, &[], TRACE), text);
    }
}
