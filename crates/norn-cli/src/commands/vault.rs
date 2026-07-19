//! `norn vault` — the registry verb surface: `register` / `unregister` /
//! `list` / `set`.
//!
//! This is an intentionally-new CLI surface (no oracle, no parity constraint):
//! the sanctioned way to manage norn's central config — the machine-local
//! authoritative source of vault identity that gates every durable artifact
//! (persistent cache, event stream, logs) per ADR 0017. Registration is the
//! explicit setup act that promotes a vault from disposable/tmp-homed to
//! durable; `set` is the sanctioned in-place edit so agents and humans never
//! hand-author the config file to fill gaps.
//!
//! Unlike the read-verb exemplars (`find` / `get`), these verbs EXECUTE now:
//! they call into [`norn_config`], the sole performer of central-config IO. The
//! ambient environment read ([`ConfigHome::from_env`]) happens once, at the
//! dispatch boundary; `run` takes an already-constructed home so it stays
//! testable against an injected config directory.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args, Subcommand};
use norn_config::{ConfigHome, RegisteredVault, Registry, VaultChanges, VaultOverrides};
use serde::Serialize;

use crate::display::{Format, Presenter, EXIT_OK, EXIT_OPERATIONAL};

/// The `vault` namespace: manage the central vault registry.
#[derive(Subcommand, Debug)]
pub enum VaultCmd {
    #[command(
        disable_help_flag = true,
        about = "Register a vault under a short name — the setup act that unlocks durable artifacts (cache, event stream, logs)"
    )]
    Register(RegisterArgs),
    #[command(
        disable_help_flag = true,
        about = "Remove a vault registration (durable artifacts are no longer kept)"
    )]
    Unregister(UnregisterArgs),
    #[command(
        disable_help_flag = true,
        about = "List registered vaults — name, root, and any stored location overrides"
    )]
    List(ListArgs),
    #[command(
        disable_help_flag = true,
        about = "Edit a registration in place — the sanctioned mutation path, so the config file is never hand-edited to fill gaps"
    )]
    Set(SetArgs),
}

/// `norn vault register <name> [PATH]`.
#[derive(Args, Debug)]
pub struct RegisterArgs {
    /// Short name to register the vault under ([a-z0-9][a-z0-9_-]*).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Vault root directory (default: the current directory). Must be an
    /// existing directory; stored canonicalized.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Config-location override, stored verbatim (not canonicalized).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Cache-location override, stored verbatim (not canonicalized).
    #[arg(long, value_name = "PATH")]
    pub cache: Option<PathBuf>,

    /// Logs-location override, stored verbatim (not canonicalized).
    #[arg(long, value_name = "PATH")]
    pub logs: Option<PathBuf>,
}

impl RegisterArgs {
    fn overrides(&self) -> VaultOverrides {
        VaultOverrides {
            config: self.config.clone(),
            cache: self.cache.clone(),
            logs: self.logs.clone(),
        }
    }
}

/// `norn vault unregister <name>`.
#[derive(Args, Debug)]
pub struct UnregisterArgs {
    /// The registered name to remove.
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// `norn vault list [--format human|json]`.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Output format.
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    pub format: Format,
}

/// `norn vault set <name> ...`. At least one change flag is required (an empty
/// change set is a usage error); each `--clear-*` competes with its `--*` over a
/// single override, resolved last-wins (NRN-331) rather than as a hard conflict.
#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("changes")
        .required(true)
        .multiple(true)
        .args(["root", "config", "clear_config", "cache", "clear_cache", "logs", "clear_logs"])
))]
pub struct SetArgs {
    /// The registered name to edit.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Re-point the vault root (canonicalized; must be an existing directory).
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Set the config-location override (stored verbatim).
    #[arg(long, value_name = "PATH", overrides_with = "clear_config")]
    pub config: Option<PathBuf>,
    /// Remove the config-location override.
    #[arg(long = "clear-config", overrides_with = "config")]
    pub clear_config: bool,

    /// Set the cache-location override (stored verbatim).
    #[arg(long, value_name = "PATH", overrides_with = "clear_cache")]
    pub cache: Option<PathBuf>,
    /// Remove the cache-location override.
    #[arg(long = "clear-cache", overrides_with = "cache")]
    pub clear_cache: bool,

    /// Set the logs-location override (stored verbatim).
    #[arg(long, value_name = "PATH", overrides_with = "clear_logs")]
    pub logs: Option<PathBuf>,
    /// Remove the logs-location override.
    #[arg(long = "clear-logs", overrides_with = "logs")]
    pub clear_logs: bool,
}

impl SetArgs {
    fn to_changes(&self) -> VaultChanges {
        VaultChanges {
            root: self.root.clone(),
            config: tri_state(self.config.clone(), self.clear_config),
            cache: tri_state(self.cache.clone(), self.clear_cache),
            logs: tri_state(self.logs.clone(), self.clear_logs),
        }
    }
}

