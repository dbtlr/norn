//! `norn find` — the exemplar read command over the filter + sort/paging
//! surface.
//!
//! The command-module pattern, proven here and in `get`: the module owns (a)
//! its clap `Args`, (b) a `to_params` mapping into the wire vocabulary, and (c)
//! a `run` entry that presents the outcome. This phase's `run` parses to Params
//! then presents the uniform not-yet-ported outcome; the port swaps the
//! presented outcome for `execute(env, params)`'s Report (ADR 0016).

use std::io::Write;

use clap::Args;
use norn_wire::{FilterParams, SortPaginateParams};

use crate::commands::args::{FilterArgs, SortPaginateArgs};
use crate::display::Presenter;

/// The command name, used in the not-yet-ported diagnostic.
const NAME: &str = "find";

#[derive(Args, Debug)]
pub struct FindArgs {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Return every document — escape hatch when no predicate is given.
    #[arg(long, help_heading = "Filter options")]
    pub all: bool,

    #[command(flatten)]
    pub paging: SortPaginateArgs,
}

impl FindArgs {
    /// Parse the flags into the shared read-verb wire vocabulary.
    pub fn to_params(&self) -> (FilterParams, SortPaginateParams) {
        (self.filter.to_params(), self.paging.to_params())
    }
}

/// Present the command's outcome and return the process exit code.
pub fn run<O: Write, E: Write>(args: &FindArgs, presenter: &mut Presenter<O, E>) -> i32 {
    // The adapter's job: argv → Params. `execute(env, params)` and Report
    // presentation land when the read verbs port.
    let (_filter, _paging) = args.to_params();
    presenter.not_yet_ported(NAME)
}
