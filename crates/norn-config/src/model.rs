//! The on-disk config model and its read side.
//!
//! Unknown keys — top-level and per-vault — are captured into `#[serde(flatten)]`
//! `extra` maps and re-serialized on write, so an older binary editing the file
//! never destroys future keys (auth tokens, centrally-located per-vault config).
//! This is data round-trip, not verbatim formatting preservation.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The whole config file. `vaults` is the registry; `extra` preserves any
/// top-level keys this binary does not model.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ConfigFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vaults: BTreeMap<String, VaultEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// A single `[vaults.<name>]` table. Only what was explicitly registered is
/// stored — defaults are never synthesized here. `extra` preserves unknown
/// per-vault keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct VaultEntry {
    /// Absolute, canonicalized vault root.
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<PathBuf>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// Read and parse the config file. An absent file is an empty registry, not an
/// error. This read is lock-free by contract.
pub(crate) fn read(path: &Path) -> Result<ConfigFile, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::ConfigParse {
            path: path.to_path_buf(),
            source,
        }),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(source) => Err(ConfigError::io("failed to read config", path, source)),
    }
}

/// The generated banner prepended to every write. Data-only round-trip means
/// hand-authored comments never survive a mutation anyway, so the write path
/// re-stamps this line to tell an operator the file is norn-managed and where
/// to reach for the sanctioned edit verbs. It is an ordinary TOML comment, so
/// it re-parses as a no-op.
pub(crate) const MANAGED_HEADER: &str = "# Managed by norn — use `norn vault register/set/unregister`. Hand edits work but formatting and comments are not preserved.";

/// Serialize the config to TOML text, prefixed with the managed-file banner.
pub(crate) fn to_toml(config: &ConfigFile) -> Result<String, ConfigError> {
    let body =
        toml::to_string_pretty(config).map_err(|source| ConfigError::ConfigSerialize { source })?;
    Ok(format!("{MANAGED_HEADER}\n\n{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_is_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("config.toml");
        let config = read(&missing).unwrap();
        assert!(config.vaults.is_empty());
        assert!(config.extra.is_empty());
    }

    #[test]
    fn unknown_keys_round_trip() {
        let input = "\
schema_version = 7

[vaults.docs]
root = \"/vaults/docs\"
auth_token = \"secret\"

[vaults.docs.remote]
endpoint = \"https://example.invalid\"
";
        let parsed: ConfigFile = toml::from_str(input).unwrap();
        assert_eq!(parsed.vaults["docs"].root, PathBuf::from("/vaults/docs"));
        assert!(parsed.vaults["docs"].extra.contains_key("auth_token"));
        assert!(parsed.vaults["docs"].extra.contains_key("remote"));
        assert!(parsed.extra.contains_key("schema_version"));

        let out = to_toml(&parsed).unwrap();
        let reparsed: ConfigFile = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed.vaults["docs"].extra["auth_token"],
            toml::Value::String("secret".into())
        );
        assert_eq!(reparsed.extra["schema_version"], toml::Value::Integer(7));
        assert!(reparsed.vaults["docs"].extra.contains_key("remote"));
    }

    #[test]
    fn serialize_prepends_managed_header_and_reparses() {
        let mut config = ConfigFile::default();
        config.vaults.insert(
            "docs".into(),
            VaultEntry {
                root: PathBuf::from("/vaults/docs"),
                config: None,
                cache: None,
                logs: None,
                extra: BTreeMap::new(),
            },
        );
        let out = to_toml(&config).unwrap();
        assert!(
            out.starts_with(MANAGED_HEADER),
            "header not prepended: {out}"
        );
        // The banner is a TOML comment, so a round-trip is a no-op.
        let reparsed: ConfigFile = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.vaults["docs"].root, PathBuf::from("/vaults/docs"));
        assert!(reparsed.extra.is_empty());
    }

    #[test]
    fn optional_overrides_omitted_when_absent() {
        let mut config = ConfigFile::default();
        config.vaults.insert(
            "docs".into(),
            VaultEntry {
                root: PathBuf::from("/vaults/docs"),
                config: None,
                cache: None,
                logs: None,
                extra: BTreeMap::new(),
            },
        );
        let out = to_toml(&config).unwrap();
        assert!(out.contains("root = \"/vaults/docs\""));
        assert!(!out.contains("config ="));
        assert!(!out.contains("cache ="));
        assert!(!out.contains("logs ="));
    }
}
