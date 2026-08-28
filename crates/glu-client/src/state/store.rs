use crate::state::{
    declaration::Declaration,
    installed::InstalledState,
    receipts::{self, GluInstallReceipt, ReceiptStatus},
};
use anyhow::{bail, Context, Result};
use glu_core::{
    DependencyRequires, InstalledPackage, KegVersion, PackageId, PackageKey, PackageLinkMetadata,
    PackageName, PackageSelector, Prefix, RuntimeDependencyRequirement,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Durable installed-state store for one prefix.
///
/// This type owns local state records and record IO: the top-level declaration
/// (`glu.json`) and package-local receipts (`.glu/receipt.json`). It also owns
/// the one Cellar scan used to build an `InstalledState` snapshot. It does not
/// perform package filesystem transitions, prefix projection, opt linking, or
/// install/remove ordering; those remain in transition and linker code.
#[derive(Debug, Clone)]
pub struct InstalledStateStore {
    prefix: Prefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledArtifact {
    pub name: PackageName,
    pub keg_version: KegVersion,
    pub sha256: String,
}

impl InstalledStateStore {
    pub fn new(prefix: Prefix) -> Self {
        Self { prefix }
    }

    /// Loads `<prefix>/glu.json`. `None` means the declaration has not been
    /// created yet.
    pub fn read_declaration(&self) -> Result<Option<Declaration>> {
        Declaration::load(&self.prefix)
    }

    /// Loads `<prefix>/glu.json`, returning an empty declaration when absent.
    pub fn load_declaration(&self) -> Result<Declaration> {
        Ok(self.read_declaration()?.unwrap_or_default())
    }

    /// Atomically replaces `<prefix>/glu.json` with `declaration`.
    pub fn write_declaration(&self, declaration: &Declaration) -> Result<()> {
        declaration.write(&self.prefix)
    }

    fn receipt_path(keg_path: &Path) -> PathBuf {
        receipts::receipt_path_for_keg(keg_path)
    }

    /// Whether a keg has a package-local receipt record.
    pub(crate) fn receipt_exists_for_keg(keg_path: &Path) -> Result<bool> {
        let path = Self::receipt_path(keg_path);
        path.try_exists()
            .with_context(|| format!("checking {}", path.display()))
    }

    #[cfg(test)]
    pub(crate) fn receipt_path_for_keg(keg_path: &Path) -> PathBuf {
        Self::receipt_path(keg_path)
    }

    /// Reads the receipt stored inside one keg.
    pub fn read_receipt_for_keg(&self, keg_path: &Path) -> Result<GluInstallReceipt> {
        Self::read_receipt_at_keg(keg_path)
    }

    /// Atomically writes or rewrites the receipt stored inside one keg.
    pub fn write_receipt_for_keg(
        &self,
        keg_path: &Path,
        receipt: &GluInstallReceipt,
    ) -> Result<()> {
        Self::write_receipt_at_keg(keg_path, receipt)
    }

    /// Linker-facing metadata reconstructed from the keg's receipt. This keeps
    /// activation offline while keeping receipt parsing behind the store.
    pub fn read_link_metadata_for_keg(&self, keg_path: &Path) -> Result<PackageLinkMetadata> {
        let receipt = self.read_receipt_for_keg(keg_path)?;
        Ok(receipt.link_metadata())
    }

    /// Updates only the receipt's linked bit. Activation/deactivation own the
    /// filesystem transition; the store owns the receipt mutation.
    pub fn set_receipt_linked_for_keg(&self, keg_path: &Path, linked: bool) -> Result<()> {
        let mut receipt = self.read_receipt_for_keg(keg_path)?;
        receipt.install.linked = linked;
        self.write_receipt_for_keg(keg_path, &receipt)
    }

    /// Reads the receipt stored inside one keg. Associated form for callers
    /// that already have the keg path and do not otherwise need the prefix.
    pub fn read_receipt_at_keg(keg_path: &Path) -> Result<GluInstallReceipt> {
        receipts::read_receipt_file(&Self::receipt_path(keg_path))
    }

    /// Atomically writes or rewrites the receipt stored inside one keg.
    /// Associated form for callers that already have the keg path and do not
    /// otherwise need the prefix.
    pub fn write_receipt_at_keg(keg_path: &Path, receipt: &GluInstallReceipt) -> Result<()> {
        receipts::write_receipt_file(&Self::receipt_path(keg_path), receipt)
    }

    /// Loads declaration and installed receipt records into an in-memory query
    /// snapshot.
    pub fn load_installed_state(&self) -> Result<InstalledState> {
        let declaration = self.load_declaration()?;
        self.load_installed_state_with_declaration(&declaration)
    }

    /// Loads installed receipt records into an in-memory query snapshot using
    /// an already-loaded declaration. This avoids redundant `glu.json` reads
    /// when a command needs both records.
    pub(crate) fn load_installed_state_with_declaration(
        &self,
        declaration: &Declaration,
    ) -> Result<InstalledState> {
        self.load_installed_state_with_warnings(declaration, &mut Vec::new())
    }

    pub(crate) fn load_installed_state_with_warnings(
        &self,
        declaration: &Declaration,
        warnings: &mut Vec<String>,
    ) -> Result<InstalledState> {
        InstalledState::from_loaded_packages(
            self.load_installed_packages(warnings)?,
            self.prefix.clone(),
            declaration.names(),
            declaration.deactivated_names(),
        )
    }

    /// Returns the package identity attached to each installed bottle digest.
    /// Receipt IO remains inside the store; malformed and incomplete receipts
    /// are ignored just as they are for the installed-state snapshot.
    pub fn load_installed_artifacts(&self) -> Result<Vec<InstalledArtifact>> {
        Ok(self
            .load_complete_receipts(&mut Vec::new())?
            .into_iter()
            .map(|receipt| InstalledArtifact {
                name: receipt.package.name,
                keg_version: receipt.package.keg_version,
                sha256: receipt.artifact.sha256,
            })
            .collect())
    }

    fn load_installed_packages(&self, warnings: &mut Vec<String>) -> Result<Vec<InstalledPackage>> {
        let receipts = self.load_complete_receipts(warnings)?;
        let mut packages: Vec<InstalledPackage> = receipts
            .iter()
            .map(GluInstallReceipt::installed_package)
            .collect();

        // Receipts persist what each package asked for, not which package
        // happened to provide that selector at install time. Resolve those
        // selectors against the complete installed package set when building
        // the in-memory state. Exact installed names always beat aliases.
        let mut providers: HashMap<PackageSelector, (PackageKey, PackageId)> = HashMap::new();
        let mut exact_names = HashSet::new();
        for package in &packages {
            let selector = PackageSelector(package.name.0.clone());
            exact_names.insert(selector.clone());
            match providers.get(&selector) {
                Some((key, _)) if key != &package.package_key => bail!(
                    "installed exact selector '{}' belongs to both {} and {}",
                    selector.0,
                    key.0,
                    package.package_key.0
                ),
                _ => {
                    providers.insert(selector, (package.package_key.clone(), package.id.clone()));
                }
            }
        }
        for package in &packages {
            for selector in package.aliases.iter().chain(&package.oldnames) {
                if exact_names.contains(selector) {
                    continue;
                }
                match providers.get(selector) {
                    Some((key, _)) if key != &package.package_key => bail!(
                        "installed selector '{}' belongs to both {} and {}",
                        selector.0,
                        key.0,
                        package.package_key.0
                    ),
                    _ => {
                        providers.insert(
                            selector.clone(),
                            (package.package_key.clone(), package.id.clone()),
                        );
                    }
                }
            }
        }

        for (package, receipt) in packages.iter_mut().zip(&receipts) {
            package.deps = receipt
                .install
                .deps
                .iter()
                .map(|requested_as| {
                    let (package_key, package_id) =
                        providers.get(requested_as).ok_or_else(|| {
                            anyhow::anyhow!(
                                "installed package {} requires missing selector {}",
                                receipt.package.name.0,
                                requested_as.0
                            )
                        })?;
                    let requires = receipt
                        .install
                        .min_versions
                        .get(&requested_as.0)
                        .map(|minimum| DependencyRequires {
                            version: minimum.version.clone(),
                            revision: minimum.revision.unwrap_or(0),
                        })
                        .unwrap_or(DependencyRequires {
                            version: String::new(),
                            revision: 0,
                        });
                    Ok(RuntimeDependencyRequirement {
                        package_key: package_key.clone(),
                        package: package_id.clone(),
                        requested_as: requested_as.clone(),
                        requires,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(packages)
    }

    /// Reads every complete keg receipt under `prefix/Cellar`. Missing Cellar
    /// is an empty state. Malformed receipts are skipped so one damaged keg
    /// does not brick read-only commands.
    fn load_complete_receipts(&self, warnings: &mut Vec<String>) -> Result<Vec<GluInstallReceipt>> {
        let cellar = self.prefix.0.join("Cellar");
        if !cellar.exists() {
            return Ok(Vec::new());
        }

        let mut receipts = Vec::new();
        for rack in
            fs::read_dir(&cellar).with_context(|| format!("reading {}", cellar.display()))?
        {
            let rack = rack?;
            if !rack.file_type()?.is_dir() {
                continue;
            }

            for keg in fs::read_dir(rack.path())? {
                let keg = keg?;
                if !keg.file_type()?.is_dir() {
                    continue;
                }

                let receipt_path = Self::receipt_path(&keg.path());
                if !receipt_path.exists() {
                    continue;
                }

                let Ok(bytes) = fs::read(&receipt_path) else {
                    continue;
                };
                // A malformed/truncated receipt (e.g. an interrupted write, or
                // manual tampering) must not brick every command. Skip this keg
                // rather than erroring the whole load — read-only queries keep
                // working, and the keg is treated as absent.
                let receipt: GluInstallReceipt = match serde_json::from_slice(&bytes) {
                    Ok(receipt) => receipt,
                    Err(_) => {
                        warnings.push(format!(
                            "glu: warning: skipped unreadable install receipt {}",
                            receipt_path.display()
                        ));
                        continue;
                    }
                };
                if receipt.status != ReceiptStatus::Complete {
                    continue;
                }

                // A receipt's stored keg path is what later removal and unlink
                // operate on. Ignore any receipt whose path
                // escapes the Cellar so it can never drive a `remove_dir_all`
                // outside the prefix. Skipping keeps one bad receipt from
                // bricking every command.
                if !keg_path_inside_cellar(&self.prefix, &receipt.paths.keg) {
                    continue;
                }
                receipts.push(receipt);
            }
        }

        Ok(receipts)
    }
}

/// Whether a stored receipt keg path lives directly under
/// `Cellar/<name>/<ver>` (at least two components below the Cellar rack root)
/// with no `..`/`.` traversal. `starts_with` alone is not enough, since
/// `Cellar/../etc` lexically escapes.
fn keg_path_inside_cellar(prefix: &Prefix, path: &Path) -> bool {
    let cellar = prefix.0.join("Cellar");
    path.starts_with(&cellar)
        && path
            .strip_prefix(&cellar)
            .map(|rel| rel.components().count() >= 2)
            .unwrap_or(false)
        && !has_traversal(path)
}

fn has_traversal(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
        ReceiptStatus,
    };
    use glu_core::{ArtifactId, KegVersion, PackageId, PackageName};

    #[test]
    fn keg_path_inside_cellar_accepts_normal_kegs() {
        let prefix = Prefix(PathBuf::from("/opt/glustore"));
        assert!(keg_path_inside_cellar(
            &prefix,
            Path::new("/opt/glustore/Cellar/vips/8.19.0")
        ));
        assert!(keg_path_inside_cellar(
            &prefix,
            Path::new("/opt/glustore/Cellar/openssl@3/3.0.13_1")
        ));
    }

    #[test]
    fn keg_path_inside_cellar_rejects_escapes() {
        let prefix = Prefix(PathBuf::from("/opt/glustore"));
        assert!(!keg_path_inside_cellar(
            &prefix,
            Path::new("/opt/glustore/var/glu")
        ));
        assert!(!keg_path_inside_cellar(&prefix, Path::new("/etc/passwd")));
        assert!(!keg_path_inside_cellar(
            &prefix,
            Path::new("/opt/glustore/Cellar/vips")
        ));
        assert!(!keg_path_inside_cellar(
            &prefix,
            Path::new("/opt/glustore/Cellar/../etc")
        ));
    }

    fn write_good_receipt(prefix: &Prefix, name: &str, version: &str) {
        let keg = prefix.0.join("Cellar").join(name).join(version);
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId("pkg:test".to_string()),
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
                sha256: "a".repeat(64),
                bottle_tag: "arm64_test".to_string(),
                cellar: ":any".to_string(),
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
                keg_only: false,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                min_versions: Default::default(),
            },
        };
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn malformed_receipt_is_skipped_not_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "good", "1.0");
        let bad = prefix.0.join("Cellar/bad/1.0");
        std::fs::create_dir_all(bad.join(".glu")).unwrap();
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&bad),
            b"{\"schema\": \"truncated",
        )
        .unwrap();

        let state = InstalledStateStore::new(prefix)
            .load_installed_state()
            .unwrap();
        assert_eq!(state.names(), vec![PackageName("good".to_string())]);
    }

    #[test]
    fn installed_artifacts_are_projected_from_store_owned_receipt_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "demo", "1.0");

        let artifacts = InstalledStateStore::new(prefix)
            .load_installed_artifacts()
            .unwrap();

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name.0, "demo");
        assert_eq!(artifacts[0].keg_version.0, "1.0");
        assert_eq!(artifacts[0].sha256, "a".repeat(64));
    }

    #[test]
    fn atomic_receipt_write_leaves_no_temp() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "good", "1.0");
        let keg = prefix.0.join("Cellar/good/1.0");
        for entry in std::fs::read_dir(&keg).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.ends_with(".json.tmp"),
                "atomic write left temp: {name}"
            );
        }
    }
}
