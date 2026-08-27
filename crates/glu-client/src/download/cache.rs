use anyhow::{Context, Result};
use glu_core::{Prefix, ResolvedArtifact};
use std::path::PathBuf;

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
    pub fn temp_path_for_sha256(&self, sha256: &str) -> PathBuf {
        self.root
            .join("tmp")
            .join(format!("{sha256}.{}.tmp", std::process::id()))
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

    pub fn temp_path_for_artifact(&self, artifact: &ResolvedArtifact) -> PathBuf {
        self.temp_path_for_sha256(&artifact.sha256)
    }

    pub async fn remove_artifact(&self, artifact: &ResolvedArtifact) -> Result<()> {
        match tokio::fs::remove_file(self.path_for_artifact(artifact)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing cached artifact"),
        }
    }
}
