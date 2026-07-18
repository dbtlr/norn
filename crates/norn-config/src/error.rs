//! The public error vocabulary. Every variant is specific enough for a CLI to
//! render an operator-quality message; the public API never surfaces `anyhow`.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Every way a central-config operation can fail.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A vault name did not match `[a-z0-9][a-z0-9_-]*`.
    #[error("invalid vault name {name:?}: must be lowercase and match [a-z0-9][a-z0-9_-]* (no path separators)")]
    InvalidName { name: String },

    /// A vault with this name is already registered.
    #[error("a vault named {name:?} is already registered")]
    DuplicateName { name: String },

    /// The (canonicalized) root is already registered under a different name.
    /// Bidirectionality means one name per root.
    #[error("root {root} is already registered under the name {existing:?}")]
    RootAlreadyRegistered { root: PathBuf, existing: String },

    /// A registration root path is not an existing directory.
    #[error("vault root {root} is not an existing directory")]
    RootNotDirectory { root: PathBuf },

    /// No vault with this name is registered.
    #[error("no vault named {name:?} is registered")]
    UnknownName { name: String },

    /// `NORN_CONFIG_DIR` was set to a relative path. The config home must
    /// never depend on ambient process cwd.
    #[error("NORN_CONFIG_DIR must be an absolute path, got {path}")]
    RelativeConfigDir { path: PathBuf },

    /// [`crate::ResolveInput`]'s cwd was relative. Resolution is deterministic
    /// only over absolute inputs.
    #[error("resolver cwd must be an absolute path, got {path}")]
    RelativeCwd { path: PathBuf },

    /// A registry entry resolved to a root that no longer exists or is not a
    /// directory. Fail-loud: never a silent fallthrough to the next step.
    #[error(
        "registered vault {name:?} points at {path}, which no longer exists or is not a directory"
    )]
    StaleEntry { name: String, path: PathBuf },

    /// A repo binding named a vault that is not in the registry.
    #[error("repo binding {file} names vault {name:?}, which is not registered")]
    BindingUnregistered { file: PathBuf, name: String },

    /// A repo binding file exists but could not be parsed as TOML.
    #[error("failed to parse repo binding {file}: {source}")]
    BindingParse {
        file: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// A repo binding file exists but has no `vault` key.
    #[error("repo binding {file} is missing the required `vault` key")]
    BindingMissingVault { file: PathBuf },

    /// The config file exists but could not be parsed as TOML.
    #[error("failed to parse config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The in-memory config could not be serialized back to TOML.
    #[error("failed to serialize config: {source}")]
    ConfigSerialize {
        #[source]
        source: toml::ser::Error,
    },

    /// The config home could not be determined from the environment.
    #[error("could not determine the config home: {reason}")]
    NoConfigHome { reason: String },

    /// A filesystem operation failed, with the path and what was attempted.
    #[error("{context} ({path}): {source}")]
    Io {
        context: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ConfigError {
    /// Build an [`ConfigError::Io`] with path context.
    pub(crate) fn io(
        context: impl Into<String>,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        ConfigError::Io {
            context: context.into(),
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}
