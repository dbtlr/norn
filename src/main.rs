pub mod apply_report;
mod cache;
mod cache_cmd;
mod cli;
mod completions;
mod config;
mod config_loader;
mod core;
mod count;
pub mod delete_doc;
mod filter;
mod filter_args;
mod find;
mod frontmatter;
mod graph;
mod help;
mod init;
mod init_scan;
mod links;
pub mod migration_plan;
pub mod move_doc;
pub mod mutation_report;
mod new;
mod output;
pub mod prompt;
mod query;
mod repair;
mod repair_apply;
mod self_update;
mod set;
mod show;
mod standards;
mod target;
mod validate;
mod validate_filter;

use std::{fs, process};

use crate::cli::{
    CacheSubcommand, Cli, Command, ConfigSubcommand, RepairApplyFormat, RepairPlanFormat,
    RepairSubcommand,
};
use crate::config_loader::{effective_cwd, load_config, resolve_path};
use crate::core::GraphIndex;
use crate::graph::{concise_diagnostics, has_errors};
use crate::output::primitives::is_broken_pipe;
use crate::repair::skip_reasons::code_matches_any;
use crate::repair_apply::{apply_repair_plan, with_verification};
use crate::standards::{plan_repairs, validate_with_compiled, RepairPlanFilters, SkippedSummary};
use crate::validate_filter::{filter_findings, ValidateFilterOptions};
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

