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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_name_matches_cargo_bin_name() {
        assert_eq!(BIN_NAME, "norn");
    }
}
