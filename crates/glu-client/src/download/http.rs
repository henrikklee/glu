use crate::download::ghcr::repo_for_blob_url;
use anyhow::{Context, Result};
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

impl GhcrTransport {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            install_token: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.http
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
            Ok(response)
                if matches!(
                    response.status(),
                    reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
                ) || response.status().is_server_error() =>
            {
                if retry >= TOKEN_RETRIES {
                    return Err(response
                        .error_for_status()
                        .expect_err("retryable status must be an error"))
                    .context("GHCR token request returned an error");
                }
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs);
                retry += 1;
                tokio::time::sleep(retry_after.unwrap_or_else(|| token_retry_delay(retry))).await;
            }
            Ok(response) => {
                return Ok(response
                    .error_for_status()
                    .context("GHCR token request returned an error")?
                    .json::<GhcrTokenResponse>()
                    .await
                    .context("failed to decode GHCR token response")?
                    .token);
            }
            Err(error) => {
                if retry >= TOKEN_RETRIES {
                    return Err(error).context("GHCR token request failed");
                }
                retry += 1;
                tokio::time::sleep(token_retry_delay(retry)).await;
            }
        }
    }
}

fn token_retry_delay(retry: u32) -> Duration {
    Duration::from_millis(250 * (1_u64 << retry.saturating_sub(1).min(3)))
}

const TOKEN_RETRIES: u32 = 3;

#[derive(Debug, Deserialize)]
struct GhcrTokenResponse {
    token: String,
}
