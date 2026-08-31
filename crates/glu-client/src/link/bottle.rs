use anyhow::{bail, Context, Result};
use glu_core::{Prefix, ResolvedPackage};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Homebrew-parity handling of `keg/.bottle/{etc,var}`: copy the bottled
/// default config files into `prefix/{etc,var}` as real, mutable files with
/// config-preserving diff logic. Previously glu symlinked `.bottle` contents
/// into the prefix (read-only, no config preservation on upgrade); this module
/// is the Homebrew-compatible copy-semantics fix.
///
/// Source: Homebrew `Formula#install_etc_var` (formula.rb:1588-1600) +
/// `InstallRenamed#append_default_if_different` (install_renamed.rb:44-72),
/// called from `formula_installer.rb:1015` right before `post_install` — an
/// install-time step, not a link operation. `brew link`/`unlink` never touch
/// these files, and `uninstall` leaves them in place with a notice
/// (uninstall.rb:76-118). glu improves removal semantics by deleting an exact
/// copied default when it is unchanged and no retained package claims it,
/// while preserving modified/shared files by default. Installation still runs
/// from `commit_prepared_keg` (install/relink), never from `link_keg` or
/// `unlink_keg`; removal derives its own pre-mutation `.bottle` inventory.
///
/// Only `etc` and `var` are considered — formula.rb:1589
/// (`etc_var_dirs = [bottle_prefix/"etc", bottle_prefix/"var"]`). Real
/// bottles ship nothing else under `.bottle` (verified against every
/// `/opt/homebrew/Cellar/*/*/.bottle`).
pub fn install_etc_var(prefix: &Prefix, package: &ResolvedPackage, keg: &Path) -> Result<usize> {
    let bottle = keg.join(".bottle");
    let mut count = 0;
    for sub in ["etc", "var"] {
        let root = bottle.join(sub);
        if !root.is_dir() {
            continue;
        }
        walk_bottle_dir(prefix, package, keg, &bottle, &root, &mut count)?;
    }
    Ok(count)
}

/// Recurses `current` (a subtree of `keg/.bottle/{etc,var}`), mapping each
/// path relative to `keg/.bottle` onto the prefix
/// (`keg/.bottle/etc/fonts/fonts.conf` → `prefix/etc/fonts/fonts.conf`).
/// Directories, including empty directories, are materialized as real prefix
/// directories; files go through `append_default_if_different`.
fn walk_bottle_dir(
    prefix: &Prefix,
    package: &ResolvedPackage,
    keg: &Path,
    bottle: &Path,
    current: &Path,
    count: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(current).with_context(|| format!("reading {}", current.display()))? {
        let entry = entry?;
        let src = entry.path();
        let rel = src.strip_prefix(bottle).unwrap_or(&src).to_path_buf();
        // `entry.file_type()` is lstat-like; descend into symlinked dirs too,
        // mirroring Ruby's `Find.find` which follows directory symlinks.
        let dst = prefix.0.join(&rel);
        if entry.file_type()?.is_dir()
            || (entry.file_type()?.is_symlink()
                && src.metadata().map(|m| m.is_dir()).unwrap_or(false))
        {
            fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
            walk_bottle_dir(prefix, package, keg, bottle, &src, count)?;
            continue;
        }
        append_default_if_different(package, keg, &src, &dst, &rel)?;
        *count += 1;
    }
    Ok(())
}

/// Where a bottled default was actually written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// Written to the live path (`dst`) — fresh install, byte-identical
    /// overwrite, or untouched-previous-default advance.
    Dst,
    /// Written to `dst.default` because `dst` holds user-modified content.
    Default,
}

