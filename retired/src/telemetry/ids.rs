//! ID generation and the clock seam for telemetry.
//!
//! [`IdGen`] produces hex trace/span IDs deterministically from a seed (so tests
//! can pin them) while defaulting to a process-unique seed in production.
//! [`Clock`] abstracts the current time so emits can be made deterministic.

use sha2::{Digest, Sha256};

/// Deterministic hex ID generator. Seeded once; each call advances an internal
/// counter, so IDs are distinct within a run and reproducible across runs given
/// the same seed.
pub struct IdGen {
    seed: u64,
    counter: u64,
}

impl IdGen {
    /// Process-unique seed derived from the PID and wall-clock nanos.
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h = Sha256::new();
        h.update(std::process::id().to_le_bytes());
        h.update(nanos.to_le_bytes());
        let digest = h.finalize();
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&digest[..8]);
        Self::with_seed(u64::from_le_bytes(seed_bytes))
    }

    /// Fixed-seed generator for deterministic tests.
    pub fn with_seed(seed: u64) -> Self {
        Self { seed, counter: 0 }
    }

    /// A 32-char lowercase-hex trace ID.
    pub fn trace_id(&mut self) -> String {
        self.hex(32)
    }

    /// A 16-char lowercase-hex span ID.
    pub fn span_id(&mut self) -> String {
        self.hex(16)
    }

    fn hex(&mut self, n: usize) -> String {
        self.counter += 1;
        let mut h = Sha256::new();
        h.update(self.seed.to_le_bytes());
        h.update(self.counter.to_le_bytes());
        crate::cache::hex_lower(h.finalize().as_ref())[..n].to_string()
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Time seam: real wall clock, or a fixed timestamp for deterministic emits.
pub enum Clock {
    System,
    /// Frozen timestamp for deterministic test emits. Only constructed from
    /// tests; `now_rfc3339` still matches it in production builds.
    #[allow(dead_code)]
    Fixed(String),
}

impl Clock {
    /// Convenience constructor for a fixed-timestamp clock. Test-only seam.
    #[allow(dead_code)]
    pub fn fixed(s: &str) -> Self {
        Clock::Fixed(s.to_string())
    }

    /// Current time as RFC-3339 UTC with millisecond precision.
    pub fn now_rfc3339(&self) -> String {
        match self {
            Clock::System => {
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            }
            Clock::Fixed(s) => s.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idgen_is_deterministic_under_fixed_seed() {
        let mut g = IdGen::with_seed(42);
        let t = g.trace_id();
        assert_eq!(t.len(), 32);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let s = g.span_id();
        assert_eq!(s.len(), 16);
        let mut g2 = IdGen::with_seed(42);
        assert_eq!(g2.trace_id(), t);
    }

    #[test]
    fn span_ids_are_distinct_per_op() {
        let mut g = IdGen::with_seed(1);
        let _ = g.trace_id();
        assert_ne!(g.span_id(), g.span_id());
    }
}
