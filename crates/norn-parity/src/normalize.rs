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
        }
    }
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
