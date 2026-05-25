//! Text + JSON renderers for `vault self-update`.

use std::io::Write;

use crate::output::palette::Palette;
use crate::output::primitives;
use crate::self_update::resolve::Action;
use crate::self_update::SelfUpdateReport;

pub fn render_json<W: Write>(out: &mut W, report: &SelfUpdateReport) -> std::io::Result<()> {
    let body =
        serde_json::to_string_pretty(report).expect("SelfUpdateReport is always serializable");
    writeln!(out, "{body}")
}

pub fn render_text<W: Write>(
    out: &mut W,
    palette: &Palette,
    report: &SelfUpdateReport,
) -> std::io::Result<()> {
    primitives::status_headline(&mut *out, palette, "Checking releases")?;

    writeln!(out, "  current:       {}", report.current_version)?;
    writeln!(out, "  latest:        {}", report.latest_version)?;
    writeln!(out, "  target:        {}", report.target_version)?;
    writeln!(out, "  triple:        {}", report.target_triple)?;
    writeln!(out, "  install path:  {}", report.install_path)?;
    if let Some(url) = &report.asset_url {
        writeln!(out, "  asset url:     {url}")?;
    }
    if let Some(sha) = &report.asset_sha256 {
        writeln!(out, "  asset sha256:  {sha}")?;
    }

    match report.action {
        Action::WouldUpdate => {
            writeln!(out, "Dry run — would update to {}", report.target_version)?
        }
        Action::WouldNoOp => writeln!(
            out,
            "Dry run — already on {}, no update available",
            report.current_version
        )?,
        Action::Updated => writeln!(out, "Updated vault to {}", report.target_version)?,
        Action::NoOp => writeln!(
            out,
            "Already on {} — no update available",
            report.current_version
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_update::resolve::Action;
    use crate::self_update::SELF_UPDATE_SCHEMA_VERSION;

    fn sample_report() -> SelfUpdateReport {
        SelfUpdateReport {
            schema_version: SELF_UPDATE_SCHEMA_VERSION,
            update_available: true,
            current_version: "0.32.0".to_string(),
            latest_version: "0.33.1".to_string(),
            target_version: "0.33.1".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            install_path: "/Users/drew/.cargo/bin/vault".to_string(),
            asset_url: Some("https://example/v0.33.1/vault-arm64.tar.xz".to_string()),
            asset_sha256: Some("abc123".to_string()),
            dry_run: true,
            action: Action::WouldUpdate,
        }
    }

    #[test]
    fn json_envelope_round_trips() {
        let report = sample_report();
        let mut buf = Vec::new();
        render_json(&mut buf, &report).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["update_available"], true);
        assert_eq!(v["current_version"], "0.32.0");
        assert_eq!(v["target_version"], "0.33.1");
        assert_eq!(v["action"], "would_update");
        assert_eq!(v["asset_sha256"], "abc123");
    }

    #[test]
    fn json_omits_asset_fields_on_no_op() {
        let mut report = sample_report();
        report.update_available = false;
        report.target_version = "0.32.0".to_string();
        report.action = Action::WouldNoOp;
        report.asset_url = None;
        report.asset_sha256 = None;

        let mut buf = Vec::new();
        render_json(&mut buf, &report).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(v.get("asset_url").is_none(), "asset_url should be omitted");
        assert!(
            v.get("asset_sha256").is_none(),
            "asset_sha256 should be omitted"
        );
    }

    #[test]
    fn text_dry_run_includes_versions_and_dry_run_marker() {
        let report = sample_report();
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_text(&mut buf, &palette, &report).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("0.32.0"), "current version missing: {text}");
        assert!(text.contains("0.33.1"), "target version missing: {text}");
        assert!(text.contains("Dry run"), "dry-run marker missing: {text}");
    }

    #[test]
    fn text_no_op_says_no_update_available() {
        let mut report = sample_report();
        report.update_available = false;
        report.target_version = "0.32.0".to_string();
        report.latest_version = "0.32.0".to_string();
        report.action = Action::NoOp;
        report.dry_run = false;
        report.asset_url = None;
        report.asset_sha256 = None;

        let palette = Palette::off();
        let mut buf = Vec::new();
        render_text(&mut buf, &palette, &report).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("Already on 0.32.0"),
            "expected 'Already on 0.32.0' in output: {text}"
        );
    }

    #[test]
    fn text_updated_says_updated_to_target() {
        let mut report = sample_report();
        report.action = Action::Updated;
        report.dry_run = false;
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_text(&mut buf, &palette, &report).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("Updated vault to 0.33.1"),
            "expected 'Updated vault to 0.33.1' in output: {text}"
        );
    }
}
