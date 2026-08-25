//! Contract checks for the installable norn skill package.

use std::fs;
use std::path::Path;
use std::process::Command;

const MANIFEST_ROOT: &str = env!("CARGO_MANIFEST_DIR");
const SKILL_PATH: &str = "skills/norn/SKILL.md";

fn norn_skill() -> String {
    let skill_path = Path::new(MANIFEST_ROOT).join(SKILL_PATH);
    fs::read_to_string(&skill_path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", skill_path.display()))
}

#[test]
fn norn_skill_uses_the_standard_package_path() {
    let skill = norn_skill();

    assert!(
        skill.starts_with("---\nname: norn\n"),
        "{SKILL_PATH} must declare the discovered skill name as norn"
    );
    assert!(
        skill.contains("description:"),
        "{SKILL_PATH} must declare a discovery description"
    );
    assert!(
        !Path::new(MANIFEST_ROOT)
            .join("integrations/agent-skill/SKILL.md")
            .exists(),
        "the legacy agent-skill package path must be removed"
    );
    assert!(
        !skill.contains("a finding has no path"),
        "the skill must not claim that JSONL validation findings omit their path"
    );
    assert!(
        skill.contains("norn -C /path/to/vault describe --format json"),
        "the orient-first example must preserve a known vault path"
    );
}

#[test]
fn linked_agent_workflow_teaches_current_write_contracts() {
    let workflow_path = Path::new(MANIFEST_ROOT).join("docs/agent-workflows.md");
    let workflow = fs::read_to_string(&workflow_path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", workflow_path.display()));

    for retired in [
        "skipped_findings",
        "skip_reason",
        "\"schema_version\": 1",
        "`--format jsonl` on every command",
        "point releases",
    ] {
        assert!(
            !workflow.contains(retired),
            "agent workflow must not teach retired contract `{retired}`"
        );
    }
    for current in [
        "\"schema_version\": 2",
        "\"schema_version\": 3",
        "norn edit",
        "norn rewrite-wikilink",
        "norn apply - --yes",
    ] {
        assert!(
            workflow.contains(current),
            "agent workflow must teach current contract `{current}`"
        );
    }
}

#[test]
fn visible_root_commands_are_classified_by_the_skill() {
    let skill = norn_skill();
    let help = norn_help(&["--help"]);
    let commands = help
        .split_once("COMMANDS\n")
        .expect("root help must have a COMMANDS block")
        .1
        .split_once("\nEXAMPLES\n")
        .expect("root help must end the COMMANDS block before EXAMPLES")
        .0
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();

    assert!(!commands.is_empty(), "root help must list visible commands");
    for command in commands {
        let spelling = format!("norn {command}");
        assert!(
            skill.contains(&spelling),
            "{SKILL_PATH} must teach or explicitly classify `{spelling}`"
        );
    }
}

fn norn_help(args: &[&str]) -> String {
    let xdg = tempfile::tempdir().expect("temporary XDG root must be created");
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .env("XDG_CACHE_HOME", xdg.path().join("cache"))
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .output()
        .expect("norn help must run");

    assert!(
        output.status.success(),
        "norn {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("norn help must be UTF-8")
}

#[test]
fn repair_help_teaches_the_migration_plan_v2_envelope() {
    let help = norn_help(&["repair", "--help"]);

    for field in ["schema_version", "preconditions", "operations", "skipped"] {
        assert!(
            help.contains(field),
            "repair help must teach the MigrationPlan v2 {field} field:\n{help}"
        );
    }
    assert!(
        !help.contains("PlannedChange"),
        "repair help must not teach the retired PlannedChange wire shape:\n{help}"
    );
    assert!(
        help.contains("norn apply - --yes"),
        "repair help must give consent in its applying pipeline example:\n{help}"
    );
}

#[test]
fn apply_help_teaches_current_preflight_and_follow_up() {
    let help = norn_help(&["apply", "--help"]);

    assert!(
        help.contains("owner-set"),
        "apply help must teach owner-set preconditions:\n{help}"
    );
    assert!(
        help.contains("create_document"),
        "apply help must teach create-path resolution:\n{help}"
    );
    assert!(
        help.contains("norn validate"),
        "apply help must teach validation as a separate follow-up:\n{help}"
    );
    assert!(
        !help.contains("--verify"),
        "apply help must not teach the removed --verify flag:\n{help}"
    );
    assert!(
        !help.contains("checking every precondition before touching a file"),
        "apply help must not promise atomic preflight across operation classes:\n{help}"
    );
}

#[test]
fn high_risk_skill_options_exist_in_cli_help() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["find", "--help"],
            &[
                "--eq",
                "--not-eq",
                "--starts-with",
                "--contains",
                "--links-to",
                "--unresolved-links",
                "--all-cols",
            ],
        ),
        (&["get", "--help"], &["--col", "--section", "--format"]),
        (
            &["describe", "--help"],
            &["--data", "--stats", "--by", "--limit"],
        ),
        (&["count", "--help"], &["--by", "--format"]),
        (
            &["set", "--help"],
            &[
                "--field",
                "--field-json",
                "--push",
                "--pop",
                "--remove",
                "--body-from-stdin",
                "--yes",
                "--dry-run",
            ],
        ),
        (&["edit", "--help"], &["--edits-json", "--yes", "--dry-run"]),
        (
            &["new", "--help"],
            &[
                "--as",
                "--title",
                "--var",
                "--field",
                "--body-from-stdin",
                "--yes",
                "--dry-run",
            ],
        ),
        (&["move", "--help"], &["--recursive", "--yes", "--dry-run"]),
        (
            &["delete", "--help"],
            &["--rewrite-to", "--allow-broken-links", "--yes", "--dry-run"],
        ),
        (
            &["validate", "--help"],
            &["--summary", "--code", "--severity", "--path", "--format"],
        ),
        (
            &["repair", "--help"],
            &[
                "--plan",
                "--out",
                "--skip-reason",
                "--confidence",
                "--format",
            ],
        ),
        (
            &["apply", "--help"],
            &[
                "--dry-run",
                "--yes",
                "--format",
                "--input-format",
                "--parents",
            ],
        ),
        (
            &["audit", "--help"],
            &[
                "--trace", "--status", "--target", "--since", "--until", "--raw",
            ],
        ),
    ];

    for (command, options) in cases {
        let help = norn_help(command);
        for option in *options {
            assert!(
                help.contains(option),
                "norn {} help must contain taught option {option}:\n{help}",
                command.join(" ")
            );
        }
    }
}

