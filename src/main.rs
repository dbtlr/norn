pub mod applier;
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
mod migrate_cmd;
pub mod migration_plan;
pub mod move_doc;
pub mod mutation_report;
mod new;
mod output;
pub mod planner;
pub mod prompt;
mod query;
mod repair;
mod repair_apply;
mod rewrite_wikilink_cmd;
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
use crate::migrate_cmd::MigrateRunArgs;
use crate::output::primitives::is_broken_pipe;
use crate::repair::skip_reasons::code_matches_any;
use crate::repair_apply::{apply_repair_plan, with_verification};
use crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs;
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
        Command::Migrate(args) => {
            let run_args = MigrateRunArgs {
                plan_path: args.plan_path,
                dry_run: args.dry_run,
                yes: args.yes,
                format: args.format,
                input_format: args.input_format,
                out: args.out,
            };
            migrate_cmd::run(run_args, &cwd, no_cache_refresh, config_path.as_ref())
        }
        Command::RewriteWikilink(args) => {
            let run_args = RewriteWikilinkRunArgs {
                old: args.old,
                new: args.new,
                dry_run: args.dry_run,
                yes: args.yes,
                format: args.format,
                out: args.out,
            };
            rewrite_wikilink_cmd::run(run_args, &cwd, no_cache_refresh, config_path.as_ref())
        }
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
            use crate::applier::{apply_migration_plan, ApplyContext};
            use crate::migration_plan::{
                MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION,
            };
            use std::io::Write;

            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);

            // Auto-detect folder move: if SRC is a directory on disk (or --recursive
            // is explicit), route through the planner via a move_folder op.
            // This matches the "warn don't block" pattern — an operator who typed
            // `norn move src_dir dst_dir` without -r almost certainly meant folder-move.
            let src_full = cwd.join(&args.src);
            let src_is_dir = src_full.as_std_path().is_dir();
            let is_folder = args.recursive || src_is_dir;

            // --parents: for single-file moves, create missing destination parent
            // directories before preflight. (Folder moves handle parents via the expander.)
            if !is_folder && args.parents {
                let dst_path = camino::Utf8Path::new(&args.dst);
                if let Some(parent) = dst_path.parent() {
                    if !parent.as_str().is_empty() {
                        std::fs::create_dir_all(cwd.join(parent)).map_err(|e| {
                            anyhow::anyhow!(
                                "failed to create destination parents for {}: {e}",
                                args.dst
                            )
                        })?;
                    }
                }
            }

            // ----------------------------------------------------------------
            // Pre-flight (single-file only): validate src/dst before building
            // the MigrationPlan so we can exit 2 on refusal.
            // ----------------------------------------------------------------
            // For folder moves the expander handles its own validation; we only
            // run the explicit preflight for single-file moves.
            let (link_total, link_files) = if !is_folder {
                let cfg = crate::move_doc::PreflightConfig {
                    src: &args.src,
                    dst: &args.dst,
                    force: args.force,
                    no_link_rewrite: args.no_link_rewrite,
                    vault_root: &cwd,
                    index: &index,
                };
                let repair_plan = match crate::move_doc::preflight_and_plan(cfg) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }
                };
                // Compute link counts from link_risk for TTY rendering.
                let move_op = repair_plan
                    .changes
                    .iter()
                    .find(|c| c.operation == "move_document")
                    .expect("plan must have a move_document op");
                let (lt, lf) = if let Some(risk) = &move_op.link_risk {
                    use std::collections::BTreeSet;
                    let mut files: BTreeSet<&camino::Utf8Path> = BTreeSet::new();
                    let mut total = 0usize;
                    for a in risk
                        .stem_links
                        .iter()
                        .chain(risk.path_qualified_wikilinks.iter())
                        .chain(risk.markdown_links.iter())
                    {
                        files.insert(a.source_path.as_path());
                        total += 1;
                    }
                    (total, files.len())
                } else {
                    (0, 0)
                };
                (lt, lf)
            } else {
                (0, 0)
            };

            // ----------------------------------------------------------------
            // Resolve dry_run (extracted helper logic, shared across both paths).
            // --format json → implicit non-interactive (no apply without --yes).
            // ----------------------------------------------------------------
            let dry_run = resolve_move_dry_run(args.dry_run, args.yes, &args.format)?;

            // ----------------------------------------------------------------
            // Build one-op MigrationPlan.
            // ----------------------------------------------------------------
            let op_kind = if is_folder {
                "move_folder"
            } else {
                "move_document"
            };
            let mut fields = serde_json::json!({
                "src": args.src.clone(),
                "dst": args.dst.clone(),
                "parents": args.parents,
            });
            if !is_folder && args.force {
                fields["force"] = serde_json::Value::Bool(true);
            }
            if !is_folder && args.no_link_rewrite {
                fields["no_link_rewrite"] = serde_json::Value::Bool(true);
            }
            let migration_plan = MigrationPlan {
                schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
                vault_root: cwd.to_string(),
                generator: None,
                generated_at: None,
                operations: vec![MigrationOp {
                    kind: op_kind.into(),
                    id: None,
                    requires: vec![],
                    fields,
                    footnote: None,
                }],
                skipped: vec![],
                plan_footnote: None,
            };

            let ctx = ApplyContext {
                dry_run,
                parents: args.parents,
            };
            let report = match apply_migration_plan(&migration_plan, &index, ctx) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };

            let exit = if report.failed > 0 { 1 } else { 0 };

            // After a live folder move, clean up empty source directories.
            if is_folder && !dry_run && exit == 0 {
                remove_empty_dirs(src_full.as_std_path());
            }

            // ----------------------------------------------------------------
            // Render output.
            // ----------------------------------------------------------------
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match args.format {
                crate::cli::MoveFormat::Json => {
                    let json = serde_json::to_string_pretty(&report)?;
                    out.write_all(json.as_bytes())?;
                    out.write_all(b"\n")?;
                }
                crate::cli::MoveFormat::Records => {
                    if is_folder {
                        crate::move_doc::render_folder_apply_tty(&mut out, &report, dry_run)?;
                    } else {
                        let applied = !dry_run && exit == 0;
                        crate::move_doc::render_move_apply_tty(
                            &mut out, &args.src, &args.dst, link_total, link_files, applied,
                        )?;
                    }
                }
            }

            Ok(exit)
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

