//! The `norn` binary: a thin shell over the `norn_run` library.
//!
//! All command dispatch, the module tree, and the query/mutate core live in
//! the library so that a second binary (`norn-service`, the warm daemon) can
//! link the same code without going through this one-shot CLI entrypoint.

fn main() {
    norn_run::cli_main();
}
