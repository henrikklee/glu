use crate::link::{
    destination::{lexical_resolved_path, symlink_points_to},
    info::uninstall_info,
    keg::{link_keg_with_mode, LinkMode},
    opt::make_relative_symlink,
    policy::is_info_path,
};
use anyhow::{bail, Context, Result};
use glu_core::{PackageLinkMetadata, PackageName, Prefix};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// Mirrors Homebrew's `keg_link_directories` (keg.rb) with one deliberate
/// extension: `Frameworks` is included because glu's own `link_keg` links it
/// (`link_dir("Frameworks", ...)` in Homebrew's `Keg#link`), unlike stock
/// Homebrew's unlink directory list, which omits it (a real asymmetry in
/// Homebrew itself — see docs/explanation/install-pipeline.md).
const KEG_LINK_DIRECTORIES: [&str; 8] = [
    "etc",
    "bin",
    "sbin",
    "include",
    "share",
    "lib",
    "var",
    "Frameworks",
];

/// Removes this keg's prefix-tree symlinks and linked marker. Mirrors
/// Homebrew's `Keg#unlink` (keg.rb:356-393) — not `Keg#uninstall`: the opt
/// link is left untouched and nothing is deleted from the Cellar. Safe to call on a keg that isn't linked at all
/// (no-op). Takes just `name` (not full link metadata, unlike
/// `link_keg`/`relink_keg`) — that's the only field this ever touches, which
/// matters for `rm`: removal can work from locally installed receipt data
/// alone, with no live resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlinkReport {
    pub prefix_links_removed: usize,
}

impl UnlinkReport {
    pub fn links_removed(self) -> usize {
        self.prefix_links_removed
    }
}

pub fn unlink_keg(prefix: &Prefix, name: &PackageName, keg: &Path) -> Result<usize> {
    Ok(unlink_keg_report(prefix, name, keg)?.links_removed())
}

pub fn unlink_keg_report(prefix: &Prefix, name: &PackageName, keg: &Path) -> Result<UnlinkReport> {
    let mut count = 0;
    let mut real_dirs: BTreeSet<PathBuf> = BTreeSet::new();

    for dir_name in KEG_LINK_DIRECTORIES {
        let root = keg.join(dir_name);
        if !root.exists() {
            continue;
        }
        unlink_tree(prefix, keg, &root, &mut real_dirs, &mut count)?;
    }

    // `.bottle`-sourced config files (now real copies under `prefix/{etc,var}`
    // via `link/bottle.rs::install_etc_var`, matching Homebrew's
    // `Formula#install_etc_var`) are deliberately NOT part of this walk: real
    // files don't resolve into the keg, and real Homebrew never removes them
    // on unlink/uninstall (see docs/explanation/install-pipeline.md). An older
    // walk covered `keg/.bottle` symlinks only because glu symlinked those
    // files before the copy-semantics fix.

    remove_linked_marker_if_mine(prefix, name, keg)?;
    prune_empty_dirs(prefix, &real_dirs);
    dematerialize_single_contributor_dirs(prefix, real_dirs);

    Ok(UnlinkReport {
        prefix_links_removed: count,
    })
}

/// Walks `current` (a subtree of `rel_root`), removing any mirrored prefix
/// path that is a symlink resolving back into the keg. `rel_root` is the path
/// prefix paths are computed relative to — the keg itself for every link tree
/// (`unlink_keg`'s link directories). Ported from Homebrew's `Keg#unlink`'s
/// `dir.find` loop: every entry is visited (files and directories), a real
/// (non-symlink) mirrored directory is remembered as a prune candidate and
/// still recursed into (shared/materialized directories can hold other
/// packages' files), and recursion into an entry stops only when its own
/// mirrored path was just removed as a matching directory symlink (removing
/// the link already removed everything beneath it).
fn unlink_tree(
    prefix: &Prefix,
    rel_root: &Path,
    current: &Path,
    real_dirs: &mut BTreeSet<PathBuf>,
    count: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(current).with_context(|| format!("reading {}", current.display()))? {
        let entry = entry?;
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        let src = entry.path();
        let rel = src.strip_prefix(rel_root).unwrap_or(&src);
        let dst = prefix.0.join(rel);
        let file_type = entry.file_type()?;
        let is_src_dir = file_type.is_dir();

        if dst.is_dir() && !dst.is_symlink() {
            real_dirs.insert(dst.clone());
        }

        if dst.is_symlink() && symlink_points_to(&dst, &src) {
            if is_info_path(&dst) {
                uninstall_info(prefix, &dst);
            }
            fs::remove_file(&dst).with_context(|| format!("removing {}", dst.display()))?;
            *count += 1;
            if is_src_dir {
                continue;
            }
        } else if is_src_dir {
            unlink_tree(prefix, rel_root, &src, real_dirs, count)?;
        }
    }
    Ok(())
}

