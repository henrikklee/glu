//! Filesystem mutation for link projection.
//!
//! Policy decides what should happen, the walker decides traversal, destination
//! classification describes what is already present, and this module applies the
//! explicit state/action table.

use crate::link::{
    destination::{self, symlink_points_to, DestinationState},
    info::install_info,
    opt::make_relative_symlink,
    overwrite::OverwritePolicy,
    policy::{self, EntryKind, LinkRoot, Projection},
    walk::{self, WalkControl},
};
use anyhow::{bail, Context, Result};
use glu_core::{PackageLinkMetadata, Prefix};
use std::{fs, path::Path};

pub(crate) struct LinkApplier<'a> {
    prefix: &'a Prefix,
    package: &'a PackageLinkMetadata,
    keg: &'a Path,
    overwrite: OverwritePolicy,
}

impl<'a> LinkApplier<'a> {
    pub(crate) fn new(prefix: &'a Prefix, package: &'a PackageLinkMetadata, keg: &'a Path) -> Self {
        Self {
            prefix,
            package,
            keg,
            overwrite: OverwritePolicy::new(&package.link_overwrite),
        }
    }

    pub(crate) fn link_root(&self, relative_dir: &str, root_kind: LinkRoot) -> Result<usize> {
        let mut count = 0;
        walk::walk_link_root(self.prefix, self.keg, relative_dir, |entry| {
            let projection = policy::classify(root_kind, &entry.rel_from_root, entry.kind);

            match entry.kind {
                EntryKind::Directory => {
                    if projection == Projection::SkipSubtree {
                        return Ok(WalkControl::Prune);
                    }
                    if projection != Projection::MkpathAndDescend
                        && symlink_points_to(&entry.dst, &entry.src)
                    {
                        count += 1;
                        return Ok(WalkControl::Prune);
                    }
                    self.materialize_directory_symlink(&entry.dst)?;
                    if projection == Projection::MkpathAndDescend
                        || (entry.dst.is_dir() && !entry.dst.is_symlink())
                    {
                        fs::create_dir_all(&entry.dst)
                            .with_context(|| format!("creating {}", entry.dst.display()))?;
                        Ok(WalkControl::Continue)
                    } else {
                        self.link_path(&entry.src, &entry.dst)?;
                        count += 1;
                        Ok(WalkControl::Prune)
                    }
                }
                EntryKind::File | EntryKind::Symlink => {
                    if projection == Projection::SkipFile {
                        return Ok(WalkControl::Continue);
                    }
                    // Homebrew prunes keg entries whose resolved path is the destination
                    // (keg.rb Keg#link_dir: `Find.prune if src.resolved_path == dst`), e.g.
                    // qt's share/qt -> ../../../../share/qt baked into the bottle: the entry
                    // points at the prefix share dir that other packages already materialized,
                    // so linking it would conflict (and it is already "there" conceptually).
                    if entry.kind == EntryKind::Symlink && resolves_to(&entry.src, &entry.dst) {
                        return Ok(WalkControl::Continue);
                    }
                    self.link_path(&entry.src, &entry.dst)?;
                    if projection == Projection::LinkAndInstallInfo {
                        install_info(self.prefix, &entry.dst);
                    }
                    count += 1;
                    Ok(WalkControl::Continue)
                }
                EntryKind::Other => Ok(WalkControl::Continue),
            }
        })?;
        Ok(count)
    }

    pub(crate) fn link_path(&self, src: &Path, dst: &Path) -> Result<()> {
        let destination = destination::classify_for_link(self.prefix, self.keg, dst, src);
        match destination.state {
            DestinationState::Missing => {}
            DestinationState::SameTarget => return Ok(()),
            DestinationState::BrokenSymlink | DestinationState::SymlinkToStaleKeg => {
                fs::remove_file(dst)
                    .with_context(|| format!("removing broken symlink {}", dst.display()))?;
            }
            DestinationState::RealFile | DestinationState::SymlinkToNonKeg => {
                if !self.should_link_overwrite(dst) {
                    self.conflict(dst)?;
                }
                self.back_up_overwritten_path(dst)?;
            }
            // Existing real directories may contain projections from many kegs;
            // never move them out of the prefix as an overwrite side effect.
            DestinationState::RealDirectory
            | DestinationState::SymlinkToThisKeg
            | DestinationState::SymlinkToOtherLiveKeg
            | DestinationState::SymlinkToKegDirectory => self.conflict(dst)?,
        }
        make_relative_symlink(dst, src, false)
    }

