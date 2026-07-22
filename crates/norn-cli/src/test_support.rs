//! Crate-internal, test-only helpers shared across `norn-cli` modules.
//!
//! Compiled only under `#[cfg(test)]`.

use std::io::{self, Write};

use crate::cli::{ColorWhen, GlobalArgs};

/// A [`Write`] that fails every write with a fixed [`io::ErrorKind`] — the
/// render IO-error-policy fixture shared by every `display` test module (the
/// `BrokenPipe`-is-success vs. everything-else-is-operational split, NRN-372).
pub(crate) struct FailingWriter(pub(crate) io::ErrorKind);

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(self.0))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A minimal non-styling [`GlobalArgs`] (`--color never`) for renderer tests —
/// the palette resolves off, so asserted bytes stay plain.
pub(crate) fn global_args() -> GlobalArgs {
    GlobalArgs {
        cwd: None,
        verbose: false,
        no_cache_refresh: false,
        color: ColorWhen::Never,
        vault: None,
        help_short: false,
        help_long: false,
        dynamic_fields: Vec::new(),
    }
}

/// Serializes env-mutating tests and restores every touched variable on drop —
/// so parallel tests never observe a half-set environment and nothing leaks
/// past the test, **even if the test body panics** (the restore runs in `Drop`,
/// not straight-line after the assertions).
///
/// The process environment (`std::env`) is global, so a SINGLE shared mutex
/// must guard every env-touching test across the whole crate — two independent
/// locks would let parallel tests interleave their mutations and flake. Every
/// consumer (glyph locale detection, the help live-marker renderer) goes
/// through this one type.
///
/// Only the keys passed to [`EnvGuard::new`] are saved and restored, so a test
/// that must isolate against ambient values should pass every relevant key
/// explicitly (with `None` to clear it for the duration of the guard).
pub(crate) struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    /// Acquire the shared env lock, then set (or, with `None`, clear) each named
    /// variable, remembering its prior value for restoration on drop.
    pub(crate) fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // Poisoning is irrelevant here — the guard only serializes access and
        // holds no invariant a panicking test could corrupt.
        let lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let mut saved = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
