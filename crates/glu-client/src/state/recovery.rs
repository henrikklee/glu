use crate::link::unlink::remove_keg;
use crate::state::{receipts::ReceiptStatus, store::InstalledStateStore};
use anyhow::{Context, Result};
use glu_core::{PackageName, Prefix};
use std::fs;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryCleanup {
    pub removed_staging: bool,
    pub removed_incomplete_kegs: usize,
}

pub fn cleanup_interrupted(prefix: &Prefix) -> Result<RecoveryCleanup> {
    // Validate every persisted record before mutating anything. This prevents
    // cleanup from trusting one damaged record after already applying another.
    let incomplete = find_incomplete_installations(prefix)?;

    let mut cleanup = RecoveryCleanup::default();
    if remove_staging(prefix)? {
        cleanup.removed_staging = true;
    }
    for (name, path) in incomplete {
        remove_keg(prefix, &name, &path)?;
        cleanup.removed_incomplete_kegs += 1;
    }
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

fn find_incomplete_installations(
    prefix: &Prefix,
) -> Result<Vec<(PackageName, std::path::PathBuf)>> {
    let cellar = prefix.0.join("Cellar");
    let metadata = match fs::symlink_metadata(&cellar) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", cellar.display())),
    };
    if !metadata.is_dir() {
        anyhow::bail!(
            "cannot safely modify packages because package storage {} is not a real directory",
            cellar.display()
        );
    }

    let mut incomplete = Vec::new();
    for package_dir in
        fs::read_dir(&cellar).with_context(|| format!("reading {}", cellar.display()))?
    {
        let package_dir = package_dir?;
        if !package_dir.file_type()?.is_dir() {
            continue;
        }
        let name = PackageName(package_dir.file_name().to_string_lossy().into_owned());
        for installation in fs::read_dir(package_dir.path())? {
            let installation = installation?;
            if !installation.file_type()?.is_dir() {
                continue;
            }
            let path = installation.path();
            if !InstalledStateStore::receipt_exists_for_keg(&path)? {
                continue;
            }
            let receipt =
                InstalledStateStore::read_unbound_receipt_at_keg(&path).with_context(|| {
                    format!(
                        "cannot safely modify packages because metadata under {} is invalid",
                        path.display()
                    )
                })?;
            if receipt.status == ReceiptStatus::Incomplete {
                // Incomplete metadata may have moved with its package directory
                // before the final record was committed. Never trust its stored
                // path; cleanup removes only the directory found by this scan.
                incomplete.push((name.clone(), path));
                continue;
            }
            InstalledStateStore::validate_receipt_at_keg(&receipt, &path).with_context(|| {
                format!(
                    "cannot safely modify packages because metadata under {} is invalid",
                    path.display()
                )
            })?;
        }
    }
    Ok(incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
    };
    use glu_core::{ArtifactId, KegVersion, PackageId};
    use std::path::Path;
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

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_symlinked_package_storage() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let external = tmp.path().join("external");
        fs::create_dir_all(external.join("demo/1.0")).unwrap();
        fs::write(external.join("demo/1.0/keep"), b"keep").unwrap();
        fs::create_dir_all(&prefix.0).unwrap();
        symlink(&external, &prefix.0.join("Cellar"));

        let error = cleanup_interrupted(&prefix).unwrap_err().to_string();

        assert!(error.contains("package storage"));
        assert_eq!(fs::read(external.join("demo/1.0/keep")).unwrap(), b"keep");
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
    fn incomplete_cleanup_uses_only_the_physically_scanned_directory() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "node", "1.0", ReceiptStatus::Incomplete);
        let installed = prefix.0.join("Cellar/node/1.0");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        let receipt_path = InstalledStateStore::receipt_path_for_keg(&installed);
        let mut receipt: GluInstallReceipt =
            serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        receipt.paths.keg = outside.clone();
        receipt.package.name = PackageName("other".to_string());
        fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();

        let cleanup = cleanup_interrupted(&prefix).unwrap();

        assert_eq!(cleanup.removed_incomplete_kegs, 1);
        assert!(!installed.exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
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

    #[test]
    fn invalid_metadata_stops_cleanup_before_any_mutation() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "good", "1.0", ReceiptStatus::Incomplete);
        write_receipt(&prefix, "bad", "2.0", ReceiptStatus::Complete);
        let good = prefix.0.join("Cellar/good/1.0");
        let bad = prefix.0.join("Cellar/bad/2.0");
        fs::write(
            InstalledStateStore::receipt_path_for_keg(&bad),
            b"{not valid json",
        )
        .unwrap();
        let staging = prefix.0.join("var/glu/staging/new-package");
        fs::create_dir_all(&staging).unwrap();

        let error = cleanup_interrupted(&prefix).unwrap_err().to_string();

        assert!(error.contains("cannot safely modify packages"));
        assert!(good.exists());
        assert!(bad.exists());
        assert!(staging.exists());
    }
}
