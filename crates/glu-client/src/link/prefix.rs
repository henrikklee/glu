use crate::{
    bottle::prepare::PreparedKeg,
    link::{bottle::install_etc_var, keg::activate_keg_projection, opt::link_opt},
    state::{
        store::InstalledStateStore, GluInstallReceipt, ReceiptArtifact, ReceiptInstall,
        ReceiptLinkNames, ReceiptPackage, ReceiptPaths, ReceiptSizes, ReceiptStatus,
    },
};
use anyhow::{bail, Context, Result};
use glu_core::{ArtifactId, PackageLinkMetadata, Prefix, ResolvedArtifact, ResolvedPackage};
use std::fs;
use std::path::Path;

pub fn commit_prepared_keg(
    prefix: &Prefix,
    prepared: &PreparedKeg,
    package: &ResolvedPackage,
    replace_existing: bool,
    active: bool,
) -> Result<usize> {
    // Never move the staged tree (or later remove it via the receipt) outside
    // the Cellar. `validate_manifest_path_components`
    // already rejected separator/traversal characters in name/keg_version;
    // this is the structural Cellar guard on top.
    let cellar = prefix.0.join("Cellar");
    if !prepared.final_keg_path.starts_with(&cellar)
        || prepared
            .final_keg_path
            .strip_prefix(&cellar)
            .map(|rel| rel.components().count() < 2)
            .unwrap_or(true)
    {
        bail!(
            "refusing to commit keg outside Cellar: {}",
            prepared.final_keg_path.display()
        );
    }

    if prepared.final_keg_path.exists() {
        if replace_existing {
            replace_existing_keg(&prepared.final_keg_path)?;
        } else {
            let _ = fs::remove_dir_all(&prepared.staging_keg_path);
            return project_active_and_install_etc_var(
                prefix,
                package,
                &prepared.final_keg_path,
                active,
            );
        }
    }

    let cellar_rack = prepared.final_keg_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "invalid final keg path {}",
            prepared.final_keg_path.display()
        )
    })?;
    fs::create_dir_all(cellar_rack)
        .with_context(|| format!("creating {}", cellar_rack.display()))?;
    fs::rename(&prepared.staging_keg_path, &prepared.final_keg_path).with_context(|| {
        format!(
            "moving {} to {}",
            prepared.staging_keg_path.display(),
            prepared.final_keg_path.display()
        )
    })?;

    project_active_and_install_etc_var(prefix, package, &prepared.final_keg_path, active)
}

/// Links `new_keg` (via `link_or_relink`) and then installs the bottled
/// `etc`/`var` defaults as real copies (`install_etc_var`). The copy is
/// deliberately separate from `link_keg` (symlink linking): real Homebrew's
/// `Formula#install_etc_var` is an install-time step that runs right before
/// postinstall (formula_installer.rb:1015), never part of link/unlink
/// (see docs/explanation/install-pipeline.md).
fn project_active_and_install_etc_var(
    prefix: &Prefix,
    package: &ResolvedPackage,
    new_keg: &Path,
    active: bool,
) -> Result<usize> {
    let links = if active {
        activate_keg_projection(prefix, &PackageLinkMetadata::from(package), new_keg)?
    } else {
        // Deactivation removes public prefix projection and the linked marker,
        // but keeps stable opt links so dependencies can still find the keg.
        link_opt(prefix, &package.name, &package.install.opt_names, new_keg)?;
        0
    };
    let copied = install_etc_var(prefix, package, new_keg)?;
    Ok(links + copied)
}

pub fn write_prepared_receipt(
    prepared: &PreparedKeg,
    package: &ResolvedPackage,
    artifact_id: &ArtifactId,
    artifact: &ResolvedArtifact,
    linked: bool,
) -> Result<()> {
    write_receipt_at(
        &prepared.staging_keg_path,
        prepared,
        package,
        artifact_id,
        artifact,
        linked,
        ReceiptStatus::Incomplete,
    )
}

pub fn write_install_receipt(
    prepared: &PreparedKeg,
    package: &ResolvedPackage,
    artifact_id: &ArtifactId,
    artifact: &ResolvedArtifact,
    linked: bool,
) -> Result<()> {
    write_receipt_at(
        &prepared.final_keg_path,
        prepared,
        package,
        artifact_id,
        artifact,
        linked,
        ReceiptStatus::Complete,
    )
}