/// Fold a `--set PATH` / `--clear` flag pair into the override tri-state:
/// `None` = untouched, `Some(None)` = clear, `Some(Some(path))` = set. clap's
/// `overrides_with` resolves a set-and-clear on one field last-wins BEFORE this
/// runs, so at most one of the pair is ever present here (NRN-331).
fn tri_state(set: Option<PathBuf>, clear: bool) -> Option<Option<PathBuf>> {
    match (set, clear) {
        (Some(path), _) => Some(Some(path)),
        (None, true) => Some(None),
        (None, false) => None,
    }
}

/// Run a `vault` subcommand against an injected config home and effective
/// cwd. The ambient reads (config env, process cwd + `-C`) happen once at the
/// dispatch boundary; this entry stays a pure function of its inputs so tests
/// inject a temp directory.
pub fn run<O: Write, E: Write>(
    cmd: &VaultCmd,
    home: ConfigHome,
    cwd: &Path,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let registry = Registry::new(home);
    match cmd {
        VaultCmd::Register(args) => register(&registry, args, cwd, presenter),
        VaultCmd::Unregister(args) => unregister(&registry, args, presenter),
        VaultCmd::List(args) => list(&registry, args, presenter),
        VaultCmd::Set(args) => set(&registry, args, cwd, presenter),
    }
}

fn register<O: Write, E: Write>(
    registry: &Registry,
    args: &RegisterArgs,
    cwd: &Path,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    // PATH defaults to the effective cwd (which honors `-C`); a relative PATH
    // is grounded against it. Registration names a concrete root, it does not
    // resolve one — but the root it names must respect the invocation's cwd.
    let root = match args.path.clone() {
        Some(path) => cwd.join(path),
        None => cwd.to_path_buf(),
    };
    match registry.register(&args.name, &root, args.overrides()) {
        Ok(vault) => {
            confirm(presenter, "registered", &vault);
            EXIT_OK
        }
        Err(err) => fail(presenter, err),
    }
}

fn unregister<O: Write, E: Write>(
    registry: &Registry,
    args: &UnregisterArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    match registry.unregister(&args.name) {
        Ok(()) => {
            let _ = writeln!(
                presenter.out(),
                "norn: unregistered {name:?}",
                name = args.name
            );
            EXIT_OK
        }
        Err(err) => fail(presenter, err),
    }
}

fn set<O: Write, E: Write>(
    registry: &Registry,
    args: &SetArgs,
    cwd: &Path,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let mut changes = args.to_changes();
    // A relative --root is grounded against the effective cwd, like register's
    // PATH. Overrides stay verbatim by design.
    if let Some(root) = changes.root.take() {
        changes.root = Some(cwd.join(root));
    }
    match registry.set(&args.name, changes) {
        Ok(outcome) if outcome.changed => {
            confirm(presenter, "updated", &outcome.vault);
            EXIT_OK
        }
        Ok(outcome) => {
            let _ = writeln!(
                presenter.out(),
                "norn: no changes for {name:?}",
                name = outcome.vault.name
            );
            EXIT_OK
        }
        Err(err) => fail(presenter, err),
    }
}

fn list<O: Write, E: Write>(
    registry: &Registry,
    args: &ListArgs,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let vaults = match registry.list() {
        Ok(vaults) => vaults,
        Err(err) => return fail(presenter, err),
    };
    match args.format {
        Format::Human => list_human(&vaults, presenter),
        Format::Json => list_json(&vaults, presenter),
    }
}

fn list_human<O: Write, E: Write>(
    vaults: &[RegisteredVault],
    presenter: &mut Presenter<O, E>,
) -> i32 {
    if vaults.is_empty() {
        presenter.diagnostic("no vaults registered");
        return EXIT_OK;
    }
    let out = presenter.out();
    for vault in vaults {
        let _ = writeln!(
            out,
            "{name}  {root}",
            name = vault.name,
            root = display(&vault.root)
        );
        for (label, path) in [
            ("config", &vault.config),
            ("cache", &vault.cache),
            ("logs", &vault.logs),
        ] {
            if let Some(path) = path {
                let _ = writeln!(out, "    {label} = {path}", path = display(path));
            }
        }
    }
    EXIT_OK
}

/// The stable machine shape: an array of objects, one per vault, each with
/// `name`, `root`, and the three override fields — absent overrides are
/// explicit JSON `null`, so every object carries the same fixed key set. Order
/// is `Registry::list`'s deterministic name order.
#[derive(Serialize)]
struct VaultJson {
    name: String,
    root: String,
    config: Option<String>,
    cache: Option<String>,
    logs: Option<String>,
}

impl From<&RegisteredVault> for VaultJson {
    fn from(vault: &RegisteredVault) -> Self {
        Self {
            name: vault.name.clone(),
            root: display(&vault.root),
            config: vault.config.as_deref().map(display),
            cache: vault.cache.as_deref().map(display),
            logs: vault.logs.as_deref().map(display),
        }
    }
}