/// Homebrew's `InstallRenamed#append_default_if_different`
/// (install_renamed.rb:44-72), ported:
///
/// 1. `dst` missing → copy `src` → `dst`. (Fresh install.)
/// 2. `src` and `dst` byte-identical → copy `src` → `dst` (no-op in effect;
///    refreshes the inode and converts a legacy pre-fix symlink into a real
///    file).
/// 3. `dst` differs but is byte-identical to the same relative path's default
///    in any *other* installed version of this formula
///    (`Cellar/<name>/<other-version>/.bottle/<rel>`) → copy `src` → `dst`.
///    This is the "untouched config advances on upgrade" rule (the user never
///    edited it; an older default changed upstream).
/// 4. Otherwise (user-modified) → copy `src` → `dst.default` (filename +
///    `.default` suffix), leaving the user's file alone.
///
/// `FileUtils.identical?` is content comparison (fileutils.rb:790-803:
/// size + byte compare — not inode), so a byte-copied untouched default reads
/// as identical to the new source.
///
/// Divergences from source, deliberately: a directory occupying `dst` is a
/// hard conflict (Homebrew's `FileUtils.cp` would copy *into* it — silent
/// behavior nobody relies on), and a dangling legacy symlink at `dst` (a
/// pre-fix glu install whose old keg was deleted) is replaced by a fresh copy
/// — there is nothing to preserve. Both are safety over silent surprise.
fn append_default_if_different(
    package: &ResolvedPackage,
    keg: &Path,
    src: &Path,
    dst: &Path,
    rel: &Path,
) -> Result<Written> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    // Legacy migration: a symlink `dst` (pre-fix installs symlinked `.bottle`
    // into the prefix) must never be written *through* — `fs::copy` would
    // mutate the keg's file behind the link. Materialize its target content
    // as a real file first, so the decision below compares real bytes and a
    // later `rm` of the old keg cannot leave a dangling link.
    let materialized_legacy = if dst.is_symlink() {
        if dst.metadata().map(|m| m.is_file()).unwrap_or(false) {
            let bytes = fs::read(dst).with_context(|| format!("reading {}", dst.display()))?;
            fs::remove_file(dst).with_context(|| format!("removing {}", dst.display()))?;
            fs::write(dst, bytes).with_context(|| format!("writing {}", dst.display()))?;
            true
        } else if dst.metadata().is_err() {
            // Dangling: target gone, nothing to preserve — fall through to the
            // missing-dst branch below (fresh copy of the default).
            fs::remove_file(dst).with_context(|| format!("removing {}", dst.display()))?;
            false
        } else {
            // Symlink to a non-file (directory) — the directory-conflict bail
            // below reports it.
            false
        }
    } else {
        false
    };

    match fs::metadata(dst) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            // Exists but isn't a file (real directory or symlink to one).
            bail!(
                "bottle config conflict for {}: {} is not a file",
                package.name.0,
                dst.display()
            );
        }
        Err(_) => {
            // Missing: plain fresh install of the default.
            copy_over(src, dst)?;
            return Ok(Written::Dst);
        }
    }

    if contents_equal(src, dst) {
        copy_over(src, dst)?;
        return Ok(Written::Dst);
    }

    // A materialized legacy symlink's content *is* the old keg's file —
    // byte-identical to the peer default by construction (the link pointed at
    // it), so the peer heuristic cannot distinguish "user edited through the
    // link" from "untouched default". Advancing would silently drop edits on
    // migration, so legacy content is always preserved instead (the new
    // default parks as `*.default`).
    if !materialized_legacy && peer_default_matches(keg, rel, dst) {
        copy_over(src, dst)?;
        return Ok(Written::Dst);
    }

    let default = default_path(dst);
    fs::copy(src, &default).with_context(|| format!("writing default {}", default.display()))?;
    Ok(Written::Default)
}

/// Copies `src` onto `dst`, replacing a symlink `dst` with a real file
/// instead of writing through it (`fs::copy` opens the destination with
/// create+truncate, which would follow a symlink and mutate the keg behind
/// it).
fn copy_over(src: &Path, dst: &Path) -> Result<()> {
    if dst.is_symlink() {
        fs::remove_file(dst).with_context(|| format!("removing {}", dst.display()))?;
    }
    fs::copy(src, dst)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(())
}

