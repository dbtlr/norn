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
mod mcp;
mod migrate_cmd;
pub mod migration_plan;
pub mod move_doc;
mod mutation_lock;
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
mod telemetry;
mod validate;
mod validate_filter;

use std::process;

use crate::cli::{CacheSubcommand, Cli, Command, ConfigSubcommand};
use crate::config_loader::{effective_cwd, load_config};
use crate::core::GraphIndex;
use crate::graph::{concise_diagnostics, has_errors};
use crate::migrate_cmd::MigrateRunArgs;
use crate::output::primitives::is_broken_pipe;
use crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs;
use crate::standards::validate_with_compiled;
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

    // The MCP server owns its own tokio runtime and vault open, so it is
    // pre-handled here — after cwd/config resolution but before the
    // cache-opening match arms below.
    if let Command::Mcp(args) = &command {
        crate::mcp::run(args, &cwd, config_path.as_ref())?;
        return Ok(0);
    }

    // The explicit `cache prune` manages the sweep itself (and a --dry-run
    // must not be followed by a real sweep in the same invocation), so the
    // tail-hook lazy sweep is skipped for it.
    let is_explicit_prune = matches!(
        &command,
        Command::Cache(c) if matches!(c.command, CacheSubcommand::Prune(_))
    );

    let outcome = match command {
        Command::Migrate(args) => {
            let run_args = MigrateRunArgs {
                plan_path: args.plan_path,
                dry_run: args.dry_run,
                yes: args.yes,
                format: args.format,
                input_format: args.input_format,
                out: args.out,
            };
            migrate_cmd::run(
                run_args,
                &cwd,
                no_cache_refresh,
                config_path.as_ref(),
                verbose,
            )
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
            rewrite_wikilink_cmd::run(
                run_args,
                &cwd,
                no_cache_refresh,
                config_path.as_ref(),
                verbose,
            )
        }
        Command::Repair(args) => {
            let ctx = crate::repair::RepairRunContext {
                cwd: &cwd,
                config_path: config_path.as_ref(),
                no_cache_refresh,
                verbose,
            };
            if args.plan {
                repair::run_plan(&args, &ctx)
            } else {
                repair::run_summary(&args, &ctx)
            }
        }
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
                CacheSubcommand::Prune(args) => crate::cache_cmd::run_prune(
                    &cwd,
                    loaded_config.vault_config.cache.as_ref(),
                    args,
                )?,
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

            // `markdown` is the one principled divergence: a single, byte-faithful
            // document straight from disk. It is selection-bound (meaningful only
            // for one doc), so it errors unless exactly one document is selected.
            if matches!(args.format, cli::GetFormat::Markdown) {
                let stderr = std::io::stderr();
                let mut stderr_lock = stderr.lock();
                crate::output::projection::warn_col_ignored(
                    &args.col,
                    Some("markdown"),
                    &mut stderr_lock,
                )?;
                for note in &report.notes {
                    eprintln!("{}", note);
                }
                return match report.records.len() {
                    1 => {
                        let path = &report.records[0].path;
                        match crate::output::projection::read_raw(&cache.vault_root, path) {
                            // Byte-faithful: print verbatim, no trailing-newline fixup.
                            Some(raw) => {
                                print!("{}", raw);
                                Ok(0)
                            }
                            None => {
                                eprintln!("error: could not read source file for '{}'", path);
                                Ok(1)
                            }
                        }
                    }
                    // No records: the per-target errors are already in `notes`.
                    0 => Ok(1),
                    n => {
                        eprintln!(
                            "error: --format markdown returns a single document; {n} selected \
                             — use --format json --col .raw for multiple"
                        );
                        Ok(1)
                    }
                };
            }

            let stdout_text = match args.format {
                cli::GetFormat::Json => show::render::render_json_with_col(&report, &args.col),
                cli::GetFormat::Jsonl => show::render::render_jsonl_with_col(&report, &args.col),
                cli::GetFormat::Paths => show::render::render_paths(&report),
                cli::GetFormat::Records => {
                    show::render::render_records_with_col(&report, &args.col)
                }
                cli::GetFormat::Markdown => unreachable!("markdown handled above"),
            };
            print!("{}", stdout_text);
            if !stdout_text.ends_with('\n') {
                println!();
            }

            let stderr = std::io::stderr();
            let mut stderr_lock = stderr.lock();
            crate::output::projection::warn_col_ignored(
                &args.col,
                matches!(args.format, cli::GetFormat::Paths).then_some("paths"),
                &mut stderr_lock,
            )?;
            show::render::warn_unknown_cols(&args.col, &report, &mut stderr_lock)?;

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
            use crate::cache::CacheError;
            use crate::migration_plan::{
                MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION,
            };
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;
            use std::io::Write;

            // Acquire mutation lock before cache load.
            // Note: for move, --format json is an implicit DRY-RUN (unlike migrate),
            // so JSON format alone does NOT force is_apply here.
            let (_, state_dir) = crate::cache::state_dir_for(&cwd)
                .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
            sweep_pending(&state_dir);
            let _mutation_lock = {
                use std::io::IsTerminal;
                let is_apply = !args.dry_run && (args.yes || std::io::stdin().is_terminal());
                match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
                    Ok(guard) => guard,
                    Err(CacheError::MutationLockTimeout) => {
                        eprintln!(
                            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                        );
                        return Ok(2);
                    }
                    Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
                }
            };

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

            // Pre-flight (single-file only): validate src/dst before building
            // the MigrationPlan so we can exit 2 on refusal. The cascade counts
            // for TTY rendering are read from the report after apply, not here.
            if !is_folder {
                let cfg = crate::move_doc::PreflightConfig {
                    src: &args.src,
                    dst: &args.dst,
                    force: args.force,
                    no_link_rewrite: args.no_link_rewrite,
                    vault_root: &cwd,
                    index: &index,
                };
                if let Err(e) = crate::move_doc::preflight_and_plan(cfg) {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            }

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
                verbose,
            };

            let argv: Vec<String> = std::env::args().collect();
            let mut sink = open_event_sink(
                &cwd,
                dry_run,
                loaded_config.vault_config.telemetry.as_ref(),
                &argv,
            );
            emit_invocation_started(
                &mut sink,
                "move",
                &cwd,
                &migration_plan.vault_root,
                dry_run,
                &argv,
            );

            let report = match apply_migration_plan(&migration_plan, &index, ctx, &mut sink) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };

            let exit = if report.failed > 0 { 1 } else { 0 };

            emit_invocation_finished(&mut sink, "move", exit, &report);

            emit_cascade_failure_warnings(&report);

            // After a live folder move, clean up empty source directories.
            if is_folder && !dry_run && exit == 0 {
                remove_empty_dirs(src_full.as_std_path());
            }

            // TTY cascade counts come from the move_document op's cascade
            // (dry-run: applied == planned forecast; live: actuals).
            let (link_total, link_files) = report
                .operations
                .iter()
                .find(|o| o.kind == "move_document")
                .and_then(|o| o.cascade.as_ref())
                .map_or((0, 0), |c| (c.applied, c.files));

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
                    if !dry_run {
                        writeln!(out, "trace: {}", report.trace_id)?;
                    }
                }
            }

            Ok(exit)
        }
        Command::Delete(args) => {
            use crate::applier::{apply_migration_plan, ApplyContext};
            use crate::cache::CacheError;
            use crate::migration_plan::{
                MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION,
            };
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;
            use std::io::Write;

            // Acquire mutation lock before cache load.
            // For delete: --format json is also an implicit dry-run.
            let (_, state_dir) = crate::cache::state_dir_for(&cwd)
                .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
            sweep_pending(&state_dir);
            let _mutation_lock = {
                use std::io::IsTerminal;
                let is_apply = !args.dry_run && (args.yes || std::io::stdin().is_terminal());
                match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
                    Ok(guard) => guard,
                    Err(CacheError::MutationLockTimeout) => {
                        eprintln!(
                            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                        );
                        return Ok(2);
                    }
                    Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
                }
            };

            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let mut index = crate::cache_cmd::load_graph_index(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            trim_diagnostics(&mut index, verbose);

            // ----------------------------------------------------------------
            // Pre-flight: validate doc exists + enforce backlinks policy.
            // Backlinks-present + no --rewrite-to + no --allow-broken-links → exit 2.
            // Extract incoming-link data for TTY rendering.
            // ----------------------------------------------------------------
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

            // Compute incoming-links info for TTY rendering.
            let delete_op = outcome
                .plan
                .changes
                .iter()
                .find(|c| c.operation == "delete_document")
                .expect("preflight_and_plan must produce a delete_document op");
            let bl = crate::target::backlinks(&index, &delete_op.path);
            let incoming_total = bl.len();
            let mut incoming_file_paths: Vec<camino::Utf8PathBuf> = {
                use std::collections::BTreeSet;
                let mut seen: BTreeSet<camino::Utf8PathBuf> = BTreeSet::new();
                for link in &bl {
                    seen.insert(link.source_path.clone());
                }
                seen.into_iter().collect()
            };
            // If rewrite_to is present but no incoming links broke, files list is the
            // rewrite sources (from link_risk source_path).
            if args.rewrite_to.is_some() && incoming_file_paths.is_empty() {
                if let Some(risk) = &delete_op.link_risk {
                    use std::collections::BTreeSet;
                    let mut seen: BTreeSet<camino::Utf8PathBuf> = BTreeSet::new();
                    for a in risk
                        .stem_links
                        .iter()
                        .chain(risk.path_qualified_wikilinks.iter())
                        .chain(risk.markdown_links.iter())
                    {
                        seen.insert(a.source_path.clone());
                    }
                    incoming_file_paths = seen.into_iter().collect();
                }
            }
            let resolved_rewrite_to = outcome.resolved_rewrite_to.clone();

            // ----------------------------------------------------------------
            // Resolve dry_run.
            // ----------------------------------------------------------------
            let dry_run = resolve_delete_dry_run(args.dry_run, args.yes, args.format)?;

            // ----------------------------------------------------------------
            // Build one-op MigrationPlan.
            // ----------------------------------------------------------------
            let plan = MigrationPlan {
                schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
                vault_root: cwd.to_string(),
                generator: None,
                generated_at: None,
                operations: vec![MigrationOp {
                    kind: "delete_document".into(),
                    id: None,
                    requires: vec![],
                    fields: serde_json::json!({
                        "path": args.doc,
                        "rewrite_to": args.rewrite_to.as_ref(),
                        "allow_broken_links": args.allow_broken_links,
                    }),
                    footnote: None,
                }],
                skipped: vec![],
                plan_footnote: None,
            };

            let ctx = ApplyContext {
                dry_run,
                parents: false,
                verbose,
            };

            let argv: Vec<String> = std::env::args().collect();
            let mut sink = open_event_sink(
                &cwd,
                dry_run,
                loaded_config.vault_config.telemetry.as_ref(),
                &argv,
            );
            emit_invocation_started(&mut sink, "delete", &cwd, &plan.vault_root, dry_run, &argv);

            let report = match apply_migration_plan(&plan, &index, ctx, &mut sink) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };

            let exit = if report.failed > 0 { 1 } else { 0 };

            emit_invocation_finished(&mut sink, "delete", exit, &report);

            emit_cascade_failure_warnings(&report);

            // rewrite_total comes from the delete_document op's cascade.
            let rewrite_total = report
                .operations
                .iter()
                .find(|o| o.kind == "delete_document")
                .and_then(|o| o.cascade.as_ref())
                .map_or(0, |c| c.applied);

            // ----------------------------------------------------------------
            // Render output.
            // ----------------------------------------------------------------
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            match args.format {
                crate::cli::DeleteFormat::Json => {
                    let json = serde_json::to_string_pretty(&report)?;
                    out.write_all(json.as_bytes())?;
                    out.write_all(b"\n")?;
                }
                crate::cli::DeleteFormat::Records => {
                    let applied = !dry_run && exit == 0;
                    crate::delete_doc::render_delete_apply_tty(
                        &mut out,
                        &args.doc,
                        incoming_total,
                        &incoming_file_paths,
                        resolved_rewrite_to.as_deref().map(camino::Utf8Path::as_str),
                        rewrite_total,
                        applied,
                    )?;
                    if !dry_run {
                        writeln!(out, "trace: {}", report.trace_id)?;
                    }
                }
            }

            Ok(exit)
        }
        Command::Set(args) => {
            use crate::cache::CacheError;
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;
            use std::io::{IsTerminal, Write};

            // Acquire mutation lock before cache load.
            // Set: --format json without --yes is implicit dry-run (early-return preview),
            // so JSON alone does NOT force is_apply here.
            let (_, state_dir) = crate::cache::state_dir_for(&cwd)
                .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
            sweep_pending(&state_dir);
            let _mutation_lock = {
                let is_apply = !args.dry_run && (args.yes || std::io::stdin().is_terminal());
                match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
                    Ok(guard) => guard,
                    Err(CacheError::MutationLockTimeout) => {
                        eprintln!(
                            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                        );
                        return Ok(2);
                    }
                    Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
                }
            };

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
                let preview = crate::set::report::build_report(&outcome, false, "");
                crate::set::report::render_json(&mut out, &preview)?;
                return Ok(0);
            } else if std::io::stdin().is_terminal() {
                // TTY interactive: render preview first so the operator can see what
                // they're confirming, then prompt.
                let preview = crate::set::report::build_report(&outcome, false, "");
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
                let preview = crate::set::report::build_report(&outcome, false, "");
                crate::set::report::render_records(&mut out, &preview)?;
                return Ok(0);
            };

            if should_apply {
                // Real apply: open a file-backed sink and emit the full event
                // stream (lifecycle → op_planned → action → finished). Only the
                // real-apply branch persists telemetry; dry-run/preview branches
                // above early-return without opening a disk sink.
                let argv: Vec<String> = std::env::args().collect();
                let mut sink = open_event_sink(
                    &cwd,
                    /*dry_run=*/ false,
                    vault_cfg.telemetry.as_ref(),
                    &argv,
                );
                emit_invocation_started(
                    &mut sink,
                    "set",
                    &cwd,
                    outcome.plan.vault_root.as_str(),
                    /*dry_run=*/ false,
                    &argv,
                );

                let mut spans = std::collections::HashMap::new();
                for change in &outcome.plan.changes {
                    let span = sink.start_op(&change.operation, change.path.as_str(), None);
                    spans.insert(change.change_id.clone(), span);
                }

                let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
                    &cwd,
                    &index,
                    &outcome.plan,
                    /*dry_run=*/ false,
                    &crate::repair_apply::CreateApplyContext::default(),
                    &mut sink,
                    &spans,
                );

                let trace_id = sink.trace_id().to_string();
                let exit = if apply_outcome.is_ok() { 0 } else { 2 };
                emit_single_op_finished(&mut sink, "set", exit, apply_outcome.is_ok());
                apply_outcome?;

                let applied = crate::set::report::build_report(&outcome, true, &trace_id);
                match args.format {
                    crate::cli::SetFormat::Records => {
                        crate::set::report::render_records(&mut out, &applied)?;
                        // TTY `trace:` footer on real apply (Records only; JSON
                        // carries trace_id as a field).
                        writeln!(out, "trace: {trace_id}")?;
                    }
                    crate::cli::SetFormat::Json => {
                        crate::set::report::render_json(&mut out, &applied)?;
                    }
                }
            } else {
                // --dry-run: render preview, respecting --format.
                let preview = crate::set::report::build_report(&outcome, false, "");
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
        Command::New(args) => {
            use crate::cache::CacheError;
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;

            // Acquire mutation lock before preflight_and_plan (which does the cache load).
            // New uses stdout for TTY detection (interactive preview shown on stdout).
            let (_, state_dir) = crate::cache::state_dir_for(&cwd)
                .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
            sweep_pending(&state_dir);
            let _mutation_lock = {
                use std::io::IsTerminal;
                let is_apply = !args.dry_run && (args.yes || std::io::stdout().is_terminal());
                match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
                    Ok(guard) => guard,
                    Err(CacheError::MutationLockTimeout) => {
                        eprintln!(
                            "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                        );
                        return Ok(2);
                    }
                    Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
                }
            };
            // _mutation_lock held here; dropped when arm returns.
            match crate::new::preflight_and_plan(&args, &cwd) {
                Ok(bundle) => {
                    print!("{}", bundle.rendered);
                    Ok(bundle.exit_code)
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    Ok(2)
                }
            }
        }
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
        Command::Mcp(_) => {
            unreachable!("mcp is handled before the cache-opening dispatch")
        }
    };
    // Per-invocation throttled lazy GC: best-effort, never affects the
    // command's outcome or exit code. Arms that early-`return` or call
    // `process::exit` (completions, self-update, markdown get, TTY prompt
    // decline) skip the sweep by design — the 24 h throttle self-heals on
    // the next invocation. The sweep must remain the last thing before
    // returning `outcome`; do not insert post-dispatch work after it.
    // Explicit `cache prune` is also skipped: it manages the sweep itself,
    // and a --dry-run must never be followed by a real sweep.
    if !is_explicit_prune {
        crate::cache::prune::lazy_sweep(&cwd, config_path.as_ref());
    }
    outcome
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