/// Unlinks `old_keg` (a no-op if it isn't actually linked) and links
/// `new_keg` in its place. This is the version-bump case `commit_prepared_keg`
/// needs: a new version lands at a new, different Cellar path, so `link_keg`
/// alone has no way to know the old version's still-present prefix symlinks
/// are its own to replace — it would just hit `link_path`'s conflict check
/// and hard-fail. Both primitives are individually idempotent/retryable, so
/// a failure mid-relink is safe to retry from either state; no transactional
/// rollback needed.
pub fn relink_keg(
    prefix: &Prefix,
    old_keg: &Path,
    package: &PackageLinkMetadata,
    new_keg: &Path,
) -> Result<usize> {
    unlink_keg(prefix, &package.name, old_keg)?;
    Ok(link_keg_with_mode(prefix, package, new_keg, LinkMode::Relink)?.links_created())
}

/// Full teardown of a keg's presence: prefix links + linked marker
/// (`unlink_keg`), every `opt/*` identity link that resolves to this keg
/// (name, aliases, oldname records — see below), then the keg directory
/// itself and its now-possibly-empty rack. Mirrors Homebrew's
/// `keg.unlink; keg.uninstall` (keg.rb:324-393) combination. Opt-record
/// removal remains separate from `unlink_keg` because unlink keeps the keg.
pub fn remove_keg(prefix: &Prefix, name: &PackageName, keg: &Path) -> Result<()> {
    // Defense in depth on the receipt-path check in `load_installed`: never
    // `remove_dir_all` a path outside the Cellar, even
    // if a receipt was tampered with after load. Rejects `..`/`.` traversal
    // lexically (`starts_with` alone would let `Cellar/../etc` through).
    let cellar = prefix.0.join("Cellar");
    if has_traversal(keg)
        || !keg.starts_with(&cellar)
        || keg
            .strip_prefix(&cellar)
            .map(|rel| rel.components().count() < 2)
            .unwrap_or(true)
    {
        bail!("refusing to remove non-Cellar keg path {}", keg.display());
    }
    unlink_keg(prefix, name, keg)?;
    remove_opt_links(prefix, keg)?;
    fs::remove_dir_all(keg).with_context(|| format!("removing {}", keg.display()))?;
    if let Some(rack) = keg.parent() {
        let _ = fs::remove_dir(rack);
    }
    Ok(())
}

/// Removes every symlink directly under `prefix/opt` that resolves to
/// `keg`. Deliberately doesn't take name/aliases/oldnames and instead scans
/// `opt/` directly, mirroring how `link_opt`'s own `existing_oldname_opt_records`
/// already discovers oldname entries by filesystem scan rather than from
/// declared metadata (`link/opt.rs:46-83`) — since every entry `link_opt`
/// creates (primary name, aliases, retargeted oldname records) points at the
/// exact same `keg` target, "remove anything under opt/ pointing here" is
/// the complete, symmetric inverse without needing aliases/oldnames at removal
/// time at all.
fn remove_opt_links(prefix: &Prefix, keg: &Path) -> Result<usize> {
    let opt = prefix.0.join("opt");
    if !opt.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(&opt).with_context(|| format!("reading {}", opt.display()))? {
        let entry = entry?;
        let path = entry.path();
        if symlink_points_to(&path, keg) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            count += 1;
        }
    }
    let _ = fs::remove_dir(&opt);
    Ok(count)
}

fn remove_linked_marker_if_mine(prefix: &Prefix, name: &PackageName, keg: &Path) -> Result<()> {
    let linked = prefix.0.join("var/homebrew/linked").join(&name.0);
    if symlink_points_to(&linked, keg) {
        fs::remove_file(&linked)
            .with_context(|| format!("removing linked marker {}", linked.display()))?;
    }
    Ok(())
}

fn prune_empty_dirs(prefix: &Prefix, dirs: &BTreeSet<PathBuf>) {
    let must_exist = must_exist_dirs(prefix);
    let mut candidates: Vec<PathBuf> = dirs.iter().cloned().collect();
    candidates.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in candidates {
        if must_exist.contains(&dir) {
            continue;
        }
        let _ = fs::remove_dir(&dir);
    }
}

