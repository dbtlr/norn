//! The vault-env value-carrier: a vault root paired with its resolved config.
//!
//! [`VaultEnv`] is a pure value: everything it holds is injected at construction
//! and handed back out on demand. It performs no ambient reads — no process CWD,
//! no environment variables, no filesystem or cache access. Callers that resolve
//! a config from disk or a registry do so in their own layer and inject the
//! result here.
//!
//! # What lives elsewhere
//!
//! This value-carrier stays narrow. Warm/cold cache modes, a held-open `Cache`
//! per generation, a writer queue, a read pool, and the per-request self-heal /
//! freshness pipeline are all cache-engine and owner-daemon machinery, and are
//! deliberately NOT here:
//!
//! - warm-slot / generation / read-pool state and the `query_cache` /
//!   `load_graph_index` cache path → the cache-engine and `norn-owner` layers;
//! - config *resolution* from a file or registry → the config layer
//!   (norn-core may not depend on `norn-config`, so the resolved config is
//!   injected as values, not loaded here).
//!
//! What survives is the invariant the seam was built to express: a
//! surface-neutral handle onto one vault, carrying its root and its config, that
//! any consumer can hold and read from without the handle reaching for ambient
//! state.

use camino::{Utf8Path, Utf8PathBuf};

use crate::standards::{CompiledConfig, VaultConfig};

/// A value handle onto one vault: its absolute root plus the resolved config
/// (raw [`VaultConfig`] and the pre-compiled [`CompiledConfig`] path patterns).
///
/// Constructed by value injection ([`VaultEnv::new`]); it never loads or watches
/// anything. Consumers read the root and config back through the accessors.
#[derive(Debug, Clone)]
pub struct VaultEnv {
    vault_root: Utf8PathBuf,
    config: VaultConfig,
    compiled: CompiledConfig,
}

impl VaultEnv {
    /// Build a vault env from already-resolved values. The `vault_root` is taken
    /// as-is (the caller is responsible for having made it absolute); `config`
    /// and `compiled` are the parsed config and its compiled path patterns,
    /// typically from [`crate::standards::parse_config_compiled`].
    pub fn new(vault_root: Utf8PathBuf, config: VaultConfig, compiled: CompiledConfig) -> Self {
        Self {
            vault_root,
            config,
            compiled,
        }
    }

    /// The vault root this env is bound to.
    pub fn vault_root(&self) -> &Utf8Path {
        &self.vault_root
    }

    /// The resolved vault config.
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// The pre-compiled path patterns for the config's validate rules.
    pub fn compiled(&self) -> &CompiledConfig {
        &self.compiled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    fn build_env(yaml: &str) -> VaultEnv {
        let (config, compiled) =
            crate::standards::parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml"))
                .expect("config parses");
        VaultEnv::new(Utf8PathBuf::from("/vault"), config, compiled)
    }

    #[test]
    fn carries_injected_root_and_config_unchanged() {
        let env = build_env("links:\n  alias_field: aliases\n");
        assert_eq!(env.vault_root(), Utf8Path::new("/vault"));
        assert_eq!(env.config().links.alias_field.as_deref(), Some("aliases"));
    }

    #[test]
    fn compiled_patterns_track_the_rules() {
        let yaml = r#"
validate:
  rules:
    - name: task
      match:
        path: "tasks/*.md"
"#;
        let env = build_env(yaml);
        assert_eq!(env.config().validate.rules.len(), 1);
        assert_eq!(env.compiled().rules.len(), 1);
        assert!(env.compiled().rules[0].path.is_some());
    }

    #[test]
    fn clone_is_an_independent_value() {
        let env = build_env("");
        let cloned = env.clone();
        assert_eq!(cloned.vault_root(), env.vault_root());
    }
}
