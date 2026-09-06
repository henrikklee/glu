pub mod cache;
pub(crate) mod ghcr;
pub(crate) mod http;
mod transfer;

use crate::{
    download::{
        cache::ArtifactCache,
        transfer::{TransferDiagnostics, TransferFailure, TransferRequest, TransferRuntime},
    },
    hash::Sha256Mismatch,
};
use anyhow::{bail, Context, Result};
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
    use super::{parse_download_part_size_mib, DownloadProgress, TempArtifactCleanup};

    #[test]
    fn download_part_size_is_bounded_and_zero_disables_multipart() {
        assert_eq!(parse_download_part_size_mib(None).unwrap(), 1);
        assert_eq!(parse_download_part_size_mib(Some("0")).unwrap(), 0);
        assert_eq!(parse_download_part_size_mib(Some("8")).unwrap(), 8);
        assert_eq!(parse_download_part_size_mib(Some("64")).unwrap(), 64);
        for invalid in ["", "-1", "1.5", "65", "nope"] {
            assert!(parse_download_part_size_mib(Some(invalid)).is_err());
        }
    }

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
    pub(crate) transfer_diagnostics: Option<TransferDiagnostics>,
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
    transfers: TransferRuntime,
}

const DOWNLOAD_PART_SIZE_ENV: &str = "GLU_DOWNLOAD_PART_SIZE_MIB";
const DEFAULT_DOWNLOAD_PART_SIZE_MIB: u64 = 1;
const MAX_DOWNLOAD_PART_SIZE_MIB: u64 = 64;

fn parse_download_part_size_mib(value: Option<&str>) -> Result<u64> {
    let Some(value) = value else {
        return Ok(DEFAULT_DOWNLOAD_PART_SIZE_MIB);
    };
    let parsed = value.parse::<u64>().map_err(|_| {
        anyhow::anyhow!(
            "{DOWNLOAD_PART_SIZE_ENV} must be an integer from 0 to {MAX_DOWNLOAD_PART_SIZE_MIB}"
        )
    })?;
    if parsed > MAX_DOWNLOAD_PART_SIZE_MIB {
        bail!("{DOWNLOAD_PART_SIZE_ENV} must be an integer from 0 to {MAX_DOWNLOAD_PART_SIZE_MIB}");
    }
    Ok(parsed)
}

fn configured_download_part_size_mib() -> Result<u64> {
    let Some(value) = std::env::var_os(DOWNLOAD_PART_SIZE_ENV) else {
        return Ok(DEFAULT_DOWNLOAD_PART_SIZE_MIB);
    };
    let value = value.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{DOWNLOAD_PART_SIZE_ENV} must be an integer from 0 to {MAX_DOWNLOAD_PART_SIZE_MIB}"
        )
    })?;
    parse_download_part_size_mib(Some(value))
}

impl ArtifactDownloader {
    pub fn new(prefix: &Prefix) -> Result<Self> {
        let part_size_mib = configured_download_part_size_mib()?;
        // Resolve each authenticated GHCR redirect once, then reuse its signed CDN URL for all
        // ranges. Four fixed four-slot pools bound a slow connection to four assignments; each
        // free slot pulls its lane's highest-priority waiting range. A zero part size disables
        // proactive multipart splitting; retries may still resume with a range request.
        let resolver_http = artifact_redirect_client();
        let ordinary_http = (0..ORDINARY_TRANSPORT_POOLS)
            .map(|_| artifact_http_client())
            .collect();
        let layout = if part_size_mib == 0 {
            "full".to_string()
        } else {
            format!("{part_size_mib}mib")
        };
        Ok(Self {
            cache: ArtifactCache::new(prefix),
            transfers: TransferRuntime::new(
                resolver_http,
                ordinary_http,
                part_size_mib,
                format!("direct-cdn-{layout}-{ORDINARY_TRANSPORT_POOLS}x4"),
            ),
        })
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
                transfer_diagnostics: None,
            });
        }

        let on_progress = Arc::new(on_progress);
        let mut digest_failures = 0_u8;
        loop {
            let temp = self.cache.temp_path_for_artifact(artifact)?;
            let _temp_cleanup = TempArtifactCleanup { path: &temp };
            let report = match self
                .transfers
                .download_blob_to_path(
                    TransferRequest {
                        artifact_id: &id.0,
                        url: &artifact.url,
                        dest: &temp,
                        expected_sha256: &artifact.sha256,
                        expected_size: artifact.bytes,
                        priority,
                    },
                    Arc::clone(&on_progress),
                )
                .await
            {
                Ok(report) => report,
                Err(error) if is_sha256_mismatch(&error) && digest_failures == 0 => {
                    digest_failures += 1;
                    let _ = tokio::fs::remove_file(&temp).await;
                    continue;
                }
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
            let transfer_diagnostics = report.diagnostics().clone();
            report
                .validate_staging_path(&temp)
                .await
                .with_context(|| format!("validating staging file for artifact {}", id.0))?;
            // Cache admission boundary: the temp file reached here only after download-side
            // sha256 verification and sync. After this rename, `sha256/<digest>` means
            // "verified at ingest"; prepare still re-verifies every use before commit.
            tokio::fs::rename(&temp, &cached)
                .await
                .with_context(|| format!("moving {} to {}", temp.display(), cached.display()))?;

            return Ok(VerifiedArtifact {
                id,
                path: cached,
                sha256: artifact.sha256.clone(),
                reused: false,
                transfer_diagnostics: Some(transfer_diagnostics),
            });
        }
    }
}

pub(crate) fn transfer_failure_diagnostics(error: &anyhow::Error) -> Option<TransferDiagnostics> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TransferFailure>())
        .map(|failure| failure.diagnostics().clone())
}

fn is_sha256_mismatch(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Sha256Mismatch>())
}

const ORDINARY_TRANSPORT_POOLS: usize = 4;

fn artifact_http_client() -> reqwest::Client {
    artifact_http_client_builder()
        .build()
        .expect("failed to build HTTP client")
}

fn artifact_redirect_client() -> reqwest::Client {
    artifact_http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build redirect HTTP client")
}

fn artifact_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
}
