pub mod applier;
mod apply_cmd;
pub mod apply_report;
mod audit;
mod cache;
// The (cache-home, vault-root) → cache-dir identity mapping, re-exported for
// test harnesses (the NRN-83 acceptance benchmark) that must locate a vault's
// cache.db under a private cache home exactly the way production does — pure
// function, no env read. `#[doc(hidden)]` seam, not stable public API — see
// `cache::resolve_cache_dir_in`.
pub use cache::resolve_cache_dir_in;
mod cache_cmd;
mod cli;
mod completions;
mod config;
mod config_loader;
mod core;
mod count;
pub mod delete_doc;
mod describe;
mod edit;
mod filter;
mod filter_args;
mod find;
mod frontmatter;
mod grammar;
mod graph;
mod help;
mod init;
mod init_scan;
mod links;
mod mcp;
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
mod route_wire;
mod self_update;
mod seq_alloc;
mod serve;
mod service;
mod set;
mod show;
mod standards;
mod target;
mod telemetry;
mod validate;
mod validate_filter;

use std::process;

use crate::apply_cmd::ApplyRunArgs;
use crate::cli::{CacheSubcommand, Cli, Command, ConfigSubcommand};
use crate::config_loader::{effective_cwd, load_config};
use crate::core::GraphIndex;
use crate::graph::{concise_diagnostics, has_errors};
use crate::output::primitives::is_broken_pipe;
use crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs;
use crate::standards::validate_with_compiled;
use crate::validate_filter::{filter_findings, ValidateFilterOptions};
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

/// CLI entrypoint. The `norn` binary is a thin shell over this; a future
/// `norn-service` binary links the same library (the module tree below) but
/// enters through its own accept loop rather than this one-shot dispatch.
pub fn cli_main() {
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
    // ADR 0010 forgiving-input pass (NRN-206/207/209): resolve aliases and
    // desugar dynamic `--key value` predicates into canonical `--eq`/`--in`
    // BEFORE clap parses. Canonical invocations pass through byte-identically.
    // The dynamic keys are validated against the vault's field universe once
    // the cache is open (see `gate_dynamic_fields` calls in `run`).
    let raw_argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let (matches, dynamic_keys) = match raw_argv
        .iter()
        .map(|s| s.to_str().map(str::to_string))
        .collect::<Option<Vec<String>>>()
    {
        Some(utf8_argv) => match grammar::normalize_argv(utf8_argv) {
            Ok(normalized) => (
                cmd.get_matches_from(normalized.argv),
                normalized.dynamic_keys,
            ),
            Err(error) => {
                eprintln!("error: {error:#}");
                process::exit(2);
            }
        },
        // Non-UTF-8 argv: skip the forgiving pass and let clap parse verbatim.
        None => (cmd.get_matches(), Vec::new()),
    };
    let cli = Cli::from_arg_matches(&matches).expect("clap-derive contract: parse from matches");
    match run(cli, &dynamic_keys) {
        Ok(exit_code) => process::exit(exit_code),
        Err(error) if is_broken_pipe(&error) => process::exit(0),
        Err(error) => {
            eprintln!("{error:#}");
            process::exit(1);
        }
    }
}

/// Whether a routable read must bypass the warm daemon and run Direct, purely
/// from the two invocation flags that break the routed↔direct equivalence.
///
/// - `--config` (`explicit_config`): the wire speaks canonical vault roots only,
///   never config paths, so a warm context (which loads each vault's own default
///   config) could silently ignore the flag. The verified direct open honors
///   `--config` exactly (ADR 0005 config-freshness note).
/// - `--no-cache-refresh` (`no_cache_refresh`): the daemon ALWAYS serves from a
///   freshly-refreshed warm cache, so routing a `--no-cache-refresh` read would
///   contradict the flag's intent (serve whatever the on-disk cache holds without
///   a refresh) and could return counts that differ from the direct path on a
///   stale cache. Direct honors it exactly.
///
/// Unix-only, like the routing seam that calls it: on non-Unix targets
/// `try_route_read` is a compile-time Direct stub that consults nothing.
#[cfg(unix)]
fn routing_forced_direct(explicit_config: bool, no_cache_refresh: bool) -> bool {
    explicit_config || no_cache_refresh
}

/// Whether stdin carries a redirected/piped payload — a FIFO pipe (`echo … |`)
/// or a regular file (`< ops.json`) — as opposed to a terminal or an empty
/// source such as `/dev/null`. Used to refuse `norn edit` op-flag sugar combined
/// with an ops array on stdin (F1): a pipe/file there would be silently ignored
/// by the sugar path. A TTY and `/dev/null` (a character device) carry no ops
/// and must stay allowed so interactive use and scripted `… --yes </dev/null`
/// both work.
#[cfg(unix)]
fn stdin_carries_redirected_payload() -> bool {
    use std::io::IsTerminal as _;
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::io::FromRawFd as _;

    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return false;
    }
    // SAFETY: fd 0 is a valid open descriptor for the process lifetime; the File
    // is wrapped in ManuallyDrop so it is never closed and ownership of fd 0 is
    // not taken.
    let file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    match file.metadata() {
        Ok(md) => {
            let ft = md.file_type();
            ft.is_fifo() || ft.is_file()
        }
        Err(_) => false,
    }
}

/// Non-unix fallback: without a portable way to distinguish a pipe from an empty
/// source, stay conservative and never refuse (preserves existing behavior).
#[cfg(not(unix))]
fn stdin_carries_redirected_payload() -> bool {
    false
}

/// The CLI→service routing seam (NRN-92/94).
///
/// For a routable read, probe for a live warm host daemon; if one answers,
/// translate the parsed args to the MCP tool contract, delegate to the warm
/// cache, and render the structured response in CLI format. Returns
/// `Some(result)` when the request was served by routing, or `None` to fall
/// through to the direct, integrity-verified dispatch (today's behavior).
/// `--config` / `--no-cache-refresh` force Direct up front (see
/// [`routing_forced_direct`]).
///
/// **Routing coverage.** `count` (NRN-94), `find` and `get` (NRN-222), and
/// `repair --plan` (NRN-231) route today. Each command's MCP tool returns a
/// `structuredContent` payload that the client rebuilds into the command's
/// native result type and renders through the SAME renderers the direct path
/// uses, so routed and direct output are byte-identical (the load-bearing
/// isomorphism, ADR 0005):
///
/// - `count` — `vault.count`'s `CountEnvelope` losslessly re-encodes `CountOutput`.
/// - `find` — `vault.find` carries the `total`/`returned`/`truncated`/`starts_at`
///   envelope (NRN-214), the vault's `has_diagnostic_errors` bit (the exit-2
///   signal — NRN-222), and the SAME projected per-document JSON `--format json`
///   emits; the client rebuilds a `FindResult` + deep/raw and renders via `find::emit`.
/// - `get` — `vault.get` ships each full serialized `ShowRecord` plus `notes`
///   (NRN-214); the client rebuilds a `ShowReport` and renders via `show::emit`,
///   applying the CLI's client-side `--col` narrowing. `--format markdown` and
///   `--section` stay Direct (see `route_get`).
/// - `repair --plan` — `vault.repair` carries the full `MigrationPlan` (byte-equal
///   to `serde_json::to_value(&plan)`) plus `has_diagnostic_errors` (the exit-1
///   signal); the client rebuilds the `MigrationPlan` and renders/writes it via
///   the SAME `repair::emit_plan` the direct path uses (report / json / paths,
///   `--out`). Bare `repair` (summary mode) has no wire analogue and stays
///   Direct (see `route_repair`).
///
/// **byte-identical output outranks routing coverage** — routing a read whose
/// output would differ is worse than not routing it. Any daemon-side failure
/// falls back to Direct silently; a daemon can never fail a read that direct
/// execution could serve.
fn try_route_read(
    command: &Command,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    color: crate::cli::ColorWhen,
    verbose: bool,
    dynamic_keys: &[String],
) -> Option<Result<i32>> {
    // The ONE non-Unix stub for the whole routing seam: the warm daemon rides
    // Unix-domain sockets, so every read always runs Direct. Future routed
    // commands inherit this — no per-command stub needed.
    #[cfg(not(unix))]
    {
        let _ = (
            command,
            cwd,
            explicit_config,
            no_cache_refresh,
            color,
            verbose,
            dynamic_keys,
        );
        None
    }
    #[cfg(unix)]
    {
        if routing_forced_direct(explicit_config, no_cache_refresh) {
            return None;
        }
        match command {
            // NRN-218: dynamic-field predicates now route too — the desugared keys
            // ride the wire so the daemon runs the field-universe gate against its
            // warm cache. `get` takes no filter predicates, so `dynamic_keys` is
            // always empty there.
            Command::Count(args) => route_count(args, cwd, verbose, dynamic_keys),
            Command::Find(args) => route_find(args, cwd, color, verbose, dynamic_keys),
            Command::Get(args) => route_get(args, cwd, verbose),
            Command::Repair(args) => route_repair(args, cwd, verbose),
            _ => None,
        }
    }
}

/// What a routed tool call does when the call fails AFTER the `tools/call`
/// frame has been sent to the daemon (NRN-228). Decided at send time and passed
/// down into [`execute_routed_call`], which shares one body between reads and
/// mutations. A failure BEFORE the send always falls back to Direct regardless
/// of this policy (the tool never ran, so a Direct retry cannot double-apply).
///
/// [NRN-151/CAS seam: a future safe-retry policy — e.g. a compare-and-swap
/// precondition that makes a post-send retry provably safe — slots in here as
/// another variant without reshaping the seam.]
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackAfterSend {
    /// Any post-send failure falls back to Direct silently. The request is
    /// idempotent — a read, or a dry-run mutation that writes nothing — so a
    /// second verified direct open is safe and byte-identical.
    Fallback,
    /// A post-send failure is NOT retried: the daemon may have applied the
    /// mutation, so a Direct re-run could double-apply. Surface an explicit
    /// uncertainty error (exit 1) naming the inspect / `--dry-run` remedy.
    Commit,
}

/// The literate description of a single routed tool call: the pieces every layer
/// of the routing seam needs to name the tool, thread its arguments, and decide
/// what an `isError` result means (NRN-229). A caller builds one `CallSpec` and
/// hands it to [`route_call`] (mutations) or [`route_read`] (reads) — nothing
/// lower in the seam takes bare `tool`/`arguments`/`on_tool_error`/`verbose`
/// params anymore.
#[cfg(unix)]
pub struct CallSpec<'a> {
    pub tool: &'a str,
    pub arguments: serde_json::Value,
    pub on_tool_error: crate::service::OnToolError,
    pub verbose: bool,
}

/// A [`CallSpec`] plus the send-commit policy (NRN-228) — the shape the inner
/// seam ([`route_tool_call`]/[`execute_routed_call`]) actually executes against.
/// Built by [`route_call`] (via [`after_send_for`]) and [`route_read`] (which
/// always hardcodes [`FallbackAfterSend::Fallback`]); a command's `route_*`
/// wrapper never constructs one directly.
#[cfg(unix)]
struct RoutedCall<'a> {
    spec: CallSpec<'a>,
    after_send: FallbackAfterSend,
}

