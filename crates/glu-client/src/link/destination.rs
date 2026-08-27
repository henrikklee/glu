//! Prefix destination classification for linking.
//!
//! This module is the explicit ownership/state layer used by linking. It does
//! not mutate the filesystem; callers decide how each state maps to
//! link/materialization behavior.

use glu_core::Prefix;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestinationState {
    Missing,
    SameTarget,
    BrokenSymlink,
    SymlinkToThisKeg,
    SymlinkToOtherLiveKeg,
    SymlinkToStaleKeg,
    SymlinkToKegDirectory,
    SymlinkToNonKeg,
    RealDirectory,
    RealFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Destination {
    pub(crate) state: DestinationState,
    pub(crate) resolved: Option<PathBuf>,
}

pub(crate) fn classify_for_link(
    prefix: &Prefix,
    current_keg: &Path,
    dst: &Path,
    src: &Path,
) -> Destination {
    classify(prefix, current_keg, dst, Some(src))
}

pub(crate) fn classify_existing(prefix: &Prefix, current_keg: &Path, dst: &Path) -> Destination {
    classify(prefix, current_keg, dst, None)
}

fn classify(
    prefix: &Prefix,
    current_keg: &Path,
    dst: &Path,
    same_target: Option<&Path>,
) -> Destination {
    if dst.is_symlink() {
        return classify_symlink(prefix, current_keg, dst, same_target);
    }
    if dst.is_dir() {
        return Destination::new(DestinationState::RealDirectory, None);
    }
    if dst.exists() {
        return Destination::new(DestinationState::RealFile, None);
    }
    Destination::new(DestinationState::Missing, None)
}

fn classify_symlink(
    prefix: &Prefix,
    current_keg: &Path,
    dst: &Path,
    same_target: Option<&Path>,
) -> Destination {
    if let Some(src) = same_target {
        if canonical_eq(dst, src) {
            return Destination::new(DestinationState::SameTarget, dst.canonicalize().ok());
        }
    }

    let Some(resolved) = lexical_resolved_path(dst) else {
        return Destination::new(DestinationState::BrokenSymlink, None);
    };

    let Ok(metadata) = fs::symlink_metadata(&resolved) else {
        let state = if is_inside_cellar_lexical(prefix, &resolved) {
            DestinationState::SymlinkToStaleKeg
        } else {
            DestinationState::BrokenSymlink
        };
        return Destination::new(state, Some(resolved));
    };

    if metadata.file_type().is_dir() && is_inside_valid_keg(prefix, &resolved) {
        return Destination::new(DestinationState::SymlinkToKegDirectory, Some(resolved));
    }

    if is_inside_current_keg(current_keg, &resolved) {
        return Destination::new(DestinationState::SymlinkToThisKeg, Some(resolved));
    }

    if is_inside_valid_keg(prefix, &resolved) {
        return Destination::new(DestinationState::SymlinkToOtherLiveKeg, Some(resolved));
    }

    Destination::new(DestinationState::SymlinkToNonKeg, Some(resolved))
}

impl Destination {
    fn new(state: DestinationState, resolved: Option<PathBuf>) -> Self {
        Self { state, resolved }
    }
}

pub(crate) fn lexical_resolved_path(src: &Path) -> Option<PathBuf> {
    let Ok(target) = fs::read_link(src) else {
        return None;
    };
    Some(normalize_path(
        &src.parent().unwrap_or_else(|| Path::new(".")).join(target),
    ))
}

pub(crate) fn symlink_points_to(link: &Path, target: &Path) -> bool {
    link.is_symlink()
        && matches!((link.canonicalize(), target.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn canonical_eq(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn is_inside_current_keg(current_keg: &Path, path: &Path) -> bool {
    let keg = current_keg
        .canonicalize()
        .unwrap_or_else(|_| normalize_path(current_keg));
    let path = path.canonicalize().unwrap_or_else(|_| normalize_path(path));
    path.starts_with(keg)
}

pub(crate) fn is_inside_valid_keg(prefix: &Prefix, path: &Path) -> bool {
    let cellar = prefix.0.join("Cellar");
    let cellar = cellar.canonicalize().unwrap_or(cellar);
    let path = path.canonicalize().unwrap_or_else(|_| normalize_path(path));
    let Ok(rel) = path.strip_prefix(&cellar) else {
        return false;
    };
    rel.components().count() >= 2
}

fn is_inside_cellar_lexical(prefix: &Prefix, path: &Path) -> bool {
    let cellar = normalize_path(&prefix.0.join("Cellar"));
    normalize_path(path).starts_with(cellar)
}

/// Lexically normalize a path (resolve `.` and `..` without touching the
/// filesystem), matching Ruby's `Pathname#join` / `cleanpath` behavior used by
/// Homebrew's `Pathname#resolved_path`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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

    fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) {
        if let Some(parent) = dst.as_ref().parent() {
            fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[test]
    fn classifies_missing_file_and_directory() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/tool");
        touch(&src);
        assert_eq!(
            classify_for_link(&prefix, &keg, &prefix.0.join("bin/tool"), &src).state,
            DestinationState::Missing
        );

        touch(&prefix.0.join("bin/real-file"));
        assert_eq!(
            classify_existing(&prefix, &keg, &prefix.0.join("bin/real-file")).state,
            DestinationState::RealFile
        );
        fs::create_dir_all(prefix.0.join("share/real-dir")).unwrap();
        assert_eq!(
            classify_existing(&prefix, &keg, &prefix.0.join("share/real-dir")).state,
            DestinationState::RealDirectory
        );
    }

    #[test]
    fn classifies_same_target_symlink() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/tool");
        let dst = prefix.0.join("bin/tool");
        touch(&src);
        symlink("../Cellar/one/1.0/bin/tool", &dst);

        assert_eq!(
            classify_for_link(&prefix, &keg, &dst, &src).state,
            DestinationState::SameTarget
        );
        assert_eq!(
            classify_existing(&prefix, &keg, &dst).state,
            DestinationState::SymlinkToThisKeg
        );
    }

    #[test]
    fn classifies_broken_and_stale_keg_symlinks() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let src = keg.join("bin/tool");
        touch(&src);

        let broken = prefix.0.join("bin/broken");
        symlink("/no/such/path", &broken);
        assert_eq!(
            classify_for_link(&prefix, &keg, &broken, &src).state,
            DestinationState::BrokenSymlink
        );

        let stale = prefix.0.join("bin/stale");
        symlink("../Cellar/two/1.0/bin/tool", &stale);
        assert_eq!(
            classify_for_link(&prefix, &keg, &stale, &src).state,
            DestinationState::SymlinkToStaleKeg
        );
    }

    #[test]
    fn classifies_this_keg_other_keg_and_keg_directory_symlinks() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let one = prefix.0.join("Cellar/one/1.0");
        let two = prefix.0.join("Cellar/two/1.0");
        touch(&one.join("bin/current"));
        touch(&one.join("bin/other"));
        touch(&two.join("bin/tool"));
        touch(&two.join("share/custom/file"));

        let this_keg = prefix.0.join("bin/this-keg");
        symlink("../Cellar/one/1.0/bin/other", &this_keg);
        assert_eq!(
            classify_for_link(&prefix, &one, &this_keg, &one.join("bin/current")).state,
            DestinationState::SymlinkToThisKeg
        );

        let other_keg = prefix.0.join("bin/other-keg");
        symlink("../Cellar/two/1.0/bin/tool", &other_keg);
        assert_eq!(
            classify_for_link(&prefix, &one, &other_keg, &one.join("bin/current")).state,
            DestinationState::SymlinkToOtherLiveKeg
        );

        let keg_dir = prefix.0.join("share/custom");
        symlink("../Cellar/two/1.0/share/custom", &keg_dir);
        assert_eq!(
            classify_existing(&prefix, &one, &keg_dir).state,
            DestinationState::SymlinkToKegDirectory
        );
    }

    #[test]
    fn symlink_points_to_requires_matching_resolved_target() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let target = prefix.0.join("Cellar/one/1.0/bin/one");
        let other = prefix.0.join("Cellar/two/1.0/bin/two");
        let link = prefix.0.join("bin/one");
        touch(&target);
        touch(&other);
        symlink("../Cellar/one/1.0/bin/one", &link);

        assert!(symlink_points_to(&link, &target));
        assert!(!symlink_points_to(&link, &other));
    }

    #[test]
    fn classifies_non_keg_symlinks() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = prefix.0.join("Cellar/one/1.0");
        let external = tmp.path().join("external");
        touch(&keg.join("bin/tool"));
        touch(&external.join("file"));
        fs::create_dir_all(external.join("dir")).unwrap();

        let file_link = prefix.0.join("bin/non-keg-file");
        symlink(external.join("file"), &file_link);
        assert_eq!(
            classify_for_link(&prefix, &keg, &file_link, &keg.join("bin/tool")).state,
            DestinationState::SymlinkToNonKeg
        );

        let dir_link = prefix.0.join("share/non-keg-dir");
        symlink(external.join("dir"), &dir_link);
        assert_eq!(
            classify_existing(&prefix, &keg, &dir_link).state,
            DestinationState::SymlinkToNonKeg
        );
    }
}
