use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct PackageName(pub String);

/// A user- or dependency-supplied package spelling. Selectors are resolved at
/// authority boundaries and never participate in graph topology.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct PackageSelector(pub String);

/// Stable logical package identity across concrete releases.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct PackageKey(pub String);

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct PackageId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub package_key: PackageKey,
    pub name: PackageName,
    pub aliases: Vec<PackageSelector>,
    pub oldnames: Vec<PackageSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSelection {
    pub requested_as: PackageSelector,
    pub package_key: PackageKey,
    pub package: PackageId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArtifactId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KegVersion(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Prefix(pub PathBuf);

/// Default distribution root for the glu client itself (releases).
/// `scripts/install.sh` mirrors this constant.
pub const DEFAULT_DISTRIBUTION_BASE_URL: &str = "https://github.com/henrikklee/glu/releases";

/// Registry origin the *published* client talks to — build-time policy, not
/// runtime config: production builds (default Cargo features) use this fixed
/// HTTPS origin with no `GLU_REGISTRY` override, no HTTP, and no mirrors;
/// dev builds opt in via the `dev-registry` feature (see
/// crates/glu-client/src/config/mod.rs).
pub const DEFAULT_REGISTRY_BASE_URL: &str = "https://registry.glu.run";

/// Release asset target for the running host. glu is Apple Silicon-only
/// (arm64) for v0.1, matching the installer's platform gate.
pub const DISTRIBUTION_TARGET: &str = "aarch64-apple-darwin";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallManifest {
    pub schema: String,
    pub request: ResolveRequestEcho,
    pub roots: Vec<PackageSelection>,
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
    pub artifacts: BTreeMap<ArtifactId, ResolvedArtifact>,
}

impl InstallManifest {
    pub fn root_package_ids(&self) -> Vec<PackageId> {
        self.roots.iter().map(|root| root.package.clone()).collect()
    }

    pub fn is_root(&self, package_id: &PackageId) -> bool {
        self.roots.iter().any(|root| &root.package == package_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRequestEcho {
    pub name: Vec<PackageSelector>,
    pub target: Target,
    /// Whether the response is the slim variant (no artifact/install
    /// metadata). Full responses omit the field, which represents `false`.
    #[serde(default)]
    pub slim: bool,
}

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub names: Vec<PackageSelector>,
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPackage {
    pub package_key: PackageKey,
    pub name: PackageName,
    pub aliases: Vec<PackageSelector>,
    pub oldnames: Vec<PackageSelector>,
    pub version: String,
    pub revision: u32,
    pub keg_version: KegVersion,
    #[serde(default)]
    pub deps: Vec<PackageDependency>,
    /// Complete flattened minimum versions recorded by this package's
    /// selected artifact. These constraints belong to the package, not to
    /// its direct dependency edges.
    pub dependency_requirements: BTreeMap<PackageKey, MinimumVersion>,
    pub artifact: ArtifactId,
    pub install: PackageInstallMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLinkMetadata {
    pub name: PackageName,
    /// Additional filesystem opt-link names. These are not selector aliases
    /// and never participate in package identity or graph traversal.
    pub opt_names: Vec<PackageName>,
    pub keg_only: bool,
    pub link_overwrite: Vec<String>,
}

impl From<&ResolvedPackage> for PackageLinkMetadata {
    fn from(package: &ResolvedPackage) -> Self {
        Self {
            name: package.name.clone(),
            opt_names: package.install.opt_names.clone(),
            keg_only: package.install.keg_only,
            link_overwrite: package.install.link_overwrite.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PackageDependency {
    pub package_key: PackageKey,
    pub package: PackageId,
    pub requested_as: PackageSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MinimumVersion {
    pub version: String,
    /// An omitted revision means there is no revision floor. That is distinct
    /// from an explicitly recorded revision zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInstallMetadata {
    /// Additional filesystem opt-link names. These are not package selectors.
    pub opt_names: Vec<PackageName>,
    #[serde(default)]
    pub keg_only: bool,
    #[serde(default)]
    pub link_overwrite: Vec<String>,
    #[serde(default)]
    pub post_install_defined: bool,
    #[serde(default)]
    pub post_install_steps: Vec<serde_json::Value>,
    /// Formula-level Homebrew postinstall sandbox network policy:
    /// `formula.network_access_allowed?(:postinstall)`. Bulk formula.json does
    /// not expose this today; defaulting to Homebrew's default (`true`) keeps
    /// old registry responses compatible until ingest derives the exact fact.
    #[serde(default = "default_true")]
    pub postinstall_network_access_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedArtifact {
    pub url: String,
    pub sha256: String,
    pub bytes: Option<u64>,
    pub bottle_tag: String,
    pub cellar: String,
    /// The bottle build's preferred perl version, when the registry has it.
    /// Older registry responses may send the full Homebrew `built_on` object;
    /// serde ignores the unused fields.
    #[serde(default)]
    pub built_on: Option<BuiltOn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltOn {
    /// The perl version on the build machine, e.g. `"5.34"`.
    #[serde(default)]
    pub preferred_perl: Option<String>,
}

/// Slim resolve/uses package record: the dependency structure without any
/// install or artifact metadata — what read-only consumers need (`glu deps`
/// on a not-installed package, `glu uses`), an order of magnitude smaller
/// for large closures. Never an install manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlimPackage {
    pub package_key: PackageKey,
    pub name: PackageName,
    pub aliases: Vec<PackageSelector>,
    pub oldnames: Vec<PackageSelector>,
    pub version: String,
    pub revision: u32,
    pub keg_version: KegVersion,
    #[serde(default)]
    pub deps: Vec<PackageDependency>,
    #[serde(default)]
    pub dependency_requirements: BTreeMap<PackageKey, MinimumVersion>,
}

/// Slim resolve response (`?slim=true`): the same graph as the full
/// resolve — same roots, same resolution, same closure — with minimal
/// package records (see `SlimPackage`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlimManifest {
    pub schema: String,
    pub request: ResolveRequestEcho,
    pub roots: Vec<PackageSelection>,
    pub packages: BTreeMap<PackageId, SlimPackage>,
}

/// `GET /v1/uses` request echo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsesRequestEcho {
    pub name: PackageSelector,
    pub target: Target,
    #[serde(default)]
    pub direct: bool,
}

/// `GET /v1/uses` response: the requested package plus every package that
/// (transitively) depends on it, as slim records with full `deps` edges so
/// the client walks upward (packages whose deps include the target,
/// recursively) to build the reverse tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsesResponse {
    pub schema: String,
    pub request: UsesRequestEcho,
    pub roots: Vec<PackageSelection>,
    pub packages: BTreeMap<PackageId, SlimPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InfoResponse {
    pub schema: String,
    pub requested_as: PackageSelector,
    pub package_key: PackageKey,
    pub package: PackageId,
    pub name: PackageName,
    pub version: String,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    pub dependencies: InfoDependencies,
    #[serde(default)]
    pub download_bytes: Option<u64>,
    #[serde(default)]
    pub installed_bytes: Option<u64>,
    #[serde(default)]
    pub download_bytes_with_dependencies: Option<u64>,
    #[serde(default)]
    pub installed_bytes_with_dependencies: Option<u64>,
    pub bottle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InfoDependencies {
    pub direct: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutdatedResponse {
    pub schema: String,
    pub packages: Vec<OutdatedPackage>,
}

/// One package's staleness envelope from `/v1/outdated`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutdatedPackage {
    pub requested_as: PackageSelector,
    pub package_key: PackageKey,
    pub name: PackageName,
    /// Newest visible version installable on the requested target; `null`
    /// when no compatible bottle exists (e.g. published but bottle-less).
    pub update: Option<VersionRevision>,
    /// Newest visible version overall, regardless of target installability.
    pub latest: VersionRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRevision {
    pub package: PackageId,
    pub version: String,
    #[serde(default)]
    pub revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InstalledPackage {
    pub id: PackageId,
    pub package_key: PackageKey,
    pub name: PackageName,
    pub aliases: Vec<PackageSelector>,
    pub oldnames: Vec<PackageSelector>,
    pub version: String,
    pub revision: u32,
    pub keg_version: KegVersion,
    pub keg_path: PathBuf,
    pub opt_path: PathBuf,
    #[serde(default)]
    pub keg_only: bool,
    #[serde(default)]
    pub linked: bool,
    pub deps: Vec<PackageDependency>,
    pub dependency_requirements: BTreeMap<PackageKey, MinimumVersion>,
    #[serde(default)]
    pub download_bytes: Option<u64>,
    #[serde(default)]
    pub installed_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_install_metadata_defaults_postinstall_network_to_homebrew_default() {
        let json = r#"{
            "opt_names": [],
            "keg_only": false,
            "link_overwrite": [],
            "post_install_defined": false,
            "post_install_steps": []
        }"#;
        let metadata: PackageInstallMetadata = serde_json::from_str(json).unwrap();
        assert!(metadata.postinstall_network_access_allowed);
    }

    #[test]
    fn package_install_metadata_defaults_omitted_false_and_empty_fields() {
        let metadata: PackageInstallMetadata = serde_json::from_str(r#"{"opt_names":[]}"#).unwrap();
        assert!(metadata.opt_names.is_empty());
        assert!(!metadata.keg_only);
        assert!(metadata.link_overwrite.is_empty());
        assert!(!metadata.post_install_defined);
        assert!(metadata.post_install_steps.is_empty());
    }

    #[test]
    fn slim_package_decodes_v1_registry_shape() {
        let json = r#"{"package_key":"package:pkg","name":"pkg","aliases":[],"oldnames":[],"version":"1.0","revision":0,"keg_version":"1.0"}"#;
        let package: SlimPackage = serde_json::from_str(json).unwrap();
        assert_eq!(package.name.0, "pkg");
        assert_eq!(package.keg_version.0, "1.0");
        assert_eq!(package.version, "1.0");
        assert_eq!(package.revision, 0);
        assert!(package.deps.is_empty());
        assert!(package.dependency_requirements.is_empty());
    }

    #[test]
    fn resolved_package_decodes_v1_identity() {
        let json = r#"{
            "package_key":"package:pkg",
            "name":"pkg",
            "aliases":[],
            "oldnames":[],
            "version":"1.0",
            "revision":0,
            "keg_version":"1.0",
            "deps":[{
                "package_key":"package:dep",
                "package":"pkg:homebrew/core/dep@2.0",
                "requested_as":"dep@2"
            }],
            "dependency_requirements":{
                "package:dep":{"version":"2.0"}
            },
            "artifact":"art:sha256:abc",
            "install":{"opt_names":[]}
        }"#;
        let package: ResolvedPackage = serde_json::from_str(json).unwrap();
        assert!(package.aliases.is_empty());
        assert!(package.oldnames.is_empty());
        assert_eq!(package.deps[0].requested_as.0, "dep@2");
        assert_eq!(
            package.dependency_requirements[&PackageKey("package:dep".to_string())].revision,
            None
        );
        assert!(!package.install.keg_only);
    }
}
