//! The launchd unit for `norn serve` (NRN-115).
//!
//! norn has exactly ONE supervised unit — the warm host daemon — so this is a
//! single plist shape, not Mimir's serve+snapshot pair: a `KeepAlive` +
//! `RunAtLoad` daemon whose `ProgramArguments` are `<resolved binary> serve`
//! and whose stdout/stderr redirect to one log file. The daemon names vaults
//! per connection, so the plist carries no vault path and never needs
//! rewriting to retarget.

use camino::Utf8PathBuf;

/// launchd label for the serve daemon, following Mimir's `com.dbtlr.<app>.serve`
/// precedent. This is also the plist basename and the `gui/<uid>/<label>` service
/// target `launchctl` addresses.
pub const SERVE_LABEL: &str = "com.dbtlr.norn.serve";

/// `~/Library/LaunchAgents/<label>.plist` — the per-user LaunchAgents location a
/// `gui/<uid>` bootstrap loads from. Requires `$HOME`; the surface is macOS-only.
pub fn plist_path() -> anyhow::Result<Utf8PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow::anyhow!("cannot locate the LaunchAgents directory: $HOME is not set")
    })?;
    Ok(Utf8PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVE_LABEL}.plist")))
}

/// The daemon's launchd stdout/stderr sink: `<XDG_CACHE_HOME>/norn/log/serve.log`.
///
/// A sibling of the daemon's `run/` directory under the norn cache tree, so a
/// short (non-64-hex) name the cache pruner never treats as a vault entry.
/// launchd does no `~`/`$VAR` expansion and will not create the parent, so
/// `service install` must `mkdir -p` this file's directory before bootstrapping.
pub fn log_path() -> anyhow::Result<Utf8PathBuf> {
    Ok(crate::cache::cache_tree_root()?
        .join("log")
        .join("serve.log"))
}

/// Escape XML element-content specials (ampersand first). launchctl rejects a
/// malformed plist loudly but without pointing at the offending byte; escaping
/// the two interpolated paths keeps a stray `&`/`<`/`>` in a home directory from
/// producing an opaque load failure.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the serve daemon's plist. `bin_path` is the resolved absolute binary
/// (launchd gives no `PATH` and does no expansion, so a bare `norn` is
/// unresolvable — the caller passes an absolute, existence-checked path);
/// `log_path` is the stdout/stderr sink.
pub fn render_serve_plist(bin_path: &str, log_path: &str) -> String {
    let bin = xml_escape(bin_path);
    let log = xml_escape(log_path);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_carries_the_serve_argv_keepalive_and_log_sink() {
        let plist = render_serve_plist("/opt/norn/bin/norn", "/home/u/.cache/norn/log/serve.log");
        // Label matches the Mimir precedent, adapted to norn.
        assert!(plist.contains("<string>com.dbtlr.norn.serve</string>"));
        // ProgramArguments = <bin> serve (no port, no extra flags).
        assert!(plist.contains("<string>/opt/norn/bin/norn</string>"));
        assert!(plist.contains("<string>serve</string>"));
        // A KeepAlive + RunAtLoad daemon (always warm; adoption over auto-spawn).
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        // Both stdout and stderr go to the one log sink.
        let log_lines = plist.matches("/home/u/.cache/norn/log/serve.log").count();
        assert_eq!(log_lines, 2, "stdout + stderr both redirect to the log");
        // Well-formed prolog.
        assert!(plist.starts_with("<?xml version=\"1.0\""));
    }

    #[test]
    fn interpolated_paths_are_xml_escaped() {
        let plist = render_serve_plist("/home/a&b/norn", "/home/a&b/log");
        assert!(
            plist.contains("/home/a&amp;b/norn"),
            "ampersand escaped in bin"
        );
        assert!(
            !plist.contains("/home/a&b/norn"),
            "raw ampersand must not survive into the plist"
        );
    }
}