fn write_receipt_at(
    keg_path: &Path,
    prepared: &PreparedKeg,
    package: &ResolvedPackage,
    artifact_id: &ArtifactId,
    artifact: &ResolvedArtifact,
    linked: bool,
    status: ReceiptStatus,
) -> Result<()> {
    let receipt = GluInstallReceipt {
        schema: "glu.install-receipt.v1".to_string(),
        status,
        package: ReceiptPackage {
            id: prepared.package_id.clone(),
            package_key: package.package_key.clone(),
            name: package.name.clone(),
            aliases: package.aliases.clone(),
            oldnames: package.oldnames.clone(),
            version: package.version.clone(),
            revision: package.revision,
            keg_version: package.keg_version.clone(),
        },
        artifact: ReceiptArtifact {
            id: artifact_id.clone(),
            sha256: artifact.sha256.clone(),
            bottle_tag: artifact.bottle_tag.clone(),
            cellar: artifact.cellar.clone(),
        },
        sizes: ReceiptSizes {
            download_bytes: artifact.bytes,
            installed_bytes: logical_dir_size(keg_path).ok(),
        },
        paths: ReceiptPaths {
            keg: prepared.final_keg_path.clone(),
            opt: prepared
                .final_keg_path
                .ancestors()
                .nth(3)
                .unwrap_or(&prepared.final_keg_path)
                .join("opt")
                .join(&package.name.0),
        },
        links: ReceiptLinkNames {
            opt_names: PackageLinkMetadata::from(package).opt_names,
        },
        install: ReceiptInstall {
            keg_only: package.install.keg_only,
            linked,
            link_overwrite: package.install.link_overwrite.clone(),
            deps: package.deps.clone(),
        },
    };

    InstalledStateStore::write_receipt_at_keg(keg_path, &receipt)
}

fn logical_dir_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if metadata.file_type().is_symlink() {
        return Ok(fs::read_link(path)
            .with_context(|| format!("reading symlink {}", path.display()))?
            .to_string_lossy()
            .len() as u64);
    }
    let mut total = 0_u64;
    if metadata.is_dir() {
        for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
            let entry = entry.with_context(|| format!("reading entry under {}", path.display()))?;
            if entry.file_name() == ".glu" {
                continue;
            }
            total = total
                .checked_add(logical_dir_size(&entry.path())?)
                .ok_or_else(|| {
                    anyhow::anyhow!("installed size overflow while scanning {}", path.display())
                })?;
        }
    }
    Ok(total)
}