fn main() {
    // Intercept -h / --help before Cli::parse() so that subcommands with
    // required positionals (e.g. `norn completions init --help`) can render
    // help without clap erroring out on the missing positional arg.
    if let Some(exit_code) = help::intercept_from_args() {
        process::exit(exit_code);
    }
    let mut cmd = Cli::command();
    if !self_update::receipt::exists() {
        cmd = cmd.mut_subcommand("self-update", |sc| sc.hide(true));
    }
    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).expect("clap-derive contract: parse from matches");
    match run(cli) {
        Ok(exit_code) => process::exit(exit_code),
        Err(error) if is_broken_pipe(&error) => process::exit(0),
        Err(error) => {
            eprintln!("{error:#}");
            process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let Cli {
        cwd,
        config,
        verbose,
        no_cache_refresh,
        color,
        help_short: _,
        help_long: _,
        command,
    } = cli;

    let command = match command {
        Command::Completions(args) => return run_completions_command(args),
        Command::Manpage => return run_manpage_command(),
        Command::SelfUpdate(args) => return run_self_update_command(args, color),
        command => command,
    };

    let cwd = effective_cwd(cwd.as_ref())?;
    let config_path = config;

    match command {
        Command::Repair(repair_command) => match repair_command.command {
            RepairSubcommand::Plan(args) => {
                let loaded_config = load_config(&cwd, config_path.as_ref())?;
                let mut index = crate::cache_cmd::load_graph_index(
                    &cwd,
                    &loaded_config.index_options,
                    no_cache_refresh,
                )?;
                trim_diagnostics(&mut index, verbose);
                let findings = validate_with_compiled(
                    &index,
                    &loaded_config.validate,
                    &loaded_config.compiled,
                    loaded_config.index_options.alias_field.as_deref(),
                );
                let filters = ValidateFilterOptions::from(&args);
                let findings = filter_findings(findings, &filters)?;
                let mut plan = plan_repairs(
                    cwd.clone(),
                    repair_plan_filters(&args),
                    findings,
                    &loaded_config.repair,
                    &index,
                );
                if !args.skip_reason.is_empty() {
                    plan.skipped_findings
                        .retain(|f| code_matches_any(f.skip_reason.code(), &args.skip_reason));
                    plan.summary.skipped = SkippedSummary::from_skipped(&plan.skipped_findings);
                }
                // --out: always writes JSON to the file (independent of --format).
                if let Some(out) = &args.out {
                    let out_path = resolve_path(&cwd, out);
                    let plan_text = serde_json::to_string_pretty(&plan)?;
                    fs::write(&out_path, format!("{plan_text}\n")).map_err(|error| {
                        anyhow::anyhow!("failed to write repair plan {out_path}: {error}")
                    })?;
                }

                // --format: governs stdout. When --out is set without --format, stdout stays silent.
                let stdout_format = if args.format.is_none() && args.out.is_some() {
                    None // silent when --out alone
                } else {
                    Some(args.format.unwrap_or_else(|| {
                        use std::io::IsTerminal;
                        if std::io::stdout().is_terminal() {
                            RepairPlanFormat::Report
                        } else {
                            RepairPlanFormat::Json
                        }
                    }))
                };

                if let Some(format) = stdout_format {
                    use std::io::Write;
                    match format {
                        RepairPlanFormat::Report => repair::render::write_report(&plan, &args)?,
                        RepairPlanFormat::Json => {
                            // Pretty-printed JSON with trailing newline — matches write_item_output behavior
                            let json = serde_json::to_string_pretty(&plan)?;
                            let stdout = std::io::stdout();
                            let mut stdout = stdout.lock();
                            stdout.write_all(json.as_bytes())?;
                            stdout.write_all(b"\n")?;
                        }
                        RepairPlanFormat::Paths => repair::render::write_paths(&plan)?,
                    }
                }
                Ok(exit_code_for(&index))
            }
            RepairSubcommand::Apply(args) => {
                // Determine plan source: positional path, '-' (stdin), or absent (stdin).
                let (plan_text, plan_source) = match args.plan.as_deref().map(|p| p.as_str()) {
                    None | Some("-") => {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf).map_err(|error| {
                            anyhow::anyhow!("could not read plan from stdin: {error}")
                        })?;
                        (buf, crate::repair::apply_render::PlanSource::Stdin)
                    }
                    Some(_) => {
                        let plan_path_arg = args.plan.as_ref().unwrap();
                        let plan_path = resolve_path(&cwd, plan_path_arg);
                        let body = fs::read_to_string(&plan_path).map_err(|error| {
                            anyhow::anyhow!("failed to read repair plan {plan_path}: {error}")
                        })?;
                        (
                            body,
                            crate::repair::apply_render::PlanSource::File(plan_path),
                        )
                    }
                };
                let plan = serde_json::from_str::<crate::standards::RepairPlan>(&plan_text)
                    .map_err(|error| match &plan_source {
                        crate::repair::apply_render::PlanSource::Stdin => {
                            anyhow::anyhow!("could not parse plan from stdin: {error}")
                        }
                        crate::repair::apply_render::PlanSource::File(p) => {
                            anyhow::anyhow!("failed to parse repair plan {p}: {error}")
                        }
                    })?;
                let loaded_config = load_config(&cwd, config_path.as_ref())?;
                let mut index = crate::cache_cmd::load_graph_index(
                    &cwd,
                    &loaded_config.index_options,
                    no_cache_refresh,
                )?;
                trim_diagnostics(&mut index, verbose);
                let mut report = apply_repair_plan(&cwd, &index, &plan, args.dry_run)?;
                if args.verify {
                    let mut verify_index = crate::cache_cmd::load_graph_index(
                        &cwd,
                        &loaded_config.index_options,
                        false,
                    )?;
                    trim_diagnostics(&mut verify_index, verbose);
                    let findings = validate_with_compiled(
                        &verify_index,
                        &loaded_config.validate,
                        &loaded_config.compiled,
                        loaded_config.index_options.alias_field.as_deref(),
                    );
                    report = with_verification(report, &findings);
                }
                // --out: always writes JSON to file (independent of --format).
                if let Some(out) = &args.out {
                    let out_path = resolve_path(&cwd, out);
                    let report_json = serde_json::to_string_pretty(&report)?;
                    fs::write(&out_path, format!("{report_json}\n")).map_err(|error| {
                        anyhow::anyhow!("failed to write apply report {out_path}: {error}")
                    })?;
                }

                // --format: governs stdout. Silent when --out is set without --format.
                let stdout_format = if args.format.is_none() && args.out.is_some() {
                    None
                } else {
                    Some(args.format.unwrap_or_else(|| {
                        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                            RepairApplyFormat::Report
                        } else {
                            RepairApplyFormat::Json
                        }
                    }))
                };

                if let Some(format) = stdout_format {
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut stdout = stdout.lock();
                    match format {
                        RepairApplyFormat::Report => {
                            crate::repair::apply_render::render_report(
                                &report,
                                &plan,
                                plan_source,
                                &mut stdout,
                            )?;
                        }
                        RepairApplyFormat::Json => {
                            let json = serde_json::to_string_pretty(&report)?;
                            stdout.write_all(json.as_bytes())?;
                            stdout.write_all(b"\n")?;
                        }
                        RepairApplyFormat::Paths => {
                            crate::repair::apply_render::write_paths(&report, &mut stdout)?;
                        }
                    }
                }
                Ok(exit_code_for(&index))
            }
        },
        Command::Cache(cache_command) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let alias_field = loaded_config.index_options.alias_field.as_deref();
            match &cache_command.command {
                CacheSubcommand::Index(args) => {
                    crate::cache_cmd::run_index(&cwd, alias_field, args)?
                }
                CacheSubcommand::Rebuild => crate::cache_cmd::run_rebuild(&cwd, alias_field)?,
                CacheSubcommand::Clear => crate::cache_cmd::run_clear(&cwd)?,
                CacheSubcommand::Status(args) => {
                    crate::cache_cmd::run_status(&cwd, alias_field, args)?
                }
            }
            Ok(0)
        }
        Command::Config(cfg) => match cfg.command {
            ConfigSubcommand::Show(args) => {
                crate::config::run_show(&cwd, config_path.as_ref(), &args, color)
            }
            ConfigSubcommand::Validate(args) => {
                crate::config::run_validate(&cwd, config_path.as_ref(), &args, color)
            }
            ConfigSubcommand::Migrate => crate::config::run_migrate(&cwd, config_path.as_ref()),
            ConfigSubcommand::Edit(args) => {
                crate::config::run_edit(&cwd, config_path.as_ref(), &args, color)
            }
        },
        Command::Validate(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);
            let findings = validate_with_compiled(
                &index,
                &loaded_config.validate,
                &loaded_config.compiled,
                loaded_config.index_options.alias_field.as_deref(),
            );
            let filters = ValidateFilterOptions::from(&args);
            let findings = filter_findings(findings, &filters)?;

            let format = args.format.unwrap_or_else(|| {
                if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                    cli::ValidateFormat::Records
                } else {
                    cli::ValidateFormat::Jsonl
                }
            });
            let palette = crate::output::palette::resolve(color);
            let rules_count = loaded_config.validate.rules.len()
                + loaded_config.validate.required_frontmatter.len();
            let total_docs = index.documents.len();

            let mut stdout = std::io::stdout().lock();
            validate::render::render(
                &findings,
                args.summary,
                rules_count,
                total_docs,
                format,
                &palette,
                &mut stdout,
            )?;

            Ok(exit_code_for(&index))
        }
        Command::Get(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                loaded_config.index_options.alias_field.as_deref(),
                no_cache_refresh,
            )?;
            let report = show::run(&cache, &args)?;

            let stdout_text = match args.format {
                cli::GetFormat::Json => show::render::render_json_with_col(&report, &args.col),
                cli::GetFormat::Text => show::render::render_text_with_col(&report, &args.col),
            };
            print!("{}", stdout_text);
            if !stdout_text.ends_with('\n') {
                println!();
            }

            let stderr = std::io::stderr();
            let mut stderr_lock = stderr.lock();
            show::render::warn_unknown_cols(&args.col, &mut stderr_lock)?;

            let mut any_error = false;
            for note in &report.notes {
                eprintln!("{}", note);
                if note.starts_with("error:") {
                    any_error = true;
                }
            }
            if any_error {
                std::process::exit(1);
            }
            Ok(0)
        }
        Command::Find(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            find::run(
                args,
                &cwd,
                loaded_config.index_options.alias_field.as_deref(),
                no_cache_refresh,
                color,
            )
        }
        Command::Count(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                loaded_config.index_options.alias_field.as_deref(),
                no_cache_refresh,
            )?;
            let out = count::run(&cache, &args)?;
            let text = match args.format {
                cli::CountFormat::Json => count::render::render_json(&out),
                cli::CountFormat::Text => count::render::render_text(&out),
            };
            print!("{}", text);
            if !text.ends_with('\n') {
                println!();
            }
            Ok(0)
        }
        Command::Move(args) => {
            use std::io::{IsTerminal, Write};

            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);

            let cfg = crate::move_doc::PreflightConfig {
                src: &args.src,
                dst: &args.dst,
                force: args.force,
                no_link_rewrite: args.no_link_rewrite,
                vault_root: &cwd,
                index: &index,
            };
            let plan = match crate::move_doc::preflight_and_plan(cfg) {
                Ok(plan) => plan,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            let warnings = crate::move_doc::collect_warnings(&plan, &index, &cwd);

            let stdout = std::io::stdout();
            let mut out = stdout.lock();

            // Determine whether to apply, and handle the TTY-interactive branch specially
            // (it needs to render the preview before prompting).
            // In JSON mode we must render exactly once — skip the preview when we're
            // going to apply so callers never see two concatenated JSON objects.
            let should_apply = if args.dry_run {
                false
            } else if args.yes {
                true
            } else if matches!(args.format, crate::cli::MoveFormat::Json) {
                // --format json is implicitly non-interactive; render preview and exit.
                let preview = crate::move_doc::build_report(&plan, false, warnings.clone());
                crate::move_doc::render_json(&mut out, &preview)?;
                return Ok(0);
            } else if std::io::stdin().is_terminal() {
                // TTY interactive: render preview first so the operator can see what
                // they're confirming, then prompt.
                let preview = crate::move_doc::build_report(&plan, false, warnings.clone());
                crate::move_doc::render_records(&mut out, &preview)?;
                let stdin = std::io::stdin();
                let mut reader = stdin.lock();
                let mut prompt_out = std::io::stderr();
                writeln!(prompt_out)?;
                let ok = crate::prompt::confirm(&mut reader, &mut prompt_out, "Proceed? [y/N] ")?;
                if !ok {
                    std::process::exit(1);
                }
                true
            } else {
                // Non-TTY without --yes = implicit dry-run: render preview and exit.
                let preview = crate::move_doc::build_report(&plan, false, warnings.clone());
                crate::move_doc::render_records(&mut out, &preview)?;
                return Ok(0);
            };

            if should_apply {
                crate::repair_apply::apply_repair_plan(
                    &cwd, &index, &plan, /*dry_run=*/ false,
                )?;
                let applied = crate::move_doc::build_report(&plan, true, warnings);
                match args.format {
                    crate::cli::MoveFormat::Records => {
                        crate::move_doc::render_records(&mut out, &applied)?;
                    }
                    crate::cli::MoveFormat::Json => {
                        crate::move_doc::render_json(&mut out, &applied)?;
                    }
                }
            } else {
                // --dry-run: render preview respecting --format.
                let preview = crate::move_doc::build_report(&plan, false, warnings);
                match args.format {
                    crate::cli::MoveFormat::Records => {
                        crate::move_doc::render_records(&mut out, &preview)?;
                    }
                    crate::cli::MoveFormat::Json => {
                        crate::move_doc::render_json(&mut out, &preview)?;
                    }
                }
            }

            Ok(0)
        }
        Command::Delete(args) => {
            use std::io::{IsTerminal, Write};

            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);

            let cfg = crate::delete_doc::PreflightConfig {
                doc: &args.doc,
                allow_broken_links: args.allow_broken_links,
                rewrite_to: args.rewrite_to.as_deref(),
                vault_root: &cwd,
                index: &index,
            };
            let outcome = match crate::delete_doc::preflight_and_plan(cfg) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };
            let plan = &outcome.plan;
            let rewrite_to = outcome.resolved_rewrite_to.as_ref();

            let stdout = std::io::stdout();
            let mut out = stdout.lock();

            // Determine whether to apply, and handle the TTY-interactive branch specially
            // (it needs to render the preview before prompting).
            // In JSON mode we must render exactly once — skip the preview when we're
            // going to apply so callers never see two concatenated JSON objects.
            let should_apply = if args.dry_run {
                false
            } else if args.yes {
                true
            } else if matches!(args.format, crate::cli::DeleteFormat::Json) {
                // --format json is implicitly non-interactive; render preview and exit.
                let preview = crate::delete_doc::build_report(plan, &index, rewrite_to, false);
                crate::delete_doc::render_json(&mut out, &preview)?;
                return Ok(0);
            } else if std::io::stdin().is_terminal() {
                // TTY interactive: render preview first so the operator can see what
                // they're confirming, then prompt.
                let preview = crate::delete_doc::build_report(plan, &index, rewrite_to, false);
                crate::delete_doc::render_records(&mut out, &preview)?;
                let stdin = std::io::stdin();
                let mut reader = stdin.lock();
                let mut prompt_out = std::io::stderr();
                writeln!(prompt_out)?;
                let ok = crate::prompt::confirm(&mut reader, &mut prompt_out, "Proceed? [y/N] ")?;
                if !ok {
                    std::process::exit(1);
                }
                true
            } else {
                // Non-TTY without --yes = implicit dry-run: render preview and exit.
                let preview = crate::delete_doc::build_report(plan, &index, rewrite_to, false);
                crate::delete_doc::render_records(&mut out, &preview)?;
                return Ok(0);
            };

            if should_apply {
                crate::repair_apply::apply_repair_plan(&cwd, &index, plan, false)?;
                let applied = crate::delete_doc::build_report(plan, &index, rewrite_to, true);
                match args.format {
                    crate::cli::DeleteFormat::Records => {
                        crate::delete_doc::render_records(&mut out, &applied)?;
                    }
                    crate::cli::DeleteFormat::Json => {
                        crate::delete_doc::render_json(&mut out, &applied)?;
                    }
                }
            } else {
                // --dry-run: render preview respecting --format.
                let preview = crate::delete_doc::build_report(plan, &index, rewrite_to, false);
                match args.format {
                    crate::cli::DeleteFormat::Records => {
                        crate::delete_doc::render_records(&mut out, &preview)?;
                    }
                    crate::cli::DeleteFormat::Json => {
                        crate::delete_doc::render_json(&mut out, &preview)?;
                    }
                }
            }

            Ok(0)
        }
        Command::Set(args) => {
            use std::io::{IsTerminal, Write};

            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);

            // Open a Cache for resolve_target (needs document query, not just index).
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                loaded_config.index_options.alias_field.as_deref(),
                no_cache_refresh,
            )?;

            let vault_cfg = loaded_config.vault_config;

            let outcome = match crate::set::synth::preflight_and_plan(
                &cwd, &cache, &index, &vault_cfg, &args,
            ) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            let stdout = std::io::stdout();
            let mut out = stdout.lock();

            // Determine whether to apply, and handle the TTY-interactive branch specially
            // (it needs to render the preview before prompting).
            // In JSON mode we must render exactly once — skip the preview when we're
            // going to apply so callers never see two concatenated JSON objects.
            let should_apply = if args.dry_run {
                false
            } else if args.yes {
                true
            } else if matches!(args.format, crate::cli::SetFormat::Json) {
                // --format json is implicitly non-interactive; render preview and exit.
                let preview = crate::set::report::build_report(&outcome, false);
                crate::set::report::render_json(&mut out, &preview)?;
                return Ok(0);
            } else if std::io::stdin().is_terminal() {
                // TTY interactive: render preview first so the operator can see what
                // they're confirming, then prompt.
                let preview = crate::set::report::build_report(&outcome, false);
                crate::set::report::render_records(&mut out, &preview)?;
                let stdin = std::io::stdin();
                let mut reader = stdin.lock();
                let mut prompt_out = std::io::stderr();
                writeln!(prompt_out)?;
                let ok = crate::prompt::confirm(&mut reader, &mut prompt_out, "Proceed? [y/N] ")?;
                if !ok {
                    std::process::exit(1);
                }
                true
            } else {
                // Non-TTY without --yes = implicit dry-run: render preview and exit.
                let preview = crate::set::report::build_report(&outcome, false);
                crate::set::report::render_records(&mut out, &preview)?;
                return Ok(0);
            };

            if should_apply {
                crate::repair_apply::apply_repair_plan(
                    &cwd,
                    &index,
                    &outcome.plan,
                    /*dry_run=*/ false,
                )?;
                let applied = crate::set::report::build_report(&outcome, true);
                match args.format {
                    crate::cli::SetFormat::Records => {
                        crate::set::report::render_records(&mut out, &applied)?;
                    }
                    crate::cli::SetFormat::Json => {
                        crate::set::report::render_json(&mut out, &applied)?;
                    }
                }
            } else {
                // --dry-run: render preview, respecting --format.
                let preview = crate::set::report::build_report(&outcome, false);
                match args.format {
                    crate::cli::SetFormat::Records => {
                        crate::set::report::render_records(&mut out, &preview)?;
                    }
                    crate::cli::SetFormat::Json => {
                        crate::set::report::render_json(&mut out, &preview)?;
                    }
                }
            }

            Ok(0)
        }
        Command::New(args) => match crate::new::preflight_and_plan(&args, &cwd) {
            Ok(bundle) => {
                print!("{}", bundle.rendered);
                std::process::exit(bundle.exit_code);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
        Command::Init(args) => init::run(&cwd, &args),
        Command::Completions(_) => {
            unreachable!("completions are handled before vault targeting")
        }
        Command::Manpage => {
            unreachable!("manpage is handled before vault targeting")
        }
        Command::SelfUpdate(_) => {
            unreachable!("self-update is handled before vault targeting")
        }
    }
}

