use std::fs;

use anyhow::Result;
use camino::Utf8PathBuf;
use vault_graph::IndexOptions;
use vault_standards::{validate_config_yaml, ValidateConfig, VaultConfig};

pub struct LoadedConfig {
    pub index_options: IndexOptions,
    pub validate: ValidateConfig,
}

pub fn effective_cwd(cwd: &Utf8PathBuf) -> Result<Utf8PathBuf> {
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

    let config = match resolved_config_path {
        Some(config_path) => {
            let config_text = fs::read_to_string(&config_path)
                .map_err(|error| anyhow::anyhow!("failed to read config {config_path}: {error}"))?;
            let config_value =
                serde_yaml::from_str::<serde_yaml::Value>(&config_text).map_err(|error| {
                    anyhow::anyhow!("failed to parse config {config_path}: {error}")
                })?;
            validate_config_yaml(&config_path, &config_value)?;
            serde_yaml::from_value::<VaultConfig>(config_value)
                .map_err(|error| anyhow::anyhow!("failed to parse config {config_path}: {error}"))?
        }
        None => VaultConfig::default(),
    };

    Ok(LoadedConfig {
        index_options: IndexOptions {
            ignore: config.files.ignore,
        },
        validate: config.validate,
    })
}