/// Emit a loud stderr warning for any backlink that remained failed after the
/// retry pass. The primary op still succeeded (exit code unaffected); this is
/// the explainability signal the exit code deliberately doesn't carry.
fn emit_cascade_failure_warnings(report: &crate::apply_report::ApplyReport) {
    for op in &report.operations {
        let Some(cascade) = op.cascade.as_ref() else {
            continue;
        };
        if cascade.failed == 0 {
            continue;
        }
        eprintln!(
            "warning: {} backlink{} could not be rewritten after retries and now dangle{}:",
            cascade.failed,
            if cascade.failed == 1 { "" } else { "s" },
            if cascade.failed == 1 { "s" } else { "" },
        );
        for f in &cascade.failures {
            match &f.detail {
                Some(d) => eprintln!("  {}: {} → {} ({}: {})", f.file, f.from, f.to, f.reason, d),
                None => eprintln!("  {}: {} → {} ({})", f.file, f.from, f.to, f.reason),
            }
        }
        eprintln!("  fix manually, or run `norn validate` to list dangling links.");
    }
}

/// Build the telemetry EventSink for a mutating command. Dry-runs and resolution
/// failures yield an in-memory `discard` sink (best-effort; never fails the command).
fn open_event_sink(
    cwd: &camino::Utf8Path,
    dry_run: bool,
    telemetry: Option<&crate::standards::TelemetryConfig>,
    _argv: &[String], // accepted for future use; argv is set on the started event by the caller
) -> crate::telemetry::EventSink {
    use crate::telemetry::{Clock, EventSink, IdGen};
    let ids = IdGen::new();
    let clock = Clock::System;
    if dry_run {
        return EventSink::discard(ids, clock); // dry-runs never persist
    }
    let start_ts = clock.now_rfc3339();
    let dir = telemetry
        .and_then(|t| t.location.clone())
        .map(camino::Utf8PathBuf::from)
        .or_else(|| crate::cache::events_dir_for(cwd).ok().map(|(_, d)| d));
    let retention = telemetry
        .and_then(|t| t.retention)
        .unwrap_or(crate::standards::DEFAULT_RETENTION);
    if let Some(dir) = dir.as_ref() {
        let today = &start_ts[..10];
        crate::telemetry::store::prune_events(dir, retention, today);
        crate::telemetry::store::enforce_size_cap(
            dir,
            crate::telemetry::store::EVENTS_SIZE_CAP_BYTES,
            today,
        );
        EventSink::open(dir, start_ts, ids, clock)
            .unwrap_or_else(|_| EventSink::discard(IdGen::new(), Clock::System))
    } else {
        EventSink::discard(ids, clock)
    }
}