    pub(crate) fn materialize_directory_symlink(&self, dst: &Path) -> Result<bool> {
        if !dst.is_symlink() {
            return Ok(false);
        }
        let destination = destination::classify_existing(self.prefix, self.keg, dst);
        match destination.state {
            DestinationState::BrokenSymlink | DestinationState::SymlinkToStaleKeg => {
                fs::remove_file(dst)
                    .with_context(|| format!("removing broken symlink {}", dst.display()))?;
                fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
                Ok(true)
            }
            DestinationState::SymlinkToKegDirectory => {
                let old_src = destination
                    .resolved
                    .context("directory symlink classification missing target")?;
                fs::remove_file(dst)
                    .with_context(|| format!("removing directory symlink {}", dst.display()))?;
                fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
                self.link_materialized_old_keg_dir(&old_src, dst)?;
                Ok(true)
            }
            DestinationState::SymlinkToNonKeg => {
                let Some(path) = destination.resolved else {
                    return Ok(false);
                };
                if fs::symlink_metadata(&path)
                    .map(|metadata| metadata.file_type().is_dir())
                    .unwrap_or(false)
                {
                    bail!(
                        "link conflict for {}: {} is a symlink to non-keg directory {}",
                        self.package.name.0,
                        dst.display(),
                        path.display()
                    );
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn link_materialized_old_keg_dir(&self, old_src: &Path, dst_root: &Path) -> Result<usize> {
        let mut count = 0;
        for entry in
            fs::read_dir(old_src).with_context(|| format!("reading {}", old_src.display()))?
        {
            let entry = entry?;
            if entry.file_name() == ".DS_Store" {
                continue;
            }
            let src = entry.path();
            let rel = src.strip_prefix(old_src).unwrap_or(&src);
            let dst = dst_root.join(rel);
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
                count += self.link_materialized_old_keg_dir(&src, &dst)?;
            } else if file_type.is_file() || file_type.is_symlink() {
                if policy::is_python_bytecode_in_site_packages(&src) {
                    continue;
                }
                self.link_path(&src, &dst)?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn should_link_overwrite(&self, dst: &Path) -> bool {
        let Ok(rel) = dst.strip_prefix(&self.prefix.0) else {
            return false;
        };
        self.overwrite.allows(rel)
    }

    fn back_up_overwritten_path(&self, dst: &Path) -> Result<()> {
        let rel = dst.strip_prefix(&self.prefix.0).with_context(|| {
            format!(
                "cannot back up overwrite destination outside prefix: {}",
                dst.display()
            )
        })?;
        let backup = self.unique_backup_path(rel);
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::rename(dst, &backup).with_context(|| {
            format!(
                "backing up overwritten link destination {} to {}",
                dst.display(),
                backup.display()
            )
        })?;
        Ok(())
    }

    fn unique_backup_path(&self, rel: &Path) -> std::path::PathBuf {
        let base = self
            .prefix
            .0
            .join("var/glu/link-overwrite-backups")
            .join(&self.package.name.0)
            .join(rel);
        if !base.exists() && !base.is_symlink() {
            return base;
        }
        for idx in 1.. {
            let candidate = suffixed_path(&base, idx);
            if !candidate.exists() && !candidate.is_symlink() {
                return candidate;
            }
        }
        unreachable!("unbounded backup suffix search")
    }

    fn conflict(&self, dst: &Path) -> Result<()> {
        bail!(
            "link conflict for {}: {} already exists",
            self.package.name.0,
            dst.display()
        )
    }
}

fn resolves_to(src: &Path, dst: &Path) -> bool {
    destination::lexical_resolved_path(src).is_some_and(|resolved| resolved == dst)
}

fn suffixed_path(path: &Path, idx: usize) -> std::path::PathBuf {
    let Some(file_name) = path.file_name() else {
        return path.with_extension(idx.to_string());
    };
    let mut file_name = file_name.to_os_string();
    file_name.push(format!(".{idx}"));
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::PackageName;
    use tempfile::TempDir;

    fn package(name: &str) -> PackageLinkMetadata {
        PackageLinkMetadata {
            name: PackageName(name.to_string()),
            opt_names: vec![],
            keg_only: false,
            link_overwrite: vec![],
        }
    }

    fn package_with_overwrite(name: &str, patterns: Vec<&str>) -> PackageLinkMetadata {
        PackageLinkMetadata {
            link_overwrite: patterns.into_iter().map(str::to_string).collect(),
            ..package(name)
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) {
        if let Some(parent) = dst.as_ref().parent() {
            fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[test]
    fn link_path_creates_and_noops_same_target() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let pkg = package("one");
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/one");
        let dst = prefix.0.join("bin/one");
        touch(&src);
        let applier = LinkApplier::new(&prefix, &pkg, &keg);

        applier.link_path(&src, &dst).unwrap();
        let first = fs::read_link(&dst).unwrap();
        applier.link_path(&src, &dst).unwrap();
        assert_eq!(fs::read_link(&dst).unwrap(), first);
    }

    #[test]
    fn link_path_replaces_broken_and_stale_symlinks() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let pkg = package("one");
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/one");
        touch(&src);
        let applier = LinkApplier::new(&prefix, &pkg, &keg);

        let broken = prefix.0.join("bin/broken");
        symlink("/no/such/path", &broken);
        applier.link_path(&src, &broken).unwrap();
        assert_eq!(broken.canonicalize().unwrap(), src.canonicalize().unwrap());

        let stale = prefix.0.join("bin/stale");
        symlink("../Cellar/two/1.0/bin/tool", &stale);
        applier.link_path(&src, &stale).unwrap();
        assert_eq!(stale.canonicalize().unwrap(), src.canonicalize().unwrap());
    }

    #[test]
    fn link_path_conflicts_unless_overwrite_allows() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/one");
        let dst = prefix.0.join("bin/one");
        touch(&src);
        touch(&dst);

        let pkg = package("one");
        let applier = LinkApplier::new(&prefix, &pkg, &keg);
        assert!(applier
            .link_path(&src, &dst)
            .unwrap_err()
            .to_string()
            .contains("link conflict"));

        let pkg = package_with_overwrite("one", vec!["bin/one"]);
        let applier = LinkApplier::new(&prefix, &pkg, &keg);
        applier.link_path(&src, &dst).unwrap();
        assert!(dst.is_symlink());
        assert_eq!(
            fs::read_to_string(prefix.0.join("var/glu/link-overwrite-backups/one/bin/one"))
                .unwrap(),
            "fixture"
        );
    }

    #[test]
    fn link_path_overwrite_uses_homebrew_style_patterns() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/gstreamer/1.0");
        let pkg = package_with_overwrite(
            "gstreamer",
            vec![
                "bin/gst-*",
                "share/locale/*/LC_MESSAGES/gst-*.mo",
                "lib/cmake/ggml/",
            ],
        );
        let applier = LinkApplier::new(&prefix, &pkg, &keg);

        for rel in [
            "bin/gst-launch-1.0",
            "share/locale/de/LC_MESSAGES/gst-plugins.mo",
            "lib/cmake/ggml/GGMLConfig.cmake",
        ] {
            let src = keg.join(rel);
            let dst = prefix.0.join(rel);
            touch(&src);
            touch(&dst);
            applier.link_path(&src, &dst).unwrap();
            assert!(dst.is_symlink(), "{} should be replaced", dst.display());
        }
    }

    #[test]
    fn link_path_overwrite_never_replaces_other_live_keg_projection() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let one = prefix.0.join("Cellar/one/1.0");
        let two = prefix.0.join("Cellar/two/1.0");
        let src = two.join("bin/tool");
        let other_src = one.join("bin/tool");
        let dst = prefix.0.join("bin/tool");
        touch(&src);
        touch(&other_src);
        symlink("../Cellar/one/1.0/bin/tool", &dst);

        let pkg = package_with_overwrite("two", vec!["bin/tool"]);
        let applier = LinkApplier::new(&prefix, &pkg, &two);
        let err = applier.link_path(&src, &dst).unwrap_err();

        assert!(err.to_string().contains("link conflict"));
        assert_eq!(
            dst.canonicalize().unwrap(),
            other_src.canonicalize().unwrap()
        );
    }

    #[test]
    fn link_path_overwrite_does_not_move_real_directories() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/tool");
        let dst = prefix.0.join("bin/tool");
        touch(&src);
        fs::create_dir_all(&dst).unwrap();

        let pkg = package_with_overwrite("one", vec!["bin/tool"]);
        let applier = LinkApplier::new(&prefix, &pkg, &keg);
        let err = applier.link_path(&src, &dst).unwrap_err();

        assert!(err.to_string().contains("link conflict"));
        assert!(dst.is_dir());
    }

    #[test]
    fn materialize_directory_symlink_merges_old_keg_contents() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let pkg = package("two");
        let one = prefix.0.join("Cellar/one/1.0");
        let two = prefix.0.join("Cellar/two/1.0");
        touch(&one.join("share/custom/one.txt"));
        touch(&two.join("share/custom/two.txt"));
        let dst = prefix.0.join("share/custom");
        symlink("../Cellar/one/1.0/share/custom", &dst);

        let applier = LinkApplier::new(&prefix, &pkg, &two);
        assert!(applier.materialize_directory_symlink(&dst).unwrap());
        assert!(dst.is_dir());
        assert!(!dst.is_symlink());
        assert!(dst.join("one.txt").is_symlink());
    }

    #[test]
    fn materialize_non_keg_directory_symlink_conflicts() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let pkg = package("one");
        let keg = prefix.0.join("Cellar/one/1.0");
        let external = tmp.path().join("external");
        fs::create_dir_all(&external).unwrap();
        let dst = prefix.0.join("share/custom");
        symlink(&external, &dst);

        let applier = LinkApplier::new(&prefix, &pkg, &keg);
        let err = applier.materialize_directory_symlink(&dst).unwrap_err();
        assert!(err.to_string().contains("symlink to non-keg directory"));
    }
}
