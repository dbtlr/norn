//! `vault list` (NRN-409).

use std::io::{self, Write};

use norn_config::RegisteredVault;
use serde::Serialize;

use crate::display::emit::{is_stdout_tty, render_outcome};
use crate::display::format::Format;
use crate::display::output::VaultListView;
use crate::display::{Presenter, EXIT_OK};

pub(crate) fn render_vault_list<O: Write, E: Write>(
    view: VaultListView,
    presenter: &mut Presenter<O, E>,
) -> i32 {
    let format = view.spec.resolve(view.explicit, is_stdout_tty());
    match format {
        Format::Json => list_json(&view.vaults, presenter),
        _ => list_human(&view.vaults, presenter),
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
    let (out, err) = presenter.streams();
    let result: io::Result<i32> = (|| {
        for vault in vaults {
            writeln!(
                out,
                "{name}  {root}",
                name = vault.name,
                root = path_display(&vault.root)
            )?;
            for (label, path) in [
                ("config", &vault.config),
                ("cache", &vault.cache),
                ("logs", &vault.logs),
            ] {
                if let Some(path) = path {
                    writeln!(out, "    {label} = {path}", path = path_display(path))?;
                }
            }
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, err)
}

/// The stable machine shape: an array of objects, one per vault, absent overrides
/// explicit JSON `null` (donor `vault::VaultJson`).
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
            root: path_display(&vault.root),
            config: vault.config.as_deref().map(path_display),
            cache: vault.cache.as_deref().map(path_display),
            logs: vault.logs.as_deref().map(path_display),
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
            let (out, err) = presenter.streams();
            let result: io::Result<i32> = (|| {
                writeln!(out, "{text}")?;
                Ok(EXIT_OK)
            })();
            render_outcome(result, err)
        }
        Err(source) => {
            presenter.diagnostic(&format!("failed to serialize registry as JSON: {source}"));
            crate::display::EXIT_OPERATIONAL
        }
    }
}

/// Lossy path→string for display and JSON (donor `vault::display`).
fn path_display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::format::FormatSpec;
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;

    /// A `Write` that fails every write with a fixed [`io::ErrorKind`].
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sample_vault() -> RegisteredVault {
        RegisteredVault {
            name: "docs".into(),
            root: std::path::PathBuf::from("/vaults/docs"),
            config: None,
            cache: None,
            logs: None,
        }
    }

    fn vault_list_view(explicit: Format) -> VaultListView {
        VaultListView {
            vaults: vec![sample_vault()],
            explicit: Some(explicit),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        }
    }

    #[test]
    fn render_vault_list_human_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_vault_list(vault_list_view(Format::Records), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_vault_list_human_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_vault_list(vault_list_view(Format::Records), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn render_vault_list_json_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            render_vault_list(vault_list_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty());
    }

    #[test]
    fn render_vault_list_json_reports_other_io_errors() {
        let mut err = Vec::new();
        let code = {
            let mut presenter =
                Presenter::new(FailingWriter(io::ErrorKind::PermissionDenied), &mut err);
            render_vault_list(vault_list_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }
}
