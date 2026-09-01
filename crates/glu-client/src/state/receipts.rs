use anyhow::{Context, Result};
use glu_core::{
    ArtifactId, Exposure, InstalledPackage, KegVersion, MinimumVersion, PackageId, PackageKey,
    PackageLinkMetadata, PackageName, PackageSelector,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Package-local receipt path, relative to one installed package directory.
pub(super) const RECEIPT_RELATIVE_PATH: &str = ".glu/receipt.json";
const RECEIPT_SCHEMA: &str = "glu.install-receipt.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GluInstallReceipt {
    pub schema: String,
    pub status: ReceiptStatus,
    pub package: ReceiptPackage,
    pub artifact: ReceiptArtifact,
    #[serde(default)]
    pub sizes: ReceiptSizes,
    pub paths: ReceiptPaths,
    pub links: ReceiptLinkNames,
    pub install: ReceiptInstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Incomplete,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptPackage {
    pub id: PackageId,
    pub package_key: PackageKey,
    pub name: PackageName,
    pub aliases: Vec<PackageSelector>,
    pub oldnames: Vec<PackageSelector>,
    pub version: String,
    pub revision: u32,
    pub keg_version: KegVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptArtifact {
    pub id: ArtifactId,
    pub sha256: String,
    pub bottle_tag: String,
    pub cellar: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptLinkNames {
    /// Additional stable opt-link names required by this installed package.
    /// These are filesystem facts, separate from selector aliases.
    pub opt_names: Vec<PackageName>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptPaths {
    pub keg: PathBuf,
    pub opt: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReceiptSizes {
    #[serde(default)]
    pub download_bytes: Option<u64>,
    #[serde(default)]
    pub installed_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptInstall {
    pub exposure: Exposure,
    pub linked: bool,
    /// Link overwrite patterns from the resolved package metadata. These are
    /// required to reconstruct activation locally without resolving online.
    pub link_overwrite: Vec<String>,
    /// Exact direct dependency spellings from the resolved package. Provider
    /// identity is deliberately not persisted on the dependent's receipt.
    pub deps: Vec<PackageSelector>,
    /// This installed package's complete flattened minimum-version map.
    /// It is separate from direct graph topology.
    pub dependency_requirements: BTreeMap<PackageKey, MinimumVersion>,
}

impl GluInstallReceipt {
    pub fn installed_package(&self) -> InstalledPackage {
        InstalledPackage {
            id: self.package.id.clone(),
            package_key: self.package.package_key.clone(),
            name: self.package.name.clone(),
            aliases: self.package.aliases.clone(),
            oldnames: self.package.oldnames.clone(),
            version: self.package.version.clone(),
            revision: self.package.revision,
            keg_version: self.package.keg_version.clone(),
            keg_path: self.paths.keg.clone(),
            opt_path: self.paths.opt.clone(),
            exposure: self.install.exposure.clone(),
            linked: self.install.linked,
            // Provider identities are resolved across the complete receipt
            // set by InstalledStateStore. One receipt alone only knows the
            // exact dependency spellings it persisted.
            deps: Vec::new(),
            dependency_requirements: self.install.dependency_requirements.clone(),
            download_bytes: self.sizes.download_bytes,
            installed_bytes: self.sizes.installed_bytes,
        }
    }

    /// Reconstructs the small package metadata contract needed by the linker
    /// from local receipt facts. Activation must be offline and exact: it needs
    /// opt-link names, keg-only state, and overwrite policy, but never registry, artifact,
    /// version, dependency, or postinstall data.
    pub fn link_metadata(&self) -> PackageLinkMetadata {
        PackageLinkMetadata {
            name: self.package.name.clone(),
            opt_names: self.links.opt_names.clone(),
            exposure: self.install.exposure.clone(),
            link_overwrite: self.install.link_overwrite.clone(),
        }
    }
}

pub(super) fn receipt_path_for_keg(keg_path: &Path) -> PathBuf {
    keg_path.join(RECEIPT_RELATIVE_PATH)
}

pub(super) fn read_receipt_file(path: &Path) -> Result<GluInstallReceipt> {
    #[derive(Deserialize)]
    struct ReceiptSchema {
        schema: String,
    }

    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let receipt_schema: ReceiptSchema =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    if receipt_schema.schema != RECEIPT_SCHEMA {
        anyhow::bail!(
            "unsupported package metadata schema {} in {}; update glu before modifying this installation",
            receipt_schema.schema,
            path.display()
        );
    }

    serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))
}

/// Binds persisted identity and paths to the directory that physically owns
/// the receipt. Callers must use the scanned path for filesystem operations;
/// receipt-controlled paths are never authority.
pub(super) fn validate_receipt_location(
    receipt: &GluInstallReceipt,
    installed_path: &Path,
) -> Result<()> {
    if receipt.paths.keg != installed_path
        && !paths_resolve_to_same_directory(&receipt.paths.keg, installed_path)
    {
        anyhow::bail!(
            "package metadata at {} claims installation path {}",
            installed_path.display(),
            receipt.paths.keg.display()
        );
    }

    let physical_name = installed_path
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid installed package path {}",
                installed_path.display()
            )
        })?;
    if physical_name != receipt.package.name.0.as_str() {
        anyhow::bail!(
            "package metadata at {} names package {}",
            installed_path.display(),
            receipt.package.name.0
        );
    }

    let physical_version = installed_path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "invalid installed package path {}",
            installed_path.display()
        )
    })?;
    if physical_version != receipt.package.keg_version.0.as_str() {
        anyhow::bail!(
            "package metadata at {} names installed version {}",
            installed_path.display(),
            receipt.package.keg_version.0
        );
    }

    Ok(())
}

