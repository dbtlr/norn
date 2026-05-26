use std::fs;

use anyhow::Result;
use camino::Utf8PathBuf;
use vault_graph::IndexOptions;
use vault_standards::{
    parse_config_compiled, CompiledConfig, RepairConfig, ValidateConfig, VaultConfig,
};

pub struct LoadedConfig {
    pub index_options: IndexOptions,
    pub validate: ValidateConfig,
    pub repair: RepairConfig,
    /// Full parsed vault config. Commands that need the whole VaultConfig
    /// (e.g. `vault set`'s schema-aware path) should use this field.
    pub vault_config: VaultConfig,
    /// Pre-compiled path patterns for hot-path matching (validate engine).
    pub compiled: CompiledConfig,
}

pub fn effective_cwd(cwd: Option<&Utf8PathBuf>) -> Result<Utf8PathBuf> {
    let Some(cwd) = cwd else {
        let current_dir = std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("failed to read current directory: {error}"))?;
        return Utf8PathBuf::from_path_buf(current_dir).map_err(|path| {
            anyhow::anyhow!("current directory is not valid UTF-8: {}", path.display())
        });
    };

    if cwd.is_absolute() {
        return Ok(cwd.clone());
    }

    let current_dir = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("failed to read current directory: {error}"))?;
    let current_dir = Utf8PathBuf::from_path_buf(current_dir).map_err(|path| {
        anyhow::anyhow!("current directory is not valid UTF-8: {}", path.display())
    })?;
    Ok(current_dir.join(cwd))
}

pub fn resolve_path(cwd: &Utf8PathBuf, path: &Utf8PathBuf) -> Utf8PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        cwd.join(path)
    }
}

pub fn load_config(cwd: &Utf8PathBuf, config_path: Option<&Utf8PathBuf>) -> Result<LoadedConfig> {
    let resolved_config_path = config_path
        .map(|config_path| resolve_path(cwd, config_path))
        .or_else(|| {
            let discovered = cwd.join(".vault/config.yaml");
            discovered.exists().then_some(discovered)
        });

    let Some(config_path) = resolved_config_path else {
        return Ok(LoadedConfig {
            index_options: IndexOptions::default(),
            validate: ValidateConfig::default(),
            repair: RepairConfig::default(),
            vault_config: VaultConfig::default(),
            compiled: CompiledConfig::default(),
        });
    };

    let config_text = fs::read_to_string(&config_path)
        .map_err(|error| anyhow::anyhow!("failed to read config {config_path}: {error}"))?;
    let (config, compiled) =
        parse_config_compiled(&config_text, &config_path).map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(LoadedConfig {
        index_options: IndexOptions {
            ignore: config.files.ignore.clone(),
            alias_field: config.links.alias_field.clone(),
        },
        validate: config.validate.clone(),
        repair: config.repair.clone(),
        vault_config: config,
        compiled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_field_propagates_from_config_to_index_options() {
        let dir = tempfile::Builder::new()
            .prefix("vault-cli-alias-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".vault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.yaml"),
            "links:\n  alias_field: aliases\n",
        )
        .unwrap();

        let loaded = load_config(&root, None).unwrap();
        assert_eq!(loaded.index_options.alias_field.as_deref(), Some("aliases"));
    }

    #[test]
    fn alias_field_absent_in_config_yields_none() {
        let dir = tempfile::Builder::new()
            .prefix("vault-cli-alias-none-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".vault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.yaml"), "files:\n  ignore: []\n").unwrap();

        let loaded = load_config(&root, None).unwrap();
        assert!(loaded.index_options.alias_field.is_none());
    }
}
