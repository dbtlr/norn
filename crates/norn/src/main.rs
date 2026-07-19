#![forbid(unsafe_code)]
//! Composition root: the CLI entrypoint and the owner entrypoint live in
//! this one binary (single-artifact distribution).
//!
//! Argv parsing and dispatch live in `norn-cli`; this bin only maps the
//! adapter's exit code onto the process. The crate-map edges to the adapter
//! crates are kept compile-load-bearing by `CRATE_MAP` so a declared
//! dependency can never decay to a manifest-only entry (ADR 0018).

/// The ADR 0018 crate-map contracts this binary composes. Referencing all
/// three keeps every declared edge (`norn-cli`, `norn-mcp`, `norn-owner`)
/// load-bearing at compile time.
const CRATE_MAP: &[&str] = &[norn_cli::CONTRACT, norn_mcp::CONTRACT, norn_owner::CONTRACT];

fn main() {
    // Reference the crate-map so it is used in every build profile (an unused
    // const would both warn and let the edges decay to manifest-only).
    let _ = CRATE_MAP;
    // Owner mode (ADR 0017): when the summoner spawns this executable as a
    // vault owner, argv[1] is the owner-mode sentinel — run the owner runtime
    // instead of the CLI. A normal argv returns `None` and CLI dispatch runs.
    if let Some(code) = norn_owner::run_if_owner_mode() {
        std::process::exit(code);
    }
    std::process::exit(norn_cli::run());
}
