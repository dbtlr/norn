//! `norn count` — total, single-field distribution, or nested group tree.
//!
//! The command module maps its clap `Args` (`--by`, the shared filter surface,
//! `--format`) into [`CountParams`], summons the vault owner, and returns the
//! [`CountReport`] as an [`Output`] (NRN-370). The display layer renders it
//! byte-faithfully to the donor: `text` (the default) is padded columns; `json`
//! is the untagged compact serialization.

use norn_wire::CountParams;

use crate::cli::{CountArgs, CountFormat, GlobalArgs};
use crate::display::{CountView, Diagnostic, Format, FormatSpec, Output};

impl From<CountFormat> for Format {
    fn from(f: CountFormat) -> Self {
        match f {
            // `count`'s bespoke padded text is rendered in the records slot; it
            // never composed the record primitives, so it stays unstyled.
            CountFormat::Text => Format::Records,
            CountFormat::Json => Format::Json,
        }
    }
}

/// Resolve the counts and return the report as an [`Output`] for the layer to
/// render, or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &CountArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let mut session = crate::routed::open_session(global)?;

    let params = CountParams {
        by: args.by.clone(),
        filter: args.filters.to_params(),
    };
    let report = session
        .count(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Count(CountView {
        report,
        explicit: Some(args.format.into()),
        spec: FormatSpec {
            tty: Format::Records,
            piped: Format::Records,
        },
    }))
}
