#![forbid(unsafe_code)]
//! Composition root: the CLI entrypoint and the owner entrypoint live in
//! this one binary (single-artifact distribution).

fn main() {
    eprintln!("norn (rewrite skeleton): ported surfaces land phase by phase — ADR 0018.");
    eprintln!(
        "crate map: {}",
        [norn_cli::CONTRACT, norn_mcp::CONTRACT, norn_owner::CONTRACT].join(" | ")
    );
    std::process::exit(2);
}
