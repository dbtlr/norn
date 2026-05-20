//! `vault init` — scaffold `.vault/config.yaml` for a fresh vault.
//!
//! This is the bootstrap command. It runs in folders that may or may not
//! contain Markdown, and crucially runs WITHOUT a config (it's creating
//! one). The cache infrastructure works on the vault root directly, so we
//! open it, run a from-scratch rebuild, and tally the top-level
//! frontmatter keys observed across documents. The scaffold YAML embeds
//! the tally as commented hints so the operator can see what fields the
//! vault actually uses before authoring validation rules.

use anyhow::{anyhow, Result};
use camino::Utf8Path;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;

use vault_cache::{Cache, DocumentQuery};

use crate::cli::InitArgs;
use crate::init_scan::{tally_from_keys, ScanResult};
use crate::output::palette::Palette;
use crate::output::primitives::{self, NoteLabel};

const SCAFFOLD_TOP: &str = r#"version: 1

# Files inventoried by vault-cli. Patterns here are excluded from
# the graph AND from all validation.
files:
  # Pre-filled with universally-ignorable patterns.
  # Remove any you actually want vault-cli to track.
  ignore:
    - .obsidian/
    - .git/
    - .trash/
    - node_modules/

# Validation behavior. `validate.ignore` skips paths during validation
# but keeps them in the graph for queries.
validate:
  ignore: []
  required_frontmatter: []   # fields required on every doc
  rules: []                  # scoped validation rules

# Repair planning rules — turn findings into mutation plans.
repair:
  rules: []
"#;

const TOP_N: usize = 30;

/// Run `vault init`. Returns the process exit code.
pub fn run(cwd: &Utf8Path, args: &InitArgs) -> Result<i32> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    run_to(cwd, args, &mut lock)
}

#[cfg(test)]
pub(crate) fn run_capturing_output(cwd: &Utf8Path, args: &InitArgs) -> Result<String> {
    let mut buf = Vec::new();
    run_to(cwd, args, &mut buf)?;
    Ok(String::from_utf8(buf)?)
}

fn run_to(cwd: &Utf8Path, args: &InitArgs, out: &mut dyn Write) -> Result<i32> {
    let vault_dir = cwd.join(".vault");
    let config_path = vault_dir.join("config.yaml");

    if config_path.exists() && !args.force {
        return Err(anyhow!(
            ".vault/config.yaml already exists at {}\nhint: pass --force to overwrite",
            config_path
        ));
    }

    let scan = scan_vault(cwd)?;
    fs::create_dir_all(&vault_dir)?;
    let body = render_scaffold(&scan);
    fs::write(&config_path, body)?;

    writeln!(out, "created {config_path}")?;
    if scan.total_docs == 0 {
        writeln!(out, "no markdown files found during scan")?;
    } else {
        let field_word = if scan.fields.len() == 1 {
            "field"
        } else {
            "fields"
        };
        let doc_word = if scan.total_docs == 1 {
            "document"
        } else {
            "documents"
        };
        let top_n = TOP_N.min(scan.fields.len());
        writeln!(
            out,
            "observed {} {field_word} across {} {doc_word} — top {} written as commented hints",
            scan.fields.len(),
            scan.total_docs,
            top_n
        )?;
    }
    // init output is one-shot status — no TTY-detecting palette needed.
    let palette = Palette::off();
    primitives::note_line(
        out,
        &palette,
        NoteLabel::Tip,
        &format!("edit `{config_path}`, then run `vault validate`"),
    )?;
    Ok(0)
}

