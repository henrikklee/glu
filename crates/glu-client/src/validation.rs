use crate::{
    path_component::is_safe_path_component,
    postinstall::structured::validate_structured_postinstall_steps,
};
use anyhow::{bail, Result};
use glu_core::{InstallManifest, Prefix};

pub fn validate_client_support(manifest: &InstallManifest, prefix: &Prefix) -> Result<()> {
    validate_prefix(prefix)?;
    validate_fixed_cellar_prefix(manifest, prefix)?;
    validate_manifest_path_components(manifest)?;

    for (package_id, package) in &manifest.packages {
        if package.install.post_install_defined && package.install.post_install_steps.is_empty() {
            bail!(
                "{} defines an unsupported Ruby postinstall. This client cannot install it yet.",
                package_id.0
            );
        }

        validate_structured_postinstall_steps(&package_id.0, &package.install.post_install_steps)?;
        if package
            .install
            .link_overwrite
            .iter()
            .any(|path| path.is_empty())
        {
            bail!("{} has an empty link_overwrite entry", package_id.0);
        }
    }

    for (artifact_id, artifact) in &manifest.artifacts {
        if artifact.url.is_empty() {
            bail!("artifact {} has an empty URL", artifact_id.0);
        }
        if artifact.sha256.len() != 64 || !artifact.sha256.chars().all(|ch| ch.is_ascii_hexdigit())
        {
            bail!("artifact {} has an invalid sha256", artifact_id.0);
        }
        validate_cellar(&artifact.cellar)?;
    }

    Ok(())
}

/// Every manifest identifier that becomes a path component must be a safe
/// single segment — the registry is the trust root, but a mirror/compromise
/// must not be able to direct the client's writes or removals outside the
/// prefix (see `commit_prepared_keg`'s Cellar guard and receipt confinement in
/// `state/installed.rs`).
fn validate_manifest_path_components(manifest: &InstallManifest) -> Result<()> {
    for (package_id, package) in &manifest.packages {
        for (label, value) in [
            ("name", &package.name.0),
            ("version", &package.version),
            ("keg_version", &package.keg_version.0),
        ] {
            if !is_safe_path_component(value) {
                bail!("{} has unsafe package {label} {:?}", package_id.0, value);
            }
        }
        for alias in package.aliases.iter().chain(package.oldnames.iter()) {
            if !is_safe_path_component(&alias.0) {
                bail!("{} has unsafe alias {:?}", package_id.0, alias.0);
            }
        }
    }
    Ok(())
}

fn validate_prefix(prefix: &Prefix) -> Result<()> {
    if prefix.0.as_os_str().is_empty() {
        bail!("empty install prefix");
    }
    Ok(())
}

fn validate_fixed_cellar_prefix(manifest: &InstallManifest, prefix: &Prefix) -> Result<()> {
    let prefix = prefix.0.to_string_lossy();
    let has_fixed_cellar = manifest
        .artifacts
        .values()
        .any(|artifact| artifact.cellar == "/opt/homebrew/Cellar");
    if has_fixed_cellar && prefix.len() != "/opt/homebrew".len() {
        bail!(
            "fixed-cellar bottles require an equal-length prefix; use /opt/glustore or another 13-byte prefix"
        );
    }
    Ok(())
}

fn validate_cellar(cellar: &str) -> Result<()> {
    match cellar {
        ":any" | ":any_skip_relocation" | "/opt/homebrew/Cellar" => Ok(()),
        other => bail!("unsupported bottle cellar value: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, KegVersion, PackageId, PackageInstallMetadata, PackageName, ResolveRequestEcho,
        ResolvedArtifact, ResolvedPackage, Target,
    };
    use std::collections::BTreeMap;

    fn manifest_with_version(version: &str) -> InstallManifest {
        let package_id = PackageId("pkg".to_string());
        let artifact_id = ArtifactId("artifact".to_string());
        InstallManifest {
            schema: "v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![glu_core::PackageSelector("pkg".to_string())],
                target: Target("arm64_tahoe".to_string()),
                slim: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("pkg".to_string()),
                package_key: glu_core::PackageKey("package:pkg".to_string()),
                package: package_id.clone(),
            }],
            packages: BTreeMap::from([(
                package_id,
                ResolvedPackage {
                    package_key: glu_core::PackageKey("package:pkg".to_string()),
                    name: PackageName("pkg".to_string()),
                    aliases: vec![],
                    oldnames: vec![],
                    version: version.to_string(),
                    revision: 0,
                    keg_version: KegVersion(version.to_string()),
                    deps: vec![],
                    dependency_requirements: Default::default(),
                    artifact: artifact_id.clone(),
                    install: PackageInstallMetadata {
                        opt_names: Vec::new(),
                        keg_only: false,
                        link_overwrite: vec![],
                        post_install_defined: false,
                        post_install_steps: vec![],
                        postinstall_network_access_allowed: true,
                    },
                },
            )]),
            artifacts: BTreeMap::from([(
                artifact_id,
                ResolvedArtifact {
                    url: "https://ghcr.io/v2/homebrew/core/pkg/blobs/sha256:abc".to_string(),
                    sha256: "0".repeat(64),
                    bytes: Some(1),
                    bottle_tag: "arm64_tahoe".to_string(),
                    cellar: ":any".to_string(),
                    built_on: None,
                },
            )]),
        }
    }

    #[test]
    fn manifest_version_must_be_safe_path_component() {
        let manifest = manifest_with_version("3.12/../../..");
        let err = validate_manifest_path_components(&manifest).unwrap_err();
        assert!(err.to_string().contains("unsafe package version"));
    }

    #[test]
    fn manifest_version_accepts_real_homebrew_shape() {
        let manifest = manifest_with_version("1.2.3_1");
        validate_manifest_path_components(&manifest).unwrap();
    }
}
