//! `norn get` — the second exemplar read command, over one or more targets
//! plus the shared sort/paging surface. Same command-module pattern as `find`.

use std::io::Write;

use clap::Args;
use norn_wire::SortPaginateParams;

use crate::commands::args::SortPaginateArgs;
use crate::display::Presenter;

/// The command name, used in the not-yet-ported diagnostic.
const NAME: &str = "get";

#[derive(Args, Debug)]
pub struct GetArgs {
    /// One or more doc targets. Each accepts a path, a stem, or a
    /// wikilink-shaped string.
    #[arg(required = true, num_args = 1.., value_name = "DOC")]
    pub targets: Vec<String>,

    #[command(flatten)]
    pub paging: SortPaginateArgs,
}

impl GetArgs {
    /// Parse the sort/paging flags into the shared wire vocabulary. The targets
    /// carry through as the verb's own request field once `get` ports.
    pub fn to_params(&self) -> SortPaginateParams {
        self.paging.to_params()
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(args: &GetArgs, presenter: &mut Presenter<O, E>) -> i32 {
    let _paging = args.to_params();
    presenter.not_yet_ported(NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn get_args(argv: &[&str]) -> GetArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Get(a) => a,
            other => panic!("expected get, got {other:?}"),
        }
    }

    #[test]
    fn targets_are_collected_and_paging_defaults() {
        let args = get_args(&["norn", "get", "alpha", "notes/beta.md"]);
        assert_eq!(args.targets, vec!["alpha", "notes/beta.md"]);
        assert_eq!(args.to_params(), SortPaginateParams::default());
    }
}