fn run_completions_command(cmd: crate::cli::CompletionsCommand) -> Result<i32> {
    match cmd.command {
        crate::cli::CompletionsSubcommand::Init(args) => {
            completions::run_init(args.shell)?;
            Ok(0)
        }
        crate::cli::CompletionsSubcommand::Install(args) => {
            completions::run_install(args)?;
            Ok(0)
        }
    }
}

fn run_manpage_command() -> Result<i32> {
    completions::run_manpage()?;
    Ok(0)
}

fn run_self_update_command(args: cli::SelfUpdateArgs, color: cli::ColorWhen) -> Result<i32> {
    use std::io::IsTerminal;

    let install_path =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("resolve current_exe: {e}"))?;

    let cfg = self_update::RunConfig {
        dry_run: args.dry_run,
        pinned_version: args.version.clone(),
        receipt_path_override: None,
        install_path,
        releases_url: "https://github.com/dbtlr/norn/releases".to_string(),
        target_triple: self_update::resolve::TARGET_TRIPLE.map(str::to_string),
        current_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let result = self_update::run(&cfg);
    let format = args.format.unwrap_or_else(|| {
        if std::io::stdout().is_terminal() {
            cli::SelfUpdateFormat::Text
        } else {
            cli::SelfUpdateFormat::Json
        }
    });

    match result {
        Ok((report, exit)) => {
            let palette = crate::output::palette::resolve(color);
            let mut stdout = std::io::stdout().lock();
            match format {
                cli::SelfUpdateFormat::Text => {
                    self_update::render::render_text(&mut stdout, &palette, &report)?
                }
                cli::SelfUpdateFormat::Json => {
                    self_update::render::render_json(&mut stdout, &report)?
                }
            }
            Ok(exit)
        }
        Err(err) => {
            let exit = self_update::classify_exit(&err);
            let msg = format!("{err:#}");
            if exit == 2 && msg.contains("no_receipt") {
                eprintln!("{}", self_update::BLOCK_MESSAGE);
            } else {
                // Strip the internal `BLOCK::<kind>: ` routing prefix from the
                // user-visible message — it exists for classify_exit, not the
                // human reading stderr.
                let display = strip_block_prefix(&msg);
                eprintln!("{display}");
            }
            Ok(exit)
        }
    }
}

