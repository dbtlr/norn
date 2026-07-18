//! Where the central config file lives.
//!
//! The location is XDG-style on every platform (macOS included) — deliberately
//! symmetric with norn's `~/.cache/norn` cache home. It is resolved from the
//! environment or injected directly so tests never touch a real home directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;

/// Environment variable that overrides the entire config-home location. When
/// set, it is used directly as the directory that contains `config.toml`.
pub const NORN_CONFIG_DIR_ENV: &str = "NORN_CONFIG_DIR";

/// The directory that holds `config.toml` (the `norn` subdirectory of the
/// config dir). Construct it from the environment with [`ConfigHome::from_env`]
/// or inject a directory directly with [`ConfigHome::new`].
#[derive(Debug, Clone)]
pub struct ConfigHome {
    dir: PathBuf,
}

impl ConfigHome {
    /// Use `dir` directly as the directory holding `config.toml`. This is the
    /// programmatic injection point tests use to stay off the real home dir.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Resolve the config home from the process environment.
    ///
    /// Precedence: `NORN_CONFIG_DIR` (used directly) → `$XDG_CONFIG_HOME/norn`
    /// → `$HOME/.config/norn`. Errors only if none of these can be determined.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_getenv(|key| std::env::var_os(key))
    }

    /// [`ConfigHome::from_env`] over an injected environment reader, so
    /// precedence is unit-testable without mutating the process environment.
    pub fn from_getenv(getenv: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        if let Some(dir) = getenv(NORN_CONFIG_DIR_ENV).filter(|value| !value.is_empty()) {
            return Ok(Self::new(dir));
        }
        if let Some(xdg) = getenv("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            return Ok(Self::new(PathBuf::from(xdg).join("norn")));
        }
        let home = getenv("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ConfigError::NoConfigHome {
                reason: format!("none of {NORN_CONFIG_DIR_ENV}, XDG_CONFIG_HOME, or HOME is set"),
            })?;
        Ok(Self::new(PathBuf::from(home).join(".config").join("norn")))
    }

    /// The directory that holds `config.toml`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path to the central config file.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// Path to the sidecar advisory lock file guarding mutations.
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("config.toml.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn norn_config_dir_takes_precedence() {
        let home = ConfigHome::from_getenv(env_from(&[
            ("NORN_CONFIG_DIR", "/explicit/norn"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]))
        .unwrap();
        assert_eq!(home.dir(), Path::new("/explicit/norn"));
        assert_eq!(home.config_path(), Path::new("/explicit/norn/config.toml"));
    }

    #[test]
    fn xdg_config_home_appends_norn() {
        let home = ConfigHome::from_getenv(env_from(&[
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/user"),
        ]))
        .unwrap();
        assert_eq!(home.dir(), Path::new("/xdg/norn"));
    }

    #[test]
    fn falls_back_to_home_dot_config_norn() {
        let home = ConfigHome::from_getenv(env_from(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(home.dir(), Path::new("/home/user/.config/norn"));
    }

    #[test]
    fn empty_values_are_ignored() {
        let home = ConfigHome::from_getenv(env_from(&[
            ("NORN_CONFIG_DIR", ""),
            ("XDG_CONFIG_HOME", ""),
            ("HOME", "/home/user"),
        ]))
        .unwrap();
        assert_eq!(home.dir(), Path::new("/home/user/.config/norn"));
    }

    #[test]
    fn errors_when_nothing_is_set() {
        let err = ConfigHome::from_getenv(env_from(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::NoConfigHome { .. }));
    }

    #[test]
    fn lock_path_is_sidecar_of_config() {
        let home = ConfigHome::new("/some/dir");
        assert_eq!(home.lock_path(), Path::new("/some/dir/config.toml.lock"));
    }
}
