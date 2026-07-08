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
/// short (non-64-hex) name the cache pruner never treats as a vault entry
/// (guarded by `cache::prune`'s `log_dir_under_the_cache_tree_survives_a_sweep`
/// test). launchd does no `~`/`$VAR` expansion and will not create the parent,
/// so `service install` must `mkdir -p` this file's directory before
/// bootstrapping.
pub fn log_path() -> anyhow::Result<Utf8PathBuf> {
    Ok(crate::cache::cache_tree_root()?
        .join("log")
        .join("serve.log"))
}

/// The install-time `XDG_CACHE_HOME`, when set and non-empty — the ONE
/// environment variable the daemon's socket/log derivation depends on. Empty
/// counts as unset, matching the cache tree's own derivation.
pub fn install_env_xdg_cache_home() -> Option<String> {
    std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Escape XML element-content specials (ampersand first). launchctl rejects a
/// malformed plist loudly but without pointing at the offending byte; escaping
/// the interpolated paths keeps a stray `&`/`<`/`>` in a home directory from
/// producing an opaque load failure.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the serve daemon's plist. `bin_path` is the resolved absolute binary
/// (launchd gives no `PATH` and does no expansion, so a bare `norn` is
/// unresolvable — the caller passes an absolute path); `log_path` is the
/// stdout/stderr sink.
///
/// `xdg_cache_home` bakes the install-time `XDG_CACHE_HOME` into the unit's
/// environment: a launchd agent inherits NO shell environment, so without this
/// a user who sets `XDG_CACHE_HOME` would get a daemon bound to the default
/// `~/.cache` socket while every client probes the XDG-derived one — a
/// permanently unroutable install. `None` (the common case) bakes nothing, and
/// daemon and clients both fall back to `~/.cache`.
pub fn render_serve_plist(bin_path: &str, log_path: &str, xdg_cache_home: Option<&str>) -> String {
    let bin = xml_escape(bin_path);
    let log = xml_escape(log_path);
    let env = match xdg_cache_home {
        Some(value) => format!(
            "\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>XDG_CACHE_HOME</key>\n    <string>{}</string>\n  </dict>",
            xml_escape(value)
        ),
        None => String::new(),
    };
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
  <string>{log}</string>{env}
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
        let plist = render_serve_plist(
            "/opt/norn/bin/norn",
            "/home/u/.cache/norn/log/serve.log",
            None,
        );
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

    /// Without an install-time XDG_CACHE_HOME nothing is baked: the daemon and
    /// its clients both fall back to `~/.cache` and agree on the socket.
    #[test]
    fn no_xdg_bakes_no_environment_dict() {
        let plist = render_serve_plist("/bin/norn", "/log", None);
        assert!(
            !plist.contains("EnvironmentVariables"),
            "no env dict when XDG_CACHE_HOME is unset:\n{plist}"
        );
    }

    /// With XDG_CACHE_HOME set at install time it MUST be baked into the unit:
    /// launchd agents inherit no shell env, so an unbaked value leaves the
    /// daemon on `~/.cache/.../norn.sock` while clients probe the XDG-derived
    /// socket — never routable.
    #[test]
    fn install_time_xdg_is_baked_into_the_environment_dict() {
        let plist = render_serve_plist("/bin/norn", "/log", Some("/custom/xdg-cache"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"), "{plist}");
        assert!(plist.contains("<key>XDG_CACHE_HOME</key>"), "{plist}");
        assert!(
            plist.contains("<string>/custom/xdg-cache</string>"),
            "{plist}"
        );
        // The dict sits inside the top-level dict (before </dict></plist>).
        let env_pos = plist.find("EnvironmentVariables").unwrap();
        let close_pos = plist.find("</dict>\n</plist>").unwrap();
        assert!(env_pos < close_pos, "env dict is inside the plist dict");
    }

    #[test]
    fn interpolated_paths_are_xml_escaped() {
        let plist = render_serve_plist("/home/a&b/norn", "/home/a&b/log", Some("/x&y"));
        assert!(
            plist.contains("/home/a&amp;b/norn"),
            "ampersand escaped in bin"
        );
        assert!(
            !plist.contains("/home/a&b/norn"),
            "raw ampersand must not survive into the plist"
        );
        assert!(plist.contains("<string>/x&amp;y</string>"), "env escaped");
    }
}