fn strip_block_prefix(msg: &str) -> &str {
    let Some(rest) = msg.strip_prefix("BLOCK::") else {
        return msg;
    };
    rest.split_once(": ").map(|(_, tail)| tail).unwrap_or(rest)
}

fn repair_plan_filters(args: &crate::cli::RepairPlanArgs) -> RepairPlanFilters {
    RepairPlanFilters {
        code: normalized_filter_values(&args.triage.code),
        severity: normalized_filter_values(&args.triage.severity),
        field: normalized_filter_values(&args.triage.field),
        rule: normalized_filter_values(&args.triage.rule),
        path: normalized_filter_values(&args.triage.path),
        target: normalized_filter_values(&args.triage.target),
        reason: normalized_filter_values(&args.triage.reason),
        skip_reason: normalized_filter_values(&args.skip_reason),
        confidence: args.confidence.map(|c| match c {
            crate::cli::ConfidenceArg::High => crate::standards::ConfidenceFilter::High,
        }),
    }
}

fn normalized_filter_values(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn trim_diagnostics(index: &mut GraphIndex, verbose: bool) {
    if verbose {
        return;
    }
    for document in &mut index.documents {
        document.diagnostics = concise_diagnostics(document);
    }
}

fn exit_code_for(index: &GraphIndex) -> i32 {
    if has_errors(index) {
        1
    } else {
        0
    }
}
