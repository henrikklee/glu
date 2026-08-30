use crate::link::unlink::remove_keg;
use crate::state::{receipts::ReceiptStatus, store::InstalledStateStore};
use anyhow::{Context, Result};
use glu_core::{PackageName, Prefix};
use std::{fs, path::Path};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryCleanup {
    pub removed_staging: bool,
    pub removed_incomplete_kegs: usize,
}

pub fn cleanup_interrupted(prefix: &Prefix) -> Result<RecoveryCleanup> {
    let mut cleanup = RecoveryCleanup::default();
    if remove_staging(prefix)? {
        cleanup.removed_staging = true;
    }
    cleanup.removed_incomplete_kegs = remove_incomplete_kegs(prefix)?;
    Ok(cleanup)
}

fn remove_staging(prefix: &Prefix) -> Result<bool> {
    let staging = prefix.0.join("var/glu/staging");
    if !staging.exists() {
        return Ok(false);
    }
    let metadata =
        fs::symlink_metadata(&staging).with_context(|| format!("reading {}", staging.display()))?;
    if metadata.is_dir() {
        fs::remove_dir_all(&staging).with_context(|| format!("removing {}", staging.display()))?;
    } else {
        fs::remove_file(&staging).with_context(|| format!("removing {}", staging.display()))?;
    }
    Ok(true)
}

fn remove_incomplete_kegs(prefix: &Prefix) -> Result<usize> {
    let cellar = prefix.0.join("Cellar");
    if !cellar.exists() {
        return Ok(0);
    }

    let mut removed = 0;
    for rack in fs::read_dir(&cellar).with_context(|| format!("reading {}", cellar.display()))? {
        let rack = rack?;
        if !rack.file_type()?.is_dir() {
            continue;
        }
        let name = PackageName(rack.file_name().to_string_lossy().into_owned());
        for keg in fs::read_dir(rack.path())? {
            let keg = keg?;
            if !keg.file_type()?.is_dir() {
                continue;
            }
            let keg_path = keg.path();
            if !receipt_is_incomplete(&keg_path)? {
                continue;
            }
            remove_keg(prefix, &name, &keg_path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn receipt_is_incomplete(keg: &Path) -> Result<bool> {
    if !InstalledStateStore::receipt_exists_for_keg(keg)? {
        return Ok(false);
    }
    let receipt = match InstalledStateStore::read_receipt_at_keg(keg) {
        Ok(receipt) => receipt,
        Err(_) => return Ok(false),
    };
    Ok(receipt.status == ReceiptStatus::Incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
    };
    use glu_core::{ArtifactId, KegVersion, PackageId};
    use tempfile::TempDir;

    #[cfg(unix)]
    fn symlink(src: &Path, dst: &Path) {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    fn write_receipt(prefix: &Prefix, name: &str, version: &str, status: ReceiptStatus) {
        let keg = prefix.0.join("Cellar").join(name).join(version);
        fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:test/{name}@{version}")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: KegVersion(version.to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: prefix.0.join("Cellar").to_string_lossy().to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.0.join("opt").join(name),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                exposure: glu_core::Exposure::Global,
                linked: true,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn cleanup_removes_stale_staging_root() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let staging = prefix.0.join("var/glu/staging/node-1.0");
        fs::create_dir_all(&staging).unwrap();

        let cleanup = cleanup_interrupted(&prefix).unwrap();

        assert!(cleanup.removed_staging);
        assert!(!prefix.0.join("var/glu/staging").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_removes_incomplete_keg_and_links() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "node", "1.0", ReceiptStatus::Incomplete);
        let keg = prefix.0.join("Cellar/node/1.0");
        fs::create_dir_all(keg.join("bin")).unwrap();
        fs::write(keg.join("bin/node"), b"node").unwrap();
        symlink(&keg.join("bin/node"), &prefix.0.join("bin/node"));
        symlink(&keg, &prefix.0.join("opt/node"));
        symlink(&keg, &prefix.0.join("var/homebrew/linked/node"));

        let cleanup = cleanup_interrupted(&prefix).unwrap();

        assert_eq!(cleanup.removed_incomplete_kegs, 1);
        assert!(!keg.exists());
        assert!(!prefix.0.join("bin/node").exists());
        assert!(!prefix.0.join("opt/node").exists());
        assert!(!prefix.0.join("var/homebrew/linked/node").exists());
    }

    #[test]
    fn cleanup_keeps_complete_keg() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "node", "1.0", ReceiptStatus::Complete);
        let keg = prefix.0.join("Cellar/node/1.0");

        let cleanup = cleanup_interrupted(&prefix).unwrap();

        assert_eq!(cleanup.removed_incomplete_kegs, 0);
        assert!(keg.exists());
    }
}
