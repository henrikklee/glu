use crate::{
    link::{keg::activate_keg_projection, unlink::unlink_keg},
    state::{snapshot::StateSnapshot, store::InstalledStateStore},
};
use anyhow::{Context, Result};
use glu_core::{
    InstalledPackage, KegVersion, PackageLinkMetadata, PackageName, PackageSelector, Prefix,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationResult {
    pub name: PackageName,
    pub keg_version: KegVersion,
    pub status: ActivationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationStatus {
    Activated,
    AlreadyActive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeactivationResult {
    pub name: PackageName,
    pub keg_version: KegVersion,
    pub status: DeactivationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivationStatus {
    Deactivated,
    AlreadyDeactivated,
}

struct ActivationTarget {
    package: InstalledPackage,
    link_package: PackageLinkMetadata,
}

pub fn activate_packages(
    prefix: &Prefix,
    selectors: Vec<PackageSelector>,
    force: bool,
) -> Result<Vec<ActivationResult>> {
    let snapshot = StateSnapshot::load_for_mutation(prefix)?;
    let store = InstalledStateStore::new(prefix.clone());
    let targets = resolve_targets_before_mutation(&snapshot, &store, &selectors)?;

    let mut declaration = snapshot.declaration;
    let mut receipt_updates = Vec::new();
    let mut results = Vec::new();
    for target in targets {
        let already = snapshot
            .installed
            .is_active(&PackageSelector(target.package.name.0.clone()));
        if !already || force {
            activate_keg_projection(prefix, &target.link_package, &target.package.keg_path)?;
            declaration.deactivated.remove(&target.package.name);
            receipt_updates.push((target.package.keg_path.clone(), true));
        }
        results.push(ActivationResult {
            name: target.package.name,
            keg_version: target.package.keg_version,
            status: if already && !force {
                ActivationStatus::AlreadyActive
            } else {
                ActivationStatus::Activated
            },
        });
    }

    for (keg, linked) in receipt_updates {
        store.set_receipt_linked_for_keg(&keg, linked)?;
    }
    store.write_declaration(&declaration)?;
    Ok(results)
}

pub fn deactivate_packages(
    prefix: &Prefix,
    selectors: Vec<PackageSelector>,
) -> Result<Vec<DeactivationResult>> {
    let snapshot = StateSnapshot::load_for_mutation(prefix)?;
    let store = InstalledStateStore::new(prefix.clone());
    let targets = resolve_targets_before_mutation(&snapshot, &store, &selectors)?;

    let mut declaration = snapshot.declaration;
    let mut receipt_updates = Vec::new();
    let mut results = Vec::new();
    for target in targets {
        let already = snapshot
            .installed
            .is_deactivated(&PackageSelector(target.package.name.0.clone()));
        if !already {
            unlink_keg(prefix, &target.package.name, &target.package.keg_path)?;
            declaration
                .deactivated
                .insert(target.package.name.clone(), true);
            receipt_updates.push((target.package.keg_path.clone(), false));
        }
        results.push(DeactivationResult {
            name: target.package.name,
            keg_version: target.package.keg_version,
            status: if already {
                DeactivationStatus::AlreadyDeactivated
            } else {
                DeactivationStatus::Deactivated
            },
        });
    }

    for (keg, linked) in receipt_updates {
        store.set_receipt_linked_for_keg(&keg, linked)?;
    }
    store.write_declaration(&declaration)?;
    Ok(results)
}

fn resolve_targets_before_mutation(
    snapshot: &StateSnapshot,
    store: &InstalledStateStore,
    selectors: &[PackageSelector],
) -> Result<Vec<ActivationTarget>> {
    let mut missing = Vec::new();
    let mut packages = Vec::new();
    let mut seen = BTreeSet::new();
    for selector in selectors {
        let Some(package) = snapshot.installed.resolve_selector(selector) else {
            missing.push(PackageName(selector.0.clone()));
            continue;
        };
        if seen.insert(package.package_key.clone()) {
            packages.push(package.clone());
        }
    }

    if !missing.is_empty() {
        bail_not_installed(&missing)?;
    }

    packages
        .into_iter()
        .map(|package| {
            let link_metadata = store
                .read_link_metadata_for_keg(&package.keg_path)
                .with_context(|| format!("reading link metadata for {}", package.name.0))?;
            Ok(ActivationTarget {
                package,
                link_package: link_metadata,
            })
        })
        .collect()
}

fn bail_not_installed(missing: &[PackageName]) -> Result<()> {
    let plain: Vec<&str> = missing.iter().map(|name| name.0.as_str()).collect();
    Err(crate::error::NotInstalledError::packages(
        missing.to_vec(),
        Some(format!("glu install {}", plain.join(" "))),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
        ReceiptStatus,
    };
    use glu_core::{ArtifactId, KegVersion, PackageId};

    fn write_receipt(prefix: &Prefix, name: &str, deactivated: bool) {
        let keg = prefix.0.join("Cellar").join(name).join("1.0");
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(keg.join("bin").join(name), b"tool").unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:test/{name}@1.0")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "1.0".to_string(),
                revision: 0,
                keg_version: KegVersion("1.0".to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId(format!("art:test/{name}")),
                sha256: "deadbeef".to_string(),
                bottle_tag: "test".to_string(),
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
                linked: !deactivated,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        InstalledStateStore::write_receipt_at_keg(&keg, &receipt).unwrap();
        let store = InstalledStateStore::new(prefix.clone());
        let mut declaration = store.load_declaration().unwrap();
        if deactivated {
            declaration
                .deactivated
                .insert(PackageName(name.to_string()), true);
        }
        store.write_declaration(&declaration).unwrap();
    }

    #[cfg(unix)]
    fn symlink(src: &std::path::Path, dst: &std::path::Path) {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deactivate_unlinks_public_projection_but_keeps_opt_and_marks_receipt_unlinked() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", false);
        let keg = prefix.0.join("Cellar/foo/1.0");
        let store = InstalledStateStore::new(prefix.clone());
        let mut receipt = store.read_receipt_for_keg(&keg).unwrap();
        receipt
            .package
            .aliases
            .push(PackageSelector("foo@1".to_string()));
        store.write_receipt_for_keg(&keg, &receipt).unwrap();
        symlink(&keg.join("bin/foo"), &prefix.0.join("bin/foo"));
        symlink(&keg, &prefix.0.join("var/homebrew/linked/foo"));
        symlink(&keg, &prefix.0.join("opt/foo"));

        let results =
            deactivate_packages(&prefix, vec![PackageSelector("foo@1".to_string())]).unwrap();

        assert_eq!(results[0].status, DeactivationStatus::Deactivated);
        assert!(!prefix.0.join("bin/foo").exists());
        assert!(!prefix.0.join("var/homebrew/linked/foo").exists());
        assert_eq!(
            prefix.0.join("opt/foo").canonicalize().unwrap(),
            keg.canonicalize().unwrap()
        );
        let store = InstalledStateStore::new(prefix);
        let declaration = store.load_declaration().unwrap();
        assert!(declaration
            .deactivated_names()
            .contains(&PackageName("foo".to_string())));
        let receipt = store.read_receipt_for_keg(&keg).unwrap();
        assert!(!receipt.install.linked);
    }

    #[test]
    fn deactivate_resolves_all_missing_before_mutating() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", false);

        let err = deactivate_packages(
            &prefix,
            vec![
                PackageSelector("foo".to_string()),
                PackageSelector("bar".to_string()),
            ],
        )
        .unwrap_err();

        assert!(err.to_string().contains("'bar'"));
        let declaration = InstalledStateStore::new(prefix).load_declaration().unwrap();
        assert!(declaration.deactivated_names().is_empty());
    }

    #[test]
    fn deactivate_already_deactivated_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", true);

        let results =
            deactivate_packages(&prefix, vec![PackageSelector("foo".to_string())]).unwrap();

        assert_eq!(results[0].status, DeactivationStatus::AlreadyDeactivated);
    }

    #[cfg(unix)]
    #[test]
    fn activate_relinks_public_projection_and_marks_receipt_linked() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", true);
        let keg = prefix.0.join("Cellar/foo/1.0");
        symlink(&keg, &prefix.0.join("opt/foo"));

        let results =
            activate_packages(&prefix, vec![PackageSelector("foo".to_string())], false).unwrap();

        assert_eq!(results[0].status, ActivationStatus::Activated);
        assert_eq!(
            prefix.0.join("bin/foo").canonicalize().unwrap(),
            keg.join("bin/foo").canonicalize().unwrap()
        );
        assert_eq!(
            prefix
                .0
                .join("var/homebrew/linked/foo")
                .canonicalize()
                .unwrap(),
            keg.canonicalize().unwrap()
        );
        let store = InstalledStateStore::new(prefix);
        let declaration = store.load_declaration().unwrap();
        assert!(declaration.deactivated_names().is_empty());
        let receipt = store.read_receipt_for_keg(&keg).unwrap();
        assert!(receipt.install.linked);
    }

    #[cfg(unix)]
    #[test]
    fn activating_an_isolated_package_preserves_policy_without_public_projection() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", true);
        let keg = prefix.0.join("Cellar/foo/1.0");
        let store = InstalledStateStore::new(prefix.clone());
        let mut receipt = store.read_receipt_for_keg(&keg).unwrap();
        receipt.install.exposure = glu_core::Exposure::Isolated {
            reason: Some("Conflicts with another package".to_string()),
        };
        store.write_receipt_for_keg(&keg, &receipt).unwrap();
        symlink(&keg, &prefix.0.join("opt/foo"));

        let results =
            activate_packages(&prefix, vec![PackageSelector("foo".to_string())], false).unwrap();

        assert_eq!(results[0].status, ActivationStatus::Activated);
        assert!(!prefix.0.join("bin/foo").exists());
        let receipt = store.read_receipt_for_keg(&keg).unwrap();
        assert!(receipt.install.linked);
        assert_eq!(
            receipt.install.exposure,
            glu_core::Exposure::Isolated {
                reason: Some("Conflicts with another package".to_string()),
            }
        );
    }

    #[test]
    fn activate_resolves_all_missing_before_mutating() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", true);

        let err = activate_packages(
            &prefix,
            vec![
                PackageSelector("foo".to_string()),
                PackageSelector("bar".to_string()),
            ],
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("'bar'"));
        assert!(!prefix.0.join("bin/foo").exists());
        let declaration = InstalledStateStore::new(prefix).load_declaration().unwrap();
        assert!(declaration
            .deactivated_names()
            .contains(&PackageName("foo".to_string())));
    }

    #[test]
    fn activate_already_active_is_idempotent_without_force() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", false);

        let results =
            activate_packages(&prefix, vec![PackageSelector("foo".to_string())], false).unwrap();

        assert_eq!(results[0].status, ActivationStatus::AlreadyActive);
    }

    #[cfg(unix)]
    #[test]
    fn activate_force_repairs_active_projection() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(dir.path().join("prefix"));
        write_receipt(&prefix, "foo", false);
        let keg = prefix.0.join("Cellar/foo/1.0");

        let results =
            activate_packages(&prefix, vec![PackageSelector("foo".to_string())], true).unwrap();

        assert_eq!(results[0].status, ActivationStatus::Activated);
        assert_eq!(
            prefix.0.join("bin/foo").canonicalize().unwrap(),
            keg.join("bin/foo").canonicalize().unwrap()
        );
    }
}
