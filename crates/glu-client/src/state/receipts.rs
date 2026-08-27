use anyhow::{Context, Result};
use glu_core::{
    ArtifactId, InstalledPackage, KegVersion, PackageId, PackageKey, PackageLinkMetadata,
    PackageName, PackageSelector, RuntimeDependencyRequirement,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Package-local receipt path, relative to one package/keg directory.
pub(super) const RECEIPT_RELATIVE_PATH: &str = ".glu/receipt.json";

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
    pub keg_only: bool,
    pub linked: bool,
    /// Link overwrite patterns from the resolved package metadata. These are
    /// required to reconstruct activation locally without resolving online.
    #[serde(default)]
    pub link_overwrite: Vec<String>,
    /// This package's own direct runtime dependencies, pinned to the exact
    /// resolved version/revision it was installed against — not the full
    /// transitive closure (unlike Homebrew's Tab
    /// `runtime_dependencies`). Direct-only is sufficient here: `rm`'s
    /// dependents check walks transitive reachability across installed
    /// receipts' direct edges at check time instead of storing a precomputed
    /// closure per receipt.
    #[serde(default)]
    pub deps: Vec<RuntimeDependencyRequirement>,
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
            keg_only: self.install.keg_only,
            linked: self.install.linked,
            deps: self.install.deps.clone(),
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
            keg_only: self.install.keg_only,
            link_overwrite: self.install.link_overwrite.clone(),
        }
    }
}

pub(super) fn receipt_path_for_keg(keg_path: &Path) -> PathBuf {
    keg_path.join(RECEIPT_RELATIVE_PATH)
}

pub(super) fn read_receipt_file(path: &Path) -> Result<GluInstallReceipt> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let receipt: GluInstallReceipt =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    if receipt.schema != "glu.install-receipt.v1" {
        anyhow::bail!(
            "unsupported install receipt schema {} in {}; reinstall this prefix with the current glu client",
            receipt.schema,
            path.display()
        );
    }
    Ok(receipt)
}

pub(super) fn write_receipt_file(path: &Path, receipt: &GluInstallReceipt) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(receipt).context("encoding install receipt")?;
    let tmp = path.with_extension("json.tmp");
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
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
    fn receipt_reader_rejects_unsupported_schema_with_reinstall_guidance() {
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
                "install":{"keg_only":false,"linked":true}
            }"#,
        )
        .unwrap();

        let error = read_receipt_file(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported install receipt schema glu.install-receipt.v0"));
        assert!(error.contains("reinstall this prefix"));
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
                "keg_only": false,
                "linked": true
            }
        }"#;

        let receipt: GluInstallReceipt = serde_json::from_str(json).unwrap();

        assert_eq!(receipt.sizes.download_bytes, None);
        assert_eq!(receipt.sizes.installed_bytes, None);
    }
}
