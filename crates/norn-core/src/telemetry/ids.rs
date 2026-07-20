//! ID generation and the clock seam for telemetry.
//!
//! [`IdGen`] produces hex trace/span IDs deterministically from a seed (so tests
//! can pin them) while defaulting to a process-unique seed in production.
//! [`Clock`] abstracts the current time so emits can be made deterministic.

/// Deterministic hex ID generator. Seeded once; each call advances an internal
/// counter, so IDs are distinct within a run and reproducible across runs given
/// the same seed.
///
/// The IDs are opaque correlation tokens (never a wire contract): the port hashes
/// `seed || counter` with BLAKE3 (already a crate dependency) rather than the
/// donor's SHA-256, so no new dependency is pulled in. The only observable
/// contract is length (32/16 lowercase-hex chars) and per-seed determinism.
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
        let mut hasher = blake3::Hasher::new();
        hasher.update(&std::process::id().to_le_bytes());
        hasher.update(&nanos.to_le_bytes());
        let digest = hasher.finalize();
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&digest.as_bytes()[..8]);
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
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.seed.to_le_bytes());
        hasher.update(&self.counter.to_le_bytes());
        hasher.finalize().to_hex().as_str()[..n].to_string()
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
    ///
    /// The `System` arm reads the wall clock via `std::time::SystemTime` and
    /// formats through chrono's timestamp constructor, so norn-core does not need
    /// chrono's ambient `clock` feature (`Utc::now`) — keeping the dependency
    /// surface minimal and the injected-clock seam (`Fixed`) the deterministic
    /// default for tests and the executor's report fold.
    pub fn now_rfc3339(&self) -> String {
        match self {
            Clock::System => {
                let since_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                chrono::DateTime::<chrono::Utc>::from_timestamp(
                    since_epoch.as_secs() as i64,
                    since_epoch.subsec_nanos(),
                )
                .unwrap_or_default()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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
