use std::fs;

use crate::graph::IndexOptions;
use crate::standards::{
    parse_config_compiled, CompiledConfig, RepairConfig, ValidateConfig, VaultConfig,
};
use anyhow::Result;
use camino::Utf8PathBuf;

pub struct LoadedConfig {
    pub index_options: IndexOptions,
    pub validate: ValidateConfig,
    pub repair: RepairConfig,
    /// Full parsed vault config. Commands that need the whole VaultConfig
    /// (e.g. `norn set`'s schema-aware path) should use this field.
    pub vault_config: VaultConfig,
    /// Pre-compiled path patterns for hot-path matching (validate engine).
    pub compiled: CompiledConfig,
}

/// Environment variable that supplies the default vault root when `-C/--cwd`
/// is not passed.
pub const NORN_ROOT_ENV: &str = "NORN_ROOT";

/// Which vault root was *requested*, applying precedence `-C/--cwd` >
/// `NORN_ROOT`. An empty or whitespace-only `NORN_ROOT` is ignored. `None`
/// means neither was supplied and the caller falls back to the process cwd.
///
/// A pure function of its inputs so precedence is unit-testable without
/// mutating the process environment.
fn requested_root(explicit: Option<&Utf8PathBuf>, env_root: Option<&str>) -> Option<Utf8PathBuf> {
    explicit.cloned().or_else(|| {
        env_root
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(Utf8PathBuf::from)
    })
}

/// Ground a non-absolute requested root against the process cwd: a relative
/// request is joined to `current_dir`, and `None` (nothing requested) yields
/// `current_dir` itself. Callers handle absolute requests before this point,
/// so they never reach here.
fn ground_relative(requested: Option<Utf8PathBuf>, current_dir: &Utf8PathBuf) -> Utf8PathBuf {
    match requested {
        Some(relative) => current_dir.join(relative),
        None => current_dir.clone(),
    }
}

/// Resolve the effective vault root.
///
/// Precedence: `-C/--cwd` > `$NORN_ROOT` > the process working directory. A
/// relative path from either explicit source is resolved against the process
/// cwd; an absolute one is used as-is (and never reads the process cwd).
pub fn effective_cwd(cwd: Option<&Utf8PathBuf>) -> Result<Utf8PathBuf> {
    let env_root = std::env::var(NORN_ROOT_ENV).ok();
    let requested = requested_root(cwd, env_root.as_deref());

    // An absolute request never needs the process cwd.
    if let Some(path) = &requested {
        if path.is_absolute() {
            return Ok(path.clone());
        }
    }

    let current_dir = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("failed to read current directory: {error}"))?;
    let current_dir = Utf8PathBuf::from_path_buf(current_dir).map_err(|path| {
        anyhow::anyhow!("current directory is not valid UTF-8: {}", path.display())
    })?;

    Ok(ground_relative(requested, &current_dir))
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
            let discovered = cwd.join(".norn/config.yaml");
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

    let (resolved_index_set, resolved_index_set_hash) =
        crate::standards::resolved_index_set(&config);

    Ok(LoadedConfig {
        index_options: IndexOptions {
            ignore: config.files.ignore.clone(),
            alias_field: config.links.alias_field.clone(),
            auto: config.index.auto,
            resolved_index_set,
            resolved_index_set_hash,
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
            .prefix("norn-alias-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
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
    fn index_auto_defaults_true_when_no_config_present() {
        let dir = tempfile::Builder::new()
            .prefix("norn-index-auto-default-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let loaded = load_config(&root, None).unwrap();
        assert!(loaded.index_options.auto);
    }

    #[test]
    fn index_auto_false_propagates_from_config() {
        let dir = tempfile::Builder::new()
            .prefix("norn-index-auto-false-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.yaml"), "index:\n  auto: false\n").unwrap();

        let loaded = load_config(&root, None).unwrap();
        assert!(!loaded.index_options.auto);
    }

    #[test]
    fn alias_field_absent_in_config_yields_none() {
        let dir = tempfile::Builder::new()
            .prefix("norn-alias-none-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.yaml"), "files:\n  ignore: []\n").unwrap();

        let loaded = load_config(&root, None).unwrap();
        assert!(loaded.index_options.alias_field.is_none());
    }

    #[test]
    fn requested_root_prefers_explicit_over_env() {
        let explicit = Utf8PathBuf::from("/explicit");
        assert_eq!(
            requested_root(Some(&explicit), Some("/from-env")),
            Some(Utf8PathBuf::from("/explicit"))
        );
    }

    #[test]
    fn requested_root_falls_back_to_env() {
        assert_eq!(
            requested_root(None, Some("/from-env")),
            Some(Utf8PathBuf::from("/from-env"))
        );
    }

    #[test]
    fn requested_root_ignores_empty_or_whitespace_env() {
        assert_eq!(requested_root(None, Some("")), None);
        assert_eq!(requested_root(None, Some("   ")), None);
        assert_eq!(requested_root(None, None), None);
    }

    #[test]
    fn requested_root_trims_env_value() {
        assert_eq!(
            requested_root(None, Some("  /padded  ")),
            Some(Utf8PathBuf::from("/padded"))
        );
    }

    #[test]
    fn ground_relative_joins_relative_request_against_cwd() {
        let cwd = Utf8PathBuf::from("/base/dir");
        assert_eq!(
            ground_relative(Some(Utf8PathBuf::from("vaults/atlas")), &cwd),
            Utf8PathBuf::from("/base/dir/vaults/atlas")
        );
    }

    #[test]
    fn ground_relative_yields_cwd_when_nothing_requested() {
        let cwd = Utf8PathBuf::from("/base/dir");
        assert_eq!(ground_relative(None, &cwd), cwd);
    }
}
