//! Declarative keg-to-prefix projection policy.
//!
//! Homebrew source oracle: `Keg#link` / `Keg#link_dir` in
//! `/opt/homebrew/Library/Homebrew/keg.rb` at 5b90e281d:
//! roots/action table at keg.rb:491-574; `INFOFILE_RX`/`LOCALEDIR_RX` and
//! `SHARE_PATHS` at keg.rb:91-109; file/dir branch pruning and install-info
//! handling at keg.rb:814-831; shared-dir handling at keg.rb:833-851.

use std::{fs::FileType, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkRoot {
    Etc,
    Bin,
    Sbin,
    Include,
    Share,
    Lib,
    Frameworks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    pub(crate) fn from_file_type(file_type: FileType) -> Self {
        if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_file() {
            Self::File
        } else if file_type.is_symlink() {
            Self::Symlink
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Projection {
    Link,
    LinkAndInstallInfo,
    MkpathAndDescend,
    LinkDirectoryAndPrune,
    SkipFile,
    SkipSubtree,
}

pub(crate) fn classify(root: LinkRoot, rel: &Path, kind: EntryKind) -> Projection {
    if rel.file_name().and_then(|value| value.to_str()) == Some(".DS_Store") {
        return skip_for_kind(kind);
    }
    if kind == EntryKind::Directory
        && rel.extension().and_then(|value| value.to_str()) == Some("app")
    {
        return Projection::SkipSubtree;
    }
    if matches!(kind, EntryKind::File | EntryKind::Symlink)
        && is_python_bytecode_in_site_packages(rel)
    {
        return Projection::SkipFile;
    }

    match root {
        LinkRoot::Etc => {
            if kind == EntryKind::Directory {
                Projection::MkpathAndDescend
            } else {
                directory_or_link(kind)
            }
        }
        LinkRoot::Bin | LinkRoot::Sbin => {
            if kind == EntryKind::Directory && rel.components().count() == 1 {
                Projection::SkipSubtree
            } else {
                directory_or_link(kind)
            }
        }
        LinkRoot::Include => {
            if kind == EntryKind::Directory && is_postgresql_versioned(rel) {
                Projection::MkpathAndDescend
            } else {
                directory_or_link(kind)
            }
        }
        LinkRoot::Share => share_projection(rel, kind),
        LinkRoot::Lib => lib_projection(rel, kind),
        LinkRoot::Frameworks => {
            if kind == EntryKind::Directory && is_framework_merge_dir(rel) {
                Projection::MkpathAndDescend
            } else {
                directory_or_link(kind)
            }
        }
    }
}

fn directory_or_link(kind: EntryKind) -> Projection {
    match kind {
        EntryKind::Directory => Projection::LinkDirectoryAndPrune,
        EntryKind::File | EntryKind::Symlink => Projection::Link,
        EntryKind::Other => Projection::SkipFile,
    }
}

fn skip_for_kind(kind: EntryKind) -> Projection {
    match kind {
        EntryKind::Directory => Projection::SkipSubtree,
        _ => Projection::SkipFile,
    }
}

fn share_projection(rel: &Path, kind: EntryKind) -> Projection {
    let s = rel.to_string_lossy().replace('\\', "/");
    let basename = rel
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if is_info_path(rel) {
        return if basename == "dir" {
            Projection::SkipFile
        } else if matches!(kind, EntryKind::File | EntryKind::Symlink) {
            Projection::LinkAndInstallInfo
        } else {
            directory_or_link(kind)
        };
    }
    if s == "locale/locale.alias" || (s.starts_with("icons/") && s.ends_with("/icon-theme.cache")) {
        return Projection::SkipFile;
    }
    if kind == EntryKind::Directory
        && (share_mkpath_exact(&s)
            || s.starts_with("zsh")
            || s.starts_with("fish")
            || s.starts_with("pwsh")
            || s.starts_with("lua/")
            || s.starts_with("guile/")
            || s.starts_with("pypy")
            || s.starts_with("icons/")
            || is_postgresql_versioned(rel)
            || is_locale_directory(&s))
    {
        Projection::MkpathAndDescend
    } else {
        directory_or_link(kind)
    }
}

fn lib_projection(rel: &Path, kind: EntryKind) -> Projection {
    let s = rel.to_string_lossy().replace('\\', "/");
    if s == "charset.alias" {
        return Projection::SkipFile;
    }
    if kind == EntryKind::Directory && lib_mkpath(&s) {
        Projection::MkpathAndDescend
    } else {
        directory_or_link(kind)
    }
}

fn share_mkpath_exact(s: &str) -> bool {
    matches!(
        s,
        "aclocal"
            | "cps"
            | "doc"
            | "info"
            | "java"
            | "locale"
            | "man"
            | "man/man1"
            | "man/man2"
            | "man/man3"
            | "man/man4"
            | "man/man5"
            | "man/man6"
            | "man/man7"
            | "man/man8"
            | "man/cat1"
            | "man/cat2"
            | "man/cat3"
            | "man/cat4"
            | "man/cat5"
            | "man/cat6"
            | "man/cat7"
            | "man/cat8"
            | "applications"
            | "gnome"
            | "gnome/help"
            | "icons"
            | "mime"
            | "mime/packages"
            | "mime-info"
            | "pixmaps"
            | "postgresql"
            | "sounds"
    )
}

fn lib_mkpath(s: &str) -> bool {
    matches!(s, "cps" | "pkgconfig" | "cmake" | "dtrace" | "ghc" | "php")
        || s.starts_with("gdk-pixbuf")
        || s.starts_with("gio")
        || s.starts_with("lua")
        || s.starts_with("mecab")
        || s.starts_with("node")
        || s.starts_with("ocaml")
        || s.starts_with("perl5")
        || is_postgresql_versioned(Path::new(s))
        || s.starts_with("pypy")
        || s.starts_with("python2.")
        || s.starts_with("python3.")
        || s.starts_with('R')
        || s.starts_with("ruby")
}

pub(crate) fn is_info_path(path: &Path) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    s.match_indices("info/").any(|(idx, _)| {
        let rest = &s[idx + "info/".len()..];
        rest == "dir"
            || (!rest.starts_with('.') && (rest.ends_with(".info") || rest.ends_with(".info.gz")))
    })
}

fn is_locale_directory(s: &str) -> bool {
    let rest = s.strip_prefix("locale/").or_else(|| s.strip_prefix("man/"));
    let Some(rest) = rest else {
        return false;
    };
    let first = rest.split('/').next().unwrap_or("");
    if matches!(first, "C" | "POSIX") {
        return true;
    }
    let bytes = first.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1].is_ascii_lowercase()
}

fn is_postgresql_versioned(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    let Some(rest) = s.strip_prefix("postgresql@") else {
        return false;
    };
    rest.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

fn is_framework_merge_dir(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    s.ends_with(".framework") || s.ends_with(".framework/Versions")
}

pub(crate) fn is_python_bytecode_in_site_packages(path: &Path) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    (s.ends_with(".pyc") || s.ends_with(".pyo"))
        && (s.contains("/site-packages/") || s.starts_with("site-packages/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> &Path {
        Path::new(path)
    }

    #[test]
    fn bin_and_sbin_skip_only_top_level_real_directories() {
        assert_eq!(
            classify(LinkRoot::Bin, p("internal"), EntryKind::Directory),
            Projection::SkipSubtree
        );
        assert_eq!(
            classify(LinkRoot::Sbin, p("internal"), EntryKind::Directory),
            Projection::SkipSubtree
        );
        assert_eq!(
            classify(LinkRoot::Bin, p("internal"), EntryKind::Symlink),
            Projection::Link
        );
        assert_eq!(
            classify(LinkRoot::Bin, p("tool"), EntryKind::File),
            Projection::Link
        );
    }

    #[test]
    fn shared_share_directories_are_mkpath() {
        for path in [
            "info",
            "man/man1",
            "locale/en",
            "man/de/man1",
            "zsh/site-functions",
            "fish/vendor_completions.d",
            "pwsh/completions",
            "lua/5.4",
            "guile/site",
            "pypy/site-packages",
            "icons/hicolor",
            "postgresql@14/extension",
        ] {
            assert_eq!(
                classify(LinkRoot::Share, p(path), EntryKind::Directory),
                Projection::MkpathAndDescend,
                "{path}"
            );
        }
    }

    #[test]
    fn info_files_are_registered_and_historical_dir_is_skipped() {
        assert_eq!(
            classify(LinkRoot::Share, p("info/foo.info"), EntryKind::File),
            Projection::LinkAndInstallInfo
        );
        assert_eq!(
            classify(LinkRoot::Share, p("info/foo.info.gz"), EntryKind::File),
            Projection::LinkAndInstallInfo
        );
        assert_eq!(
            classify(LinkRoot::Share, p("info/dir"), EntryKind::File),
            Projection::SkipFile
        );
        assert!(!is_info_path(p("info/.hidden.info")));
    }

    #[test]
    fn share_skip_files_match_homebrew_policy() {
        assert_eq!(
            classify(LinkRoot::Share, p("locale/locale.alias"), EntryKind::File),
            Projection::SkipFile
        );
        assert_eq!(
            classify(
                LinkRoot::Share,
                p("icons/hicolor/icon-theme.cache"),
                EntryKind::File
            ),
            Projection::SkipFile
        );
    }

    #[test]
    fn lib_shared_directories_are_mkpath() {
        for path in [
            "cps",
            "pkgconfig",
            "cmake",
            "dtrace",
            "gdk-pixbuf-2.0",
            "ghc",
            "gio/modules",
            "lua/5.4",
            "mecab/dic",
            "node/modules",
            "ocaml/site-lib",
            "perl5/site_perl",
            "php",
            "postgresql@16",
            "pypy",
            "python3.13/site-packages",
            "R/library",
            "ruby/gems",
        ] {
            assert_eq!(
                classify(LinkRoot::Lib, p(path), EntryKind::Directory),
                Projection::MkpathAndDescend,
                "{path}"
            );
        }
    }

    #[test]
    fn skip_global_junk_and_app_subtrees() {
        assert_eq!(
            classify(LinkRoot::Share, p("doc/.DS_Store"), EntryKind::File),
            Projection::SkipFile
        );
        assert_eq!(
            classify(LinkRoot::Share, p("Foo.app"), EntryKind::Directory),
            Projection::SkipSubtree
        );
        assert_eq!(
            classify(
                LinkRoot::Lib,
                p("python3.13/site-packages/pkg/__pycache__/x.pyc"),
                EntryKind::File
            ),
            Projection::SkipFile
        );
    }

    #[test]
    fn include_and_framework_merge_directories_are_mkpath() {
        assert_eq!(
            classify(
                LinkRoot::Include,
                p("postgresql@16/server"),
                EntryKind::Directory
            ),
            Projection::MkpathAndDescend
        );
        assert_eq!(
            classify(
                LinkRoot::Frameworks,
                p("Foo.framework"),
                EntryKind::Directory
            ),
            Projection::MkpathAndDescend
        );
        assert_eq!(
            classify(
                LinkRoot::Frameworks,
                p("Foo.framework/Versions"),
                EntryKind::Directory
            ),
            Projection::MkpathAndDescend
        );
        assert_eq!(
            classify(
                LinkRoot::Frameworks,
                p("Foo.framework/Versions/A/Foo"),
                EntryKind::File
            ),
            Projection::Link
        );
    }

    #[test]
    fn default_directories_link_and_prune_except_etc() {
        assert_eq!(
            classify(LinkRoot::Share, p("custom"), EntryKind::Directory),
            Projection::LinkDirectoryAndPrune
        );
        assert_eq!(
            classify(LinkRoot::Etc, p("conf.d"), EntryKind::Directory),
            Projection::MkpathAndDescend
        );
    }
}