fn dematerialize_single_contributor_dirs(prefix: &Prefix, dirs: BTreeSet<PathBuf>) {
    let must_exist = must_exist_dirs(prefix);
    let mut candidates: Vec<PathBuf> = dirs.into_iter().collect();
    candidates.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in candidates {
        if must_exist.contains(&dir) || !dir.is_dir() || dir.is_symlink() {
            continue;
        }
        let Some(source) = single_contributor_source_dir(prefix, &dir) else {
            continue;
        };
        if source == dir || !source.is_dir() {
            continue;
        }
        if fs::remove_dir_all(&dir).is_ok() {
            let _ = make_relative_symlink(&dir, &source, false);
        }
    }
}

fn must_exist_dirs(prefix: &Prefix) -> BTreeSet<PathBuf> {
    KEG_LINK_DIRECTORIES
        .iter()
        .map(|dir| prefix.0.join(dir))
        .chain(std::iter::once(prefix.0.join("var/homebrew/linked")))
        .collect()
}

fn single_contributor_source_dir(prefix: &Prefix, dst_dir: &Path) -> Option<PathBuf> {
    let mut contributor: Option<PathBuf> = None;
    let entries = fs::read_dir(dst_dir).ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        let dst_child = entry.path();
        let source_parent = if dst_child.is_symlink() {
            let target = lexical_resolved_path(&dst_child)?;
            if !is_live_cellar_path(prefix, &target) {
                return None;
            }
            target.parent()?.to_path_buf()
        } else if dst_child.is_dir() {
            let source_child = single_contributor_source_dir(prefix, &dst_child)?;
            source_child.parent()?.to_path_buf()
        } else {
            return None;
        };

        if let Some(existing) = &contributor {
            if existing != &source_parent {
                return None;
            }
        } else {
            contributor = Some(source_parent);
        }
    }
    contributor
}

