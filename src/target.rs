//! Target resolution and backlink lookup over a built graph.
//!
//! `resolve_target_path` turns a user-supplied target (an exact path or a stem)
//! into a single vault path, erroring on an ambiguous stem; `backlinks`
//! collects the links across the index that resolve to a given path. The
//! mutation appliers (`applier.rs`) and `delete` use these to find what points
//! at a document before they move or remove it.

use crate::core::{GraphIndex, Link};
use anyhow::{bail, Result};
use camino::Utf8PathBuf;

pub fn backlinks<'a>(index: &'a GraphIndex, target_path: &Utf8PathBuf) -> Vec<&'a Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.resolved_path.as_ref() == Some(target_path))
        .collect()
}

pub fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return Ok(document.path.clone());
    }

    let matches = index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no document matched path or stem: {target}"),
        many => bail!(
            "ambiguous document stem: {target}; candidates: {}",
            many.iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