/// Resolve the `dry_run` flag for a `norn move` invocation.
///
/// - `--dry-run` → always dry-run.
/// - `--yes` → apply (no prompt).
/// - `--format json` → implicit non-interactive; apply without prompting.
///   (JSON mode is designed for script/agent use where `--yes` is implied.)
/// - TTY stdin → prompt the operator; exit 1 if declined.
/// - Non-TTY, no `--yes` → implicit dry-run.
///
/// Returns `Ok(true)` for dry-run, `Ok(false)` for apply.
fn resolve_move_dry_run(
    dry_run_flag: bool,
    yes_flag: bool,
    format: &crate::cli::MoveFormat,
) -> anyhow::Result<bool> {
    use std::io::IsTerminal;
    if dry_run_flag {
        return Ok(true);
    }
    if yes_flag {
        return Ok(false);
    }
    // --format json without --yes: implicit non-interactive dry-run (safe for
    // script/agent pipelines that haven't explicitly confirmed with --yes).
    if matches!(format, crate::cli::MoveFormat::Json) {
        return Ok(true);
    }
    if std::io::stdin().is_terminal() {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut prompt_out = std::io::stderr();
        use std::io::Write;
        writeln!(prompt_out)?;
        let ok = crate::prompt::confirm(&mut reader, &mut prompt_out, "Proceed? [y/N] ")?;
        if !ok {
            std::process::exit(1);
        }
        return Ok(false);
    }
    // Non-TTY without --yes: implicit dry-run.
    Ok(true)
}

/// Recursively remove a directory and all of its children, but only if every
/// descendant is an empty directory. If any non-directory file remains (e.g. a
/// .md file that failed to move), the directory is left intact.
///
/// Called after a `move_folder` apply to clean up the empty source tree.
fn remove_empty_dirs(path: &std::path::Path) {
    if !path.is_dir() {
        return;
    }
    // Recurse into children first (depth-first).
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                remove_empty_dirs(&child);
            }
        }
    }
    // Now attempt to remove this directory (succeeds only if empty).
    let _ = std::fs::remove_dir(path);
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
