//! The `norn config` subcommands.
//!
//! `show`, `validate`, `migrate`, and `edit` for the vault's
//! `.norn/config.yaml`. `Discovery` resolves the config file, vault root, and
//! cache path once so the four subcommands report consistent resolution. The
//! `Config` arm in `lib.rs` dispatches here; `config_loader` handles the load
//! path commands actually run against. `config edit` is `LocalOnly`; the read
//! subcommands are a tracked MCP gap (NRN-189).

pub mod edit;
pub mod migrate;
pub mod show;
pub mod validate;

use anyhow::{anyhow, Result};
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{ColorWhen, ConfigEditArgs, ConfigShowArgs, ConfigValidateArgs};

/// Resolved discovery paths for the active `.norn/config.yaml` plus the
/// vault root the command will operate against, plus the per-vault cache
/// database path. Shared across `norn config show / validate / migrate /
/// edit` so the four subcommands report consistent resolution.
pub struct Discovery {
    pub config_file: Utf8PathBuf,
    pub vault_root: Utf8PathBuf,
    pub cache: Utf8PathBuf,
}

/// Resolve the config file, vault root, and cache path for the current
/// invocation. With `config_override = Some(path)` the override wins;
/// otherwise the loader looks for `<cwd>/.norn/config.yaml` and errors
/// with a `norn init` hint when no config is present.
pub fn discover(cwd: &Utf8Path, config_override: Option<&Utf8PathBuf>) -> Result<Discovery> {
    let config_file = match config_override {
        Some(path) => path.clone(),
        None => {
            let candidate = cwd.join(".norn").join("config.yaml");
            if !candidate.exists() {
                return Err(anyhow!(
                    "no .norn/config.yaml found in {cwd}\nhint: run `norn init` to scaffold one"
                ));
            }
            candidate
        }
    };
    let vault_root = cwd.to_owned();
    let (_canonical, cache_dir) =
        crate::cache::cache_dir_for(&vault_root).map_err(|e| anyhow!("resolve cache dir: {e}"))?;
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
