use crate::state::{
    atomic_write,
    declaration::Declaration,
    installed::InstalledState,
    receipts::{self, GluInstallReceipt, ReceiptStatus},
};
use anyhow::{bail, Context, Result};
use glu_core::{
    InstalledPackage, KegVersion, PackageDependency, PackageId, PackageKey, PackageLinkMetadata,
    PackageName, PackageSelector, Prefix,
};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptLoadPolicy {
    Tolerant,
    Strict,
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
        let path = Declaration::path(&self.prefix);
        let bytes = serde_json::to_vec_pretty(declaration).context("encoding declaration")?;
        atomic_write::replace_file_at_path(&path, &bytes)
    }

    /// Removes `<prefix>/glu.json` when present.
    pub fn remove_declaration(&self) -> Result<bool> {
        let path = Declaration::path(&self.prefix);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
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

    /// Reads and validates the metadata stored inside one installed package.
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

    /// Reads package metadata and binds it to its physical installation path.
    /// Associated form for callers that already have that path.
    pub fn read_receipt_at_keg(keg_path: &Path) -> Result<GluInstallReceipt> {
        let mut receipt = Self::read_unbound_receipt_at_keg(keg_path)?;
        Self::validate_receipt_at_keg(&receipt, keg_path)?;
        // Equivalent aliases such as macOS `/var` and `/private/var` are
        // accepted by identity, but downstream filesystem authority always
        // comes from the caller's physically discovered path.
        receipt.paths.keg = keg_path.to_path_buf();
        Ok(receipt)
    }

    pub(crate) fn validate_receipt_at_keg(
        receipt: &GluInstallReceipt,
        keg_path: &Path,
    ) -> Result<()> {
        receipts::validate_receipt_location(receipt, keg_path)
    }

    /// Reads schema-checked metadata before binding it to a final path. This is
    /// limited to staged/incomplete transition handling; installed-state reads
    /// must use `read_receipt_at_keg`.
    pub(crate) fn read_unbound_receipt_at_keg(keg_path: &Path) -> Result<GluInstallReceipt> {
        receipts::read_receipt_file(&Self::receipt_path(keg_path))
    }

    /// Atomically writes or rewrites the receipt stored inside one keg.
    /// Associated form for callers that already have the keg path and do not
    /// otherwise need the prefix.
    pub fn write_receipt_at_keg(keg_path: &Path, receipt: &GluInstallReceipt) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(receipt).context("encoding install receipt")?;
        let keg = atomic_write::open_directory(keg_path)?;
        let metadata = atomic_write::open_or_create_child_directory(
            &keg,
            keg_path,
            OsStr::new(".glu"),
            0o700,
        )?;
        atomic_write::replace_file(
            &metadata,
            &keg_path.join(".glu"),
            OsStr::new("receipt.json"),
            &bytes,
        )
    }

    /// Loads declaration and installed package records for a read-only query.
    /// Invalid package metadata is omitted with a warning.
    pub fn load_installed_state(&self) -> Result<InstalledState> {
        let declaration = self.load_declaration()?;
        self.load_installed_state_with_declaration(&declaration)
    }

    /// Loads installed package records into a read-only snapshot using an
    /// already-loaded declaration.
    pub(crate) fn load_installed_state_with_declaration(
        &self,
        declaration: &Declaration,
    ) -> Result<InstalledState> {
        self.load_installed_state_with_warnings(declaration, &mut Vec::new())
    }

    /// Loads state for a mutation. Unlike read-only queries, no malformed,
    /// unsupported, or location-mismatched package metadata may be ignored.
    pub(crate) fn load_installed_state_with_declaration_strict(
        &self,
        declaration: &Declaration,
    ) -> Result<InstalledState> {
        self.load_installed_state_with_policy(
            declaration,
            &mut Vec::new(),
            ReceiptLoadPolicy::Strict,
        )
    }

    pub(crate) fn load_installed_state_with_warnings(
        &self,
        declaration: &Declaration,
        warnings: &mut Vec<String>,
    ) -> Result<InstalledState> {
        self.load_installed_state_with_policy(declaration, warnings, ReceiptLoadPolicy::Tolerant)
    }

    fn load_installed_state_with_policy(
        &self,
        declaration: &Declaration,
        warnings: &mut Vec<String>,
        policy: ReceiptLoadPolicy,
    ) -> Result<InstalledState> {
        InstalledState::from_loaded_packages(
            self.load_installed_packages(warnings, policy)?,
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
            .load_complete_receipts(&mut Vec::new(), ReceiptLoadPolicy::Tolerant)?
            .into_iter()
            .map(|receipt| InstalledArtifact {
                name: receipt.package.name,
                keg_version: receipt.package.keg_version,
                sha256: receipt.artifact.sha256,
            })
            .collect())
    }

    fn load_installed_packages(
        &self,
        warnings: &mut Vec<String>,
        policy: ReceiptLoadPolicy,
    ) -> Result<Vec<InstalledPackage>> {
        let receipts = self.load_complete_receipts(warnings, policy)?;
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
                    Ok(PackageDependency {
                        package_key: package_key.clone(),
                        package: package_id.clone(),
                        requested_as: requested_as.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(packages)
    }

    /// Reads every complete package record under `prefix/Cellar`. Missing
    /// storage is empty state. Read-only queries warn and skip invalid records;
    /// mutation loads fail before trusting any partial snapshot.
    fn load_complete_receipts(
        &self,
        warnings: &mut Vec<String>,
        policy: ReceiptLoadPolicy,
    ) -> Result<Vec<GluInstallReceipt>> {
        let cellar = self.prefix.0.join("Cellar");
        let cellar_metadata = match fs::symlink_metadata(&cellar) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", cellar.display()));
            }
        };
        if !cellar_metadata.is_dir() {
            let message = format!(
                "package storage {} is not a real directory",
                cellar.display()
            );
            if policy == ReceiptLoadPolicy::Tolerant {
                warnings.push(format!("glu: warning: {message}"));
                return Ok(Vec::new());
            }
            anyhow::bail!("cannot safely modify packages because {message}");
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

                let keg_path = keg.path();
                let receipt_path = Self::receipt_path(&keg_path);
                let receipt = match Self::read_receipt_at_keg(&keg_path) {
                    Ok(receipt) => receipt,
                    Err(error) if error_is_not_found(&error) => continue,
                    Err(error) if policy == ReceiptLoadPolicy::Tolerant => {
                        warnings.push(format!(
                            "glu: warning: ignored invalid package metadata at {}: {error:#}",
                            receipt_path.display()
                        ));
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "cannot safely modify packages because metadata at {} is invalid",
                                receipt_path.display()
                            )
                        });
                    }
                };
                if receipt.status == ReceiptStatus::Complete {
                    receipts.push(receipt);
                }
            }
        }

        Ok(receipts)
    }
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
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
                exposure: glu_core::Exposure::Global,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                dependency_requirements: Default::default(),
            },
        };
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn malformed_receipt_is_warned_for_queries_and_rejected_for_mutation() {
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

        let store = InstalledStateStore::new(prefix);
        let mut warnings = Vec::new();
        let state = store
            .load_installed_state_with_warnings(&Declaration::default(), &mut warnings)
            .unwrap();
        assert_eq!(state.names(), vec![PackageName("good".to_string())]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid package metadata"));

        let error = store
            .load_installed_state_with_declaration_strict(&Declaration::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot safely modify packages"));
    }

    #[test]
    fn receipt_claiming_another_installation_is_never_loaded() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "first", "1.0");
        write_good_receipt(&prefix, "second", "2.0");
        let first = prefix.0.join("Cellar/first/1.0");
        let receipt_path = InstalledStateStore::receipt_path_for_keg(&first);
        let mut receipt: GluInstallReceipt =
            serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
        receipt.paths.keg = prefix.0.join("Cellar/second/2.0");
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();

        let store = InstalledStateStore::new(prefix);
        let state = store.load_installed_state().unwrap();
        assert_eq!(state.names(), vec![PackageName("second".to_string())]);

        let error = store
            .load_installed_state_with_declaration_strict(&Declaration::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot safely modify packages"));
    }

    #[test]
    fn receipt_identity_must_match_physical_directory_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "demo", "1.0");
        let installed = prefix.0.join("Cellar/demo/1.0");
        let receipt_path = InstalledStateStore::receipt_path_for_keg(&installed);
        let original = std::fs::read(&receipt_path).unwrap();
        let mut receipt: GluInstallReceipt = serde_json::from_slice(&original).unwrap();

        receipt.package.name = PackageName("other".to_string());
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(InstalledStateStore::read_receipt_at_keg(&installed).is_err());

        receipt = serde_json::from_slice(&original).unwrap();
        receipt.package.keg_version = KegVersion("2.0".to_string());
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(InstalledStateStore::read_receipt_at_keg(&installed).is_err());
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
    #[cfg(unix)]
    fn atomic_receipt_write_ignores_predictable_symlink_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "good", "1.0");
        let keg = prefix.0.join("Cellar/good/1.0");
        let receipt = InstalledStateStore::read_receipt_at_keg(&keg).unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"sentinel").unwrap();
        let predictable = keg.join(".glu/receipt.json.tmp");
        std::os::unix::fs::symlink(&outside, &predictable).unwrap();

        InstalledStateStore::write_receipt_at_keg(&keg, &receipt).unwrap();

        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
        assert!(predictable.is_symlink());
        assert_eq!(
            std::fs::metadata(keg.join(".glu/receipt.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for entry in std::fs::read_dir(keg.join(".glu")).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.contains("glu-tmp"), "atomic write left temp: {name}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn receipt_write_rejects_symlinked_metadata_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_good_receipt(&prefix, "good", "1.0");
        let keg = prefix.0.join("Cellar/good/1.0");
        let receipt = InstalledStateStore::read_receipt_at_keg(&keg).unwrap();
        std::fs::remove_dir_all(keg.join(".glu")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, keg.join(".glu")).unwrap();

        let error = InstalledStateStore::write_receipt_at_keg(&keg, &receipt).unwrap_err();

        assert!(error.to_string().contains("opening state directory"));
        assert!(std::fs::read_dir(outside).unwrap().next().is_none());
    }
}
