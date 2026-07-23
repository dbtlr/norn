//! `vault list` (NRN-409).

use std::io;

use norn_config::RegisteredVault;
use serde::Serialize;

use crate::display::conversation::Conversation;
use crate::display::emit::render_outcome;
use crate::display::format::Format;
use crate::display::output::VaultListView;
use crate::display::sink::Sink;
use crate::display::EXIT_OK;

pub(crate) fn render_vault_list(
    view: VaultListView,
    format: Format,
    sink: &mut Sink<'_>,
    conv: &mut Conversation<'_>,
) -> i32 {
    match format {
        Format::Json => list_json(&view.vaults, sink, conv),
        _ => list_human(&view.vaults, sink, conv),
    }
}

fn list_human(vaults: &[RegisteredVault], sink: &mut Sink<'_>, conv: &mut Conversation<'_>) -> i32 {
    if vaults.is_empty() {
        conv.diagnostic("no vaults registered");
        return EXIT_OK;
    }
    let result: io::Result<i32> = (|| {
        for vault in vaults {
            writeln!(
                sink.writer(),
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
                    writeln!(
                        sink.writer(),
                        "    {label} = {path}",
                        path = path_display(path)
                    )?;
                }
            }
        }
        Ok(EXIT_OK)
    })();
    render_outcome(result, conv.writer())
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

fn list_json(vaults: &[RegisteredVault], sink: &mut Sink<'_>, conv: &mut Conversation<'_>) -> i32 {
    let rows: Vec<VaultJson> = vaults.iter().map(VaultJson::from).collect();
    match serde_json::to_string_pretty(&rows) {
        Ok(text) => {
            let result: io::Result<i32> = (|| {
                writeln!(sink.writer(), "{text}")?;
                Ok(EXIT_OK)
            })();
            render_outcome(result, conv.writer())
        }
        Err(source) => {
            conv.diagnostic(&format!("failed to serialize registry as JSON: {source}"));
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
    use crate::display::format::{FormatChoice, FormatSpec};
    use crate::display::Presenter;
    use crate::display::EXIT_OPERATIONAL;
    use crate::output::palette::Palette;
    use crate::test_support::FailingWriter;
    use std::io::Write;

    /// Drive `render_vault_list` through the same resolution `emit` performs —
    /// `vault list` is unstyled, so a no-op palette sink.
    fn drive<O: Write, E: Write>(view: VaultListView, presenter: &mut Presenter<O, E>) -> i32 {
        let format = view.format.resolve(false);
        let palette = Palette::off();
        let (out, err) = presenter.streams();
        let mut sink = Sink::new(out, &palette, 80);
        let mut conv = Conversation::new(err);
        render_vault_list(view, format, &mut sink, &mut conv)
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
            format: FormatChoice {
                explicit: Some(explicit),
                spec: FormatSpec {
                    tty: Format::Records,
                    piped: Format::Records,
                },
            },
        }
    }

    #[test]
    fn render_vault_list_human_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            drive(vault_list_view(Format::Records), &mut presenter)
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
            drive(vault_list_view(Format::Records), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }

    #[test]
    fn render_vault_list_json_tolerates_broken_pipe() {
        let mut err = Vec::new();
        let code = {
            let mut presenter = Presenter::new(FailingWriter(io::ErrorKind::BrokenPipe), &mut err);
            drive(vault_list_view(Format::Json), &mut presenter)
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
            drive(vault_list_view(Format::Json), &mut presenter)
        };
        assert_eq!(code, EXIT_OPERATIONAL);
        assert!(String::from_utf8(err).unwrap().starts_with("norn: "));
    }
}
