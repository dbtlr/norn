//! Target triple detection and asset selection.

#[cfg(test)]
use crate::self_update::manifest::Artifact;
use crate::self_update::manifest::DistManifest;

/// Compile-time target triple for the running binary. `None` if we built for
/// a target cargo-dist does not produce a release artifact for (developer
/// builds, unusual targets).
pub const TARGET_TRIPLE: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "aarch64",
    target_env = "musl"
)) {
    Some("aarch64-unknown-linux-musl")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "musl"
)) {
    Some("x86_64-unknown-linux-musl")
} else {
    None
};

/// Find the artifact in the manifest matching `triple`. Returns the matching
/// (name, sha256) pair on success.
pub fn select_asset<'a>(manifest: &'a DistManifest, triple: &str) -> Option<(&'a str, &'a str)> {
    manifest.artifacts.values().find_map(|art| {
        if art.kind == "executable-zip" && art.target_triples.iter().any(|t| t == triple) {
            let sha = art.checksums.get("sha256")?;
            Some((art.name.as_str(), sha.as_str()))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn artifact(name: &str, kind: &str, triple: &str, sha: &str) -> (String, Artifact) {
        let mut checksums = BTreeMap::new();
        checksums.insert("sha256".to_string(), sha.to_string());
        (
            name.to_string(),
            Artifact {
                name: name.to_string(),
                kind: kind.to_string(),
                target_triples: vec![triple.to_string()],
                checksums,
            },
        )
    }

    fn manifest_with(artifacts: Vec<(String, Artifact)>) -> DistManifest {
        DistManifest {
            announcement_tag: "v0.33.1".to_string(),
            artifacts: artifacts.into_iter().collect(),
        }
    }

    #[test]
    fn select_asset_finds_matching_triple() {
        let m = manifest_with(vec![
            artifact("a", "executable-zip", "aarch64-apple-darwin", "AA"),
            artifact("b", "executable-zip", "x86_64-apple-darwin", "BB"),
        ]);
        assert_eq!(select_asset(&m, "x86_64-apple-darwin"), Some(("b", "BB")));
    }

    #[test]
    fn select_asset_ignores_checksum_kind() {
        let m = manifest_with(vec![artifact(
            "a.sha256",
            "checksum",
            "aarch64-apple-darwin",
            "ZZ",
        )]);
        assert_eq!(select_asset(&m, "aarch64-apple-darwin"), None);
    }

    #[test]
    fn select_asset_returns_none_for_unknown_triple() {
        let m = manifest_with(vec![artifact(
            "a",
            "executable-zip",
            "aarch64-apple-darwin",
            "AA",
        )]);
        assert_eq!(select_asset(&m, "some-other-triple"), None);
    }
}
