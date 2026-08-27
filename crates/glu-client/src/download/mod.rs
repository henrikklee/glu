pub mod cache;
pub(crate) mod ghcr;
pub(crate) mod http;
mod transfer;

use crate::download::{cache::ArtifactCache, transfer::TransferManager};
use anyhow::{Context, Result};
use glu_core::{ArtifactId, Prefix, ResolvedArtifact};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Live byte counts for in-flight downloads, keyed by download node id. The
/// downloader writes absolute byte counts as it streams; the progress footer
/// reads the total each render.
#[derive(Debug, Clone, Default)]
pub struct DownloadProgress {
    bytes: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl DownloadProgress {
    pub fn report(&self, node_id: &str, downloaded_bytes: u64) {
        self.bytes
            .lock()
            .expect("download progress lock poisoned")
            .insert(node_id.to_string(), downloaded_bytes);
    }

    /// Total bytes downloaded so far across all real downloads (in-flight
    /// and completed).
    pub fn total_bytes(&self) -> u64 {
        self.bytes
            .lock()
            .expect("download progress lock poisoned")
            .values()
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadProgress, TempArtifactCleanup};

    #[test]
    fn aggregates_reports_across_nodes() {
        let progress = DownloadProgress::default();
        assert_eq!(progress.total_bytes(), 0);
        progress.report("ghcr_bottle_download:jq", 2_000_000);
        progress.report("ghcr_bottle_download:vips", 450_000_000);
        assert_eq!(progress.total_bytes(), 452_000_000);
        // Re-reporting a node replaces, not adds.
        progress.report("ghcr_bottle_download:jq", 3_000_000);
        assert_eq!(progress.total_bytes(), 453_000_000);
    }

    #[test]
    fn cancelled_download_cleanup_removes_staging_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact.tmp");
        std::fs::write(&path, b"partial").unwrap();
        {
            let _cleanup = TempArtifactCleanup { path: &path };
        }
        assert!(!path.exists());
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedArtifact {
    pub id: ArtifactId,
    pub path: PathBuf,
    pub sha256: String,
    pub reused: bool,
}

struct TempArtifactCleanup<'a> {
    path: &'a Path,
}

impl Drop for TempArtifactCleanup<'_> {
    fn drop(&mut self) {
        // Drop also runs when the async download future is cancelled. A synchronous unlink is
        // intentional here: leaving a PID-scoped partial artifact behind would make cancellation
        // leak cache staging files.
        let _ = std::fs::remove_file(self.path);
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactDownloader {
    cache: ArtifactCache,
    transfers: TransferManager,
}

impl ArtifactDownloader {
    pub fn new(prefix: &Prefix) -> Self {
        let http = reqwest::Client::builder()
            // GHCR blob downloads can be much slower with reqwest's HTTP/2 multiplexing
            // for this workload. In local probes against a 358 MB vips closure, H2
            // multiplexing collapsed to ~2-3 MB/s even when hash/cache-write time was
            // sub-second total; forcing separate HTTP/1.1 connections reached the
            // available ~90 Mbps link in the same probe. Keep artifact fetches off H2.
            .http1_only()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            cache: ArtifactCache::new(prefix),
            transfers: TransferManager::new(http),
        }
    }

    pub async fn configure_install_token<'a>(
        &self,
        artifacts: impl IntoIterator<Item = &'a ResolvedArtifact>,
    ) -> Result<()> {
        self.transfers
            .configure_install_token(artifacts.into_iter().map(|artifact| artifact.url.as_str()))
            .await
    }

    /// Fetches `artifact`, reading from the local cache when present and otherwise downloading
    /// it from the registry. Fresh downloads are sha256-verified before cache admission:
    /// `download/http.rs` writes to `tmp/`, verifies the expected digest (streaming for
    /// single-stream downloads, full-temp re-read for multipart), fsyncs, and only then do we
    /// rename into `sha256/<digest>`. Cached artifacts are reused by admitted path existence,
    /// then re-verified fused into the prepare/extract read (`bottle/prepare.rs`, S4) so local
    /// corruption or tampering still fails closed before commit.
    /// `on_progress` receives the absolute downloaded byte count during a fresh download.
    pub async fn get_or_download<F>(
        &self,
        id: ArtifactId,
        artifact: &ResolvedArtifact,
        on_progress: F,
    ) -> Result<VerifiedArtifact>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        self.get_or_download_with_priority(id, artifact, u64::MAX, on_progress)
            .await
    }

    pub(crate) async fn get_or_download_with_priority<F>(
        &self,
        id: ArtifactId,
        artifact: &ResolvedArtifact,
        priority: u64,
        on_progress: F,
    ) -> Result<VerifiedArtifact>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        self.cache.ensure_dirs().await?;

        let cached = self.cache.path_for_artifact(artifact);
        if cached.exists() {
            return Ok(VerifiedArtifact {
                id,
                path: cached,
                sha256: artifact.sha256.clone(),
                reused: true,
            });
        }

        let temp = self.cache.temp_path_for_artifact(artifact);
        let _ = tokio::fs::remove_file(&temp).await;
        let _temp_cleanup = TempArtifactCleanup { path: &temp };

        let report = match self
            .transfers
            .download_blob_to_path(
                &artifact.url,
                &temp,
                &artifact.sha256,
                artifact.bytes,
                priority,
                on_progress,
            )
            .await
        {
            Ok(report) => report,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(error).with_context(|| format!("downloading artifact {}", id.0));
            }
        };
        if let Err(error) = report.validate(artifact.bytes) {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error)
                .with_context(|| format!("validating transfer report for artifact {}", id.0));
        }
        // Cache admission boundary: the temp file reached here only after download-side
        // sha256 verification and sync. After this rename, `sha256/<digest>` means
        // "verified at ingest"; prepare still re-verifies every use before commit.
        tokio::fs::rename(&temp, &cached)
            .await
            .with_context(|| format!("moving {} to {}", temp.display(), cached.display()))?;

        Ok(VerifiedArtifact {
            id,
            path: cached,
            sha256: artifact.sha256.clone(),
            reused: false,
        })
    }
}