/// Open the per-vault cache (creating it if needed), rebuild it from
/// scratch (no config exists yet so we can't go through the normal
/// incremental refresh path), then enumerate every document and tally the
/// top-level frontmatter keys. A fresh rebuild is the cleanest approach:
/// it's deterministic and the resulting cache is immediately reusable by
/// the operator's next `vault find` / `validate` invocation.
fn scan_vault(cwd: &Utf8Path) -> Result<ScanResult> {
    let mut cache = Cache::open(cwd)?;
    cache.rebuild(cwd)?;
    let docs = cache.documents_matching(&DocumentQuery::default())?;
    let total = docs.len();
    let per_doc_keys: Vec<Vec<String>> = docs
        .into_iter()
        .map(|doc| {
            doc.frontmatter
                .as_ref()
                .and_then(|v| v.as_object())
                .map(|obj| obj.keys().map(String::from).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    Ok(tally_from_keys(per_doc_keys, total, TOP_N))
}

fn render_scaffold(scan: &ScanResult) -> String {
    let mut s = String::from(SCAFFOLD_TOP);
    s.push('\n');
    s.push_str("# ----- Observed in this vault -----\n");
    if scan.total_docs == 0 {
        s.push_str("# No markdown files found during scan.\n");
        return s;
    }
    s.push_str(&format!(
        "# Frontmatter fields seen during init scan (top {} of {} sorted by usage):\n",
        scan.fields.len().min(TOP_N),
        scan.fields.len()
    ));
    for f in &scan.fields {
        let pct = if scan.total_docs > 0 {
            (f.count as f64 * 100.0 / scan.total_docs as f64).round() as u32
        } else {
            0
        };
        let _ = writeln!(
            &mut s,
            "#   {:<20} used in {}/{} docs ({}%)",
            f.name, f.count, scan.total_docs, pct
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_scan::FieldStat;

    #[test]
    fn run_output_uses_tip_line_and_proper_plurals() {
        use camino::Utf8PathBuf;
        use std::io::Write;
        use tempfile::Builder;

        // Use a non-hidden prefix: WalkDir filters dirs starting with '.'.
        let tmp = Builder::new().prefix("vault-init-test").tempdir().unwrap();
        let cwd = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // One markdown file with frontmatter so the scan finds something.
        let doc_path = cwd.join("note.md");
        let mut f = std::fs::File::create(&doc_path).unwrap();
        writeln!(f, "---\ntype: note\n---\n\nbody").unwrap();

        let outcome = run_capturing_output(&cwd, &InitArgs { force: false }).unwrap();
        assert!(outcome.contains("created "), "actual: {outcome:?}");
        assert!(
            outcome.contains("observed 1 field across 1 document"),
            "actual: {outcome:?}"
        );
        assert!(
            outcome.contains(" — top 1 written as commented hints"),
            "actual: {outcome:?}"
        );
        assert!(outcome.contains("tip: edit "), "actual: {outcome:?}");
        assert!(outcome.contains("vault validate"), "actual: {outcome:?}");
    }

    #[test]
    fn run_output_with_no_markdown_skips_observed_line() {
        use camino::Utf8PathBuf;
        use tempfile::Builder;

        // Use a non-hidden prefix: WalkDir filters dirs starting with '.'.
        let tmp = Builder::new().prefix("vault-init-test").tempdir().unwrap();
        let cwd = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        let outcome = run_capturing_output(&cwd, &InitArgs { force: false }).unwrap();
        assert!(outcome.contains("created "));
        assert!(outcome.contains("no markdown files found during scan"));
        assert!(!outcome.contains("observed "));
        assert!(outcome.contains("tip: edit "));
    }

    #[test]
    fn render_scaffold_includes_common_ignores_and_sections() {
        let scan = ScanResult {
            total_docs: 0,
            fields: vec![],
        };
        let body = render_scaffold(&scan);
        assert!(body.contains("version: 1"));
        assert!(body.contains(".obsidian/"));
        assert!(body.contains(".git/"));
        assert!(body.contains(".trash/"));
        assert!(body.contains("node_modules/"));
        assert!(body.contains("validate:"));
        assert!(body.contains("repair:"));
    }

    #[test]
    fn render_scaffold_empty_scan_notes_no_markdown() {
        let scan = ScanResult {
            total_docs: 0,
            fields: vec![],
        };
        let body = render_scaffold(&scan);
        assert!(body.contains("No markdown files found"));
    }

    #[test]
    fn render_scaffold_with_fields_emits_count_lines() {
        let scan = ScanResult {
            total_docs: 2,
            fields: vec![FieldStat {
                name: "type".to_string(),
                count: 2,
                total_docs: 2,
            }],
        };
        let body = render_scaffold(&scan);
        assert!(body.contains("Observed in this vault"));
        assert!(body.contains("type"));
        assert!(body.contains("2/2"));
        assert!(body.contains("(100%)"));
    }
}
