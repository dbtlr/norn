//! Generates shell completion scripts and the roff man page as side effects
//! of building `vault-cli`. The outputs land under the workspace `target/`
//! directory so cargo-dist's `include` directive (in `dist-workspace.toml`)
//! can pick them up without requiring a separate `just completions` /
//! `just manpage` step in the release pipeline.
//!
//! The CLI surface is reused via `#[path = "src/cli.rs"]` so this script
//! tracks the real `clap` definitions automatically. `cli.rs` is kept free
//! of intra-crate dependencies (see commit history) to make the include
//! trick viable.

use std::env;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{generate_to, Shell};
use clap_complete_nushell::Nushell;
use clap_mangen::Man;

#[path = "src/cli.rs"]
#[allow(dead_code)]
mod cli;

fn main() -> std::io::Result<()> {
    // CARGO_MANIFEST_DIR is the repo root, so cargo-dist's `include` entries
    // and the build-script outputs share the same base directory.
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo when running build.rs"),
    );

    let completions_dir = manifest_dir.join("target").join("completions");
    let man_dir = manifest_dir.join("target").join("man");

    std::fs::create_dir_all(&completions_dir)?;
    std::fs::create_dir_all(&man_dir)?;

    let mut cmd = cli::Cli::command();
    generate_to(Shell::Bash, &mut cmd, "vault", &completions_dir)?;
    generate_to(Shell::Zsh, &mut cmd, "vault", &completions_dir)?;
    generate_to(Shell::Fish, &mut cmd, "vault", &completions_dir)?;
    generate_to(Nushell, &mut cmd, "vault", &completions_dir)?;

    let man = Man::new(cmd);
    let mut buffer = Vec::new();
    man.render(&mut buffer)?;
    std::fs::write(man_dir.join("vault.1"), buffer)?;

    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