fn replace_existing_keg(path: &Path) -> Result<()> {
    let trash = path.with_file_name(format!(
        ".{}.reinstall-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if trash.exists() {
        fs::remove_dir_all(&trash)
            .with_context(|| format!("removing stale {}", trash.display()))?;
    }
    fs::rename(path, &trash).with_context(|| {
        format!(
            "moving existing keg {} to {}",
            path.display(),
            trash.display()
        )
    })?;
    fs::remove_dir_all(&trash).with_context(|| format!("removing {}", trash.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bottle::prepare::PreparePhaseTimings;
    use crate::link::keg::link_keg;
    use glu_core::{
        DependencyRequires, KegVersion, PackageId, PackageInstallMetadata, PackageName,
        ResolvedArtifact, RuntimeDependencyRequirement,
    };
    use tempfile::TempDir;

    fn package(name: &str, version: &str) -> ResolvedPackage {
        ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: version.to_string(),
            revision: 0,
            keg_version: KegVersion(version.to_string()),
            deps: Vec::<RuntimeDependencyRequirement>::new(),
            artifact: ArtifactId(format!("art:test:{name}:{version}")),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                keg_only: false,
                link_overwrite: vec![],
                post_install_defined: false,
                post_install_steps: vec![],
                postinstall_network_access_allowed: true,
            },
        }
    }

    fn prepared(prefix: &Prefix, name: &str, version: &str, staging: &Path) -> PreparedKeg {
        PreparedKeg {
            package_id: PackageId(format!("{name}@{version}")),
            name: PackageName(name.to_string()),
            keg_version: KegVersion(version.to_string()),
            staging_keg_path: staging.to_path_buf(),
            final_keg_path: prefix.0.join("Cellar").join(name).join(version),
            files_to_codesign: vec![],
            warnings: vec![],
            phase_timings: PreparePhaseTimings::default(),
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    #[test]
    fn commit_prepared_keg_relinks_on_version_bump() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());

        let v1_keg = prefix.0.join("Cellar/vips/1.0");
        touch(&v1_keg.join("bin/vips"));
        link_keg(
            &prefix,
            &PackageLinkMetadata::from(&package("vips", "1.0")),
            &v1_keg,
        )
        .unwrap();
        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            v1_keg.join("bin/vips").canonicalize().unwrap()
        );

        let staging = tmp.path().join("staging/vips-2.0");
        touch(&staging.join("bin/vips"));
        let prep = prepared(&prefix, "vips", "2.0", &staging);

        commit_prepared_keg(&prefix, &prep, &package("vips", "2.0"), false, true).unwrap();

        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            prep.final_keg_path.join("bin/vips").canonicalize().unwrap()
        );
        // The old keg itself is untouched by relink — only its prefix link is gone.
        assert!(v1_keg.join("bin/vips").exists());
        assert!(!v1_keg.join("bin/vips").is_symlink());
    }

    #[test]
    fn commit_prepared_keg_same_version_existing_keg_does_not_relink() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("vips", "1.0");

        let keg_path = prefix.0.join("Cellar/vips/1.0");
        touch(&keg_path.join("bin/vips"));
        link_keg(&prefix, &PackageLinkMetadata::from(&pkg), &keg_path).unwrap();

        let staging = tmp.path().join("staging/vips-1.0-retry");
        touch(&staging.join("bin/vips"));
        let prep = prepared(&prefix, "vips", "1.0", &staging);

        commit_prepared_keg(&prefix, &prep, &pkg, false, true).unwrap();

        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            keg_path.join("bin/vips").canonicalize().unwrap()
        );
        assert!(!staging.exists());
    }

    #[test]
    fn write_install_receipt_round_trips_direct_deps() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let mut pkg = package("vips", "1.0");
        pkg.aliases = vec![glu_core::PackageSelector("vips7".to_string())];
        pkg.install.opt_names = vec![PackageName("vips-opt".to_string())];
        pkg.install.link_overwrite = vec!["bin/vips".to_string()];
        pkg.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:glib".to_string()),
            package: PackageId("pkg:homebrew/core/glib@2.0_1".to_string()),
            requested_as: glu_core::PackageSelector("glib".to_string()),
            requires: DependencyRequires {
                version: "2.0".to_string(),
                revision: 1,
            },
        }];

        let keg_path = prefix.0.join("Cellar/vips/1.0");
        touch(&keg_path.join("bin/vips"));
        let prep = prepared(&prefix, "vips", "1.0", &keg_path);
        let artifact = ResolvedArtifact {
            url: "https://ghcr.io/v2/homebrew/core/vips/blobs/sha256:vips".to_string(),
            sha256: "d".repeat(64),
            bytes: Some(1),
            bottle_tag: "arm64_sequoia".to_string(),
            cellar: ":any".to_string(),
            built_on: None,
        };

        write_install_receipt(&prep, &pkg, &pkg.artifact.clone(), &artifact, true).unwrap();

        let receipt = InstalledStateStore::read_receipt_at_keg(&prep.final_keg_path).unwrap();
        let installed = receipt.installed_package();

        assert_eq!(receipt.status, ReceiptStatus::Complete);
        assert_eq!(
            receipt.package.aliases,
            vec![glu_core::PackageSelector("vips7".to_string())]
        );
        assert_eq!(
            receipt.links.opt_names,
            vec![PackageName("vips-opt".to_string())]
        );
        assert_eq!(receipt.install.link_overwrite, vec!["bin/vips".to_string()]);
        assert_eq!(receipt.sizes.download_bytes, Some(1));
        assert_eq!(receipt.sizes.installed_bytes, Some(7));
        assert_eq!(installed.download_bytes, Some(1));
        assert_eq!(installed.installed_bytes, Some(7));
        assert_eq!(installed.deps.len(), 1);
        assert_eq!(
            installed.deps[0].package_key,
            glu_core::PackageKey("package:glib".to_string())
        );
        assert_eq!(
            installed.deps[0].package,
            PackageId("pkg:homebrew/core/glib@2.0_1".to_string())
        );
        assert_eq!(installed.deps[0].requires.revision, 1);
    }

    #[test]
    fn inactive_commit_skips_public_prefix_projection() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let staging = tmp.path().join("staging/vips-1.0");
        touch(&staging.join("bin/vips"));
        let prep = prepared(&prefix, "vips", "1.0", &staging);
        let pkg = package("vips", "1.0");

        let count = commit_prepared_keg(&prefix, &prep, &pkg, false, false).unwrap();

        assert_eq!(count, 0);
        assert!(prep.final_keg_path.exists());
        assert_eq!(
            prefix.0.join("opt/vips").canonicalize().unwrap(),
            prep.final_keg_path.canonicalize().unwrap()
        );
        assert!(!prefix.0.join("bin/vips").exists());
        assert!(!prefix.0.join("var/homebrew/linked/vips").exists());
    }

    #[test]
    fn write_prepared_receipt_marks_staging_incomplete() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("vips", "1.0");
        let staging = tmp.path().join("staging/vips-1.0");
        touch(&staging.join("bin/vips"));
        let prep = prepared(&prefix, "vips", "1.0", &staging);
        let artifact = ResolvedArtifact {
            url: "https://ghcr.io/v2/homebrew/core/vips/blobs/sha256:vips".to_string(),
            sha256: "d".repeat(64),
            bytes: Some(1),
            bottle_tag: "arm64_sequoia".to_string(),
            cellar: ":any".to_string(),
            built_on: None,
        };

        write_prepared_receipt(&prep, &pkg, &pkg.artifact.clone(), &artifact, false).unwrap();

        let receipt = InstalledStateStore::read_receipt_at_keg(&prep.staging_keg_path).unwrap();
        assert_eq!(receipt.status, ReceiptStatus::Incomplete);
        assert_eq!(receipt.sizes.download_bytes, Some(1));
        assert_eq!(receipt.sizes.installed_bytes, Some(7));
        assert_eq!(receipt.paths.keg, prep.final_keg_path);
    }
}