#[test]
fn standards_pack_example_is_accepted_by_config_validate() {
    let skill = norn_skill();
    let section = skill
        .split_once("### Define a Standards pack")
        .expect("skill must teach the Standards pack")
        .1;
    let yaml = section
        .split_once("```yaml\n")
        .expect("Standards pack section must contain YAML")
        .1
        .split_once("\n```")
        .expect("Standards pack YAML fence must close")
        .0;
    let vault = tempfile::tempdir().expect("temporary vault must be created");
    let config_dir = vault.path().join(".norn");
    fs::create_dir(&config_dir).expect(".norn directory must be created");
    fs::write(config_dir.join("config.yaml"), yaml).expect("config example must be written");

    let xdg = tempfile::tempdir().expect("temporary XDG root must be created");
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "-C",
            vault.path().to_str().expect("vault path must be UTF-8"),
            "config",
            "validate",
            "--format",
            "json",
        ])
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", xdg.path().join("cache"))
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .output()
        .expect("config validate must run");

    assert!(
        output.status.success(),
        "Standards pack example must validate:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let vault_path = vault.path().to_str().expect("vault path must be UTF-8");
    let created = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "-C",
            vault_path,
            "new",
            "--as",
            "task",
            "--title",
            "Fix the cache",
            "--parents",
            "--yes",
            "--format",
            "json",
        ])
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", xdg.path().join("cache"))
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .output()
        .expect("norn new must run against the Standards pack example");
    assert!(
        created.status.success(),
        "Standards pack task creation must succeed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&created.stdout).expect("norn new output must be JSON");
    let warnings = report["warnings"]
        .as_array()
        .expect("norn new report must contain warnings");
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["kind"] == "missing-required-field"),
        "a task created from the Standards pack example must satisfy required fields:\n{}",
        String::from_utf8_lossy(&created.stdout)
    );
}