fn list_json<O: Write, E: Write>(
    vaults: &[RegisteredVault],
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let rows: Vec<VaultJson> = vaults.iter().map(VaultJson::from).collect();
    match serde_json::to_string_pretty(&rows) {
        Ok(text) => {
            let _ = writeln!(presenter.out(), "{text}");
            EXIT_OK
        }
        Err(source) => {
            presenter.diagnostic(&format!("failed to serialize registry as JSON: {source}"));
            EXIT_OPERATIONAL
        }
    }
}

/// One stdout confirmation line, matching norn's voice: `norn: <verb> "name" -> <root>`.
fn confirm<O: Write, E: Write>(
    presenter: &mut Presenter<O, E>,
    verb: &str,
    vault: &RegisteredVault,
) {
    let _ = writeln!(
        presenter.out(),
        "norn: {verb} {name:?} -> {root}",
        name = vault.name,
        root = display(&vault.root)
    );
}

/// Render a [`norn_config::ConfigError`] through the stderr diagnostic seam and
/// return the operational exit code.
fn fail<O: Write, E: Write>(presenter: &mut Presenter<O, E>, err: norn_config::ConfigError) -> i32 {
    presenter.diagnostic(&err.to_string());
    EXIT_OPERATIONAL
}

/// Lossy path→string for display and JSON. Registered roots are canonical UTF-8
/// in practice; the lossy fallback only affects non-UTF-8 override paths.
fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn vault_cmd(argv: &[&str]) -> VaultCmd {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Vault(v) => v,
            other => panic!("expected vault, got {other:?}"),
        }
    }

    #[test]
    fn register_defaults_path_to_none_and_collects_overrides() {
        let cmd = vault_cmd(&[
            "norn", "vault", "register", "docs", "--cache", "/c", "--logs", "/l",
        ]);
        match cmd {
            VaultCmd::Register(a) => {
                assert_eq!(a.name, "docs");
                assert_eq!(a.path, None);
                let o = a.overrides();
                assert_eq!(o.config, None);
                assert_eq!(o.cache, Some(PathBuf::from("/c")));
                assert_eq!(o.logs, Some(PathBuf::from("/l")));
            }
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn register_takes_explicit_path() {
        let cmd = vault_cmd(&["norn", "vault", "register", "docs", "/vaults/docs"]);
        match cmd {
            VaultCmd::Register(a) => assert_eq!(a.path, Some(PathBuf::from("/vaults/docs"))),
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn set_maps_flags_to_tri_state_changes() {
        let cmd = vault_cmd(&[
            "norn",
            "vault",
            "set",
            "docs",
            "--root",
            "/new/root",
            "--cache",
            "/c",
            "--clear-logs",
        ]);
        match cmd {
            VaultCmd::Set(a) => {
                let changes = a.to_changes();
                assert_eq!(changes.root, Some(PathBuf::from("/new/root")));
                assert_eq!(changes.config, None); // untouched
                assert_eq!(changes.cache, Some(Some(PathBuf::from("/c")))); // set
                assert_eq!(changes.logs, Some(None)); // cleared
            }
            other => panic!("expected set, got {other:?}"),
        }
    }

    #[test]
    fn set_with_no_change_flags_is_a_usage_error() {
        let err = Cli::try_parse_from(["norn", "vault", "set", "docs"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn set_and_clear_on_one_field_is_last_wins() {
        // NRN-331: `--cache PATH --clear-cache` no longer errors; the last of
        // the two wins. Clear is last → the override is cleared.
        let clear_last = match vault_cmd(&[
            "norn",
            "vault",
            "set",
            "docs",
            "--cache",
            "/c",
            "--clear-cache",
        ]) {
            VaultCmd::Set(a) => a.to_changes(),
            other => panic!("expected set, got {other:?}"),
        };
        assert_eq!(
            clear_last.cache,
            Some(None),
            "clear is last → clear the override"
        );

        // Set is last → the override is set to the path.
        let set_last = match vault_cmd(&[
            "norn",
            "vault",
            "set",
            "docs",
            "--clear-cache",
            "--cache",
            "/c",
        ]) {
            VaultCmd::Set(a) => a.to_changes(),
            other => panic!("expected set, got {other:?}"),
        };
        assert_eq!(
            set_last.cache,
            Some(Some(PathBuf::from("/c"))),
            "set is last → set the override verbatim"
        );
    }

    #[test]
    fn list_format_defaults_to_human_and_parses_json() {
        match vault_cmd(&["norn", "vault", "list"]) {
            VaultCmd::List(a) => assert_eq!(a.format, Format::Human),
            other => panic!("expected list, got {other:?}"),
        }
        match vault_cmd(&["norn", "vault", "list", "--format", "json"]) {
            VaultCmd::List(a) => assert_eq!(a.format, Format::Json),
            other => panic!("expected list, got {other:?}"),
        }
    }
}
