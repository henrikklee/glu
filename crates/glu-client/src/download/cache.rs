use anyhow::{bail, Context, Result};
use glu_core::{KegVersion, PackageName, Prefix, ResolvedArtifact};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBottlePackage {
    pub name: PackageName,
    pub keg_version: KegVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBottle {
    relative_path: PathBuf,
    bytes: u64,
    package: Option<CachedBottlePackage>,
}

impl CachedBottle {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn package(&self) -> Option<&CachedBottlePackage> {
        self.package.as_ref()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheCleanupPlan {
    bottles: Vec<CachedBottle>,
    reclaimable_bytes: u64,
}

impl CacheCleanupPlan {
    pub fn bottles(&self) -> &[CachedBottle] {
        &self.bottles
    }

    pub fn reclaimable_bytes(&self) -> u64 {
        self.reclaimable_bytes
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheCleanupResult {
    removed: Vec<CachedBottle>,
    reclaimed_bytes: u64,
}

impl CacheCleanupResult {
    pub fn removed(&self) -> &[CachedBottle] {
        &self.removed
    }

    pub fn reclaimed_bytes(&self) -> u64 {
        self.reclaimed_bytes
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactCache {
    root: PathBuf,
}

impl ArtifactCache {
    pub fn new(prefix: &Prefix) -> Self {
        Self {
            root: prefix.0.join("var/glu/cache/artifacts"),
        }
    }

    /// Admitted artifact path. A file here is named by digest and should only
    /// arrive via temp-file rename after download-side sha256 verification.
    pub fn path_for_sha256(&self, sha256: &str) -> PathBuf {
        self.root.join("sha256").join(sha256)
    }

    /// In-progress artifact path. Single-stream and multipart downloads both
    /// write here first; incomplete/range-written bytes never live in `sha256/`.
    pub fn temp_path_for_sha256(&self, sha256: &str) -> Result<PathBuf> {
        let mut random = [0_u8; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| anyhow::anyhow!("generating artifact staging name"))?;
        Ok(self.root.join("tmp").join(format!(
            "{sha256}.{}.{}.tmp",
            std::process::id(),
            crate::hash::hex_lower(&random)
        )))
    }

    pub async fn ensure_dirs(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.root.join("sha256"))
            .await
            .with_context(|| format!("creating {}", self.root.join("sha256").display()))?;
        tokio::fs::create_dir_all(self.root.join("tmp"))
            .await
            .with_context(|| format!("creating {}", self.root.join("tmp").display()))?;
        Ok(())
    }

    pub fn path_for_artifact(&self, artifact: &ResolvedArtifact) -> PathBuf {
        self.path_for_sha256(&artifact.sha256)
    }

    pub fn temp_path_for_artifact(&self, artifact: &ResolvedArtifact) -> Result<PathBuf> {
        self.temp_path_for_sha256(&artifact.sha256)
    }

    pub async fn remove_artifact(&self, artifact: &ResolvedArtifact) -> Result<()> {
        match tokio::fs::remove_file(self.path_for_artifact(artifact)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing cached artifact"),
        }
    }

    /// Builds a read-only snapshot of admitted bottles and incomplete bottle
    /// downloads. Only the two directories owned by the artifact cache are
    /// inspected; directory symlinks are rejected rather than followed.
    pub fn plan_cleanup(&self) -> Result<CacheCleanupPlan> {
        self.plan_cleanup_with_packages(&BTreeMap::new())
    }

    pub fn plan_cleanup_with_packages(
        &self,
        packages_by_sha256: &BTreeMap<String, CachedBottlePackage>,
    ) -> Result<CacheCleanupPlan> {
        self.validate_cache_root()?;
        let mut bottles = Vec::new();
        self.scan_cleanup_dir("sha256", packages_by_sha256, &mut bottles)?;
        self.scan_cleanup_dir("tmp", packages_by_sha256, &mut bottles)?;
        bottles.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let reclaimable_bytes = bottles.iter().try_fold(0_u64, |total, bottle| {
            total
                .checked_add(bottle.bytes)
                .context("cached bottle size exceeds supported range")
        })?;
        Ok(CacheCleanupPlan {
            bottles,
            reclaimable_bytes,
        })
    }

    /// Removes exactly the entries captured by `plan`. Artifacts created after
    /// planning are retained, so confirmation never approves a broader cleanup
    /// than the user was shown.
    pub fn execute_cleanup(&self, plan: &CacheCleanupPlan) -> Result<CacheCleanupResult> {
        self.validate_cache_root()?;
        let mut removed = Vec::new();
        let mut reclaimed_bytes = 0_u64;
        for bottle in &plan.bottles {
            let Some(parent) = bottle.relative_path.parent() else {
                bail!(
                    "invalid cached bottle path {}",
                    bottle.relative_path.display()
                );
            };
            if parent != Path::new("sha256") && parent != Path::new("tmp") {
                bail!("cached bottle escaped artifact cache");
            }
            self.validate_cleanup_dir(parent)?;
            let path = self.root.join(&bottle.relative_path);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", path.display()))
                }
            };
            if metadata.file_type().is_dir() {
                bail!("refusing to remove cache directory {}", path.display());
            }
            fs::remove_file(&path)
                .with_context(|| format!("removing cached bottle {}", path.display()))?;
            reclaimed_bytes = reclaimed_bytes
                .checked_add(metadata.len())
                .context("removed bottle size exceeds supported range")?;
            removed.push(CachedBottle {
                relative_path: bottle.relative_path.clone(),
                bytes: metadata.len(),
                package: bottle.package.clone(),
            });
        }
        Ok(CacheCleanupResult {
            removed,
            reclaimed_bytes,
        })
    }

    fn validate_cache_root(&self) -> Result<()> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
            Ok(_) => bail!(
                "artifact cache root is not a directory: {}",
                self.root.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.root.display())),
        }
    }

    fn validate_cleanup_dir(&self, relative: &Path) -> Result<()> {
        let path = self.root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
            Ok(_) => bail!("cache path is not a directory: {}", path.display()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn scan_cleanup_dir(
        &self,
        name: &str,
        packages_by_sha256: &BTreeMap<String, CachedBottlePackage>,
        bottles: &mut Vec<CachedBottle>,
    ) -> Result<()> {
        let relative = Path::new(name);
        self.validate_cleanup_dir(relative)?;
        let path = self.root.join(relative);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading entry in {}", path.display()))?;
            let metadata = fs::symlink_metadata(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if metadata.file_type().is_dir() {
                continue;
            }
            let file_name = entry.file_name();
            let digest = if name == "sha256" {
                file_name.to_str()
            } else {
                file_name.to_str().and_then(|name| name.split('.').next())
            };
            bottles.push(CachedBottle {
                relative_path: relative.join(&file_name),
                bytes: metadata.len(),
                package: digest.and_then(|digest| packages_by_sha256.get(digest).cloned()),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ArtifactCache, CachedBottlePackage};
    use glu_core::{KegVersion, PackageName, Prefix};
    use std::{collections::BTreeMap, fs};

    fn cache(prefix: &std::path::Path) -> ArtifactCache {
        ArtifactCache::new(&Prefix(prefix.to_path_buf()))
    }

    #[test]
    fn staging_paths_are_random_per_transfer_and_keep_the_digest_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let cache = cache(temp.path());
        let digest = "a".repeat(64);
        let first = cache.temp_path_for_sha256(&digest).unwrap();
        let second = cache.temp_path_for_sha256(&digest).unwrap();
        assert_ne!(first, second);
        for path in [first, second] {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with(&format!("{digest}.{}.", std::process::id())));
            assert!(name.ends_with(".tmp"));
        }
    }

    #[test]
    fn cleanup_plans_admitted_and_partial_bottles_with_sizes() {
        let prefix = tempfile::tempdir().unwrap();
        let root = prefix.path().join("var/glu/cache/artifacts");
        fs::create_dir_all(root.join("sha256")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        fs::write(root.join("sha256/bbb"), b"bottle").unwrap();
        fs::write(root.join("sha256/aaa"), b"abc").unwrap();
        fs::write(root.join("tmp/partial.tmp"), b"12").unwrap();

        let packages = BTreeMap::from([(
            "aaa".to_string(),
            CachedBottlePackage {
                name: PackageName("demo".to_string()),
                keg_version: KegVersion("1.0".to_string()),
            },
        )]);
        let plan = cache(prefix.path())
            .plan_cleanup_with_packages(&packages)
            .unwrap();

        let entries = plan
            .bottles()
            .iter()
            .map(|bottle| {
                (
                    bottle.relative_path().to_string_lossy().into_owned(),
                    bottle.bytes(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![
                ("sha256/aaa".to_string(), 3),
                ("sha256/bbb".to_string(), 6),
                ("tmp/partial.tmp".to_string(), 2),
            ]
        );
        assert_eq!(plan.reclaimable_bytes(), 11);
        let package = plan.bottles()[0].package().unwrap();
        assert_eq!(package.name.0, "demo");
        assert_eq!(package.keg_version.0, "1.0");
        assert!(plan.bottles()[1].package().is_none());
    }

    #[test]
    fn cleanup_executes_only_the_planned_entries() {
        let prefix = tempfile::tempdir().unwrap();
        let root = prefix.path().join("var/glu/cache/artifacts");
        fs::create_dir_all(root.join("sha256")).unwrap();
        fs::write(root.join("sha256/planned"), b"old").unwrap();
        let cache = cache(prefix.path());
        let plan = cache.plan_cleanup().unwrap();
        fs::write(root.join("sha256/created-later"), b"new bottle").unwrap();

        let result = cache.execute_cleanup(&plan).unwrap();

        assert_eq!(result.removed().len(), 1);
        assert_eq!(
            result.removed()[0].relative_path(),
            std::path::Path::new("sha256/planned")
        );
        assert_eq!(result.reclaimed_bytes(), 3);
        assert!(!root.join("sha256/planned").exists());
        assert!(root.join("sha256/created-later").exists());
    }

    #[test]
    fn cleanup_leaves_installed_and_unrelated_glu_state_untouched() {
        let prefix = tempfile::tempdir().unwrap();
        let artifact = prefix.path().join("var/glu/cache/artifacts/sha256/bottle");
        let other_cache = prefix.path().join("var/glu/cache/postinstall/state");
        let receipt = prefix.path().join("Cellar/pkg/1.0/.glu/receipt.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::create_dir_all(other_cache.parent().unwrap()).unwrap();
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(&artifact, b"bottle").unwrap();
        fs::write(&other_cache, b"state").unwrap();
        fs::write(&receipt, b"receipt").unwrap();
        let cache = cache(prefix.path());

        let result = cache
            .execute_cleanup(&cache.plan_cleanup().unwrap())
            .unwrap();

        assert_eq!(result.removed().len(), 1);
        assert!(other_cache.exists());
        assert!(receipt.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_to_follow_cache_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let prefix = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = prefix.path().join("var/glu/cache/artifacts");
        fs::create_dir_all(&root).unwrap();
        fs::write(outside.path().join("bottle"), b"outside").unwrap();
        symlink(outside.path(), root.join("sha256")).unwrap();

        let error = cache(prefix.path()).plan_cleanup().unwrap_err();

        assert!(error.to_string().contains("cache path is not a directory"));
        assert!(outside.path().join("bottle").exists());
    }
}
