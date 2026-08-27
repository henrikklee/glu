use anyhow::Result;
use glu_core::{ArtifactId, InstallManifest, PackageId, ResolvedArtifact, ResolvedPackage};

pub(crate) trait ManifestLookup {
    fn require_package(&self, package_id: &PackageId) -> Result<&ResolvedPackage>;
    fn require_artifact(&self, artifact_id: &ArtifactId) -> Result<&ResolvedArtifact>;
}

impl ManifestLookup for InstallManifest {
    fn require_package(&self, package_id: &PackageId) -> Result<&ResolvedPackage> {
        self.packages
            .get(package_id)
            .ok_or_else(|| anyhow::anyhow!("resolve manifest is missing package {}", package_id.0))
    }

    fn require_artifact(&self, artifact_id: &ArtifactId) -> Result<&ResolvedArtifact> {
        self.artifacts.get(artifact_id).ok_or_else(|| {
            anyhow::anyhow!("resolve manifest is missing artifact {}", artifact_id.0)
        })
    }
}