/// `dst + ".default"` — Homebrew's `Pathname.new("#{dst}.default")`
/// (install_renamed.rb:72), i.e. the suffix is appended to the whole path.
fn default_path(dst: &Path) -> PathBuf {
    PathBuf::from(format!("{}.default", dst.display()))
}

/// `FileUtils.identical?` — content comparison. `fs::metadata`/`fs::read`
/// follow symlinks, matching Ruby's `File.stat` which resolves the final
/// target.
fn contents_equal(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() {
        return false;
    }
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The "untouched config advances on upgrade" scan: is `dst` byte-identical
/// to `rel`'s bottled default in any other installed version of this formula?
/// This is the expansion of Homebrew's `src.ascend` walk
/// (install_renamed.rb:62-68) — the source lives under
/// `Cellar/<name>/<version>/.bottle`, so its `.bottle` ancestor is unique and
/// the scan is exactly "every sibling version dir of `keg`, except itself".
/// Only fires while another version's keg is still installed; if the old keg
/// was removed, a modified config gets the `.default` treatment — same as
/// Homebrew.
fn peer_default_matches(keg: &Path, rel: &Path, dst: &Path) -> bool {
    let Some(rack) = keg.parent() else {
        return false;
    };
    let Some(current_version) = keg.file_name() else {
        return false;
    };
    let Ok(entries) = fs::read_dir(rack) else {
        return false;
    };
    for entry in entries.flatten() {
        let peer = entry.path();
        if !peer.is_dir() || peer.file_name() == Some(current_version) {
            continue;
        }
        let default_file = peer.join(".bottle").join(rel);
        if default_file.is_file() && contents_equal(&default_file, dst) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{ArtifactId, KegVersion, PackageInstallMetadata, PackageName};
    use tempfile::TempDir;

    fn package(name: &str) -> ResolvedPackage {
        ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            deps: Vec::new(),
            dependency_requirements: Default::default(),
            exposure: glu_core::Exposure::Global,
            artifact: ArtifactId(format!("art:test:{name}")),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                link_overwrite: vec![],
                post_install_defined: false,
                post_install_steps: vec![],
                postinstall_network_access_allowed: true,
            },
        }
    }

    fn keg(prefix: &Prefix, name: &str, version: &str) -> PathBuf {
        prefix.0.join("Cellar").join(name).join(version)
    }

    fn write(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn assert_real_file(path: &Path, expected: &[u8]) {
        assert!(path.is_file(), "{} should be a real file", path.display());
        assert!(
            !path.is_symlink(),
            "{} should not be a symlink",
            path.display()
        );
        assert_eq!(fs::read(path).unwrap(), expected);
    }

    #[test]
    fn fresh_install_copies_real_files_and_nested_dirs() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        write(
            &one_keg.join(".bottle/etc/fonts/conf.d/10-foo.conf"),
            b"default-1",
        );
        write(&one_keg.join(".bottle/var/lib/foo/seed.db"), b"seed");

        let count = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(count, 2);
        assert_real_file(&prefix.0.join("etc/fonts/conf.d/10-foo.conf"), b"default-1");
        assert_real_file(&prefix.0.join("var/lib/foo/seed.db"), b"seed");
    }

    #[test]
    fn fresh_install_materializes_empty_bottled_etc_and_var_dirs() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("dbus");
        let dbus_keg = keg(&prefix, "dbus", "1.0");
        fs::create_dir_all(dbus_keg.join(".bottle/etc/dbus-1/session.d")).unwrap();
        fs::create_dir_all(dbus_keg.join(".bottle/etc/dbus-1/system.d")).unwrap();
        fs::create_dir_all(dbus_keg.join(".bottle/var/lib/dbus")).unwrap();
        fs::create_dir_all(dbus_keg.join(".bottle/var/run/dbus")).unwrap();

        let count = install_etc_var(&prefix, &pkg, &dbus_keg).unwrap();

        assert_eq!(count, 0);
        assert!(prefix.0.join("etc/dbus-1/session.d").is_dir());
        assert!(prefix.0.join("etc/dbus-1/system.d").is_dir());
        assert!(prefix.0.join("var/lib/dbus").is_dir());
        assert!(prefix.0.join("var/run/dbus").is_dir());
    }

    #[test]
    fn only_etc_and_var_are_considered() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        // A stray `.bottle/share` entry must be ignored (formula.rb:1589 only
        // copies etc/var).
        write(&one_keg.join(".bottle/etc/real.conf"), b"x");
        write(&one_keg.join(".bottle/share/not-a-config"), b"ignored");

        let count = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(count, 1);
        assert!(prefix.0.join("etc/real.conf").is_file());
        assert!(!prefix.0.join("share/not-a-config").exists());
    }

    #[test]
    fn byte_identical_dst_is_overwritten_not_defaulted() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        let src = one_keg.join(".bottle/etc/foo.conf");
        write(&src, b"default-2");
        write(&prefix.0.join("etc/foo.conf"), b"default-2");

        let written = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(written, 1);
        assert_real_file(&prefix.0.join("etc/foo.conf"), b"default-2");
        assert!(!prefix.0.join("etc/foo.conf.default").exists());
    }

    #[test]
    fn modified_dst_without_peer_is_preserved_and_default_is_parked() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        let dst = prefix.0.join("etc/foo.conf");
        write(&one_keg.join(".bottle/etc/foo.conf"), b"new-default");
        write(&dst, b"user-edited");

        let written = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(written, 1);
        assert_eq!(fs::read(&dst).unwrap(), b"user-edited");
        assert!(dst.is_file());
        assert!(!dst.is_symlink());
        assert_real_file(&prefix.0.join("etc/foo.conf.default"), b"new-default");
    }

    #[test]
    fn untouched_config_identical_to_peer_default_advances_on_upgrade() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        // v1 installed with old default, never edited by the user.
        let v1 = keg(&prefix, "one", "1.0");
        write(&v1.join(".bottle/etc/foo.conf"), b"old-default");
        write(&prefix.0.join("etc/foo.conf"), b"old-default");
        // v1's keg may not be linked — install_etc_var only needs the keg dir.
        link_bottleless(&prefix, &pkg, &v1);

        // Upgrade to v2 with a new default: the untouched config matches v1's
        // bottled default, so it must advance (overwrite), not park as .default.
        let v2 = keg(&prefix, "one", "2.0");
        write(&v2.join(".bottle/etc/foo.conf"), b"new-default");
        link_bottleless(&prefix, &pkg, &v2);

        let written = install_etc_var(&prefix, &pkg, &v2).unwrap();

        assert_eq!(written, 1);
        assert_real_file(&prefix.0.join("etc/foo.conf"), b"new-default");
        assert!(!prefix.0.join("etc/foo.conf.default").exists());
    }

    /// Links a keg without any `.bottle` handling — `install_etc_var` is what
    /// copies those, so fixtures that only need the Cellar layout call this.
    fn link_bottleless(prefix: &Prefix, pkg: &ResolvedPackage, keg: &Path) {
        crate::link::keg::link_keg(prefix, &glu_core::PackageLinkMetadata::from(pkg), keg).unwrap();
    }

    #[test]
    fn peer_scan_ignores_other_formula_racks() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        // Another formula's identical-looking default at the same rel path
        // must not trigger the advance: the scan is scoped to `Cellar/one/*`.
        let other = keg(&prefix, "two", "1.0");
        write(&other.join(".bottle/etc/foo.conf"), b"same-bytes");
        write(&prefix.0.join("etc/foo.conf"), b"same-bytes");

        let v2 = keg(&prefix, "one", "2.0");
        write(&v2.join(".bottle/etc/foo.conf"), b"brand-new");
        let written = append_default_if_different(
            &pkg,
            &v2,
            &v2.join(".bottle/etc/foo.conf"),
            &prefix.0.join("etc/foo.conf"),
            Path::new("etc/foo.conf"),
        )
        .unwrap();

        // A foreign rack's file is never a peer → treated as user-modified.
        assert_eq!(written, Written::Default);
        assert_real_file(&prefix.0.join("etc/foo.conf.default"), b"brand-new");
    }

    #[test]
    fn isolated_packages_still_get_etc_var_copied() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let mut pkg = package("one");
        pkg.exposure = glu_core::Exposure::Isolated { reason: None };
        let one_keg = keg(&prefix, "one", "1.0");
        write(&one_keg.join(".bottle/etc/foo.conf"), b"default");

        let count = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(count, 1);
        assert_real_file(&prefix.0.join("etc/foo.conf"), b"default");
    }

    #[test]
    fn legacy_symlink_with_identical_content_becomes_a_real_file() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        // Pre-fix install state: the prefix file is a symlink into the keg.
        write(&one_keg.join(".bottle/etc/foo.conf"), b"default");
        fs::create_dir_all(prefix.0.join("etc")).unwrap();
        std::os::unix::fs::symlink(
            one_keg.join(".bottle/etc/foo.conf"),
            prefix.0.join("etc/foo.conf"),
        )
        .unwrap();

        let written = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(written, 1);
        assert_real_file(&prefix.0.join("etc/foo.conf"), b"default");
        // The keg's source is untouched (the copy must not write through).
        assert_eq!(
            fs::read(one_keg.join(".bottle/etc/foo.conf")).unwrap(),
            b"default"
        );
    }

    #[test]
    fn legacy_dangling_symlink_is_replaced_by_fresh_copy() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        write(&one_keg.join(".bottle/etc/foo.conf"), b"default");
        fs::create_dir_all(prefix.0.join("etc")).unwrap();
        let gone = tmp.path().join("gone.conf");
        std::os::unix::fs::symlink(&gone, prefix.0.join("etc/foo.conf")).unwrap();

        let written = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(written, 1);
        assert_real_file(&prefix.0.join("etc/foo.conf"), b"default");
    }

    #[test]
    fn legacy_modified_symlink_is_materialized_then_default_parked() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        // The old keg's bottled default at the peer path would otherwise look
        // like an "untouched previous default" (the legacy symlink *is* that
        // file, byte-for-byte) — but it must NOT advance: the user edited the
        // config through the link, and the peer heuristic cannot tell the two
        // apart. Preservation wins: the edit survives as a real file and the
        // new default parks as `.default`.
        write(&one_keg.join(".bottle/etc/foo.conf"), b"new-default");
        fs::create_dir_all(prefix.0.join("etc")).unwrap();
        let old_keg = keg(&prefix, "one", "0.9");
        write(&old_keg.join(".bottle/etc/foo.conf"), b"user-edited-era");
        std::os::unix::fs::symlink(
            old_keg.join(".bottle/etc/foo.conf"),
            prefix.0.join("etc/foo.conf"),
        )
        .unwrap();

        let written = install_etc_var(&prefix, &pkg, &one_keg).unwrap();

        assert_eq!(written, 1);
        assert_eq!(
            fs::read(prefix.0.join("etc/foo.conf")).unwrap(),
            b"user-edited-era"
        );
        assert!(!prefix.0.join("etc/foo.conf").is_symlink());
        assert_real_file(&prefix.0.join("etc/foo.conf.default"), b"new-default");
        // The old keg's file itself is untouched.
        assert_eq!(
            fs::read(old_keg.join(".bottle/etc/foo.conf")).unwrap(),
            b"user-edited-era"
        );
    }

    #[test]
    fn directory_at_dst_is_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let pkg = package("one");
        let one_keg = keg(&prefix, "one", "1.0");
        write(&one_keg.join(".bottle/etc/foo.conf"), b"default");
        fs::create_dir_all(prefix.0.join("etc/foo.conf")).unwrap();

        let err = install_etc_var(&prefix, &pkg, &one_keg).unwrap_err();

        assert!(err.to_string().contains("bottle config conflict"));
    }
}
