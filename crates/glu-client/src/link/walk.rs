//! One-pass lstat walker for keg prefix-link roots.
//!
//! The walker owns traversal mechanics only: root existence, relative path
//! calculation, lstat-derived entry kind, and prune control. Projection policy
//! and filesystem mutation live in separate modules.

use crate::link::policy::EntryKind;
use anyhow::{Context, Result};
use glu_core::Prefix;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkControl {
    Continue,
    Prune,
}

#[derive(Debug)]
pub(crate) struct WalkEntry {
    pub(crate) src: PathBuf,
    pub(crate) dst: PathBuf,
    pub(crate) rel_from_root: PathBuf,
    pub(crate) kind: EntryKind,
}

pub(crate) fn walk_link_root<F>(
    prefix: &Prefix,
    keg: &Path,
    relative_dir: &str,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(WalkEntry) -> Result<WalkControl>,
{
    let root = keg.join(relative_dir);
    if !root.exists() {
        return Ok(());
    }
    walk_from(prefix, keg, &root, &root, &mut visit)
}

fn walk_from<F>(
    prefix: &Prefix,
    keg: &Path,
    root: &Path,
    current: &Path,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(WalkEntry) -> Result<WalkControl>,
{
    for entry in fs::read_dir(current).with_context(|| format!("reading {}", current.display()))? {
        let entry = entry?;
        let src = entry.path();
        let rel_from_root = src.strip_prefix(root).unwrap_or(&src).to_path_buf();
        let rel_from_keg = src.strip_prefix(keg).unwrap_or(&src).to_path_buf();
        let dst = prefix.0.join(&rel_from_keg);
        let file_type = entry.file_type()?;
        let kind = EntryKind::from_file_type(file_type);

        let control = visit(WalkEntry {
            src: src.clone(),
            dst,
            rel_from_root,
            kind,
        })?;

        if kind == EntryKind::Directory && control == WalkControl::Continue {
            walk_from(prefix, keg, root, &src, visit)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    #[test]
    fn missing_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let mut visited = false;
        walk_link_root(&prefix, &keg, "share", |_entry| {
            visited = true;
            Ok(WalkControl::Continue)
        })
        .unwrap();
        assert!(!visited);
    }

    #[test]
    fn computes_source_destination_and_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        touch(&keg.join("share/man/man1/one.1"));

        let mut seen = Vec::new();
        walk_link_root(&prefix, &keg, "share", |entry| {
            seen.push((entry.rel_from_root, entry.dst, entry.kind));
            Ok(WalkControl::Continue)
        })
        .unwrap();

        assert!(seen.iter().any(|(rel_root, dst, kind)| {
            rel_root == Path::new("man/man1/one.1")
                && dst == &prefix.0.join("share/man/man1/one.1")
                && *kind == EntryKind::File
        }));
    }

    #[test]
    fn prune_skips_directory_descendants() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        touch(&keg.join("share/pruned/child.txt"));

        let mut seen = Vec::new();
        walk_link_root(&prefix, &keg, "share", |entry| {
            let rel = entry.rel_from_root.clone();
            seen.push(rel.clone());
            if rel == Path::new("pruned") {
                Ok(WalkControl::Prune)
            } else {
                Ok(WalkControl::Continue)
            }
        })
        .unwrap();

        assert!(seen.iter().any(|rel| rel == Path::new("pruned")));
        assert!(!seen.iter().any(|rel| rel == Path::new("pruned/child.txt")));
    }

    #[test]
    fn symlink_to_directory_is_not_recursed() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        touch(&keg.join("target/child.txt"));
        fs::create_dir_all(keg.join("bin")).unwrap();
        std::os::unix::fs::symlink("../target", keg.join("bin/target-link")).unwrap();

        let mut seen = Vec::new();
        walk_link_root(&prefix, &keg, "bin", |entry| {
            seen.push((entry.rel_from_root, entry.kind));
            Ok(WalkControl::Continue)
        })
        .unwrap();

        assert_eq!(
            seen,
            vec![(PathBuf::from("target-link"), EntryKind::Symlink)]
        );
    }
}
