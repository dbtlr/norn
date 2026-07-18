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

pub fn normalize_text(text: &str, vault_root: &Path, steps: &[Normalization]) -> String {
    let mut out = text.to_string();
    for step in steps {
        match step {
            Normalization::VaultRoot => {
                let root = vault_root.display().to_string();
                if !root.is_empty() {
                    out = out.replace(&root, "<VAULT>");
                }
            }
        }
    }
    out
}

pub fn normalize_output(
    raw: &crate::exec::RawOutput,
    vault_root: &Path,
    steps: &[Normalization],
) -> Option<NormalizedOutput> {
    let exit_code = raw.exit_code?;
    let stdout = String::from_utf8_lossy(&raw.stdout);
    let stderr = String::from_utf8_lossy(&raw.stderr);
    Some(NormalizedOutput {
        stdout: normalize_text(&stdout, vault_root, steps),
        stderr: normalize_text(&stderr, vault_root, steps),
        exit_code,
    })
}
