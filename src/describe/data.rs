//! Vault contents-summary (`describe --data`). Populated in Task 2/3.
use serde::Serialize;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DataSummary {
    pub total: usize,
}
