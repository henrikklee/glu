use anyhow::{bail, Context, Result};
use glu_core::PackageName;
use ring::digest;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
const RECEIPT_NAME: &str = "INSTALL_RECEIPT.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomebrewRoot {
    pub name: PackageName,
    pub linked: bool,
    pub(super) receipts: Vec<ReceiptFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReceiptFingerprint {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone)]
pub struct HomebrewSnapshot {
    pub source: PathBuf,
    pub roots: Vec<HomebrewRoot>,
    pub warnings: Vec<String>,
}

impl PartialEq for HomebrewSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.roots == other.roots
    }
}

impl Eq for HomebrewSnapshot {}

#[derive(Debug, Deserialize)]
struct HomebrewReceipt {
    #[serde(default)]
    installed_on_request: bool,
}

pub fn discover(source: &Path) -> Result<HomebrewSnapshot> {
    if !source.is_absolute() {
        bail!(
            "Homebrew source prefix must be an absolute path: {}",
            source.display()
        );
    }
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Homebrew installation not found at {}", source.display()))?;
    if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
        bail!(
            "Homebrew source prefix is not a real directory: {}",
            source.display()
        );
    }

    let cellar = source.join("Cellar");
    let cellar_metadata = fs::symlink_metadata(&cellar)
        .with_context(|| format!("Homebrew Cellar not found at {}", cellar.display()))?;
    if !cellar_metadata.is_dir() || cellar_metadata.file_type().is_symlink() {
        bail!(
            "Homebrew Cellar is not a real directory: {}",
            cellar.display()
        );
    }

    let mut roots: BTreeMap<PackageName, Vec<ReceiptFingerprint>> = BTreeMap::new();
    let mut warnings = Vec::new();
    for rack in fs::read_dir(&cellar).with_context(|| format!("reading {}", cellar.display()))? {
        let rack = rack?;
        if !rack.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = rack.file_name().to_str().map(str::to_owned) else {
            warnings.push(format!(
                "glu: warning: skipped Homebrew package with a non-UTF-8 name under {}",
                cellar.display()
            ));
            continue;
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let package_name = PackageName(name);
        for keg in fs::read_dir(rack.path())
            .with_context(|| format!("reading {}", rack.path().display()))?
        {
            let keg = keg?;
            if !keg.file_type()?.is_dir() {
                continue;
            }
            let receipt_path = keg.path().join(RECEIPT_NAME);
            let metadata = match fs::symlink_metadata(&receipt_path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    metadata
                }
                Ok(_) => {
                    warnings.push(format!(
                        "glu: warning: skipped non-regular Homebrew receipt {}",
                        receipt_path.display()
                    ));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!(
                        "glu: warning: could not inspect Homebrew receipt {}: {error}",
                        receipt_path.display()
                    ));
                    continue;
                }
            };
            if metadata.len() > MAX_RECEIPT_BYTES {
                warnings.push(format!(
                    "glu: warning: skipped oversized Homebrew receipt {}",
                    receipt_path.display()
                ));
                continue;
            }
            let bytes = match fs::read(&receipt_path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warnings.push(format!(
                        "glu: warning: could not read Homebrew receipt {}: {error}",
                        receipt_path.display()
                    ));
                    continue;
                }
            };
            let receipt: HomebrewReceipt = match serde_json::from_slice(&bytes) {
                Ok(receipt) => receipt,
                Err(error) => {
                    warnings.push(format!(
                        "glu: warning: skipped malformed Homebrew receipt {}: {error}",
                        receipt_path.display()
                    ));
                    continue;
                }
            };
            if !receipt.installed_on_request {
                continue;
            }
            roots
                .entry(package_name.clone())
                .or_default()
                .push(ReceiptFingerprint {
                    path: receipt_path,
                    sha256: sha256_hex(&bytes),
                });
        }
    }

    let mut roots = roots
        .into_iter()
        .map(|(name, mut receipts)| {
            receipts.sort_by(|a, b| a.path.cmp(&b.path));
            HomebrewRoot {
                linked: linked_marker_points_into_rack(source, &cellar, &name),
                name,
                receipts,
            }
        })
        .collect::<Vec<_>>();
    roots.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(HomebrewSnapshot {
        source: source.to_path_buf(),
        roots,
        warnings,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn linked_marker_points_into_rack(source: &Path, cellar: &Path, name: &PackageName) -> bool {
    let marker = source.join("var/homebrew/linked").join(&name.0);
    let Ok(metadata) = fs::symlink_metadata(&marker) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(target) = fs::canonicalize(&marker) else {
        return false;
    };
    let Ok(rack) = fs::canonicalize(cellar.join(&name.0)) else {
        return false;
    };
    target.starts_with(&rack)
        && target
            .strip_prefix(&rack)
            .map(|relative| relative.components().count() >= 1)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(source: &Path, name: &str, version: &str, requested: bool) -> PathBuf {
        let path = source
            .join("Cellar")
            .join(name)
            .join(version)
            .join(RECEIPT_NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::json!({"installed_on_request": requested}).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn discovers_requested_roots_and_deduplicates_versions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("homebrew");
        receipt(&source, "curl", "8.0", true);
        receipt(&source, "curl", "8.1", true);
        receipt(&source, "openssl@3", "3.0", false);

        let snapshot = discover(&source).unwrap();

        assert_eq!(snapshot.roots.len(), 1);
        assert_eq!(snapshot.roots[0].name.0, "curl");
        assert_eq!(snapshot.roots[0].receipts.len(), 2);
        assert!(!snapshot.roots[0].linked);
    }

    #[test]
    fn detects_a_live_linked_marker_into_the_package_rack() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("homebrew");
        let receipt = receipt(&source, "curl", "8.1", true);
        let keg = receipt.parent().unwrap();
        let marker = source.join("var/homebrew/linked/curl");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(keg, &marker).unwrap();

        let snapshot = discover(&source).unwrap();

        assert!(snapshot.roots[0].linked);
    }

    #[test]
    fn malformed_and_dependency_receipts_do_not_become_roots() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("homebrew");
        receipt(&source, "dependency", "1.0", false);
        let malformed = receipt(&source, "broken", "1.0", true);
        fs::write(malformed, "{").unwrap();

        let snapshot = discover(&source).unwrap();

        assert!(snapshot.roots.is_empty());
        assert_eq!(snapshot.warnings.len(), 1);
    }

    #[test]
    fn snapshot_changes_when_a_receipt_or_link_marker_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("homebrew");
        let path = receipt(&source, "curl", "8.1", true);
        let before = discover(&source).unwrap();
        fs::write(&path, r#"{"installed_on_request":true,"changed":true}"#).unwrap();
        let after_receipt = discover(&source).unwrap();
        assert_ne!(before, after_receipt);

        let marker = source.join("var/homebrew/linked/curl");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(path.parent().unwrap(), marker).unwrap();
        let after_link = discover(&source).unwrap();
        assert_ne!(after_receipt, after_link);
    }
}
