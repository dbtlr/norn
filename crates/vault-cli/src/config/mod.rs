pub mod edit;
pub mod migrate;
pub mod show;
pub mod validate;

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{ColorWhen, ConfigEditArgs, ConfigShowArgs, ConfigValidateArgs};

/// Resolved discovery paths for the active `.vault/config.yaml` plus the
/// vault root the command will operate against, plus the per-vault cache
/// database path. Shared across `vault config show / validate / migrate /
/// edit` so the four subcommands report consistent resolution.
pub struct Discovery {
    pub config_file: Utf8PathBuf,
    pub vault_root: Utf8PathBuf,
    pub cache: Utf8PathBuf,
}

/// Resolve the config file, vault root, and cache path for the current
/// invocation. With `config_override = Some(path)` the override wins;
/// otherwise the loader looks for `<cwd>/.vault/config.yaml` and errors
/// with a `vault init` hint when no config is present.
pub fn discover(cwd: &Utf8Path, config_override: Option<&Utf8PathBuf>) -> Result<Discovery> {
    let config_file = match config_override {
        Some(path) => path.clone(),
        None => {
            let candidate = cwd.join(".vault").join("config.yaml");
            if !candidate.exists() {
                return Err(anyhow!(
                    "no .vault/config.yaml found in {cwd}\nhint: run `vault init` to scaffold one"
                ));
            }
            candidate
        }
    };
    let vault_root = cwd.to_owned();
    let (_canonical, cache_dir) =
        vault_cache::cache_dir_for(&vault_root).map_err(|e| anyhow!("resolve cache dir: {e}"))?;
    let cache = cache_dir.join("cache.db");
    Ok(Discovery {
        config_file,
        vault_root,
        cache,
    })
}

pub fn run_show(
    cwd: &Utf8Path,
    config: Option<&Utf8PathBuf>,
    args: &ConfigShowArgs,
    color: ColorWhen,
) -> Result<i32> {
    show::run(cwd, config, args, color)
}

pub fn run_validate(
    cwd: &Utf8Path,
    config: Option<&Utf8PathBuf>,
    args: &ConfigValidateArgs,
    color: ColorWhen,
) -> Result<i32> {
    validate::run(cwd, config, args, color)
}

pub fn run_migrate(cwd: &Utf8Path, config: Option<&Utf8PathBuf>) -> Result<i32> {
    migrate::run(cwd, config)
}

pub fn run_edit(
    cwd: &Utf8Path,
    config: Option<&Utf8PathBuf>,
    args: &ConfigEditArgs,
    color: ColorWhen,
) -> Result<i32> {
    edit::run(cwd, config, args, color)
}