/// The generic routing skeleton shared by reads (`count`/`find`/`get`, NRN-222)
/// and mutations (NRN-228).
///
/// Computes the canonical vault root ONCE (threaded into the preamble — NRN-92
/// review F5), probes the well-known socket, and delegates to
/// [`execute_routed_call`], which runs the call's tool with its arguments and
/// applies the `after_send` policy. Every pre-flight miss — an
/// un-canonicalizable root or no live daemon — returns `None`, so the direct
/// dispatch serves the request (and re-produces any error canonically).
///
/// `call.spec.on_tool_error` decides what a result flagged `isError: true` means
/// for this tool (see [`crate::service::OnToolError`]): `vault.get` accepts the
/// payload (its `isError` is the semantic not-found signal — falling back would
/// execute the failing read twice); `count`/`find` fall back to Direct.
#[cfg(unix)]
fn route_tool_call<T>(
    cwd: &camino::Utf8Path,
    call: RoutedCall<'_>,
    reconstruct: impl FnOnce(&serde_json::Value) -> Result<T>,
    emit: impl FnOnce(T) -> Result<i32>,
) -> Option<Result<i32>> {
    // A root that cannot canonicalize cannot be served warm either — fall back to
    // Direct, which reports the failure canonically. A pre-send miss, so both
    // policies fall back.
    let (canonical, _hash) = crate::cache::vault_identity(cwd).ok()?;

    // Probe the well-known control socket. No daemon => a cheap stat => Direct,
    // with zero added latency (the common case pays nothing beyond the stat).
    // Also a pre-send miss (the request never left this process).
    let client = match crate::service::probe(crate::service::handshake_timeout()) {
        crate::service::RouteDecision::Route(client) => client,
        crate::service::RouteDecision::Direct => return None,
    };

    execute_routed_call(&canonical, &client, call, reconstruct, emit)
}

/// The body shared by every routed call once a live daemon is proven (NRN-228):
/// invoke `tool` on `client`, apply the send-commit `after_send` policy to a
/// failure, then hand a success to `reconstruct` (rebuild the command's native
/// result type) and `emit` (render it exactly like the direct path).
///
/// The failure branch is the whole point of the send-commit split. A pre-send
/// failure (socket trust, connect, preamble, version gate, MCP initialize)
/// always falls back to Direct — the tool never ran. A post-send failure
/// (timeout, dropped connection, unreadable response) falls back only under
/// `Fallback`; under `Commit` it is surfaced as an explicit uncertainty error
/// (the daemon may have applied the change) instead of risking a double-apply.
///
/// The `reconstruct`-then-`emit` split keeps stdout untouched until
/// reconstruction succeeds, so a fall-back to Direct never double-writes.
///
/// Split from [`route_tool_call`] so unit tests can drive it with a stub
/// `ServiceClient` on a temp socket, without touching the process-global env the
/// well-known-socket probe reads.
#[cfg(unix)]
fn execute_routed_call<T>(
    canonical: &camino::Utf8Path,
    client: &crate::service::ServiceClient,
    call: RoutedCall<'_>,
    reconstruct: impl FnOnce(&serde_json::Value) -> Result<T>,
    emit: impl FnOnce(T) -> Result<i32>,
) -> Option<Result<i32>> {
    let RoutedCall { spec, after_send } = call;
    let CallSpec {
        tool,
        arguments,
        on_tool_error,
        verbose,
    } = spec;
    // A committing mutation must use `AcceptWithPayload`: under `FallBackDirect`
    // a clean daemon-side refusal (`isError: true`) surfaces as a post-send
    // `Err`, which Commit would misreport as a false "may have applied" alarm
    // instead of rendering the refusal envelope the payload carries.
    debug_assert!(
        !(on_tool_error == crate::service::OnToolError::FallBackDirect
            && after_send == FallbackAfterSend::Commit),
        "a committing routed mutation must use OnToolError::AcceptWithPayload"
    );
    let structured =
        match client.call_tool_structured_phased(canonical, tool, arguments, on_tool_error) {
            Ok(structured) => structured,
            Err(error) => {
                // Send-commit refusal (NRN-228): a committing mutation whose call
                // failed AFTER the request crossed to the daemon must NOT fall
                // back to a Direct re-run — the change may already be applied, so
                // a retry could double-apply. Surface the uncertainty (exit 1).
                // Every other case — any `Fallback` failure, or a PRE-send
                // failure in either policy (the tool never ran) — falls back to
                // Direct exactly like a read, with the byte-identical verbose note.
                // Exhaustive match, NOT `==`: a future policy variant (the
                // NRN-151/CAS retry seam) must fail to compile here rather than
                // silently falling through to an unguarded Direct re-run.
                match after_send {
                    FallbackAfterSend::Commit => {
                        if error.phase == crate::service::CallPhase::PostSend {
                            return Some(Err(post_send_uncertainty_error(tool, error.source)));
                        }
                    }
                    FallbackAfterSend::Fallback => {}
                }
                if error.is_service_stalled() {
                    crate::service::warn_service_stalled();
                } else if verbose {
                    eprintln!(
                        "norn: routed {tool} failed ({}); using direct execution",
                        error.source
                    );
                }
                return None;
            }
        };
    // NRN-218: a daemon-side field-universe gate refusal (an unknown dynamic-field
    // predicate) crosses back in the envelope. Commit to routing — re-emit the
    // byte-identical error the direct gate would (exit 1 at the top level), NOT a
    // fall-back to Direct (which would re-execute the read). Forward any operator
    // notes FIRST so the stderr ordering matches the direct path, where the cache
    // open prints its contention note before the gate error (NRN-215). Ignoring
    // `after_send` here is safe under any policy: the gate is pre-execution
    // validation, so a refusal is proof the daemon did NOT mutate — surfacing
    // the byte-identical refusal (not the uncertainty error) is always correct.
    if let Some(message) = crate::grammar::dynamic_field_refusal_message(&structured) {
        for note in crate::mcp::notes::operator_notes_from_structured(&structured) {
            eprintln!("{note}");
        }
        return Some(Err(anyhow::anyhow!("{message}")));
    }
    let value = match reconstruct(&structured) {
        Ok(value) => value,
        Err(error) => {
            // The call itself SUCCEEDED — the daemon executed the tool — so an
            // unreadable envelope here is post-send uncertainty for a committing
            // mutation: falling back would re-run an already-applied change.
            // Same exhaustive-match rule as the failure branch above.
            match after_send {
                FallbackAfterSend::Commit => {
                    return Some(Err(post_send_uncertainty_error(tool, error)));
                }
                FallbackAfterSend::Fallback => {}
            }
            if verbose {
                eprintln!(
                    "norn: routed {tool} envelope unreadable ({error}); using direct execution"
                );
            }
            return None;
        }
    };
    // Past reconstruction the read is committed to routing. Re-emit any
    // daemon-side operator notes on THIS process's stderr FIRST, byte-for-byte,
    // so a routed read reproduces the note the direct path would have printed
    // (e.g. the write-lock contention note) ahead of any rendered output —
    // matching the direct path, where the note prints during the cache open
    // (NRN-215). Emitting only after reconstruction succeeds means a fall-back
    // to Direct (which re-produces its own note) can never double-print it.
    for note in crate::mcp::notes::operator_notes_from_structured(&structured) {
        eprintln!("{note}");
    }
    // `emit` writes stdout, and any write failure (e.g. a closed pipe) is
    // surfaced as an error the top level maps like any other — broken pipe
    // becomes a clean exit.
    Some(emit(value))
}

/// The explicit error a committing routed mutation surfaces when the daemon call
/// fails AFTER the request was sent (NRN-228). Unlike a read, the seam does NOT
/// fall back to a Direct re-run (which could double-apply a change the daemon may
/// already have made); it names the remedy an operator can act on. Maps to exit
/// 1 at the top level — the "vault may be partially mutated" code
/// (`docs/errors.md`), distinct from a clean pre-flight refusal (exit 2). Typed
/// ([`crate::service::PostSendUncertainError`], code `post-send-uncertain`) so
/// the structured failure envelope recovers a machine-branchable code via
/// `ApplyError::from_anyhow` instead of laundering it to `internal-error`.
#[cfg(unix)]
fn post_send_uncertainty_error(tool: &str, source: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(crate::service::PostSendUncertainError {
        tool: tool.to_string(),
        cause: source,
    })
}

/// The read-routing skeleton behind `count`/`find`/`get` (NRN-222): route the
/// read, and on ANY failure fall back to Direct. A read is idempotent, so both a
/// pre-send and a post-send failure fall back safely and byte-identically —
/// hence the [`FallbackAfterSend::Fallback`] policy.
#[cfg(unix)]
fn route_read<T>(
    cwd: &camino::Utf8Path,
    spec: CallSpec<'_>,
    reconstruct: impl FnOnce(&serde_json::Value) -> Result<T>,
    emit: impl FnOnce(T) -> Result<i32>,
) -> Option<Result<i32>> {
    route_tool_call(
        cwd,
        RoutedCall {
            spec,
            after_send: FallbackAfterSend::Fallback,
        },
        reconstruct,
        emit,
    )
}

/// The mutation sibling of [`route_read`] (NRN-228): route a mutating tool call
/// to the warm daemon under a send-commit fallback policy.
///
/// A routed **dry-run** is a read in mutation clothing — it writes nothing — so
/// it uses [`FallbackAfterSend::Fallback`]: any failure, pre- or post-send,
/// silently falls back to Direct, exactly like a read. A routed **apply** uses
/// [`FallbackAfterSend::Commit`]: a failure BEFORE the tool call is sent
/// (forced-direct flags, probe miss, handshake / version-gate failure) falls
/// back to Direct like a read, but a failure AFTER send (timeout, connection
/// drop, unreadable response) does NOT — the daemon may have applied the change,
/// so the seam surfaces an explicit uncertainty error (exit 1) rather than
/// risking a double-apply.
///
/// Mutation callers pass [`crate::service::OnToolError::AcceptWithPayload`]:
/// the NRN-220 refusal envelopes ride `structuredContent` and flow through
/// `reconstruct` into the CLI's refusal rendering, whereas `FallBackDirect`
/// would turn every clean daemon-side refusal (`isError: true` → a post-send
/// `Err`) into a false "may have applied" alarm under Commit (debug-asserted
/// in `execute_routed_call`).
///
/// `pub` so it is part of the routing seam a future integration test (and the
/// mutation commands wired in NRN-229+) can reach; no production command routes
/// through it yet, so it is exercised today by the in-crate routing tests. Two
/// notes for the NRN-229 wiring:
///
/// - Like the whole seam, this is `cfg(unix)`-only (the daemon rides UDS). A
///   cfg-agnostic caller must add a `#[cfg(not(unix))]` stub returning `None`,
///   mirroring `try_route_read`'s Direct stub — always-Direct is safe there
///   because no daemon can exist to have half-applied anything.
/// - The composed path here (well-known-socket probe + policy selection) is
///   deliberately untested: the probe reads the process-global
///   `XDG_CACHE_HOME`, which in-process tests cannot set without racing other
///   tests (the same rule the service-module tests follow). The in-crate seam
///   tests inject a stub `ServiceClient` into `execute_routed_call` instead;
///   the probe half is covered by the e2e `serve_*_routing` suites and will be
///   for mutations once NRN-229 gives them a routable command.
#[cfg(unix)]
pub fn route_call<T>(
    cwd: &camino::Utf8Path,
    spec: CallSpec<'_>,
    dry_run: bool,
    reconstruct: impl FnOnce(&serde_json::Value) -> Result<T>,
    emit: impl FnOnce(T) -> Result<i32>,
) -> Option<Result<i32>> {
    route_tool_call(
        cwd,
        RoutedCall {
            spec,
            after_send: after_send_for(dry_run),
        },
        reconstruct,
        emit,
    )
}

