use crate::link::{
    apply::LinkApplier,
    opt::{link_opt, make_relative_symlink},
    policy::LinkRoot,
};
use anyhow::{bail, Result};
use glu_core::{PackageLinkMetadata, PackageName, Prefix};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Manual link semantics: refuse when this package already has an active
    /// linked marker.
    Normal,
    /// Version switch semantics: caller has already unlinked the old active keg.
    Relink,
    /// Idempotent semantics: reassert this keg's projection after resume/repair.
    Repair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkReport {
    pub prefix_links: usize,
    pub isolated: bool,
}

impl LinkReport {
    pub fn links_created(self) -> usize {
        self.prefix_links
    }
}

pub fn link_keg(prefix: &Prefix, package: &PackageLinkMetadata, keg: &Path) -> Result<usize> {
    Ok(link_keg_with_mode(prefix, package, keg, LinkMode::Repair)?.links_created())
}

/// Reasserts this keg's active public projection. If another version of the
/// same package is currently linked, switch the projection from that keg to
/// this one; otherwise repair-link this keg idempotently. This is the shared
/// activation primitive for both install commits and `glu activate`.
///
/// When the projection switches away from a keg, that keg's receipt is
/// rewritten with `linked: false` through the store — the same
/// transition-owns-filesystem / store-owns-receipt division of labor
/// activation/deactivation already use (issue 23). Without this, a version
/// bump leaves the superseded keg's receipt claiming `linked` even though
/// its prefix projection just moved.
pub fn activate_keg_projection(
    prefix: &Prefix,
    package: &PackageLinkMetadata,
    keg: &Path,
) -> Result<usize> {
    let keg_canon = keg.canonicalize().unwrap_or_else(|_| keg.to_path_buf());
    match linked_keg_path(prefix, &package.name) {
        Some(old_keg) if old_keg != keg_canon => {
            let links = crate::link::unlink::relink_keg(prefix, &old_keg, package, keg)?;
            // The projection moved away from `old_keg`, so its receipt no
            // longer claims to be linked — the store owns that receipt
            // mutation, mirroring activation/deactivation (issue 23). Only a
            // tracked keg (one with a receipt) is rewritten: a receipt-less
            // keg is invisible to installed state, so there is nothing to
            // flip.
            let store = crate::state::store::InstalledStateStore::new(prefix.clone());
            if crate::state::store::InstalledStateStore::receipt_exists_for_keg(&old_keg)? {
                store.set_receipt_linked_for_keg(&old_keg, false)?;
            }
            Ok(links)
        }
        _ => link_keg(prefix, package, keg),
    }
}

pub fn link_keg_with_mode(
    prefix: &Prefix,
    package: &PackageLinkMetadata,
    keg: &Path,
    mode: LinkMode,
) -> Result<LinkReport> {
    ensure_link_mode_allowed(prefix, package, mode)?;
    link_opt(prefix, &package.name, &package.opt_names, keg)?;
    if package.exposure.is_isolated() {
        return Ok(LinkReport {
            prefix_links: 0,
            isolated: true,
        });
    }

    let result = project_prefix_links(prefix, package, keg);
    match result {
        Ok(count) => {
            mark_linked(prefix, &package.name, keg)?;
            Ok(LinkReport {
                prefix_links: count,
                isolated: false,
            })
        }
        Err(err) => {
            let _ = crate::link::unlink::unlink_keg_report(prefix, &package.name, keg);
            Err(err)
        }
    }
}

fn project_prefix_links(
    prefix: &Prefix,
    package: &PackageLinkMetadata,
    keg: &Path,
) -> Result<usize> {
    let applier = LinkApplier::new(prefix, package, keg);
    let mut count = 0;
    count += applier.link_root("etc", LinkRoot::Etc)?;
    count += applier.link_root("bin", LinkRoot::Bin)?;
    count += applier.link_root("sbin", LinkRoot::Sbin)?;
    count += applier.link_root("include", LinkRoot::Include)?;
    count += applier.link_root("share", LinkRoot::Share)?;
    count += applier.link_root("lib", LinkRoot::Lib)?;
    count += applier.link_root("Frameworks", LinkRoot::Frameworks)?;
    Ok(count)
}

fn ensure_link_mode_allowed(
    prefix: &Prefix,
    package: &PackageLinkMetadata,
    mode: LinkMode,
) -> Result<()> {
    if mode != LinkMode::Normal || package.exposure.is_isolated() {
        return Ok(());
    }
    if let Some(active) = linked_keg_path(prefix, &package.name) {
        bail!(
            "{} is already linked at {}; unlink or relink explicitly",
            package.name.0,
            active.display()
        );
    }
    Ok(())
}