/// Emit the `invocation_started` lifecycle event for a mutating command.
pub(crate) fn emit_invocation_started(
    sink: &mut crate::telemetry::EventSink,
    cmd: &str,
    cwd: &camino::Utf8Path,
    vault_root: &str,
    dry_run: bool,
    argv: &[String],
) {
    use crate::telemetry::event::{
        ATTR_ARGV, ATTR_CWD, ATTR_DRY_RUN, ATTR_VAULT_ROOT, EVENT_INVOCATION_STARTED,
    };
    use crate::telemetry::Severity;
    sink.lifecycle(
        EVENT_INVOCATION_STARTED,
        Severity::Info,
        format!("{cmd} started"),
        vec![
            (ATTR_CWD, cwd.to_string()),
            (ATTR_VAULT_ROOT, vault_root.to_string()),
            (ATTR_DRY_RUN, dry_run.to_string()),
            (ATTR_ARGV, argv.join(" ")),
        ],
    );
}

/// Emit the `invocation_finished` lifecycle event for a mutating command.
fn emit_invocation_finished(
    sink: &mut crate::telemetry::EventSink,
    cmd: &str,
    exit_code: i32,
    report: &crate::apply_report::ApplyReport,
) {
    use crate::telemetry::event::{
        ATTR_EXIT, ATTR_TALLY_APPLIED, ATTR_TALLY_FAILED, ATTR_TALLY_SKIPPED,
        EVENT_INVOCATION_FINISHED,
    };
    use crate::telemetry::Severity;
    sink.lifecycle(
        EVENT_INVOCATION_FINISHED,
        Severity::Info,
        format!("{cmd} finished"),
        vec![
            (ATTR_EXIT, exit_code.to_string()),
            (ATTR_TALLY_APPLIED, report.applied.to_string()),
            (ATTR_TALLY_SKIPPED, report.skipped.to_string()),
            (ATTR_TALLY_FAILED, report.failed.to_string()),
        ],
    );
}

