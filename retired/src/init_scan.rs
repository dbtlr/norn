//! Frontmatter field tally for `norn init`.
//!
//! `tally_from_keys` counts how often each frontmatter key appears across the
//! vault and returns the top fields sorted by frequency. `init.rs` calls it to
//! scan a vault and scaffold a starter `.norn/config.yaml` that reflects the
//! fields already in use. Pure counting over borrowed key lists, no I/O.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldStat {
    pub name: String,
    pub count: usize,
    pub total_docs: usize,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub total_docs: usize,
    pub fields: Vec<FieldStat>, // sorted desc by count, then asc by name
}

pub fn tally_from_keys<I, S>(per_doc_keys: I, total_docs: usize, top_n: usize) -> ScanResult
where
    I: IntoIterator<Item = Vec<S>>,
    S: AsRef<str>,
{
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for keys in per_doc_keys {
        for k in keys {
            *counts.entry(k.as_ref().to_string()).or_insert(0) += 1;
        }
    }
    let mut stats: Vec<FieldStat> = counts
        .into_iter()
        .map(|(name, count)| FieldStat {
            name,
            count,
            total_docs,
        })
        .collect();
    stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    stats.truncate(top_n);
    ScanResult {
        total_docs,
        fields: stats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty_result() {
        let r = tally_from_keys(Vec::<Vec<&str>>::new(), 0, 30);
        assert_eq!(r.total_docs, 0);
        assert!(r.fields.is_empty());
    }

    #[test]
    fn counts_per_field_across_docs() {
        let docs = vec![
            vec!["type", "created"],
            vec!["type", "modified"],
            vec!["type"],
        ];
        let r = tally_from_keys(docs, 3, 30);
        assert_eq!(r.total_docs, 3);
        let names: Vec<&str> = r.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["type", "created", "modified"]);
        let counts: Vec<usize> = r.fields.iter().map(|f| f.count).collect();
        assert_eq!(counts, vec![3, 1, 1]);
    }

    #[test]
    fn top_n_truncates() {
        let docs = vec![vec!["a", "b", "c", "d", "e"]];
        let r = tally_from_keys(docs, 1, 2);
        assert_eq!(r.fields.len(), 2);
    }

    #[test]
    fn ties_break_alphabetically() {
        let docs = vec![vec!["zebra", "apple"], vec!["apple", "zebra"]];
        let r = tally_from_keys(docs, 2, 30);
        let names: Vec<&str> = r.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["apple", "zebra"]);
    }
}
