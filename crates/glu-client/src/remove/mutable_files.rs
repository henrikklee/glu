use anyhow::Result;
use glu_core::{InstalledPackage, PackageId, Prefix};
use ring::digest;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutableFileArea {
    Etc,
    Var,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    sha256: String,
    mode: u32,
}

#[derive(Debug, Clone)]
pub struct MutableFilePlan {
    pub path: PathBuf,
    pub area: MutableFileArea,
    fingerprint: FileFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainedMutableFileReason {
    Shared,
    UnsafeType,
    Unreadable,
}

#[derive(Debug, Clone)]
pub struct RetainedMutableFile {
    pub path: PathBuf,
    pub area: MutableFileArea,
    pub reason: RetainedMutableFileReason,
}

#[derive(Debug, Clone, Default)]
pub struct MutableFileCleanupPlan {
    /// Exact uniquely owned files still matching a bottled default.
    pub unchanged: Vec<MutableFilePlan>,
    /// Exact uniquely owned files whose contents or mode differ.
    pub modified: Vec<MutableFilePlan>,
    /// Existing mapped paths that cannot be removed safely.
    pub retained: Vec<RetainedMutableFile>,
    prune_directories: Vec<PathBuf>,
    retain_directories: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct MutableFileCleanupResult {
    pub removed: Vec<PathBuf>,
    pub retained_modified: Vec<PathBuf>,
    pub retained_shared: Vec<PathBuf>,
    pub retained_ambiguous: Vec<PathBuf>,
    pub retained_changed: Vec<PathBuf>,
}

#[derive(Debug)]
struct MutableFileClaim {
    package: PackageId,
    source: PathBuf,
    area: MutableFileArea,
}

/// Computes exact mutable-file ownership from installed `.bottle/{etc,var}`
/// trees before any keg is removed. Name-based guesses are deliberately not
/// part of this inventory.
pub fn plan_mutable_file_cleanup(
    prefix: &Prefix,
    installed: &[InstalledPackage],
    removing: &[InstalledPackage],
) -> Result<MutableFileCleanupPlan> {
    let removing_ids: BTreeSet<&PackageId> = removing.iter().map(|package| &package.id).collect();
    let mut claims: BTreeMap<PathBuf, Vec<MutableFileClaim>> = BTreeMap::new();
    let mut directory_claims: BTreeMap<PathBuf, BTreeSet<PackageId>> = BTreeMap::new();
    for package in installed {
        let bottle = package.keg_path.join(".bottle");
        for (subdir, area) in [("etc", MutableFileArea::Etc), ("var", MutableFileArea::Var)] {
            let root = bottle.join(subdir);
            if root.is_dir() {
                collect_mutable_file_claims(
                    prefix,
                    package,
                    &bottle,
                    &root,
                    area,
                    &mut claims,
                    &mut directory_claims,
                )?;
            }
        }
    }

    let claimed_destinations: BTreeSet<PathBuf> = claims.keys().cloned().collect();
    let mut plan = MutableFileCleanupPlan::default();
    for (path, owners) in directory_claims {
        if owners.iter().all(|owner| removing_ids.contains(owner)) {
            plan.prune_directories.push(path);
        } else {
            plan.retain_directories.insert(path);
        }
    }
    plan.prune_directories.sort();
    for (destination, owners) in claims {
        if !owners
            .iter()
            .any(|owner| removing_ids.contains(&owner.package))
        {
            continue;
        }
        let retained_owner = owners
            .iter()
            .any(|owner| !removing_ids.contains(&owner.package));
        let area = owners[0].area;
        classify_mutable_destination(
            prefix,
            &destination,
            area,
            &owners,
            retained_owner,
            &mut plan,
        );
        let default = default_sibling(&destination);
        if !claimed_destinations.contains(&default) {
            classify_mutable_destination(
                prefix,
                &default,
                area,
                &owners,
                retained_owner,
                &mut plan,
            );
        }
    }
    plan.unchanged.sort_by(|a, b| a.path.cmp(&b.path));
    plan.modified.sort_by(|a, b| a.path.cmp(&b.path));
    plan.retained.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(plan)
}

fn collect_mutable_file_claims(
    prefix: &Prefix,
    package: &InstalledPackage,
    bottle: &Path,
    current: &Path,
    area: MutableFileArea,
    claims: &mut BTreeMap<PathBuf, Vec<MutableFileClaim>>,
    directory_claims: &mut BTreeMap<PathBuf, BTreeSet<PackageId>>,
) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let source = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir()
            || (file_type.is_symlink()
                && source
                    .metadata()
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false))
        {
            let relative = source.strip_prefix(bottle).map_err(|_| {
                anyhow::anyhow!("mutable package path escaped .bottle: {}", source.display())
            })?;
            directory_claims
                .entry(prefix.0.join(relative))
                .or_default()
                .insert(package.id.clone());
            collect_mutable_file_claims(
                prefix,
                package,
                bottle,
                &source,
                area,
                claims,
                directory_claims,
            )?;
            continue;
        }
        let relative = source.strip_prefix(bottle).map_err(|_| {
            anyhow::anyhow!("mutable package path escaped .bottle: {}", source.display())
        })?;
        let destination = prefix.0.join(relative);
        claims
            .entry(destination)
            .or_default()
            .push(MutableFileClaim {
                package: package.id.clone(),
                source,
                area,
            });
    }
    Ok(())
}