/// The send-commit policy a routed mutation runs under: a dry-run writes nothing
/// (a read in mutation clothing), so it may fall back after send; an apply
/// commits, so a post-send failure is surfaced, never silently retried.
#[cfg(unix)]
fn after_send_for(dry_run: bool) -> FallbackAfterSend {
    if dry_run {
        FallbackAfterSend::Fallback
    } else {
        FallbackAfterSend::Commit
    }
}

/// Route a `count` to the warm daemon, or `None` to run Direct.
#[cfg(unix)]
fn route_count(
    args: &crate::cli::CountArgs,
    cwd: &camino::Utf8Path,
    verbose: bool,
    dynamic_keys: &[String],
) -> Option<Result<i32>> {
    route_read(
        cwd,
        CallSpec {
            tool: "vault.count",
            arguments: crate::count::route::to_mcp_arguments(args, dynamic_keys),
            on_tool_error: crate::service::OnToolError::FallBackDirect,
            verbose,
        },
        crate::count::route::reconstruct,
        |out| {
            let mut stdout = std::io::stdout().lock();
            crate::count::emit(&out, args.format, &mut stdout)?;
            Ok(0)
        },
    )
}

/// Route a `find` to the warm daemon, or `None` to run Direct.
#[cfg(unix)]
fn route_find(
    args: &crate::cli::FindArgs,
    cwd: &camino::Utf8Path,
    color: crate::cli::ColorWhen,
    verbose: bool,
    dynamic_keys: &[String],
) -> Option<Result<i32>> {
    // The missing-predicate help gate holds on the routed path too: a bare
    // `find` (no predicate, no --all) prints help and exits 2 on the direct
    // path (`find::run`), so it must never dump the vault through the daemon.
    // The SAME whole-gate predicate as the direct path, so the two cannot drift.
    if crate::find::wants_help_instead(args) {
        return None;
    }
    route_read(
        cwd,
        CallSpec {
            tool: "vault.find",
            arguments: crate::find::route::to_mcp_arguments(args, dynamic_keys),
            on_tool_error: crate::service::OnToolError::FallBackDirect,
            verbose,
        },
        |structured| crate::find::route::reconstruct(structured, args),
        |routed| {
            let palette = crate::output::palette::resolve(color);
            crate::find::emit(&routed.result, &routed.deep, args, &palette)?;
            // Direct find exits 2 when the vault carries any error-severity
            // diagnostic (`cache.has_diagnostic_errors()`); the daemon surfaces
            // the same signal in the envelope, so routed and direct exit codes
            // cannot drift (NRN-222).
            Ok(if routed.has_diagnostic_errors { 2 } else { 0 })
        },
    )
}

