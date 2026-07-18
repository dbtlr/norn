//! Single source of truth for the binary's user-facing name.
//!
//! Reads from `CARGO_BIN_NAME` so the rename to `norn` is one line change in
//! `Cargo.toml` rather than a project-wide string sweep. `CARGO_BIN_NAME` is
//! only defined when compiling a binary, so library builds (unit tests, and a
//! future `norn-service` binary linking this crate) fall back to the product
//! name `norn` — the command users type, regardless of which binary is running.

pub const BIN_NAME: &str = match option_env!("CARGO_BIN_NAME") {
    Some(name) => name,
    None => "norn",
};

// No unit test here: in a library build `CARGO_BIN_NAME` is unset, so `BIN_NAME`
// resolves to the hardcoded fallback and any `assert_eq!(BIN_NAME, "norn")`
// would be a tautology. The guarantee that matters — the `norn` *binary*
// reports "norn" as its program name — is verified against the real binary
// (compiled with `CARGO_BIN_NAME` set) in `tests/cli_output.rs`
// (`top_level_help_uses_norn_program_name`).
