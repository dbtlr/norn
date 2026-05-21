//! Single source of truth for the binary's user-facing name.
//!
//! Reads from `CARGO_BIN_NAME` so the rename to `norn` is one line change in
//! `Cargo.toml` rather than a project-wide string sweep.

#[allow(dead_code)]
pub const BIN_NAME: &str = env!("CARGO_BIN_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_name_matches_cargo_bin_name() {
        assert_eq!(BIN_NAME, "vault");
    }
}
