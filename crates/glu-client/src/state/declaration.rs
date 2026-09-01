use anyhow::{Context, Result};
use glu_core::{PackageName, Prefix};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

/// The declaration file name at the prefix root (`/opt/glustore/glu.json`).
pub const DECLARATION_FILE_NAME: &str = "glu.json";

fn default_schema() -> String {
    "glu.declaration.v1".to_string()
}

/// The declaration: which packages the user wants, each with the version
/// sync last resolved for it. Versions are advisory — the declaration is
/// written by sync and never read as a constraint; the dependency closure
/// wins when it disagrees and the recorded version is rewritten with a
/// notice (docs/explanation/state-model.md, The declaration).
///
/// The declaration is the source of declared package membership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Declaration {
    #[serde(default = "default_schema")]
    pub schema: String,
    #[serde(default)]
    pub dependencies: BTreeMap<PackageName, String>,
    #[serde(default)]
    pub deactivated: BTreeMap<PackageName, bool>,
}

impl Default for Declaration {
    fn default() -> Self {
        Self {
            schema: default_schema(),
            dependencies: BTreeMap::new(),
            deactivated: BTreeMap::new(),
        }
    }
}

impl Declaration {
    /// Loads `<prefix>/glu.json`. `None` when the file has not been created.
    pub(super) fn load(prefix: &Prefix) -> Result<Option<Self>> {
        let path = Self::path(prefix);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let declaration: Declaration = serde_json::from_slice(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        Ok(Some(declaration))
    }

    pub fn names(&self) -> BTreeSet<PackageName> {
        self.dependencies.keys().cloned().collect()
    }

    pub fn deactivated_names(&self) -> BTreeSet<PackageName> {
        self.deactivated
            .iter()
            .filter_map(|(name, deactivated)| deactivated.then_some(name.clone()))
            .collect()
    }

    pub fn contains(&self, name: &PackageName) -> bool {
        self.dependencies.contains_key(name)
    }

    pub(super) fn path(prefix: &Prefix) -> PathBuf {
        prefix.0.join(DECLARATION_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store::InstalledStateStore;
    use tempfile::TempDir;

    #[test]
    fn write_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().to_path_buf());
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("vips".to_string()), "8.19.0".to_string());
        declaration
            .dependencies
            .insert(PackageName("ffmpeg".to_string()), "7.1".to_string());
        InstalledStateStore::new(prefix.clone())
            .write_declaration(&declaration)
            .unwrap();

        let loaded = Declaration::load(&prefix).unwrap().unwrap();
        assert_eq!(loaded.schema, "glu.declaration.v1");
        assert_eq!(loaded.dependencies.len(), 2);
        assert!(loaded.deactivated.is_empty());
        assert_eq!(
            loaded.dependencies.get(&PackageName("vips".to_string())),
            Some(&"8.19.0".to_string())
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_does_not_follow_predictable_temp_or_destination_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().to_path_buf());
        let outside_temp = dir.path().join("outside-temp");
        let outside_destination = dir.path().join("outside-destination");
        std::fs::write(&outside_temp, b"temp sentinel").unwrap();
        std::fs::write(&outside_destination, b"destination sentinel").unwrap();
        std::os::unix::fs::symlink(&outside_temp, dir.path().join("glu.json.tmp")).unwrap();
        std::os::unix::fs::symlink(&outside_destination, dir.path().join("glu.json")).unwrap();

        InstalledStateStore::new(prefix)
            .write_declaration(&Declaration::default())
            .unwrap();

        assert_eq!(std::fs::read(outside_temp).unwrap(), b"temp sentinel");
        assert_eq!(
            std::fs::read(outside_destination).unwrap(),
            b"destination sentinel"
        );
        assert!(!dir.path().join("glu.json").is_symlink());
        assert_eq!(
            std::fs::metadata(dir.path().join("glu.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn load_absent_file_is_none() {
        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().to_path_buf());
        assert!(Declaration::load(&prefix).unwrap().is_none());
    }

    #[test]
    fn deactivated_round_trips() {
        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().to_path_buf());
        let mut declaration = Declaration::default();
        declaration
            .deactivated
            .insert(PackageName("openssl".to_string()), true);
        InstalledStateStore::new(prefix.clone())
            .write_declaration(&declaration)
            .unwrap();

        let loaded = Declaration::load(&prefix).unwrap().unwrap();
        assert_eq!(
            loaded.deactivated_names(),
            [PackageName("openssl".to_string())].into_iter().collect()
        );
    }

    #[test]
    fn load_or_default_prefers_the_file() {
        let dir = TempDir::new().unwrap();
        let prefix = Prefix(dir.path().to_path_buf());
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("foo".to_string()), "1.0".to_string());
        InstalledStateStore::new(prefix.clone())
            .write_declaration(&declaration)
            .unwrap();

        let loaded = Declaration::load(&prefix).unwrap().unwrap_or_default();
        assert!(loaded.contains(&PackageName("foo".to_string())));
        assert!(!loaded.contains(&PackageName("vips".to_string())));
    }
}