fn is_live_cellar_path(prefix: &Prefix, path: &Path) -> bool {
    let cellar = prefix.0.join("Cellar");
    path.starts_with(&cellar) && path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::keg::link_keg;
    use glu_core::{
        ArtifactId, KegVersion, PackageInstallMetadata, PackageName, ResolvedPackage,
        RuntimeDependencyRequirement,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn package(name: &str, aliases: Vec<&str>) -> PackageLinkMetadata {
        PackageLinkMetadata {
            name: PackageName(name.to_string()),
            opt_names: aliases
                .into_iter()
                .map(|alias| PackageName(alias.to_string()))
                .collect(),
            keg_only: false,
            link_overwrite: vec![],
        }
    }

    fn resolved_package(name: &str) -> ResolvedPackage {
        ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            deps: Vec::<RuntimeDependencyRequirement>::new(),
            artifact: ArtifactId(format!("art:test:{name}")),
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

    fn keg(prefix: &Prefix, name: &str) -> PathBuf {
        prefix.0.join("Cellar").join(name).join("1.0")
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn is_executable(path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        #[cfg(unix)]
        {
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            metadata.is_file()
        }
    }

    #[test]
    fn unlink_removes_prefix_links_and_marker_but_keeps_opt() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join("bin/one"));
        touch(&one_keg.join("lib/libone.dylib"));

        link_keg(&prefix, &pkg, &one_keg).unwrap();
        assert!(prefix.0.join("bin/one").is_symlink());
        assert!(prefix.0.join("var/homebrew/linked/one").is_symlink());
        assert!(prefix.0.join("opt/one").is_symlink());

        let removed = unlink_keg(&prefix, &pkg.name, &one_keg).unwrap();
        assert_eq!(removed, 2);
        assert!(!prefix.0.join("bin/one").exists());
        assert!(!prefix.0.join("lib/libone.dylib").exists());
        assert!(!prefix.0.join("var/homebrew/linked/one").exists());
        // opt link is Keg#uninstall's job, not Keg#unlink's — must survive.
        assert!(prefix.0.join("opt/one").is_symlink());
    }

    #[test]
    fn unlink_removes_matching_app_symlink_if_legacy_state_exists() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let one_keg = keg(&prefix, "one");
        let app_src = one_keg.join("share/Legacy.app");
        fs::create_dir_all(&app_src).unwrap();
        fs::create_dir_all(prefix.0.join("share")).unwrap();
        std::os::unix::fs::symlink(
            "../Cellar/one/1.0/share/Legacy.app",
            prefix.0.join("share/Legacy.app"),
        )
        .unwrap();

        let removed = unlink_keg(&prefix, &pkg.name, &one_keg).unwrap();

        assert_eq!(removed, 1);
        assert!(!prefix.0.join("share/Legacy.app").exists());
    }

    #[test]
    fn unlink_runs_install_info_delete_when_available() {
        if is_executable(Path::new("/usr/bin/install-info")) {
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
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join("share/info/one.info"));
        link_keg(&prefix, &pkg, &one_keg).unwrap();
        fs::write(&log, b"").unwrap();

        unlink_keg(&prefix, &pkg.name, &one_keg).unwrap();

        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("--delete"));
        assert!(calls.contains("--quiet"));
        assert!(calls.contains("one.info"));
    }

    #[test]
    fn unlink_leaves_other_packages_links_intact_in_shared_dir() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/custom/one.txt"));
        touch(&keg(&prefix, "two").join("share/custom/two.txt"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();

        unlink_keg(&prefix, &pkg1.name, &keg(&prefix, "one")).unwrap();

        assert!(!prefix.0.join("share/custom/one.txt").exists());
        assert!(prefix.0.join("share/custom/two.txt").exists());
        // With one contributor left, the materialized merge dir is compacted
        // back to a directory symlink to that keg's subtree.
        assert!(prefix.0.join("share/custom").is_symlink());
        assert_eq!(
            prefix.0.join("share/custom").canonicalize().unwrap(),
            keg(&prefix, "two")
                .join("share/custom")
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn unlink_dematerializes_nested_single_contributor_subtree() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/debug/auto-load/opt/one.py"));
        touch(&keg(&prefix, "two").join("share/debug/auto-load/two.py"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();
        assert!(prefix.0.join("share/debug").is_dir());
        assert!(!prefix.0.join("share/debug").is_symlink());

        unlink_keg(&prefix, &pkg2.name, &keg(&prefix, "two")).unwrap();

        assert!(prefix.0.join("share/debug").is_symlink());
        assert_eq!(
            prefix.0.join("share/debug").canonicalize().unwrap(),
            keg(&prefix, "one")
                .join("share/debug")
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn unlink_prunes_shared_dir_once_it_is_actually_empty() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join("share/locale/en/LC_MESSAGES/one.mo"));
        touch(&keg(&prefix, "two").join("share/locale/en/LC_MESSAGES/two.mo"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();

        unlink_keg(&prefix, &pkg1.name, &keg(&prefix, "one")).unwrap();
        unlink_keg(&prefix, &pkg2.name, &keg(&prefix, "two")).unwrap();

        assert!(!prefix.0.join("share/locale").exists());
        // must-exist skeleton itself always survives
        assert!(prefix.0.join("share").is_dir());
    }

    #[test]
    fn unlink_removes_only_symlinks_that_resolve_into_this_keg() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join("bin/one"));
        link_keg(&prefix, &pkg, &one_keg).unwrap();

        // Simulate a foreign symlink occupying the same relative path in
        // another keg's tree (never linked into the prefix): unlinking "two"
        // must not touch "one"'s real link.
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "two").join("bin/one"));
        let removed = unlink_keg(&prefix, &pkg2.name, &keg(&prefix, "two")).unwrap();
        assert_eq!(removed, 0);
        assert!(prefix.0.join("bin/one").is_symlink());
        assert_eq!(
            prefix.0.join("bin/one").canonicalize().unwrap(),
            one_keg.join("bin/one").canonicalize().unwrap()
        );
    }

    #[test]
    fn unlink_marker_only_removed_if_it_points_to_this_keg() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let old_keg = prefix.0.join("Cellar/one/0.9");
        let new_keg = keg(&prefix, "one");
        touch(&old_keg.join("bin/one"));
        touch(&new_keg.join("bin/one"));

        link_keg(&prefix, &pkg, &old_keg).unwrap();
        assert!(symlink_points_to(
            &prefix.0.join("var/homebrew/linked/one"),
            &old_keg
        ));

        // Unlinking the *new* (not currently linked) keg must not disturb
        // the marker, which still points at the old keg.
        unlink_keg(&prefix, &pkg.name, &new_keg).unwrap();
        assert!(prefix.0.join("var/homebrew/linked/one").is_symlink());
        assert_eq!(
            prefix
                .0
                .join("var/homebrew/linked/one")
                .canonicalize()
                .unwrap(),
            old_keg.canonicalize().unwrap()
        );
    }

    #[test]
    fn relink_keg_swaps_version_and_survives_a_shared_directory() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg_v1 = package("vips", vec![]);
        let pkg_v2 = package("vips", vec![]);
        let other = package("other", vec![]);

        let v1_keg = prefix.0.join("Cellar/vips/1.0");
        let v2_keg = prefix.0.join("Cellar/vips/2.0");
        let other_keg = keg(&prefix, "other");
        touch(&v1_keg.join("bin/vips"));
        touch(&v1_keg.join("share/locale/en/LC_MESSAGES/vips.mo"));
        touch(&v2_keg.join("bin/vips"));
        touch(&v2_keg.join("share/locale/en/LC_MESSAGES/vips.mo"));
        touch(&other_keg.join("share/locale/en/LC_MESSAGES/other.mo"));

        link_keg(&prefix, &pkg_v1, &v1_keg).unwrap();
        link_keg(&prefix, &other, &other_keg).unwrap();
        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            v1_keg.join("bin/vips").canonicalize().unwrap()
        );

        relink_keg(&prefix, &v1_keg, &pkg_v2, &v2_keg).unwrap();

        // v1 fully unlinked...
        assert!(!v1_keg.join("bin/vips").is_symlink());
        // ...v2 fully linked...
        assert_eq!(
            prefix.0.join("bin/vips").canonicalize().unwrap(),
            v2_keg.join("bin/vips").canonicalize().unwrap()
        );
        assert_eq!(
            prefix
                .0
                .join("share/locale/en/LC_MESSAGES/vips.mo")
                .canonicalize()
                .unwrap(),
            v2_keg
                .join("share/locale/en/LC_MESSAGES/vips.mo")
                .canonicalize()
                .unwrap()
        );
        // ...and "other"'s file in the same shared, materialized directory survived untouched.
        assert!(prefix
            .0
            .join("share/locale/en/LC_MESSAGES/other.mo")
            .is_symlink());
        assert_eq!(
            prefix
                .0
                .join("var/homebrew/linked/vips")
                .canonicalize()
                .unwrap(),
            v2_keg.canonicalize().unwrap()
        );
    }

    #[test]
    fn remove_keg_tears_down_prefix_links_opt_marker_and_cellar_dir() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join("bin/one"));
        link_keg(&prefix, &pkg, &one_keg).unwrap();

        remove_keg(&prefix, &pkg.name, &one_keg).unwrap();

        assert!(!prefix.0.join("bin/one").exists());
        assert!(!prefix.0.join("opt/one").exists());
        assert!(!prefix.0.join("var/homebrew/linked/one").exists());
        assert!(!one_keg.exists());
        // rack is now empty (this was the only version) — pruned too
        assert!(!prefix.0.join("Cellar/one").exists());
    }

    #[test]
    fn remove_keg_cleans_up_alias_and_oldname_opt_entries() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("bdw-gc", vec!["boehmgc", "libgc"]);
        let gc_keg = keg(&prefix, "bdw-gc");
        touch(&gc_keg.join("bin/gc-test"));
        fs::create_dir_all(prefix.0.join("opt")).unwrap();
        std::os::unix::fs::symlink("../Cellar/bdw-gc/1.0", prefix.0.join("opt/gc-oldname"))
            .unwrap();

        link_keg(&prefix, &pkg, &gc_keg).unwrap();
        assert!(prefix.0.join("opt/boehmgc").is_symlink());
        assert!(prefix.0.join("opt/gc-oldname").is_symlink());

        remove_keg(&prefix, &pkg.name, &gc_keg).unwrap();

        assert!(!prefix.0.join("opt/bdw-gc").exists());
        assert!(!prefix.0.join("opt/boehmgc").exists());
        assert!(!prefix.0.join("opt/libgc").exists());
        assert!(!prefix.0.join("opt/gc-oldname").exists());
    }

    #[test]
    fn remove_keg_leaves_sibling_version_rack_intact() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg_v1 = package("vips", vec![]);
        let pkg_v2 = package("vips", vec![]);
        let v1_keg = prefix.0.join("Cellar/vips/1.0");
        let v2_keg = prefix.0.join("Cellar/vips/2.0");
        touch(&v1_keg.join("bin/vips"));
        touch(&v2_keg.join("bin/vips"));

        // Neither is linked here — this exercises removing an unlinked,
        // non-default keg while a sibling version stays fully installed.
        link_keg(&prefix, &pkg_v2, &v2_keg).unwrap();

        remove_keg(&prefix, &pkg_v1.name, &v1_keg).unwrap();

        assert!(!v1_keg.exists());
        assert!(v2_keg.join("bin/vips").exists());
        assert!(prefix.0.join("Cellar/vips").is_dir());
        assert!(prefix.0.join("bin/vips").is_symlink());
    }

    #[test]
    fn unlink_leaves_copied_bottle_config_files_alone() {
        // `.bottle` files are real copies in the prefix now (install_etc_var),
        // not symlinks into the keg; unlink/rm must NOT remove them.
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one", vec![]);
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join(".bottle/etc/fonts/fonts.conf"));

        link_keg(&prefix, &pkg, &one_keg).unwrap();
        crate::link::bottle::install_etc_var(&prefix, &resolved_package("one"), &one_keg).unwrap();
        assert!(prefix.0.join("etc/fonts/fonts.conf").is_file());
        assert!(!prefix.0.join("etc/fonts/fonts.conf").is_symlink());

        let removed = unlink_keg(&prefix, &pkg.name, &one_keg).unwrap();
        assert_eq!(removed, 0);
        // The copied config survives unlink (Homebrew parity: unlink never
        // touches these; rm prints the leftover-config notice instead).
        assert!(prefix.0.join("etc/fonts/fonts.conf").is_file());
        assert!(!prefix.0.join("var/homebrew/linked/one").exists());
    }

    #[test]
    fn unlink_leaves_other_packages_copied_configs_intact() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg1 = package("one", vec![]);
        let pkg2 = package("two", vec![]);
        touch(&keg(&prefix, "one").join(".bottle/etc/one.conf"));
        touch(&keg(&prefix, "one").join("share/custom/one.txt"));
        touch(&keg(&prefix, "two").join(".bottle/etc/two.conf"));

        link_keg(&prefix, &pkg1, &keg(&prefix, "one")).unwrap();
        link_keg(&prefix, &pkg2, &keg(&prefix, "two")).unwrap();
        crate::link::bottle::install_etc_var(
            &prefix,
            &resolved_package("one"),
            &keg(&prefix, "one"),
        )
        .unwrap();
        crate::link::bottle::install_etc_var(
            &prefix,
            &resolved_package("two"),
            &keg(&prefix, "two"),
        )
        .unwrap();

        unlink_keg(&prefix, &pkg1.name, &keg(&prefix, "one")).unwrap();

        // The symlinked share file is gone, but both copied configs survive.
        assert!(!prefix.0.join("share/custom/one.txt").exists());
        assert!(prefix.0.join("etc/one.conf").is_file());
        assert!(prefix.0.join("etc/two.conf").is_file());
    }

    #[test]
    fn unlink_keg_only_package_is_a_noop_besides_marker() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let mut pkg = package("one", vec![]);
        pkg.keg_only = true;
        let one_keg = keg(&prefix, "one");
        touch(&one_keg.join("bin/one"));

        link_keg(&prefix, &pkg, &one_keg).unwrap();
        assert!(!prefix.0.join("bin/one").exists());
        assert!(prefix.0.join("opt/one").is_symlink());
        assert!(!prefix.0.join("var/homebrew/linked/one").exists());

        let removed = unlink_keg(&prefix, &pkg.name, &one_keg).unwrap();
        assert_eq!(removed, 0);
        assert!(prefix.0.join("opt/one").is_symlink());
    }
}