fn paths_resolve_to_same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_package_decodes_identity() {
        let json = r#"{
            "id": "pkg:test/foo@1.0",
            "package_key": "package:foo",
            "name": "foo",
            "aliases": [],
            "oldnames": [],
            "version": "1.0",
            "revision": 0,
            "keg_version": "1.0"
        }"#;

        let package: ReceiptPackage = serde_json::from_str(json).unwrap();

        assert!(package.aliases.is_empty());
        assert!(package.oldnames.is_empty());
    }

    #[test]
    fn receipt_keeps_requested_dependencies_and_flattened_minimum_versions_separate() {
        let install = ReceiptInstall {
            exposure: Exposure::Global,
            linked: true,
            link_overwrite: Vec::new(),
            deps: vec![PackageSelector("llvm@22".to_string())],
            dependency_requirements: BTreeMap::from([
                (
                    PackageKey("package:llvm".to_string()),
                    MinimumVersion {
                        version: "22.1.0".to_string(),
                        revision: Some(2),
                    },
                ),
                (
                    PackageKey("package:zstd".to_string()),
                    MinimumVersion {
                        version: "1.5.7".to_string(),
                        revision: None,
                    },
                ),
            ]),
        };

        let json = serde_json::to_value(&install).unwrap();
        let decoded: ReceiptInstall = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(decoded.deps, vec![PackageSelector("llvm@22".to_string())]);
        assert_eq!(
            decoded.dependency_requirements[&PackageKey("package:llvm".to_string())].revision,
            Some(2)
        );
        assert_eq!(
            decoded.dependency_requirements[&PackageKey("package:zstd".to_string())].revision,
            None
        );
        assert_eq!(json["deps"][0], "llvm@22");
        assert!(json["dependency_requirements"]["package:zstd"]
            .get("revision")
            .is_none());
    }

    #[test]
    fn receipt_install_rejects_the_old_boolean_only_shape() {
        let error = serde_json::from_str::<ReceiptInstall>(
            r#"{"keg_only":true,"linked":true,"link_overwrite":[],"deps":[],"dependency_requirements":{}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing field `exposure`"));
    }

    #[test]
    fn receipt_reader_rejects_unsupported_schema_with_update_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        std::fs::write(
            &path,
            r#"{
                "schema":"glu.install-receipt.v0",
                "status":"complete",
                "package":{
                    "id":"pkg:test/foo@1.0",
                    "package_key":"package:foo",
                    "name":"foo",
                    "aliases":[],
                    "oldnames":[],
                    "version":"1.0",
                    "revision":0,
                    "keg_version":"1.0"
                },
                "artifact":{
                    "id":"art:test/foo@1.0",
                    "sha256":"abc",
                    "bottle_tag":"arm64_test",
                    "cellar":"/tmp/Cellar"
                },
                "paths":{"keg":"/tmp/Cellar/foo/1.0","opt":"/tmp/opt/foo"},
                "links":{"opt_names":[]},
                "install":{"exposure":{"mode":"global"},"linked":true}
            }"#,
        )
        .unwrap();

        let error = read_receipt_file(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported package metadata schema glu.install-receipt.v0"));
        assert!(error.contains("update glu"));
    }

    #[test]
    fn receipt_defaults_omitted_sizes() {
        let json = r#"{
            "schema": "glu.install-receipt.v1",
            "status": "complete",
            "package": {
                "id": "pkg:test/foo@1.0",
                "package_key": "package:foo",
                "name": "foo",
                "aliases": [],
                "oldnames": [],
                "version": "1.0",
                "revision": 0,
                "keg_version": "1.0"
            },
            "artifact": {
                "id": "art:test/foo@1.0",
                "sha256": "abc",
                "bottle_tag": "arm64_test",
                "cellar": "/opt/glustore/Cellar"
            },
            "paths": {
                "keg": "/tmp/Cellar/foo/1.0",
                "opt": "/tmp/opt/foo"
            },
            "links": { "opt_names": [] },
            "install": {
                "exposure": {"mode": "global"},
                "linked": true,
                "link_overwrite": [],
                "deps": [],
                "dependency_requirements": {}
            }
        }"#;

        let receipt: GluInstallReceipt = serde_json::from_str(json).unwrap();

        assert_eq!(receipt.sizes.download_bytes, None);
        assert_eq!(receipt.sizes.installed_bytes, None);
    }
}