fn classify_mutable_destination(
    prefix: &Prefix,
    path: &Path,
    area: MutableFileArea,
    owners: &[MutableFileClaim],
    retained_owner: bool,
    plan: &mut MutableFileCleanupPlan,
) {
    if !path_has_safe_mutable_ancestors(prefix, path) {
        if path.exists() || path.is_symlink() {
            plan.retained.push(RetainedMutableFile {
                path: path.to_path_buf(),
                area,
                reason: RetainedMutableFileReason::UnsafeType,
            });
        }
        return;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            plan.retained.push(RetainedMutableFile {
                path: path.to_path_buf(),
                area,
                reason: RetainedMutableFileReason::Unreadable,
            });
            return;
        }
    };
    if retained_owner {
        plan.retained.push(RetainedMutableFile {
            path: path.to_path_buf(),
            area,
            reason: RetainedMutableFileReason::Shared,
        });
        return;
    }
    if !metadata.file_type().is_file() {
        plan.retained.push(RetainedMutableFile {
            path: path.to_path_buf(),
            area,
            reason: RetainedMutableFileReason::UnsafeType,
        });
        return;
    }
    let Ok(live) = fingerprint(path) else {
        plan.retained.push(RetainedMutableFile {
            path: path.to_path_buf(),
            area,
            reason: RetainedMutableFileReason::Unreadable,
        });
        return;
    };
    let unchanged = owners.iter().any(|owner| {
        fingerprint(&owner.source)
            .map(|source| source == live)
            .unwrap_or(false)
    });
    let file = MutableFilePlan {
        path: path.to_path_buf(),
        area,
        fingerprint: live,
    };
    if unchanged {
        plan.unchanged.push(file);
    } else {
        plan.modified.push(file);
    }
}

fn default_sibling(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".default");
    PathBuf::from(value)
}

fn fingerprint(path: &Path) -> io::Result<FileFingerprint> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a file"));
    }
    let mut file = fs::File::open(path)?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(FileFingerprint {
        sha256: crate::hash::hex_lower(context.finish().as_ref()),
        mode: metadata.permissions().mode() & 0o7777,
    })
}

pub fn execute_mutable_file_cleanup(
    prefix: &Prefix,
    plan: &MutableFileCleanupPlan,
    remove_modified: bool,
) -> Result<MutableFileCleanupResult> {
    let mut result = MutableFileCleanupResult {
        retained_modified: if remove_modified {
            Vec::new()
        } else {
            plan.modified.iter().map(|file| file.path.clone()).collect()
        },
        retained_shared: plan
            .retained
            .iter()
            .filter(|file| file.reason == RetainedMutableFileReason::Shared)
            .map(|file| file.path.clone())
            .collect(),
        retained_ambiguous: plan
            .retained
            .iter()
            .filter(|file| file.reason != RetainedMutableFileReason::Shared)
            .map(|file| file.path.clone())
            .collect(),
        ..MutableFileCleanupResult::default()
    };
    let approved = plan.unchanged.iter().chain(
        remove_modified
            .then_some(plan.modified.iter())
            .into_iter()
            .flatten(),
    );
    let mut parents: BTreeSet<PathBuf> = plan.prune_directories.iter().cloned().collect();
    for file in approved {
        if !path_has_safe_mutable_ancestors(prefix, &file.path) {
            result.retained_changed.push(file.path.clone());
            continue;
        }
        let current = fs::symlink_metadata(&file.path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .and_then(|_| fingerprint(&file.path).ok());
        if current.as_ref() != Some(&file.fingerprint) {
            if file.path.exists() || file.path.is_symlink() {
                result.retained_changed.push(file.path.clone());
            }
            continue;
        }
        fs::remove_file(&file.path)?;
        result.removed.push(file.path.clone());
        if let Some(parent) = file.path.parent() {
            parents.insert(parent.to_path_buf());
        }
    }
    prune_empty_mutable_directories(prefix, parents, &plan.retain_directories);
    Ok(result)
}

fn path_has_safe_mutable_ancestors(prefix: &Prefix, path: &Path) -> bool {
    let root = [prefix.0.join("etc"), prefix.0.join("var")]
        .into_iter()
        .find(|root| path.starts_with(root));
    let Some(root) = root else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    if !relative
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let mut current = root;
    if fs::symlink_metadata(&current)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return false;
    }
    let Some(parent) = relative.parent() else {
        return true;
    };
    for component in parent.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return false;
        }
    }
    true
}

fn prune_empty_mutable_directories(
    prefix: &Prefix,
    parents: BTreeSet<PathBuf>,
    retained: &BTreeSet<PathBuf>,
) {
    let etc = prefix.0.join("etc");
    let var = prefix.0.join("var");
    let mut directories = BTreeSet::new();
    for parent in parents {
        let mut current = Some(parent.as_path());
        while let Some(directory) = current {
            if directory == etc
                || directory == var
                || !path_has_safe_mutable_ancestors(prefix, directory)
            {
                break;
            }
            directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
    }
    for directory in directories.into_iter().rev() {
        if retained.contains(&directory) {
            continue;
        }
        match fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(_) => {}
        }
    }
}