/// Route a `get` to the warm daemon, or `None` to run Direct.
///
/// `--format markdown` (a byte-faithful single-doc disk read with bespoke
/// multi-doc handling) and `--section` (the wire serializes sections as an
/// alphabetically-keyed object, dropping the request order the `records` renderer
/// needs) are gated to Direct.
#[cfg(unix)]
fn route_get(
    args: &crate::cli::GetArgs,
    cwd: &camino::Utf8Path,
    verbose: bool,
) -> Option<Result<i32>> {
    if matches!(args.format, crate::cli::GetFormat::Markdown) || !args.section.is_empty() {
        return None;
    }
    route_read(
        cwd,
        CallSpec {
            tool: "vault.get",
            arguments: crate::show::route::to_mcp_arguments(args),
            // vault.get flags a not-found target as `isError: true` while still
            // shipping the full structuredContent (NRN-214); accept it so the
            // routed client derives the CLI's exit-1 from the wire notes instead
            // of re-executing the failing read directly.
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        |structured| crate::show::route::reconstruct(structured, args),
        |report| crate::show::emit(&report, args),
    )
}

/// Route a `repair --plan` to the warm daemon, or `None` to run Direct.
///
/// Only `--plan` shapes route: bare `norn repair` (summary mode) has no wire
/// analogue (`vault.repair` only ever returns a `MigrationPlan`), so it
/// returns `None` up front and always runs Direct.
#[cfg(unix)]
fn route_repair(
    args: &crate::cli::RepairArgs,
    cwd: &camino::Utf8Path,
    verbose: bool,
) -> Option<Result<i32>> {
    if !args.plan {
        return None;
    }
    route_read(
        cwd,
        CallSpec {
            tool: "vault.repair",
            arguments: crate::repair::route::to_mcp_arguments(args),
            on_tool_error: crate::service::OnToolError::FallBackDirect,
            verbose,
        },
        crate::repair::route::reconstruct,
        |routed| {
            // `--out` writes and stdout rendering run through the SAME
            // `emit_plan` the direct path calls, so a routed `--out` write is
            // byte-identical to the direct `fs::write`, and the exit code
            // (1 if the vault carries any error-severity diagnostic) matches
            // `crate::exit_code_for(&index)` on the direct path (NRN-231).
            crate::repair::emit_plan(&routed.plan, args, cwd, routed.has_diagnostic_errors)
        },
    )
}

/// Attempt to route a `norn set` to the warm daemon (NRN-229), or return `None`
/// to run the direct path.
///
/// **Routing runs BEFORE the direct arm's local mutation lock.** The daemon
/// acquires the SAME per-vault lock in-process, so a CLI that held the lock while
/// routing would deadlock a routed apply. Only the direct fallback performs
/// today's sweep + lock + execute sequence.
///
/// **Mode mapping** (settled NRN-229 decisions):
/// - `--dry-run` → routed dry-run (`confirm: false`).
/// - `--yes` (non-interactive apply) → routed apply (`confirm: true`).
/// - `--format json` without `--yes` → routed preview (implicit dry-run,
///   `confirm: false`).
/// - interactive TTY without `--yes` → NOT routed: the preview→prompt→apply flow
///   stays direct (the daemon cannot drive the terminal prompt).
/// - non-TTY without `--yes` (and not `--format json`) → routed dry-run,
///   preserving today's "implicit dry-run preview, no prompt" semantics.
///
/// **Gated to Direct** (no byte-faithful wire encoding, see `set::route`):
/// `--body-from-stdin` (no wire-faithful stdin analogue). `--field-json`,
/// `--push`, and `--pop` now route (NRN-238): `vault.set`'s params carry them
/// as ORDERED `Vec<String>` token lists (not sorted maps), so passing the
/// CLI's `Vec`s through verbatim preserves order and duplicate-key-accumulate
/// semantics exactly, the same way `vault.new`'s `field`/`field_json` already
/// do. `--config` / `--no-cache-refresh` force Direct up front via the SAME
/// [`routing_forced_direct`] guard the read seam uses (the daemon loads each
/// vault's own default config and always serves a refreshed warm cache, so
/// routing either flag would silently ignore it).
///
/// A routed apply runs under the seam's `Commit` policy: a post-send failure
/// surfaces `post-send-uncertain` (exit 1) rather than a double-applying Direct
/// retry. `AcceptWithPayload` lets a coded refusal (NRN-220/221) cross as
/// `isError: true` carrying the structured report, which `reconstruct` renders as
/// the byte-identical exit-2 refusal.
#[cfg(unix)]
fn try_route_set(
    args: &crate::cli::SetArgs,
    combined_fields: &[String],
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    use std::io::IsTerminal as _;

    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    // The only shape with no byte-faithful wire encoding: an MCP server has no
    // stdin. `--field-json` / `--push` / `--pop` route (NRN-238) — see
    // `set::route::to_mcp_arguments`.
    if args.body_from_stdin {
        return None;
    }

    // Decide the effective mode, mirroring the direct arm's `should_apply` ladder.
    let confirm = if args.dry_run {
        false
    } else if args.yes {
        true
    } else if matches!(args.format, crate::cli::SetFormat::Json) {
        false
    } else if std::io::stdin().is_terminal() {
        // Interactive preview→prompt→apply stays Direct.
        return None;
    } else {
        // Non-TTY without --yes: implicit dry-run preview.
        false
    };

    let format = args.format;
    route_call(
        cwd,
        CallSpec {
            tool: "vault.set",
            arguments: crate::set::route::to_mcp_arguments(args, combined_fields, confirm),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        /*dry_run=*/ !confirm,
        crate::set::route::reconstruct,
        move |report| crate::set::route::emit(report, format),
    )
}

/// Non-Unix build: the warm daemon rides Unix-domain sockets, so `set` always
/// runs Direct. Always-Direct is safe here — no daemon can exist to half-apply a
/// mutation (mirrors `try_route_read`'s Direct stub).
#[cfg(not(unix))]
fn try_route_set(
    args: &crate::cli::SetArgs,
    combined_fields: &[String],
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (
        args,
        combined_fields,
        cwd,
        explicit_config,
        no_cache_refresh,
        verbose,
    );
    None
}

/// Attempt to route a `norn new` to the warm daemon (NRN-230 PR C), or return
/// `None` to run the direct path. Copies `try_route_set`'s shape.
///
/// **Routing runs BEFORE the direct arm's local mutation lock**, same reason as
/// `set`: the daemon acquires the SAME per-vault lock in-process, so a CLI that
/// held the lock while routing would deadlock a routed apply.
///
/// `vars` is the `--var` map already parsed CLI-side by `parse_var_args` BEFORE
/// this is called — a malformed `--var` refuses pre-send in the arm, so the wire
/// only ever carries a valid `vars` map.
///
/// **Mode mapping** (mirrors `new::preflight_and_plan`'s dry-run/apply ladder,
/// which keys TTY off STDOUT — not stdin like `set`):
/// - `--dry-run` → routed dry-run (`confirm: false`).
/// - `--yes` (non-interactive apply) → routed apply (`confirm: true`).
/// - `--format json` without `--yes` → routed preview (implicit dry-run).
/// - interactive TTY (stdout) without `--yes` → NOT routed: the preview→prompt→
///   apply flow stays direct (the daemon cannot drive the terminal prompt).
/// - non-TTY without `--yes` (and not `--format json`) → routed dry-run,
///   preserving today's "implicit dry-run preview, no prompt" semantics.
///
/// **Gated to Direct**: `--body-from-stdin` (no byte-faithful stdin analogue).
/// `--config` / `--no-cache-refresh` force Direct up front via the SAME
/// [`routing_forced_direct`] guard the read seam uses. Both `--field` AND
/// `--field-json` route (unlike `set`): `vault.new`'s params carry them as
/// ordered `Vec<String>` token lists, so the wire preserves last-wins exactly.
///
/// A routed apply runs under the seam's `Commit` policy: a post-send failure
/// surfaces `post-send-uncertain` (exit 1) rather than a double-applying Direct
/// retry. `AcceptWithPayload` lets a coded refusal cross as `isError: true`
/// carrying the structured report, which `reconstruct` renders as the
/// byte-identical exit-2 refusal (`error:` prose on stderr, both formats — `new`
/// has no JSON error envelope, audit F5).
#[cfg(unix)]
fn try_route_new(
    args: &crate::cli::NewArgs,
    vars: &std::collections::BTreeMap<String, String>,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    use std::io::IsTerminal as _;

    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    // The one shape with no byte-faithful wire encoding runs Direct.
    if args.body_from_stdin {
        return None;
    }

    // Decide the effective mode, mirroring the direct arm's decision tree in
    // `new::preflight_and_plan` — which detects a terminal on STDOUT (new shows
    // its interactive preview on stdout), not stdin like `set`.
    let confirm = if args.dry_run {
        false
    } else if args.yes {
        true
    } else if matches!(args.format, crate::cli::NewFormat::Json) {
        false
    } else if std::io::stdout().is_terminal() {
        // Interactive preview→prompt→apply stays Direct.
        return None;
    } else {
        // Non-TTY without --yes: implicit dry-run preview.
        false
    };

    let format = args.format;
    route_call(
        cwd,
        CallSpec {
            tool: "vault.new",
            arguments: crate::new::route::to_mcp_arguments(args, vars, confirm),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        /*dry_run=*/ !confirm,
        crate::new::route::reconstruct,
        move |report| crate::new::route::emit(report, format),
    )
}

/// Non-Unix build: the warm daemon rides Unix-domain sockets, so `new` always
/// runs Direct. Always-Direct is safe here — no daemon can exist to half-apply a
/// mutation (mirrors `try_route_read`'s Direct stub).
#[cfg(not(unix))]
fn try_route_new(
    args: &crate::cli::NewArgs,
    vars: &std::collections::BTreeMap<String, String>,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (args, vars, cwd, explicit_config, no_cache_refresh, verbose);
    None
}

/// Attempt to route a `norn edit` to the warm daemon (NRN-229 PR A), or return
/// `None` to run the direct path. Copies `try_route_set`'s shape exactly.
///
/// **Routing runs BEFORE the direct arm's local mutation lock**, same reason as
/// `set`: the daemon acquires the SAME per-vault lock in-process.
///
/// `ops` is whatever the direct arm already resolved (sugar-desugared, or
/// parsed from `--edits-json`/`--ops-file`/stdin) BEFORE this is called — see
/// `edit::route`'s module doc for why the ops source is never a gating reason
/// (unlike `set`'s `--body-from-stdin`).
///
/// **Mode mapping** (identical ladder to `set`, NRN-229 decisions):
/// - `--dry-run` → routed dry-run (`confirm: false`).
/// - `--yes` (non-interactive apply) → routed apply (`confirm: true`).
/// - `--format json` without `--yes` → routed preview (implicit dry-run,
///   `confirm: false`).
/// - interactive TTY without `--yes` → NOT routed: the preview→prompt→apply
///   flow stays direct (the daemon cannot drive the terminal prompt).
/// - non-TTY without `--yes` (and not `--format json`) → routed dry-run,
///   preserving today's "implicit dry-run preview, no prompt" semantics.
///
/// **Gated to Direct:** no edit-specific flag today — see `edit::route`'s
/// module doc. `--config` / `--no-cache-refresh` force Direct up front via the
/// SAME [`routing_forced_direct`] guard the read seam and `try_route_set` use
/// (the daemon loads each vault's own default config and always serves a
/// refreshed warm cache, so routing either flag would silently ignore it).
///
/// A routed apply runs under the seam's `Commit` policy, same as `set`: a
/// post-send failure surfaces `post-send-uncertain` (exit 1) rather than a
/// double-applying Direct retry. `AcceptWithPayload` lets a coded refusal
/// (NRN-220) cross as `isError: true` carrying the structured report, which
/// `reconstruct` renders as the (format-independent, see `edit::route::emit`)
/// exit-2 refusal.
#[cfg(unix)]
fn try_route_edit(
    args: &crate::cli::EditArgs,
    ops: &[crate::edit::ops::EditOp],
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    use std::io::IsTerminal as _;

    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    // Decide the effective mode, mirroring the direct arm's `should_apply`
    // ladder (and `try_route_set`'s identical shape).
    let confirm = if args.dry_run {
        false
    } else if args.yes {
        true
    } else if matches!(args.format, crate::cli::EditFormat::Json) {
        false
    } else if std::io::stdin().is_terminal() {
        // Interactive preview→prompt→apply stays Direct.
        return None;
    } else {
        // Non-TTY without --yes: implicit dry-run preview.
        false
    };

    let format = args.format;
    route_call(
        cwd,
        CallSpec {
            tool: "vault.edit",
            arguments: crate::edit::route::to_mcp_arguments(args, ops, confirm),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        /*dry_run=*/ !confirm,
        crate::edit::route::reconstruct,
        move |report| crate::edit::route::emit(report, format),
    )
}

/// Non-Unix build: the warm daemon rides Unix-domain sockets, so `edit` always
/// runs Direct. Always-Direct is safe here — no daemon can exist to half-apply a
/// mutation (mirrors `try_route_set`'s Direct stub).
#[cfg(not(unix))]
fn try_route_edit(
    args: &crate::cli::EditArgs,
    ops: &[crate::edit::ops::EditOp],
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (args, ops, cwd, explicit_config, no_cache_refresh, verbose);
    None
}

/// The dry-run/apply confirm switch, mirroring `set`/`edit`'s ladder and the
/// `resolve_{move,delete}_dry_run` helpers EXACTLY: `--dry-run` → dry-run,
/// `--yes` → apply, `--format json` (without `--yes`) → implicit dry-run,
/// interactive TTY → `None` (stays Direct: the daemon cannot drive the prompt),
/// non-TTY without `--yes` → implicit dry-run. Returns `Some(confirm)` or `None`
/// to force the interactive path Direct.
#[cfg(unix)]
fn routed_confirm(dry_run: bool, yes: bool, format_is_json: bool) -> Option<bool> {
    use std::io::IsTerminal as _;
    if dry_run {
        Some(false)
    } else if yes {
        Some(true)
    } else if format_is_json {
        Some(false)
    } else if std::io::stdin().is_terminal() {
        None
    } else {
        Some(false)
    }
}

/// Attempt to route a `norn move` to the warm daemon (NRN-229 PR B), or return
/// `None` to run the direct path. Copies `try_route_set`'s shape.
///
/// **No on-disk source guard.** Both surfaces now plan the index-RESOLVED
/// source (NRN-239: `vault.move`'s handler runs the same stem-resolving
/// preflight the CLI direct arm does and builds its `MigrationPlan` from the
/// RESOLVED path, not the raw argument), so raw-vs-resolved divergence is
/// structurally impossible — the `rewrite_wikilink` model. A bare stem, an
/// exact path, and a stem shadowed by a non-doc file on disk (e.g. `foo`
/// beside `foo.md`) all route and resolve identically to Direct. An
/// unresolvable or ambiguous source still refuses (a coded `target-not-found`
/// / `target-ambiguous`), reconstructed byte-identically on the wire. See
/// `move_doc::route`.
#[cfg(unix)]
fn try_route_move(
    args: &crate::cli::MoveArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    // Folder-move detection (unrelated to routing eligibility): explicit `-r`,
    // or the raw source resolves to a directory on disk. This only selects the
    // renderer (`render_folder_apply_tty` vs `render_move_apply_tty`) and the
    // folder-op field shape — folder moves apply the raw src/dst on both
    // surfaces (a directory path isn't stem-resolved), matching the CLI.
    let src_full = cwd.join(&args.src);
    let is_folder = args.recursive || src_full.as_std_path().is_dir();

    let confirm = routed_confirm(
        args.dry_run,
        args.yes,
        matches!(args.format, crate::cli::MoveFormat::Json),
    )?;

    let format = args.format;
    let src = args.src.clone();
    let dst = args.dst.clone();
    let dry_run = !confirm;
    route_call(
        cwd,
        CallSpec {
            tool: "vault.move",
            arguments: crate::move_doc::route::to_mcp_arguments(args, confirm),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        dry_run,
        crate::apply_report::reconstruct_wire_report,
        move |report| crate::move_doc::route::emit(report, format, &src, &dst, is_folder, dry_run),
    )
}

/// Non-Unix build: `move` always runs Direct (the daemon rides Unix sockets).
#[cfg(not(unix))]
fn try_route_move(
    args: &crate::cli::MoveArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (args, cwd, explicit_config, no_cache_refresh, verbose);
    None
}

/// Attempt to route a `norn delete` to the warm daemon (NRN-229 PR B), or return
/// `None` to run the direct path.
///
/// **No on-disk target guard.** Both surfaces now plan the index-RESOLVED
/// target (NRN-239: `vault.delete`'s handler runs the same stem-resolving
/// preflight the CLI direct arm does and builds its `MigrationPlan` from the
/// RESOLVED path, not the raw argument), so raw-vs-resolved divergence is
/// structurally impossible — the `rewrite_wikilink` model. A bare stem, an
/// exact path, and a stem shadowed by a non-doc file on disk (e.g. `foo`
/// beside `foo.md`) all route and resolve identically to Direct. An
/// unresolvable or ambiguous target still refuses (a coded `target-not-found`
/// / `target-ambiguous`), reconstructed byte-identically on the wire.
/// `--rewrite-to` needed no such guard even before NRN-239: BOTH surfaces put
/// the RAW `rewrite_to` into the plan fields (the CLI arm deliberately does not
/// use the preflight-resolved value there) and run the same preflight against
/// it, so raw-vs-resolved cannot diverge for it.
///
/// `--format records` WITH `--rewrite-to` / `--allow-broken-links` now routes
/// (NRN-237): the applier attaches the records renderer's index-derived
/// incoming-link data to the `delete_document` op as `link_impact`, so it rides
/// the wire `ApplyReport` and the routed path renders byte-identically. See
/// `delete_doc::route`.
#[cfg(unix)]
fn try_route_delete(
    args: &crate::cli::DeleteArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    let confirm = routed_confirm(
        args.dry_run,
        args.yes,
        matches!(args.format, crate::cli::DeleteFormat::Json),
    )?;

    let format = args.format;
    let doc = args.doc.clone();
    let dry_run = !confirm;
    route_call(
        cwd,
        CallSpec {
            tool: "vault.delete",
            arguments: crate::delete_doc::route::to_mcp_arguments(args, confirm),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        dry_run,
        crate::apply_report::reconstruct_wire_report,
        move |report| crate::delete_doc::route::emit(report, format, &doc, dry_run),
    )
}

/// Non-Unix build: `delete` always runs Direct.
#[cfg(not(unix))]
fn try_route_delete(
    args: &crate::cli::DeleteArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (args, cwd, explicit_config, no_cache_refresh, verbose);
    None
}

/// Attempt to route a `norn rewrite-wikilink` to the warm daemon (NRN-229 PR B),
/// or return `None` to run the direct path.
///
/// The cleanest cascade command to route: BOTH surfaces apply the raw
/// `{old, new}` (no stem-resolution divergence), so no on-disk guard is needed —
/// every input routes, including an unresolvable OLD (a `target-not-found` coded
/// refusal). `--out` is reproduced client-side in `rewrite_wikilink_cmd::route::emit`.
#[cfg(unix)]
fn try_route_rewrite_wikilink(
    args: &crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    let confirm = routed_confirm(
        args.dry_run,
        args.yes,
        matches!(args.format, crate::cli::RewriteWikilinkFormat::Json),
    )?;

    let arguments = crate::rewrite_wikilink_cmd::route::to_mcp_arguments(args, confirm);
    let dry_run = !confirm;
    // The emit closure needs the run args (old/new/format/out); clone the small
    // owned fields it reads into a captured copy.
    let emit_args = crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs {
        old: args.old.clone(),
        new: args.new.clone(),
        dry_run: args.dry_run,
        yes: args.yes,
        format: args.format,
        out: args.out.clone(),
    };
    route_call(
        cwd,
        CallSpec {
            tool: "vault.rewrite_wikilink",
            arguments,
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        dry_run,
        crate::apply_report::reconstruct_wire_report,
        move |report| crate::rewrite_wikilink_cmd::route::emit(report, &emit_args),
    )
}

/// Non-Unix build: `rewrite-wikilink` always runs Direct.
#[cfg(not(unix))]
fn try_route_rewrite_wikilink(
    args: &crate::rewrite_wikilink_cmd::RewriteWikilinkRunArgs,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (args, cwd, explicit_config, no_cache_refresh, verbose);
    None
}

/// Attempt to route a `norn apply` to the warm daemon (NRN-231), or return `None`
/// to run the direct path.
///
/// **The plan crosses as the PARSED value, never a path.** The caller has already
/// run [`crate::apply_cmd::preamble`] (read + parse + schema-check, byte-identical
/// to Direct — a schema mismatch refuses exit 2 BEFORE this is reached), so the
/// wire only ever carries a valid, correctly-versioned [`MigrationPlan`],
/// re-serialized into the `vault.apply` `plan` argument. YAML input routes the
/// same way — the daemon applies the identical struct. `raw` (the plan's RAW
/// bytes) and `state_dir` (already resolved + swept before routing) are threaded
/// in for the CLI-owned lock-timeout stash (see `apply_cmd::route::emit`).
///
/// **Routing runs BEFORE the direct arm's local mutation lock** (same reason as
/// `set`/`delete`): the daemon acquires the SAME per-vault lock in-process, so a
/// CLI holding it while routing would deadlock the routed apply.
///
/// **Mode mapping** (mirrors the direct arm's ladder via [`routed_confirm`]):
/// `--dry-run` → routed dry-run (`confirm: false`, `Fallback`); `--yes` → routed
/// apply (`confirm: true`, `Commit`: a post-send failure surfaces
/// `post-send-uncertain`, never a double-applying retry); `--format json` without
/// `--yes` → routed implicit dry-run; interactive TTY without `--yes` → NOT routed
/// (the preview→prompt→apply conversation holds the lock across the prompt);
/// non-TTY without `--yes` → routed implicit dry-run. `--config` /
/// `--no-cache-refresh` force Direct via the shared [`routing_forced_direct`]
/// guard.
///
/// `AcceptWithPayload` lets a coded refusal (NRN-220 — including the daemon's
/// `mutation-lock-timeout`) cross as `isError: true` carrying the structured
/// report, which `reconstruct_wire_report` rebuilds and `emit` renders as the
/// byte-identical exit-2 refusal (or, for the lock-timeout code, the CLI-owned
/// stash branch).
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn try_route_apply(
    args: &crate::apply_cmd::ApplyRunArgs,
    plan: &crate::migration_plan::MigrationPlan,
    raw: &str,
    state_dir: &camino::Utf8Path,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    if routing_forced_direct(explicit_config, no_cache_refresh) {
        return None;
    }

    let confirm = routed_confirm(
        args.dry_run,
        args.yes,
        matches!(args.format, crate::cli::ApplyFormat::Json),
    )?;
    let dry_run = !confirm;

    // The emit closure runs after the wire round-trip, so it needs owned copies of
    // the small fields it reads (format is Copy; the rest are cloned).
    let format = args.format;
    let out = args.out.clone();
    let plan_path = args.plan_path.clone();
    let raw = raw.to_string();
    let state_dir = state_dir.to_owned();
    route_call(
        cwd,
        CallSpec {
            tool: "vault.apply",
            arguments: crate::apply_cmd::route::to_mcp_arguments(plan, confirm, args.parents),
            on_tool_error: crate::service::OnToolError::AcceptWithPayload,
            verbose,
        },
        dry_run,
        crate::apply_report::reconstruct_wire_report,
        move |report| {
            crate::apply_cmd::route::emit(
                report,
                format,
                out.as_deref(),
                &plan_path,
                &raw,
                &state_dir,
            )
        },
    )
}

/// Non-Unix build: `apply` always runs Direct (the daemon rides Unix sockets).
#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
fn try_route_apply(
    args: &crate::apply_cmd::ApplyRunArgs,
    plan: &crate::migration_plan::MigrationPlan,
    raw: &str,
    state_dir: &camino::Utf8Path,
    cwd: &camino::Utf8Path,
    explicit_config: bool,
    no_cache_refresh: bool,
    verbose: bool,
) -> Option<Result<i32>> {
    let _ = (
        args,
        plan,
        raw,
        state_dir,
        cwd,
        explicit_config,
        no_cache_refresh,
        verbose,
    );
    None
}

fn run(cli: Cli, dynamic_keys: &[String]) -> Result<i32> {
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
        // The launchd supervisor targets no vault and opens no cache — like
        // self-update it is fully self-contained, so handle it before cwd/config
        // resolution and the cache-opening dispatch.
        Command::Service(cmd) => return crate::service::command::run(&cmd),
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

    // The warm host daemon owns its own tokio runtime and opens vault contexts
    // per-connection, so — like `mcp` — it is pre-handled here, before the
    // cache-opening arms and the routing seam. It ignores `--cwd` for data
    // (vaults arrive per connection) but refuses an explicit `--config`:
    // warm contexts always load each vault's default config, so honoring a
    // single CLI-level `--config` would be misleading. Exit 2 = bad invocation.
    if let Command::Serve(_) = &command {
        if config_path.is_some() {
            eprintln!(
                "norn serve: --config is not supported (each vault loads its own default .norn/config.yaml)"
            );
            return Ok(2);
        }
        crate::serve::run()?;
        return Ok(0);
    }

    // The explicit `cache prune` manages the sweep itself (and a --dry-run
    // must not be followed by a real sweep in the same invocation), so the
    // tail-hook lazy sweep is skipped for it.
    let is_explicit_prune = matches!(
        &command,
        Command::Cache(c) if matches!(c.command, CacheSubcommand::Prune(_))
    );

    // NRN-92 routing seam: for a routable read command, decide whether a warm
    // `norn-service` daemon is live for this vault and should serve the request
    // from an already-verified cache. When it returns `Some`, the request was
    // served by routing; otherwise we fall through to the direct, integrity-
    // verified dispatch below (today's behavior). No daemon => only a `stat`.
    // NRN-218: a dynamic-predicate invocation now routes too — the desugared
    // `dynamic_keys` cross the wire so the daemon runs the field-universe gate
    // (NRN-207) against its warm cache and returns a byte-identical refusal for an
    // unknown field, instead of the old force-Direct fall-back.
    if let Some(result) = try_route_read(
        &command,
        &cwd,
        config_path.is_some(),
        no_cache_refresh,
        color,
        verbose,
        dynamic_keys,
    ) {
        return result;
    }

    let outcome = match command {
        Command::Apply(args) => {
            let run_args = ApplyRunArgs {
                plan_path: args.plan_path,
                dry_run: args.dry_run,
                yes: args.yes,
                format: args.format,
                input_format: args.input_format,
                parents: args.parents,
                out: args.out,
            };

            // Shared preamble (read + parse + schema-check) runs ONCE, before the
            // routing decision and before any lock — stdin can only be consumed
            // once, so both the routing seam and the direct tail reuse its `raw`
            // bytes + parsed plan. A schema mismatch already refused (exit 2, prose
            // on stderr) byte-identically to Direct, BEFORE any wire activity; a
            // parse failure propagates as the `Err` arm (outcome → lazy_sweep runs
            // exactly as today).
            match apply_cmd::preamble(&run_args) {
                Err(e) => Err(e),
                Ok(apply_cmd::Preamble::Refused(code)) => Ok(code),
                Ok(apply_cmd::Preamble::Ready { raw, plan }) => {
                    // Resolve the state dir and sweep stale pending plans BEFORE
                    // routing (design: the sweep runs exactly as it did before
                    // Direct); the state dir also feeds the CLI-owned lock-timeout
                    // stash on a routed apply.
                    let (_, state_dir) = crate::cache::state_dir_for(&cwd)
                        .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
                    crate::mutation_lock::pending::sweep_pending(&state_dir);

                    // NRN-231 routing seam: serve from a warm daemon BEFORE the
                    // direct tail takes the local mutation lock (the daemon takes
                    // the SAME per-vault lock in-process; holding it here would
                    // deadlock a routed apply). `None` => no daemon, forced-Direct
                    // flags, or the interactive TTY path => the direct tail below.
                    if let Some(result) = try_route_apply(
                        &run_args,
                        &plan,
                        &raw,
                        &state_dir,
                        &cwd,
                        config_path.is_some(),
                        no_cache_refresh,
                        verbose,
                    ) {
                        return result;
                    }

                    apply_cmd::run_direct(
                        &run_args,
                        plan,
                        &raw,
                        &state_dir,
                        &cwd,
                        no_cache_refresh,
                        config_path.as_ref(),
                        verbose,
                    )
                }
            }
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
            // NRN-229 routing seam: serve from a warm daemon BEFORE `run` takes the
            // local mutation lock. `None` => no daemon / forced-Direct flags / the
            // interactive path => fall through to the direct `run`, unchanged.
            if let Some(result) = try_route_rewrite_wikilink(
                &run_args,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }
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
            match &cache_command.command {
                CacheSubcommand::Index(args) => {
                    crate::cache_cmd::run_index(&cwd, &loaded_config.index_options, args)?
                }
                CacheSubcommand::Rebuild => {
                    crate::cache_cmd::run_rebuild(&cwd, &loaded_config.index_options)?
                }
                CacheSubcommand::Clear => crate::cache_cmd::run_clear(&cwd)?,
                CacheSubcommand::Status(args) => {
                    crate::cache_cmd::run_status(&cwd, &loaded_config.index_options, args)?
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
                &loaded_config.index_options,
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
                crate::output::projection::warn_section_ignored(
                    &args.section,
                    Some("markdown"),
                    &mut stderr_lock,
                )?;
                for note in &report.notes {
                    eprintln!("{}", note);
                }
                return match report.records.len() {
                    1 => {
                        let path = &report.records[0].path;
                        match std::fs::read_to_string(cache.vault_root.join(path)) {
                            // Byte-faithful: print verbatim, no trailing-newline fixup.
                            Ok(raw) => {
                                print!("{}", raw);
                                Ok(0)
                            }
                            Err(_) => {
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
                             — request one target at a time"
                        );
                        Ok(1)
                    }
                };
            }

            // Shared print seam with the daemon-routed path (`route_get`), so
            // routed and direct `get` cannot drift on rendering, warnings, note
            // forwarding, or the exit-1 signal.
            show::emit(&report, &args)
        }
        Command::Find(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            find::run(
                args,
                &cwd,
                &loaded_config,
                no_cache_refresh,
                color,
                dynamic_keys,
            )
        }
        Command::Count(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            gate_dynamic_query(
                &cache,
                &loaded_config,
                dynamic_keys,
                crate::grammar::QueryCmd::Count,
            )?;
            let out = count::run(&cache, &args)?;
            // Shared with the NRN-94 routed path (`route_count`) so routed and
            // direct `count` cannot drift on rendering or trailing-newline framing.
            let mut stdout = std::io::stdout().lock();
            count::emit(&out, args.format, &mut stdout)?;
            Ok(0)
        }
        Command::Describe(args) => {
            let loaded_config = load_config(&cwd, config_path.as_ref())?;
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            gate_dynamic_query(
                &cache,
                &loaded_config,
                dynamic_keys,
                crate::grammar::QueryCmd::Describe,
            )?;
            // Normalize `--by` ONCE up front so the want_data gate and the
            // DataOptions.by mode-selection agree (shared with MCP via
            // `normalize_by`) — a blank/whitespace-only `--by` must not gate
            // data on differently from MCP.
            let by = crate::describe::data::normalize_by(&args.by);
            let want_data = args.data || args.stats || !by.is_empty();
            let data = want_data.then(|| crate::describe::data::DataOptions {
                by,
                limit: args.limit.unwrap_or(20),
                ..Default::default()
            });
            let out = crate::describe::describe(&cache, &loaded_config, &args.filters, data)?;
            let format = args.format.unwrap_or(crate::cli::DescribeFormat::Text);
            let text = match format {
                crate::cli::DescribeFormat::Json => crate::describe::render::render_json(&out),
                crate::cli::DescribeFormat::Text => crate::describe::render::render_text(&out),
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

            // NRN-229 routing seam: serve the mutation from a warm `norn serve`
            // daemon BEFORE acquiring the local mutation lock (the daemon takes the
            // SAME per-vault lock in-process; holding it here would deadlock a
            // routed apply). `None` => no daemon, a gated shape (forced-Direct
            // flags, a stem source, the interactive path) => fall through to the
            // direct sweep + lock + execute below, unchanged.
            if let Some(result) = try_route_move(
                &args,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }

            // Acquire mutation lock before cache load.
            // Note: for move, --format json is an implicit DRY-RUN (unlike apply),
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

            // Pre-flight (single-file only): validate src/dst before building
            // the MigrationPlan so we can exit 2 on refusal. The cascade counts
            // for TTY rendering are read from the report after apply, not here.
            //
            // NRN-216: capture the RESOLVED plan (mirrors the NRN-57 delete fix).
            // `preflight_and_plan` resolves a bare stem (e.g. "b") to its full
            // vault-relative path (e.g. "b.md") via `resolve_src`; the raw CLI
            // args may not match a real filesystem path at all. Building
            // `MigrationOp.fields` from `args.src`/`args.dst` verbatim (the old
            // behavior) let `--dry-run` look fine (dry-run never touches the
            // filesystem) while `--yes` failed with "move source missing in
            // filesystem: <stem>" as soon as the applier tried to rename a
            // literal file named after the stem.
            //
            // NRN-234: `--parents` no longer pre-creates the destination parent
            // here — that was an unaudited filesystem side effect that ran even
            // on `--dry-run`. `preflight_and_plan` skips its missing-parent
            // refusal when `args.parents` is set (deferring to the applier),
            // and the containment gate (`ensure_within_vault`, which already
            // handles a not-yet-existing `-p`/`--parents` subtree) still runs
            // inside the apply orchestrator before any write, on dry-run too.
            let move_plan = if !is_folder {
                let cfg = crate::move_doc::PreflightConfig {
                    src: &args.src,
                    dst: &args.dst,
                    force: args.force,
                    no_link_rewrite: args.no_link_rewrite,
                    parents: args.parents,
                    vault_root: &cwd,
                    index: &index,
                };
                match crate::move_doc::preflight_and_plan(cfg) {
                    Ok(plan) => Some(plan),
                    Err(e) => {
                        // NRN-229: structured envelope on stdout for `--format
                        // json` (matching the applier-refusal arm below and the
                        // `set` arm) — a preflight refusal carries its stable
                        // `code`. Records/TTY stays byte-identical: prose on
                        // stderr, exit 2.
                        let e = anyhow::Error::new(e);
                        match args.format {
                            crate::cli::MoveFormat::Json => render_json_error_envelope(&e)?,
                            crate::cli::MoveFormat::Records => eprintln!("error: {e}"),
                        }
                        return Ok(2);
                    }
                }
            } else {
                None
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
            // NRN-216: use the RESOLVED src/dst from the preflight plan when
            // available (single-file moves); folder moves have no preflight
            // plan and use the raw args as before (a folder path isn't
            // stem-resolved).
            let (resolved_src, resolved_dst) = if let Some(plan) = &move_plan {
                let move_change = plan.expect_change("move_document");
                (
                    move_change.path.to_string(),
                    move_change
                        .destination
                        .as_ref()
                        .expect("move_document op must carry a destination")
                        .to_string(),
                )
            } else {
                (args.src.clone(), args.dst.clone())
            };
            let mut fields = serde_json::json!({
                "src": resolved_src,
                "dst": resolved_dst,
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
                refuse_as_report: false,
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
                    // NRN-150: structured envelope on stdout for `--format json`;
                    // prose on stderr otherwise. Preflight refusal → exit 2.
                    match args.format {
                        crate::cli::MoveFormat::Json => render_json_error_envelope(&e)?,
                        crate::cli::MoveFormat::Records => eprintln!("error: {e}"),
                    }
                    return Ok(2);
                }
            };

            // NRN-150/183: the exit code is the report's own outcome mapping — a
            // partial-apply failure (a write landed, then an op failed) is now
            // returned as `Ok(report)` with `outcome = failed` → exit 1, not the
            // hardcoded exit 2 of a preflight refusal. A byte-identical refusal
            // still arrives on the `Err` arm above (exit 2); success → exit 0.
            let exit = report.exit_code();

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

            // NRN-229 routing seam (same contract as the `move` arm): serve from a
            // warm daemon BEFORE the local mutation lock. `None` => no daemon, a
            // gated shape (forced-Direct flags, a stem target, or a records delete
            // with --rewrite-to / --allow-broken-links), or the interactive path.
            if let Some(result) = try_route_delete(
                &args,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }

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
                    // NRN-229: structured envelope on stdout for `--format json`
                    // (matching the `set` arm) — a preflight refusal carries its
                    // stable `code`. Records/TTY stays byte-identical: prose on
                    // stderr, exit 2.
                    let e = anyhow::Error::new(e);
                    match args.format {
                        crate::cli::DeleteFormat::Json => render_json_error_envelope(&e)?,
                        crate::cli::DeleteFormat::Records => eprintln!("error: {e}"),
                    }
                    return Ok(2);
                }
            };

            // The resolved delete target path feeds the plan below. Its
            // incoming-link data (counts / files / redirect target) is no longer
            // computed here: the applier attaches it to the delete_document op as
            // `link_impact` (NRN-237), so the direct and warm-daemon records paths
            // render byte-identically from the one report.
            let delete_op = outcome.plan.expect_change("delete_document");

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
                        // NRN-57: use the RESOLVED path (stem or exact path
                        // both land here via delete_doc::preflight_and_plan),
                        // not the raw CLI arg — the raw arg may be a bare stem
                        // that isn't in the index verbatim, which previously
                        // caused every stem-addressed delete to fail with a
                        // misleading "not in the index" error.
                        "path": delete_op.path,
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
                refuse_as_report: false,
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
                    // NRN-150: structured envelope on stdout for `--format json`;
                    // prose on stderr otherwise. Preflight refusal → exit 2.
                    match args.format {
                        crate::cli::DeleteFormat::Json => render_json_error_envelope(&e)?,
                        crate::cli::DeleteFormat::Records => eprintln!("error: {e}"),
                    }
                    return Ok(2);
                }
            };

            // NRN-150/183: the exit code is the report's own outcome mapping — a
            // partial-apply failure (a write landed, then an op failed) is now
            // returned as `Ok(report)` with `outcome = failed` → exit 1, not the
            // hardcoded exit 2 of a preflight refusal. A byte-identical refusal
            // still arrives on the `Err` arm above (exit 2); success → exit 0.
            let exit = report.exit_code();

            emit_invocation_finished(&mut sink, "delete", exit, &report);

            emit_cascade_failure_warnings(&report);

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
                    // The renderer reads the delete op's index-derived `link_impact`
                    // (NRN-237) and apply-time `cascade` straight off the report —
                    // the same report the routed path reconstructs, so both match.
                    crate::delete_doc::render_delete_records(
                        &mut out, &report, &args.doc, dry_run,
                    )?;
                }
            }

            Ok(exit)
        }
        Command::Set(args) => {
            use crate::cache::CacheError;
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;
            use std::io::{IsTerminal, Write};

            // F5: validate the trailing `KEY=VALUE` positional shape BEFORE the
            // mutation lock, cache load, OR routing. A pure argv error (`set doc.md
            // badtoken` with no separator) must fail fast without side effects,
            // matching the edit path which validates arg shape before the lock. The
            // combined `--field` list (explicit `--field` + desugared positionals)
            // also feeds the NRN-229 routing translation below.
            let combined_fields =
                match crate::set::synth::desugar_positional_fields(&args.field_pos, &args.fields) {
                    Ok(combined) => combined,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return Ok(2);
                    }
                };

            // NRN-229 routing seam: attempt to serve the mutation from a warm
            // `norn serve` daemon BEFORE acquiring the local mutation lock (the
            // daemon takes the SAME per-vault lock in-process; holding it here
            // would deadlock a routed apply). `Some` => the request was served (or
            // deliberately refused) by routing; `None` => no live daemon, a gated
            // shape (including `--config` / `--no-cache-refresh`, which force
            // Direct exactly as they do for routed reads), or the interactive
            // path => fall through to the direct sweep + lock + execute sequence
            // below, unchanged.
            if let Some(result) = try_route_set(
                &args,
                &combined_fields,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }

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
                &loaded_config.index_options,
                no_cache_refresh,
            )?;

            let vault_cfg = loaded_config.vault_config;

            let outcome = match crate::set::synth::preflight_and_plan(
                &cwd, &cache, &index, &vault_cfg, &args,
            ) {
                Ok(o) => o,
                Err(e) => {
                    // NRN-221: structured envelope on stdout for `--format json`
                    // (matching the move/delete arms) — a schema/argument refusal
                    // carries its stable `code` instead of bare prose. Records/TTY
                    // output stays byte-identical: prose on stderr, exit 2.
                    match args.format {
                        crate::cli::SetFormat::Json => render_json_error_envelope(&e)?,
                        crate::cli::SetFormat::Records => eprintln!("error: {e}"),
                    }
                    return Ok(2);
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

                let spans = crate::repair_apply::build_op_spans(&mut sink, &outcome.plan.changes);

                let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
                    &cwd,
                    &index,
                    &outcome.plan,
                    /*dry_run=*/ false,
                    &crate::repair_apply::CreateApplyContext::default(),
                    &mut sink,
                    &spans,
                    None,
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
        Command::Edit(args) => {
            use crate::cache::CacheError;
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;
            use std::io::{IsTerminal, Read, Write};

            // Single-op sugar (ADR 0010, NRN-210) desugars 1:1 into a one-element
            // ops array; when absent, fall back to the canonical JSON source
            // (--edits-json / --ops-file / stdin). Resolve this first so a
            // malformed input fails fast before any lock/cache work.
            let ops: Vec<crate::edit::ops::EditOp> = match crate::edit::sugar::desugar(&args) {
                Ok(Some(ops)) => {
                    // F1: op-flag sugar and an ops array on stdin are mutually
                    // exclusive. When stdin is a redirected pipe/file it carries
                    // an ops array the sugar path would silently ignore — refuse
                    // before any lock/write. A TTY or an empty source
                    // (`</dev/null`) carries no ops, so both remain allowed.
                    if stdin_carries_redirected_payload() {
                        eprintln!(
                            "error: op-flag sugar conflicts with an ops array on stdin; use one or the other"
                        );
                        return Ok(2);
                    }
                    ops
                }
                Ok(None) => {
                    let raw = match (&args.edits_json, &args.ops_file) {
                        (Some(s), _) => s.clone(),
                        (None, Some(path)) => match std::fs::read_to_string(path) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("error: failed to read ops file {path}: {e}");
                                return Ok(2);
                            }
                        },
                        (None, None) => {
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf).map_err(|e| {
                                anyhow::anyhow!("failed to read edits from stdin: {e}")
                            })?;
                            buf
                        }
                    };
                    match serde_json::from_str(&raw) {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("error: invalid edits JSON: {e}");
                            return Ok(2);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };
            if ops.is_empty() {
                eprintln!("error: edits array is empty");
                return Ok(2);
            }

            // NRN-229 routing seam: attempt to serve the mutation from a warm
            // `norn serve` daemon BEFORE acquiring the local mutation lock (the
            // daemon takes the SAME per-vault lock in-process; holding it here
            // would deadlock a routed apply). `Some` => the request was served
            // (or deliberately refused) by routing; `None` => no live daemon, a
            // forced-Direct flag (`--config` / `--no-cache-refresh`, exactly as
            // for routed reads), or the interactive path => fall through to the
            // direct sweep + lock + execute sequence below, unchanged.
            if let Some(result) = try_route_edit(
                &args,
                &ops,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }

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
            let cache = crate::cache_cmd::open_for_query(
                &cwd,
                &loaded_config.index_options,
                no_cache_refresh,
            )?;
            let vault_cfg = loaded_config.vault_config;

            let pre = match crate::edit::synth::preflight_and_plan(
                &cwd,
                &cache,
                &index,
                &vault_cfg,
                &args.target,
                &ops,
                args.expected_hash.as_deref(),
            ) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };

            let stdout = std::io::stdout();
            let mut out = stdout.lock();

            let should_apply = if args.dry_run {
                false
            } else if args.yes {
                true
            } else if matches!(args.format, crate::cli::EditFormat::Json) {
                let preview =
                    crate::edit::report::build_report(&pre.outcome, &pre.descriptors, false, "");
                crate::edit::report::render_json(&mut out, &preview)?;
                return Ok(0);
            } else if std::io::stdin().is_terminal() {
                let preview =
                    crate::edit::report::build_report(&pre.outcome, &pre.descriptors, false, "");
                crate::edit::report::render_records(&mut out, &preview)?;
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
                let preview =
                    crate::edit::report::build_report(&pre.outcome, &pre.descriptors, false, "");
                crate::edit::report::render_records(&mut out, &preview)?;
                return Ok(0);
            };

            if should_apply {
                let argv: Vec<String> = std::env::args().collect();
                let mut sink = open_event_sink(&cwd, false, vault_cfg.telemetry.as_ref(), &argv);
                emit_invocation_started(
                    &mut sink,
                    "edit",
                    &cwd,
                    pre.outcome.plan.vault_root.as_str(),
                    false,
                    &argv,
                );
                let spans =
                    crate::repair_apply::build_op_spans(&mut sink, &pre.outcome.plan.changes);
                let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
                    &cwd,
                    &index,
                    &pre.outcome.plan,
                    false,
                    &crate::repair_apply::CreateApplyContext::default(),
                    &mut sink,
                    &spans,
                    None,
                );
                let trace_id = sink.trace_id().to_string();
                let exit = if apply_outcome.is_ok() { 0 } else { 2 };
                emit_single_op_finished(&mut sink, "edit", exit, apply_outcome.is_ok());
                apply_outcome?;

                let applied = crate::edit::report::build_report(
                    &pre.outcome,
                    &pre.descriptors,
                    true,
                    &trace_id,
                );
                match args.format {
                    crate::cli::EditFormat::Records => {
                        crate::edit::report::render_records(&mut out, &applied)?;
                        writeln!(out, "trace: {trace_id}")?;
                    }
                    crate::cli::EditFormat::Json => {
                        crate::edit::report::render_json(&mut out, &applied)?;
                    }
                }
            } else {
                let preview =
                    crate::edit::report::build_report(&pre.outcome, &pre.descriptors, false, "");
                match args.format {
                    crate::cli::EditFormat::Records => {
                        crate::edit::report::render_records(&mut out, &preview)?
                    }
                    crate::cli::EditFormat::Json => {
                        crate::edit::report::render_json(&mut out, &preview)?
                    }
                }
            }
            Ok(0)
        }
        Command::New(args) => {
            use crate::cache::CacheError;
            use crate::mutation_lock::pending::sweep_pending;
            use crate::mutation_lock::MutationLock;

            // Parse `--var KEY=VALUE` BEFORE routing OR the mutation lock (the F5
            // pattern `set` uses for its positional-field shape): a malformed
            // `--var` is a pure argv error that must fail fast with the direct
            // path's exact `error: {e}` prose + exit 2 and NO side effects. The
            // parsed map also feeds the NRN-230 routed `vars` wire object below.
            // The direct path re-parses it inside `preflight_and_plan`
            // (idempotent); front-running the validation keeps the routed and
            // direct refusals byte-identical.
            let vars = match crate::new::parse_var_args(&args.var) {
                Ok(vars) => vars,
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(2);
                }
            };

            // NRN-230 routing seam: attempt to serve the creation from a warm
            // `norn serve` daemon BEFORE acquiring the local mutation lock (the
            // daemon takes the SAME per-vault lock in-process; holding it here
            // would deadlock a routed apply). `Some` => served (or deliberately
            // refused) by routing; `None` => no live daemon, a gated shape
            // (`--body-from-stdin`, `--config` / `--no-cache-refresh`), or the
            // interactive TTY path => fall through to the direct sweep + lock +
            // preflight_and_plan sequence below, unchanged.
            if let Some(result) = try_route_new(
                &args,
                &vars,
                &cwd,
                config_path.is_some(),
                no_cache_refresh,
                verbose,
            ) {
                return result;
            }

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
        Command::Audit(args) => {
            let (_, events_dir) = crate::cache::events_dir_for(&cwd)?;
            let filter = match crate::audit::build_filter(&args) {
                Ok(f) => f,
                Err(msg) => {
                    eprintln!("error: {msg}");
                    std::process::exit(2);
                }
            };
            let events = crate::telemetry::read::read_events(&events_dir, &filter, args.limit);
            let out = crate::audit::render(&events, &args);
            print!("{out}");
            if !out.ends_with('\n') {
                println!();
            }
            Ok(0)
        }
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
        Command::Serve(_) => {
            unreachable!("serve is handled before the cache-opening dispatch")
        }
        Command::Service(_) => {
            unreachable!("service is handled before vault targeting")
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

/// Validate dynamically-desugared field-predicate keys (ADR 0010 / NRN-207)
/// against this vault's field universe. A no-op when no dynamic predicate was
/// used (the common canonical path pays nothing). Shared by every query-family
/// command so the gate can't drift between `find`, `count`, and `describe`.
pub(crate) fn gate_dynamic_query(
    cache: &crate::cache::Cache,
    config: &crate::config_loader::LoadedConfig,
    dynamic_keys: &[String],
    cmd: crate::grammar::QueryCmd,
) -> Result<()> {
    if dynamic_keys.is_empty() {
        return Ok(());
    }
    let universe = crate::grammar::field_universe(cache, config)?;
    let known_flags = crate::grammar::query_known_flags(cmd);
    crate::grammar::gate_dynamic_fields(dynamic_keys, &universe, &known_flags)
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
            // A launchd-loaded serve daemon keeps running the OLD binary
            // until restarted — the swap alone does not pick it up — so a
            // REAL swap best-effort restarts a loaded unit (NRN-226). The
            // only-on-a-completed-swap gate (never dry-run/no-op) lives
            // inside `maybe_restart_after_update`, where it is unit-tested;
            // this boundary just prints the outcome. Failure is a warning
            // only: the binary IS updated either way, and the version-skew
            // fallback keeps routing safe until a manual `norn service
            // restart`.
            match crate::service::command::maybe_restart_after_update(report.action) {
                None | Some(crate::service::command::RestartOutcome::NotLoaded) => {}
                Some(crate::service::command::RestartOutcome::Restarted) => {
                    eprintln!("serve restarted to pick up the updated binary");
                }
                Some(crate::service::command::RestartOutcome::Failed(error)) => {
                    eprintln!(
                        "warning: norn updated successfully, but the running service could not be restarted ({error:#}) — run `norn service restart` to pick up the new binary"
                    );
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
pub(crate) fn emit_cascade_failure_warnings(report: &crate::apply_report::ApplyReport) {
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

/// Render the structured error envelope `{ code, message, path? }` to STDOUT as
/// pretty JSON (NRN-150). Called by a mutation command's `--format json` failure
/// arm so a JSON consumer gets a machine-branchable failure — not a bare nonzero
/// exit plus prose on stderr. The `code` is recovered by downcasting the opaque
/// `anyhow::Error` (see `apply_report::ApplyError::from_anyhow`).
pub(crate) fn render_json_error_envelope(e: &anyhow::Error) -> anyhow::Result<()> {
    use std::io::Write;
    let envelope = crate::apply_report::ApplyError::from_anyhow(e);
    let json = serde_json::to_string_pretty(&envelope)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(json.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
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
pub(crate) fn emit_invocation_finished(
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
pub(crate) fn remove_empty_dirs(path: &std::path::Path) {
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

#[cfg(all(test, unix))]
mod tests {
    use super::{
        after_send_for, execute_routed_call, routing_forced_direct, CallSpec, FallbackAfterSend,
        RoutedCall,
    };
    use crate::service::{bind_trusted, CallPhase, OnToolError, ServiceClient, CONTROL_PROTOCOL};
    use anyhow::Result;
    use camino::{Utf8Path, Utf8PathBuf};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// F1 seam-level gate: BOTH `--config` and `--no-cache-refresh` force a
    /// routable read onto the Direct path, independent of any live daemon. The
    /// `--no-cache-refresh` arm is the NRN-94 fix — the daemon always serves a
    /// freshly-refreshed cache, so routing that flag could return counts that
    /// differ from direct on a stale cache. The live-daemon half of this proof
    /// (the flag actually not incrementing the daemon's served-call counter) is
    /// the e2e `no_cache_refresh_shape_is_not_routed` test.
    #[test]
    fn routing_forced_direct_truth_table() {
        assert!(!routing_forced_direct(false, false), "no flags => routable");
        assert!(routing_forced_direct(true, false), "--config forces Direct");
        assert!(
            routing_forced_direct(false, true),
            "--no-cache-refresh forces Direct"
        );
        assert!(routing_forced_direct(true, true), "both flags force Direct");
    }

    // ── NRN-228 route_call send-commit seam ──────────────────────────────────
    //
    // These drive `execute_routed_call` (the body `route_call` and `route_read`
    // share once a daemon is proven live) against a stub UDS daemon on a temp
    // socket, so the send-commit policy is observed end-to-end without touching
    // the process-global env the well-known-socket probe reads.

    /// How far the stub daemon drives the request wire before dropping (or
    /// completing) the connection.
    enum Stub {
        /// PRE-send: read `hello`, then drop without a `ready` frame — the
        /// client fails at the preamble, before the `tools/call` frame is ever
        /// written.
        DropBeforeReady,
        /// POST-send: complete `hello`/`ready` and MCP `initialize`, read the
        /// `tools/call`, then drop WITHOUT a response — the request has crossed
        /// to the daemon, but no result comes back.
        DropAfterCall,
        /// Success: like `DropAfterCall`, but answer the `tools/call` with a
        /// valid success envelope carrying `structuredContent` — the daemon
        /// EXECUTED the tool; any later failure (e.g. `reconstruct`) is
        /// post-send by construction.
        RespondOk,
    }

    /// Spawn a stub daemon behaving per [`Stub`].
    fn spawn_stub(listener: UnixListener, mode: Stub) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut w = conn;

            let mut hello = String::new();
            reader.read_line(&mut hello).unwrap();
            if matches!(mode, Stub::DropBeforeReady) {
                // Pre-send: never answer `ready`. Dropping both fds (w now, the
                // reader clone at return) gives the client a prompt EOF.
                return;
            }
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "norn_control": "ready", "protocol": CONTROL_PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                })
            )
            .unwrap();
            w.flush().unwrap();

            let mut init = String::new();
            reader.read_line(&mut init).unwrap();
            writeln!(
                w,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
            )
            .unwrap();
            w.flush().unwrap();

            // Read the tools/call (so the client's write completes)...
            let mut call = String::new();
            reader.read_line(&mut call).unwrap();
            if matches!(mode, Stub::DropAfterCall) {
                // ...then drop without responding — the post-send failure.
                return;
            }
            // ...and answer it with a valid success envelope (`RespondOk`).
            writeln!(
                w,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": {
                        "content": [{"type": "text", "text": "{\"ok\":true}"}],
                        "structuredContent": {"ok": true},
                        "isError": false,
                    }
                })
            )
            .unwrap();
            w.flush().unwrap();
        })
    }

    /// `reconstruct`/`emit` must never run on a FAILED routed call — the failure
    /// branch returns before them. Panicking closures prove it.
    fn reconstruct_never(_: &serde_json::Value) -> Result<()> {
        panic!("reconstruct must not run on a failed routed call")
    }
    fn emit_never(_: ()) -> Result<i32> {
        panic!("emit must not run on a failed routed call")
    }

    /// The policy mapping: a dry-run writes nothing (a read in mutation
    /// clothing) so it may fall back after send; an apply commits, so a
    /// post-send failure is surfaced, never silently retried.
    #[test]
    fn after_send_for_maps_dry_run_and_apply() {
        assert_eq!(
            after_send_for(true),
            FallbackAfterSend::Fallback,
            "a dry-run may fall back after send"
        );
        assert_eq!(
            after_send_for(false),
            FallbackAfterSend::Commit,
            "an apply commits — no post-send fallback"
        );
    }

    /// Commit mode, PRE-send failure: the tool never ran, so the seam falls back
    /// to Direct (returns `None`) exactly like a read.
    #[test]
    fn commit_falls_back_to_direct_on_a_pre_send_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::DropBeforeReady);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let out = execute_routed_call(
            Utf8Path::new("/vaults/atlas"),
            &client,
            RoutedCall {
                spec: CallSpec {
                    tool: "vault.set",
                    arguments: serde_json::json!({}),
                    on_tool_error: OnToolError::AcceptWithPayload,
                    verbose: false,
                },
                after_send: FallbackAfterSend::Commit,
            },
            reconstruct_never,
            emit_never,
        );
        assert!(
            out.is_none(),
            "a pre-send failure under Commit must fall back to Direct (None)"
        );
        stub.join().unwrap();
    }

    /// Commit mode, POST-send failure: the daemon may have applied the change,
    /// so the seam does NOT fall back. It returns `Some(Err(..))` — an error the
    /// top level maps to exit 1 — whose message names the inspect / `--dry-run`
    /// remedy. `Some` (not `None`) is the no-fallback proof.
    #[test]
    fn commit_surfaces_uncertainty_on_a_post_send_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::DropAfterCall);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let out = execute_routed_call(
            Utf8Path::new("/vaults/atlas"),
            &client,
            RoutedCall {
                spec: CallSpec {
                    tool: "vault.set",
                    arguments: serde_json::json!({}),
                    on_tool_error: OnToolError::AcceptWithPayload,
                    verbose: false,
                },
                after_send: FallbackAfterSend::Commit,
            },
            reconstruct_never,
            emit_never,
        );
        let result = out.expect("Commit must NOT fall back after send (Some, not None)");
        let error = result.expect_err("a post-send Commit failure is an error, not a success");
        let message = format!("{error}");
        assert!(
            message.contains("the daemon may have applied the change"),
            "the error names the uncertainty; got: {message}"
        );
        assert!(
            message.contains("--dry-run"),
            "the error names the --dry-run remedy; got: {message}"
        );
        assert!(
            message.contains("norn get"),
            "the error names the inspect remedy; got: {message}"
        );
        // NRN-220: the error is typed, so the structured failure envelope
        // recovers the stable machine code from the seam's actual error value.
        assert_eq!(
            crate::apply_report::ApplyError::from_anyhow(&error).code,
            "post-send-uncertain"
        );
        stub.join().unwrap();
    }

    /// Fallback mode (a routed dry-run), the SAME post-send failure: because a
    /// dry-run writes nothing, the seam silently falls back to Direct (`None`).
    /// The only difference from the Commit case above is the policy.
    #[test]
    fn dry_run_fallback_is_silent_on_a_post_send_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::DropAfterCall);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let out = execute_routed_call(
            Utf8Path::new("/vaults/atlas"),
            &client,
            RoutedCall {
                spec: CallSpec {
                    tool: "vault.set",
                    arguments: serde_json::json!({}),
                    on_tool_error: OnToolError::AcceptWithPayload,
                    verbose: false,
                },
                after_send: FallbackAfterSend::Fallback,
            },
            reconstruct_never,
            emit_never,
        );
        assert!(
            out.is_none(),
            "a dry-run (Fallback) post-send failure must silently fall back to Direct (None)"
        );
        stub.join().unwrap();
    }

    /// Commit mode, `reconstruct` failure AFTER a successful call: the daemon
    /// EXECUTED the tool (the envelope came back fine — this process just can't
    /// read it), so falling back to Direct would double-apply. The seam must
    /// surface the same uncertainty error as a dropped response, not `None`.
    #[test]
    fn commit_surfaces_uncertainty_on_a_reconstruct_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::RespondOk);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let out = execute_routed_call(
            Utf8Path::new("/vaults/atlas"),
            &client,
            RoutedCall {
                spec: CallSpec {
                    tool: "vault.set",
                    arguments: serde_json::json!({}),
                    on_tool_error: OnToolError::AcceptWithPayload,
                    verbose: false,
                },
                after_send: FallbackAfterSend::Commit,
            },
            |_| -> Result<()> { Err(anyhow::anyhow!("envelope shape mismatch")) },
            emit_never,
        );
        let result = out.expect("Commit must NOT fall back on a reconstruct failure (Some)");
        let error = result.expect_err("a post-success reconstruct failure is an error");
        let message = format!("{error}");
        assert!(
            message.contains("the daemon may have applied the change"),
            "the error names the uncertainty; got: {message}"
        );
        assert!(
            message.contains("--dry-run") && message.contains("norn get"),
            "the error names the inspect / --dry-run remedy; got: {message}"
        );
        stub.join().unwrap();
    }

    /// Fallback mode (a routed dry-run), the SAME post-success `reconstruct`
    /// failure: a dry-run writes nothing, so the seam keeps today's silent
    /// fall-back to Direct (`None`).
    #[test]
    fn dry_run_fallback_is_silent_on_a_reconstruct_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::RespondOk);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let out = execute_routed_call(
            Utf8Path::new("/vaults/atlas"),
            &client,
            RoutedCall {
                spec: CallSpec {
                    tool: "vault.set",
                    arguments: serde_json::json!({}),
                    on_tool_error: OnToolError::AcceptWithPayload,
                    verbose: false,
                },
                after_send: FallbackAfterSend::Fallback,
            },
            |_| -> Result<()> { Err(anyhow::anyhow!("envelope shape mismatch")) },
            emit_never,
        );
        assert!(
            out.is_none(),
            "a dry-run (Fallback) reconstruct failure must silently fall back to Direct (None)"
        );
        stub.join().unwrap();
    }

    /// The stub's PRE-send drop really does fail the client before the tool call
    /// is sent — proven at the phase-tagged layer so the fallback tests above
    /// can't pass on a mis-tagged phase. (POST-send tagging is proven by the
    /// service-module unit tests.)
    #[test]
    fn stub_pre_send_drop_is_tagged_pre_send() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("s.sock")).unwrap();
        let stub = spawn_stub(bind_trusted(&path), Stub::DropBeforeReady);

        let client = ServiceClient {
            socket_path: path.clone(),
        };
        let error = client
            .call_tool_structured_phased(
                Utf8Path::new("/vaults/atlas"),
                "vault.set",
                serde_json::json!({}),
                OnToolError::AcceptWithPayload,
            )
            .expect_err("a dropped preamble must be Err");
        assert_eq!(error.phase, CallPhase::PreSend);
        stub.join().unwrap();
    }
}
