use crate::download::ghcr::repo_for_blob_url;
use anyhow::{bail, Context, Result};
use reqwest::{header, StatusCode, Version};
use serde::Deserialize;
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use url::Url;

/// GHCR-specific request transport and bearer-token provider.
///
/// Segmentation, retries, health, and file writes belong to `download::transfer`; this type owns
/// only registry authentication and the configured reqwest client.
#[derive(Debug, Clone)]
pub(crate) struct GhcrTransport {
    http: reqwest::Client,
    install_token: Arc<RwLock<Option<String>>>,
}

pub(crate) struct ResolvedBlobUrl {
    pub(crate) url: Url,
    pub(crate) version: Version,
    pub(crate) status: StatusCode,
    pub(crate) elapsed: Duration,
}

impl GhcrTransport {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            install_token: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) async fn configure_install_token<'a>(
        &self,
        urls: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let repos = urls
            .into_iter()
            .map(repo_for_blob_url)
            .collect::<Result<BTreeSet<_>>>()?;

        let token = if repos.is_empty() {
            None
        } else {
            Some(token_for_repos(&self.http, repos.iter().map(String::as_str)).await?)
        };
        *self.install_token.write().await = token;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn set_test_install_token(&self, token: &str) {
        *self.install_token.write().await = Some(token.to_string());
    }

    pub(crate) async fn auth_header_for_blob_url(&self, url: &str) -> Result<(String, String)> {
        let token = match self.install_token.read().await.clone() {
            Some(token) => token,
            None => {
                let repo = repo_for_blob_url(url)?;
                token_for_repos(&self.http, std::iter::once(repo.as_str())).await?
            }
        };

        Ok(("Authorization".to_string(), format!("Bearer {token}")))
    }

    /// Resolves GHCR's authenticated blob endpoint to its short-lived CDN URL without following
    /// the redirect. Callers may reuse the returned URL for every range of this artifact.
    pub(crate) async fn resolve_blob_url(&self, url: &str) -> Result<ResolvedBlobUrl> {
        let (name, value) = self.auth_header_for_blob_url(url).await?;
        self.resolve_blob_url_with_header(url, &name, &value).await
    }

    pub(crate) async fn resolve_blob_url_with_header(
        &self,
        url: &str,
        header_name: &str,
        header_value: &str,
    ) -> Result<ResolvedBlobUrl> {
        let original = Url::parse(url).context("invalid GHCR blob URL")?;
        let started = std::time::Instant::now();
        let mut retry = 0_u32;
        loop {
            let response = match self
                .http
                .get(original.clone())
                .header(header_name, header_value)
                .send()
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    retry = retry.saturating_add(1);
                    tokio::time::sleep(token_retry_delay(retry)).await;
                    continue;
                }
            };
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .context("GHCR blob redirect missing Location")?
                    .to_str()
                    .context("GHCR blob redirect contained an invalid Location")?;
                let resolved = response
                    .url()
                    .join(location)
                    .context("GHCR blob redirect contained an invalid URL")?;
                if resolved.scheme() != "https" && !resolved.host_str().is_some_and(is_loopback) {
                    bail!("GHCR blob redirect must use HTTPS");
                }
                return Ok(ResolvedBlobUrl {
                    url: resolved,
                    version: response.version(),
                    status,
                    elapsed: started.elapsed(),
                });
            }
            if status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                let retry_after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs);
                retry = retry.saturating_add(1);
                tokio::time::sleep(retry_after.unwrap_or_else(|| token_retry_delay(retry))).await;
                continue;
            }
            bail!("GHCR blob request returned HTTP {status} instead of a redirect");
        }
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn token_for_repos<'a>(
    http: &reqwest::Client,
    repos: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    let mut url = Url::parse("https://ghcr.io/token").expect("static GHCR token URL");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("service", "ghcr.io");
        for repo in repos {
            query.append_pair("scope", &format!("repository:{repo}:pull"));
        }
    }

    let mut retry = 0_u32;
    loop {
        match http.get(url.clone()).send().await {
            Ok(response) if !response.status().is_success() => {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs);
                retry = retry.saturating_add(1);
                tokio::time::sleep(retry_after.unwrap_or_else(|| token_retry_delay(retry))).await;
            }
            Ok(response) => match response.json::<GhcrTokenResponse>().await {
                Ok(response) => return Ok(response.token),
                Err(_) => {
                    retry = retry.saturating_add(1);
                    tokio::time::sleep(token_retry_delay(retry)).await;
                }
            },
            Err(_) => {
                retry = retry.saturating_add(1);
                tokio::time::sleep(token_retry_delay(retry)).await;
            }
        }
    }
}

fn token_retry_delay(retry: u32) -> Duration {
    Duration::from_millis(250 * (1_u64 << retry.saturating_sub(1).min(6)))
        .min(Duration::from_secs(15))
}

#[derive(Debug, Deserialize)]
struct GhcrTokenResponse {
    token: String,
}
