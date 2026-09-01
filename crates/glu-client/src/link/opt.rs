use crate::path_component::{filesystem_component_key, PathComponentCollisionTracker};
use anyhow::{bail, Context, Result};
use glu_core::{PackageName, Prefix};
use std::path::{Path, PathBuf};

/// Homebrew-style opt link: create `<prefix>/opt/<name>` plus active aliases
/// pointing at the keg, so dependents can reference the package via a stable
/// opt path regardless of cellar location. Old-name compatibility links are
/// deliberately not discovered here; the rename transition creates them
/// explicitly.
pub fn link_opt(
    prefix: &Prefix,
    name: &PackageName,
    aliases: &[PackageName],
    keg: &Path,
) -> Result<()> {
    let mut names = PathComponentCollisionTracker::default();
    names.insert(&name.0, &name.0, "stable package link name")?;
    for alias in aliases {
        names.insert(&alias.0, &name.0, "stable package link alias")?;
    }

    let opt = prefix.0.join("opt");
    reject_existing_case_collisions(&opt, std::iter::once(name).chain(aliases))?;
    std::fs::create_dir_all(&opt).with_context(|| format!("creating {}", opt.display()))?;

    make_relative_symlink(&opt.join(&name.0), keg, true)?;
    for alias in aliases {
        make_relative_symlink(&opt.join(&alias.0), keg, true)?;
    }
    Ok(())
}

fn reject_existing_case_collisions<'a>(
    opt: &Path,
    desired: impl IntoIterator<Item = &'a PackageName>,
) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(opt) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", opt.display())),
    };
    if !metadata.is_dir() {
        bail!(
            "stable package link directory {} is not a real directory",
            opt.display()
        );
    }

    let desired: Vec<(&str, String)> = desired
        .into_iter()
        .map(|name| (name.0.as_str(), filesystem_component_key(&name.0)))
        .collect();
    for entry in std::fs::read_dir(opt).with_context(|| format!("reading {}", opt.display()))? {
        let existing = entry?.file_name();
        let Some(existing) = existing.to_str() else {
            continue;
        };
        let existing_key = existing.to_ascii_lowercase();
        for (name, key) in &desired {
            if existing != *name && existing_key == *key {
                bail!(
                    "refusing package link {:?}: existing name {:?} is filesystem-equivalent",
                    name,
                    existing
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn make_relative_symlink(link: &Path, target: &Path, overwrite: bool) -> Result<()> {
    if overwrite && (link.exists() || link.is_symlink()) {
        if link.is_dir() && !link.is_symlink() {
            bail!(
                "refusing to replace directory {} with symlink",
                link.display()
            );
        }
        std::fs::remove_file(link).with_context(|| format!("removing {}", link.display()))?;
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let rel = relative_path(link.parent().unwrap_or_else(|| Path::new(".")), target);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&rel, link)
        .with_context(|| format!("symlink {} -> {}", link.display(), rel.display()))?;
    Ok(())
}

fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from = components(from_dir);
    let to = components(to);
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to[common..] {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::RootDir => Some("/".to_string()),
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn link_opt_rejects_unsafe_names_before_creating_link_storage() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let keg = tmp.path().join("Cellar/tool/1.0");
        std::fs::create_dir_all(&keg).unwrap();

        let error = link_opt(
            &prefix,
            &PackageName("tool".to_string()),
            &[PackageName("../escape".to_string())],
            &keg,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("stable package link alias"));
        assert!(!prefix.0.join("opt").exists());
        assert!(!prefix.0.join("escape").exists());
    }

    #[test]
    fn link_opt_rejects_existing_case_equivalent_name() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let opt = prefix.0.join("opt");
        let keg = tmp.path().join("Cellar/tool/1.0");
        std::fs::create_dir_all(&opt).unwrap();
        std::fs::create_dir_all(&keg).unwrap();
        std::fs::write(opt.join("Tool"), b"sentinel").unwrap();

        let error = link_opt(&prefix, &PackageName("tool".to_string()), &[], &keg)
            .unwrap_err()
            .to_string();

        assert!(error.contains("filesystem-equivalent"));
        assert_eq!(std::fs::read(opt.join("Tool")).unwrap(), b"sentinel");
    }

    #[test]
    fn overwrite_refuses_to_replace_real_directory_with_symlink() {
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("opt/tool");
        let target = tmp.path().join("Cellar/tool/1.0");
        std::fs::create_dir_all(&link).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(link.join("sentinel"), b"keep").unwrap();

        let err = make_relative_symlink(&link, &target, true).unwrap_err();

        assert!(err.to_string().contains("refusing to replace directory"));
        assert_eq!(
            std::fs::read_to_string(link.join("sentinel")).unwrap(),
            "keep"
        );
    }
}