pub fn mark_linked(prefix: &Prefix, name: &PackageName, keg: &Path) -> Result<()> {
    let linked = prefix.0.join("var/homebrew/linked").join(&name.0);
    make_relative_symlink(&linked, keg, true)
}

/// Returns the currently-linked keg for `name`, if the marker exists and
/// resolves. Mirrors Homebrew's `Keg#linked?` check (keg.rb:265-270).
pub fn linked_keg_path(prefix: &Prefix, name: &PackageName) -> Option<std::path::PathBuf> {
    let linked = prefix.0.join("var/homebrew/linked").join(&name.0);
    if !linked.is_symlink() {
        return None;
    }
    linked.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn package(name: &str, aliases: Vec<&str>) -> PackageLinkMetadata {
        PackageLinkMetadata {
            name: PackageName(name.to_string()),
            opt_names: aliases
                .into_iter()
                .map(|alias| PackageName(alias.to_string()))
                .collect(),
            exposure: glu_core::Exposure::Global,
            link_overwrite: vec![],
        }
    }

    fn keg(prefix: &Prefix, name: &str) -> PathBuf {
        prefix.0.join("Cellar").join(name).join("1.0")
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    fn assert_real_dir(path: &Path) {
        assert!(path.is_dir(), "{} should be a directory", path.display());
        assert!(
            !path.is_symlink(),
            "{} should not be a symlink",
            path.display()
        );
    }

    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[test]
    fn locale_language_directories_are_materialized() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/locale/en/LC_MESSAGES/one.mo"));
        touch(&keg(&prefix, "two").join("share/locale/en/LC_MESSAGES/two.mo"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();

        assert_real_dir(&prefix.0.join("share/locale"));
        assert_real_dir(&prefix.0.join("share/locale/en"));
        assert_real_dir(&prefix.0.join("share/locale/en/LC_MESSAGES"));
        assert!(prefix
            .0
            .join("share/locale/en/LC_MESSAGES/one.mo")
            .is_symlink());
        assert!(prefix
            .0
            .join("share/locale/en/LC_MESSAGES/two.mo")
            .is_symlink());
    }

    #[test]
    fn localized_man_directories_are_materialized() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/man/de/man1/one.1"));
        touch(&keg(&prefix, "two").join("share/man/de/man1/two.1"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();

        assert_real_dir(&prefix.0.join("share/man/de"));
        assert_real_dir(&prefix.0.join("share/man/de/man1"));
        assert!(prefix.0.join("share/man/de/man1/one.1").is_symlink());
        assert!(prefix.0.join("share/man/de/man1/two.1").is_symlink());
    }

    #[test]
    fn pwsh_directories_are_materialized() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("powershell", vec![]);
        touch(&keg(&prefix, "powershell").join("share/pwsh/completions/pwsh.ps1"));

        link_keg(&prefix, &pkg, &keg(&prefix, "powershell")).unwrap();

        assert_real_dir(&prefix.0.join("share/pwsh"));
        assert_real_dir(&prefix.0.join("share/pwsh/completions"));
        assert!(prefix
            .0
            .join("share/pwsh/completions/pwsh.ps1")
            .is_symlink());
    }

    #[test]
    fn bin_symlink_to_directory_is_linked_not_skipped() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let keg_path = keg(&prefix, "one");
        fs::create_dir_all(keg_path.join("bin")).unwrap();
        touch(&keg_path.join("libexec/tool-dir/payload"));
        std::os::unix::fs::symlink("../libexec/tool-dir", keg_path.join("bin/tool-dir")).unwrap();

        link_keg(&prefix, &pkg, &keg_path).unwrap();

        assert!(prefix.0.join("bin/tool-dir").is_symlink());
        assert_eq!(
            prefix.0.join("bin/tool-dir").canonicalize().unwrap(),
            keg_path.join("libexec/tool-dir").canonicalize().unwrap()
        );
    }

    #[test]
    fn directory_symlink_to_keg_symlinked_dir_is_not_materialized() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        let one_keg = keg(&prefix, "one");
        fs::create_dir_all(one_keg.join("share")).unwrap();
        touch(&one_keg.join("payload/one.txt"));
        std::os::unix::fs::symlink("../payload", one_keg.join("share/custom")).unwrap();
        touch(&keg(&prefix, "two").join("share/custom/two.txt"));

        link_keg(&prefix, &pkg1, &one_keg).unwrap();
        let err = link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap_err();

        assert!(err.to_string().contains("link conflict"));
        assert!(prefix.0.join("share/custom").is_symlink());
    }

    #[test]
    fn info_files_run_install_info_when_available() {
        if crate::link::info::is_executable_file(Path::new("/usr/bin/install-info")) {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let log = tmp.path().join("install-info.log");
        let script = prefix.0.join("opt/texinfo/bin/install-info");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\n", sh_quote(&log)),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let pkg = package("one", vec![]);
        let keg_path = keg(&prefix, "one");
        touch(&keg_path.join("share/info/one.info"));
        touch(&keg_path.join("share/info/two.info.gz"));
        touch(&keg_path.join("share/info/dir"));

        link_keg(&prefix, &pkg, &keg_path).unwrap();

        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("--quiet"));
        assert!(calls.contains("one.info"));
        assert!(calls.contains("two.info.gz"));
        assert!(!prefix.0.join("share/info/dir").exists());
    }

    #[test]
    fn non_keg_directory_symlink_is_a_conflict_not_absorbed() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let external = tmp.path().join("external");
        fs::create_dir_all(&external).unwrap();
        fs::create_dir_all(prefix.0.join("share")).unwrap();
        std::os::unix::fs::symlink(&external, prefix.0.join("share/custom")).unwrap();

        let pkg = package("one", vec![]);
        touch(&keg(&prefix, "one").join("share/custom/one.txt"));

        let err = link_keg(&prefix, &pkg, &keg(&prefix, "one")).unwrap_err();
        assert!(err.to_string().contains("symlink to non-keg directory"));
        assert!(prefix.0.join("share/custom").is_symlink());
    }

    #[test]
    fn keg_directory_symlink_is_materialized_before_merge() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/custom/one.txt"));
        touch(&keg(&prefix, "two").join("share/custom/two.txt"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        assert!(prefix.0.join("share/custom").is_symlink());

        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();
        assert_real_dir(&prefix.0.join("share/custom"));
        assert!(prefix.0.join("share/custom/one.txt").is_symlink());
        assert!(prefix.0.join("share/custom/two.txt").is_symlink());
    }

    #[test]
    fn prefix_pointing_keg_symlink_is_pruned_not_conflicted() {
        // qt's keg ships share/qt -> ../../../../share/qt (points at the prefix
        // share dir that other packages already materialized). Homebrew prunes such
        // entries (`src.resolved_path == dst`); glu must skip them instead of
        // raising a link conflict against the existing directory.
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        fs::create_dir_all(prefix.0.join("share/qt")).unwrap();
        let keg_path = keg(&prefix, "qt");
        fs::create_dir_all(keg_path.join("share")).unwrap();
        std::os::unix::fs::symlink("../../../../share/qt", keg_path.join("share/qt")).unwrap();
        touch(&keg_path.join("bin/qt-config"));
        let pkg = package("qt", vec![]);

        link_keg(&prefix, &pkg, &keg_path).unwrap();

        assert_real_dir(&prefix.0.join("share/qt"));
        assert!(prefix.0.join("bin/qt-config").is_symlink());
    }

    fn package_with_oldnames(name: &str, _oldnames: Vec<&str>) -> PackageLinkMetadata {
        package(name, vec![])
    }

    #[test]
    fn link_failure_rolls_back_partial_prefix_projection() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let keg_path = keg(&prefix, "one");
        touch(&keg_path.join("bin/one"));
        touch(&keg_path.join("share/conflict/file"));
        touch(&prefix.0.join("share/conflict/file"));

        let err = link_keg(&prefix, &pkg, &keg_path).unwrap_err();

        assert!(err.to_string().contains("link conflict"));
        assert!(!prefix.0.join("bin/one").exists());
        assert!(!prefix.0.join("var/homebrew/linked/one").exists());
        assert_eq!(
            fs::read_to_string(prefix.0.join("share/conflict/file")).unwrap(),
            "fixture"
        );
    }

    #[test]
    fn normal_mode_refuses_already_linked_package() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let v1 = prefix.0.join("Cellar/one/1.0");
        let v2 = prefix.0.join("Cellar/one/2.0");
        touch(&v1.join("bin/one"));
        touch(&v2.join("bin/one"));

        link_keg_with_mode(&prefix, &pkg, &v1, LinkMode::Normal).unwrap();
        let err = link_keg_with_mode(&prefix, &pkg, &v2, LinkMode::Normal).unwrap_err();

        assert!(err.to_string().contains("already linked"));
        assert_eq!(
            prefix
                .0
                .join("var/homebrew/linked/one")
                .canonicalize()
                .unwrap(),
            v1.canonicalize().unwrap()
        );
    }

    #[test]
    fn repair_mode_is_idempotent_and_reports_links() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let keg_path = keg(&prefix, "one");
        touch(&keg_path.join("bin/one"));

        let first = link_keg_with_mode(&prefix, &pkg, &keg_path, LinkMode::Repair).unwrap();
        let second = link_keg_with_mode(&prefix, &pkg, &keg_path, LinkMode::Repair).unwrap();

        assert_eq!(first.prefix_links, 1);
        assert_eq!(second.prefix_links, 1);
        assert!(!first.isolated);
    }

    #[test]
    fn link_report_marks_isolated_policy_without_prefix_links() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let mut pkg = package("one", vec![]);
        pkg.exposure = glu_core::Exposure::Isolated { reason: None };
        let keg_path = keg(&prefix, "one");
        touch(&keg_path.join("bin/one"));

        let report = link_keg_with_mode(&prefix, &pkg, &keg_path, LinkMode::Normal).unwrap();

        assert_eq!(report.prefix_links, 0);
        assert!(report.isolated);
        assert!(prefix.0.join("opt/one").is_symlink());
        assert!(!prefix.0.join("bin/one").exists());
    }

    #[test]
    fn opt_aliases_are_linked() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("bdw-gc", vec!["boehmgc", "libgc"]);
        touch(&keg(&prefix, "bdw-gc").join("bin/gc-test"));

        link_keg(&prefix, &pkg, &keg(&prefix, "bdw-gc")).unwrap();

        assert!(prefix.0.join("opt/bdw-gc").is_symlink());
        assert!(prefix.0.join("opt/boehmgc").is_symlink());
        assert!(prefix.0.join("opt/libgc").is_symlink());
    }

    #[test]
    fn declared_oldnames_are_not_created_as_aliases() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package_with_oldnames("newname", vec!["oldname"]);
        touch(&keg(&prefix, "newname").join("bin/tool"));

        link_keg(&prefix, &pkg, &keg(&prefix, "newname")).unwrap();

        assert!(prefix.0.join("opt/newname").is_symlink());
        assert!(!prefix.0.join("opt/oldname").exists());
        assert!(!prefix.0.join("opt/oldname").is_symlink());
    }

    #[test]
    fn normal_link_does_not_discover_or_retarget_oldname_opt_records() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package_with_oldnames("newname", vec!["oldname"]);
        let old_keg = prefix.0.join("Cellar/newname/0.9");
        touch(&old_keg.join("bin/tool"));
        touch(&keg(&prefix, "newname").join("bin/tool"));
        std::fs::create_dir_all(prefix.0.join("opt")).unwrap();
        std::os::unix::fs::symlink("../Cellar/newname/0.9", prefix.0.join("opt/oldname")).unwrap();

        link_keg(&prefix, &pkg, &keg(&prefix, "newname")).unwrap();

        assert!(prefix.0.join("opt/oldname").is_symlink());
        assert_eq!(
            prefix.0.join("opt/oldname").canonicalize().unwrap(),
            old_keg.canonicalize().unwrap()
        );
    }

    #[test]
    fn relink_marks_the_superseded_keg_receipt_unlinked() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("vips", vec![]);
        let v1 = write_linked_receipt(&prefix, "vips", "1.0", true);
        let v2 = prefix.0.join("Cellar/vips/2.0");
        touch(&v1.join("bin/vips"));
        touch(&v2.join("bin/vips"));

        link_keg(&prefix, &pkg, &v1).unwrap();
        activate_keg_projection(&prefix, &pkg, &v2).unwrap();

        // The projection moved from v1 to v2, and the superseded keg's
        // receipt no longer claims to be linked — the store owns that
        // receipt mutation, mirroring activation/deactivation.
        let v1_receipt =
            crate::state::store::InstalledStateStore::read_receipt_at_keg(&v1).unwrap();
        assert!(!v1_receipt.install.linked);
        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            v2.join("bin/vips").canonicalize().unwrap()
        );
        assert_eq!(
            prefix.0.join("opt/vips").canonicalize().unwrap(),
            v2.canonicalize().unwrap()
        );
    }

    fn write_linked_receipt(prefix: &Prefix, name: &str, version: &str, linked: bool) -> PathBuf {
        let keg = prefix.0.join("Cellar").join(name).join(version);
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = crate::state::GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: crate::state::ReceiptStatus::Complete,
            package: crate::state::ReceiptPackage {
                id: glu_core::PackageId(format!("pkg:test/{name}@{version}")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: glu_core::KegVersion(version.to_string()),
            },
            artifact: crate::state::ReceiptArtifact {
                id: glu_core::ArtifactId(format!("art:test/{name}")),
                sha256: "deadbeef".to_string(),
                bottle_tag: "test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: crate::state::ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.0.join("opt").join(name),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: crate::state::ReceiptInstall {
                exposure: glu_core::Exposure::Global,
                linked,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        crate::state::store::InstalledStateStore::write_receipt_at_keg(&keg, &receipt).unwrap();
        keg
    }
}
