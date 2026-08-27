use anyhow::{bail, Context, Result};
use url::Url;

/// Parsed GHCR blob URL used for bottle transport policy.
///
/// The registry supplies artifact URLs, but the client is responsible for
/// enforcing that bottles and bearer tokens only travel over HTTPS to GHCR and
/// that every token scope is derived by one canonical parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GhcrBlobUrl {
    repo: String,
}

impl GhcrBlobUrl {
    pub(crate) fn parse(url: &str) -> Result<Self> {
        let parsed = Url::parse(url).context("invalid GHCR blob URL")?;
        if parsed.scheme() != "https" {
            bail!("unsupported GHCR blob URL scheme: {url}");
        }
        if parsed.host_str() != Some("ghcr.io") {
            bail!("unsupported artifact host: {url}");
        }

        let parts = parsed
            .path_segments()
            .ok_or_else(|| anyhow::anyhow!("invalid GHCR blob path: {url}"))?
            .collect::<Vec<_>>();

        let v2 = parts
            .iter()
            .position(|part| *part == "v2")
            .ok_or_else(|| anyhow::anyhow!("GHCR blob URL missing v2 segment: {url}"))?;
        let blobs = parts
            .iter()
            .position(|part| *part == "blobs")
            .ok_or_else(|| anyhow::anyhow!("GHCR blob URL missing blobs segment: {url}"))?;

        if blobs <= v2 + 1 || parts[v2 + 1..blobs].iter().any(|part| part.is_empty()) {
            bail!("GHCR blob URL missing repository path: {url}");
        }
        if parts.get(blobs + 1).is_none_or(|digest| digest.is_empty()) {
            bail!("GHCR blob URL missing blob digest: {url}");
        }

        Ok(Self {
            repo: parts[v2 + 1..blobs].join("/"),
        })
    }

    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }
}

pub(crate) fn repo_for_blob_url(url: &str) -> Result<String> {
    Ok(GhcrBlobUrl::parse(url)?.repo().to_string())
}

#[cfg(test)]
mod tests {
    use super::repo_for_blob_url;

    #[test]
    fn extracts_repo_from_canonical_ghcr_blob_url() {
        assert_eq!(
            repo_for_blob_url("https://ghcr.io/v2/homebrew/core/jq/blobs/sha256:abc123").unwrap(),
            "homebrew/core/jq"
        );
    }

    #[test]
    fn rejects_plaintext_ghcr_blob_url() {
        let err = repo_for_blob_url("http://ghcr.io/v2/homebrew/core/jq/blobs/sha256:abc123")
            .unwrap_err();
        assert!(err.to_string().contains("unsupported GHCR blob URL scheme"));
    }

    #[test]
    fn rejects_non_ghcr_hosts() {
        let err = repo_for_blob_url("https://example.com/v2/homebrew/core/jq/blobs/sha256:abc123")
            .unwrap_err();
        assert!(err.to_string().contains("unsupported artifact host"));
    }

    #[test]
    fn rejects_blob_urls_without_repo_or_digest() {
        assert!(repo_for_blob_url("https://ghcr.io/v2/blobs/sha256:abc123").is_err());
        assert!(repo_for_blob_url("https://ghcr.io/v2/homebrew/core/jq/blobs/").is_err());
    }
}
