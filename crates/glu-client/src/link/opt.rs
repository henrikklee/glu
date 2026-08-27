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
    let opt = prefix.0.join("opt");
    std::fs::create_dir_all(&opt).with_context(|| format!("creating {}", opt.display()))?;

    make_relative_symlink(&opt.join(&name.0), keg, true)?;
    for alias in aliases {
        make_relative_symlink(&opt.join(&alias.0), keg, true)?;
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
