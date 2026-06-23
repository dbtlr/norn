// Minimal library stub for running tests
// Re-export only telemetry for unit tests; the binary (main.rs) is the full impl

#![allow(dead_code)]

// Redefine cache functions needed by telemetry/ids.rs
pub mod cache {
    pub fn hex_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

pub mod telemetry;