/// Emit the `invocation_finished` lifecycle event for a single-op mutator
/// (`set` / `new`) that doesn't build an `ApplyReport`. Tallies are trivial:
/// one op that either applied or failed.
pub(crate) fn emit_single_op_finished(
    sink: &mut crate::telemetry::EventSink,
    cmd: &str,
    exit_code: i32,
    applied: bool,
) {
    use crate::telemetry::event::{
        ATTR_EXIT, ATTR_TALLY_APPLIED, ATTR_TALLY_FAILED, ATTR_TALLY_SKIPPED,
        EVENT_INVOCATION_FINISHED,
    };
    use crate::telemetry::Severity;
    let (applied_n, failed_n) = if applied { (1, 0) } else { (0, 1) };
    sink.lifecycle(
        EVENT_INVOCATION_FINISHED,
        Severity::Info,
        format!("{cmd} finished"),
        vec![
            (ATTR_EXIT, exit_code.to_string()),
            (ATTR_TALLY_APPLIED, applied_n.to_string()),
            (ATTR_TALLY_SKIPPED, 0.to_string()),
            (ATTR_TALLY_FAILED, failed_n.to_string()),
        ],
    );
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

/// Resolve the `dry_run` flag for a `norn delete` invocation.
///
/// - `--dry-run` → always dry-run.
/// - `--yes` → apply (no prompt).
/// - `--format json` → implicit non-interactive dry-run (safe for pipelines).
/// - TTY stdin → prompt the operator; exit 1 if declined.
/// - Non-TTY, no `--yes` → implicit dry-run.
///
/// Returns `Ok(true)` for dry-run, `Ok(false)` for apply.
fn resolve_delete_dry_run(
    dry_run_flag: bool,
    yes_flag: bool,
    format: crate::cli::DeleteFormat,
) -> anyhow::Result<bool> {
    use std::io::IsTerminal;
    if dry_run_flag {
        return Ok(true);
    }
    if yes_flag {
        return Ok(false);
    }
    // --format json without --yes: implicit non-interactive dry-run.
    if matches!(format, crate::cli::DeleteFormat::Json) {
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
