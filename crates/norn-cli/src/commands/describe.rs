//! `norn describe` — the vault at a glance: structure (folders + declared rules
//! + inbox + schema) and, with `--data`/`--stats`, a contents-summary.
//!
//! The command maps its clap `Args` into [`DescribeParams`], summons the owner
//! (which serves the structure from its retained config and the data summary
//! from the warm cache), and returns the [`DescribeReport`] as an [`Output`]
//! (NRN-370). The display layer renders it: `text`
//! (default) is the count/summary block; `json` is the whole struct serialized.

use norn_wire::DescribeParams;

use crate::cli::{DescribeArgs, DescribeFormat, GlobalArgs};
use crate::display::{DescribeView, Diagnostic, Format, FormatChoice, FormatSpec, Output};

impl From<DescribeFormat> for Format {
    fn from(f: DescribeFormat) -> Self {
        match f {
            // `describe`'s bespoke text renders in the records slot, unstyled.
            DescribeFormat::Records => Format::Records,
            DescribeFormat::Json => Format::Json,
        }
    }
}

/// Resolve the vault summary and return the report as an [`Output`] for the layer
/// to render, or a soft-landing [`Diagnostic`] on failure.
pub fn run(args: &DescribeArgs, global: &GlobalArgs) -> Result<Output, Diagnostic> {
    let mut session = crate::routed::open_session(global)?;

    let params = DescribeParams {
        // `--stats` is a pure alias for `--data`; `--by` implies data verb-side.
        data: args.data || args.stats,
        by: args.by.clone(),
        limit: args.limit,
        filter: args.filters.to_params(),
        // The desugared dynamic-field keys ride to the owner's field-universe
        // gate (NRN-367/NRN-374), same as `find`/`count`.
        dynamic_keys: global.dynamic_fields.clone(),
    };

    let report = session
        .describe(params)
        .map_err(|e| crate::routed::client_error_diagnostic(&e))?;

    Ok(Output::Describe(DescribeView {
        report,
        by: args.by.clone(),
        format: FormatChoice {
            explicit: Some(Format::from(args.format.unwrap_or(DescribeFormat::Records))),
            spec: FormatSpec {
                tty: Format::Records,
                piped: Format::Records,
            },
        },
    }))
}