#[cfg(test)]
mod path_confinement_tests {
    use super::*;
    use glu_core::Prefix;
    use tempfile::TempDir;

    #[test]
    fn remove_keg_refuses_non_cellar_path() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        fs::create_dir_all(&prefix.0).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        let err = remove_keg(&prefix, &PackageName("evil".to_string()), &outside).unwrap_err();
        assert!(err.to_string().contains("non-Cellar"));
        // Nothing was deleted.
        assert!(outside.exists());
    }

    #[test]
    fn remove_keg_accepts_cellar_path() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/vips/8.19.0");
        fs::create_dir_all(&keg).unwrap();
        let pkg = package("vips");
        touch(&keg.join("bin/vips"));

        remove_keg(&prefix, &pkg.name, &keg).unwrap();
        assert!(!keg.exists());
    }

    fn package(name: &str) -> PackageLinkMetadata {
        PackageLinkMetadata {
            name: PackageName(name.to_string()),
            opt_names: vec![],
            keg_only: false,
            link_overwrite: vec![],
        }
    }

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }
}

/// True when `path` contains `..`/`.` components — used by the remove-keg
/// security guard (S3) so `starts_with(&Cellar)` can't be bypassed with
/// `Cellar/../etc`.
pub(crate) fn has_traversal(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    })
}
