//! Exercises norn-config through its public exports only, as an external
//! consumer would — validating the re-export surface and an end-to-end
//! register → resolve lifecycle.

use std::fs;

use norn_config::{
    ConfigError, ConfigHome, RegisteredVault, Registry, ResolveInput, Resolved, ResolvedVia,
    VaultOverrides, BINDING_FILENAME, NORN_ROOT_ENV,
};

fn registry_in(dir: &std::path::Path) -> Registry {
    Registry::new(ConfigHome::new(dir.join("norn")))
}

#[test]
fn full_lifecycle_through_public_api() {
    assert_eq!(NORN_ROOT_ENV, "NORN_ROOT");
    assert_eq!(BINDING_FILENAME, ".norn.toml");

    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    let inner = vault.join("inner");
    fs::create_dir_all(&inner).unwrap();

    let reg = registry_in(tmp.path());

    // Register with an override.
    let overrides = VaultOverrides {
        logs: Some(std::path::PathBuf::from("/central/logs/docs")),
        ..VaultOverrides::default()
    };
    let created: RegisteredVault = reg.register("docs", &vault, overrides).unwrap();
    assert_eq!(created.name, "docs");
    assert_eq!(
        created.logs.as_deref(),
        Some(std::path::Path::new("/central/logs/docs"))
    );

    // Reverse lookup from a nested dir resolves to the registered root.
    let resolved: Resolved = reg.resolve(&ResolveInput::new(inner.clone())).unwrap();
    assert_eq!(resolved.via, ResolvedVia::ReverseLookup);
    assert_eq!(resolved.name.as_deref(), Some("docs"));

    // A committable binding above the cwd resolves by name.
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join(BINDING_FILENAME), "vault = \"docs\"\n").unwrap();
    let resolved = reg.resolve(&ResolveInput::new(repo.join("sub"))).unwrap();
    assert!(matches!(resolved.via, ResolvedVia::RepoBinding { .. }));

    // Unknown name surfaces a typed error.
    let err: ConfigError = reg
        .resolve(&ResolveInput {
            explicit_name: Some("ghost".into()),
            ..ResolveInput::new(tmp.path())
        })
        .unwrap_err();
    assert!(matches!(err, ConfigError::UnknownName { .. }));

    // Unregister leaves an empty registry.
    reg.unregister("docs").unwrap();
    assert!(reg.list().unwrap().is_empty());
}
