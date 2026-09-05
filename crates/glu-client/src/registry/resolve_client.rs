use crate::error::{
    RegistryDecodeFailure, RegistryFailure, RegistryTransportFailure, RuntimeErrorCode,
};
use anyhow::{Context, Result};
use glu_core::{
    InfoResponse, InstallManifest, OutdatedResponse, PackageSelection, PackageSelector,
    ResolveRequest, ResolveRequestEcho, SlimManifest, Target, UsesResponse,
};
use reqwest::header::HeaderMap;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use url::Url;

const HEADER_LATEST_GLU_VERSION: &str = "x-glu-latest-version";
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(15);

/// Sanitized diagnostics for registry retries. Deliberately excludes URLs,
/// request parameters, response bodies, and headers so credentials can never
/// leak into an install trace or terminal diagnostic.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RegistryRequestDiagnostics {
    pub retries: Vec<RegistryRetryDiagnostic>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RegistryRetryDiagnostic {
    pub operation: String,
    pub request_id: u64,
    pub attempt: u32,
    pub failure: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<f64>,
    pub backoff_seconds: f64,
}

#[derive(Debug, Clone)]
pub struct HttpResolveClient {
    base_url: Url,
    http: reqwest::Client,
    diagnostics: Arc<Mutex<RegistryRequestDiagnostics>>,
    next_request_id: Arc<AtomicU64>,
    cancellation: tokio_util::sync::CancellationToken,
}

/// Structured error body the registry returns for bad requests.
#[derive(Debug, Deserialize)]
struct RegistryErrorBody {
    error: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    requested_by: Option<String>,
    #[serde(default)]
    suggestions: Vec<String>,
}

/// Renders a structured registry error as a friendly user-facing message.
/// `not_found` gets a `Did you mean ...?` line when the registry provides
/// suggestions (tautological echoes of the failing name are skipped); the
/// second line is indented to align under the `Error: ` prefix in the CLI.
fn format_registry_error(
    err: &RegistryErrorBody,
    operation: &str,
    status: Option<u16>,
) -> anyhow::Error {
    let (code, message) = match err.error.as_str() {
        "not_found" => {
            let name = err.name.as_deref().unwrap_or("?");
            let mut msg = format!("package '{name}' not found");
            append_requested_by(&mut msg, err.requested_by.as_deref());
            append_suggestions(&mut msg, err.name.as_deref(), &err.suggestions);
            (RuntimeErrorCode::PackageNotFound, msg)
        }
        "unavailable" => {
            // The formula exists in the registry but has no installable state
            // for the target: unpublished, disabled, tombstoned, or a
            // Linux-only formula requested on macOS.
            let name = err.name.as_deref().unwrap_or("?");
            let target = err.target.as_deref().unwrap_or("unknown");
            let mut detail: Vec<String> = Vec::new();
            if let Some(reason) = err.reason.as_deref() {
                detail.push(humanize_reason(reason).unwrap_or(reason).to_string());
            }
            if let Some(via) = err.requested_by.as_deref() {
                detail.push(format!("required by '{via}'"));
            }
            let mut msg = format!("package '{name}' is not available for '{target}'");
            if !detail.is_empty() {
                msg.push_str(&format!(" ({})", detail.join("; ")));
            }
            append_suggestions(&mut msg, err.name.as_deref(), &err.suggestions);
            (RuntimeErrorCode::PackageUnavailable, msg)
        }
        "no_compatible_bottle" => {
            let name = err.name.as_deref().unwrap_or("?");
            let mut msg = format!("no compatible bottle for '{name}'");
            if let Some(target) = err.target.as_deref() {
                msg.push_str(&format!(" on '{target}'"));
            }
            append_requested_by(&mut msg, err.requested_by.as_deref());
            (RuntimeErrorCode::PackageUnavailable, msg)
        }
        "unsupported_target" => (
            RuntimeErrorCode::RegistryError,
            format!(
                "unsupported target '{}'",
                err.target.as_deref().unwrap_or("unknown")
            ),
        ),
        other => (
            RuntimeErrorCode::RegistryError,
            format!("registry returned an error: {other}"),
        ),
    };
    RegistryFailure {
        code,
        message,
        name: err.name.clone(),
        target: err.target.clone(),
        reason: err.reason.clone(),
        requested_by: err.requested_by.clone(),
        suggestions: err.suggestions.clone(),
        operation: operation.to_string(),
        status,
    }
    .into()
}

/// Appends ` (required by '<name>')` when the registry reports which requested
/// root pulled the failing package into the dependency closure.
fn append_requested_by(msg: &mut String, requested_by: Option<&str>) {
    if let Some(via) = requested_by {
        msg.push_str(&format!(" (required by '{via}')"));
    }
}

/// Appends the `Did you mean ...?` line(s), skipping suggestions that merely
/// echo the failing name (defense in depth against stale/cached registry
/// responses). The second line is indented to align under the `Error: ` prefix.
fn append_suggestions(msg: &mut String, error_name: Option<&str>, suggestions: &[String]) {
    let error_name = error_name.map(|n| n.to_ascii_lowercase());
    let useful = suggestions
        .iter()
        .filter(|s| error_name.as_deref() != Some(&s.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    match useful.as_slice() {
        [] => {}
        [one] => msg.push_str(&format!("\n       Did you mean '{one}'?")),
        many => {
            let list = many
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ");
            msg.push_str(&format!("\n       Did you mean any of: {list}?"));
        }
    }
}

/// Maps registry publication reasons to user-facing hints.
fn humanize_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "disabled" => Some("disabled upstream"),
        "no_supported_bottle_tag" => Some("no bottle for this OS/architecture"),
        "missing_manifest" => Some("bottle metadata not yet available"),
        "missing_postinstall_support" => Some("postinstall behavior not yet supported"),
        "tombstoned" => Some("renamed or removed upstream"),
        "unpublished" => Some("not yet published"),
        _ => None,
    }
}

/// Converts a non-success registry response into a friendly error, decoding
/// the structured JSON body when present and falling back to the status code
/// otherwise (e.g. a wrong `--registry` returning HTML).
fn registry_error(status: reqwest::StatusCode, body: &[u8], operation: &str) -> anyhow::Error {
    match serde_json::from_slice::<RegistryErrorBody>(body) {
        Ok(err) => format_registry_error(&err, operation, Some(status.as_u16())),
        Err(_) => RegistryFailure {
            code: RuntimeErrorCode::RegistryError,
            message: format!("registry returned HTTP {status} during {operation}"),
            name: None,
            target: None,
            reason: None,
            requested_by: None,
            suggestions: Vec::new(),
            operation: operation.to_string(),
            status: Some(status.as_u16()),
        }
        .into(),
    }
}

fn latest_glu_version_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(HEADER_LATEST_GLU_VERSION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn invalid_registry_response(operation: &str, detail: impl Into<String>) -> anyhow::Error {
    RegistryFailure {
        code: RuntimeErrorCode::RegistryError,
        message: format!(
            "registry returned an invalid {operation} response: {}",
            detail.into()
        ),
        name: None,
        target: None,
        reason: None,
        requested_by: None,
        suggestions: Vec::new(),
        operation: operation.to_string(),
        status: None,
    }
    .into()
}

fn validate_resolve_response(
    schema: &str,
    echo: &ResolveRequestEcho,
    roots: &[PackageSelection],
    request: &ResolveRequest,
    slim: bool,
) -> Result<()> {
    if schema != "glu.resolve.v1" {
        return Err(invalid_registry_response(
            "resolve",
            format!("unsupported schema '{schema}'"),
        ));
    }
    if request.names != echo.name {
        return Err(invalid_registry_response(
            "resolve",
            "request names do not match",
        ));
    }
    if echo.target != request.target {
        return Err(invalid_registry_response(
            "resolve",
            "request target does not match",
        ));
    }
    if echo.slim != slim {
        return Err(invalid_registry_response(
            "resolve",
            "response variant does not match the request",
        ));
    }
    if !request
        .names
        .iter()
        .eq(roots.iter().map(|root| &root.requested_as))
    {
        return Err(invalid_registry_response(
            "resolve",
            "resolved roots do not match the requested names",
        ));
    }
    Ok(())
}

fn validate_uses_response(
    response: &UsesResponse,
    name: &PackageSelector,
    target: &Target,
    direct: bool,
) -> Result<()> {
    if response.schema != "glu.uses.v1" {
        return Err(invalid_registry_response(
            "uses",
            format!("unsupported schema '{}'", response.schema),
        ));
    }
    if response.request.name != *name
        || response.request.target != *target
        || response.request.direct != direct
    {
        return Err(invalid_registry_response(
            "uses",
            "request echo does not match",
        ));
    }
    if response.roots.len() != 1 || response.roots[0].requested_as != *name {
        return Err(invalid_registry_response(
            "uses",
            "resolved root does not match the requested name",
        ));
    }
    Ok(())
}

fn validate_outdated_response(
    response: &OutdatedResponse,
    names: &[PackageSelector],
) -> Result<()> {
    if response.schema != "glu.outdated.v1" {
        return Err(invalid_registry_response(
            "outdated",
            format!("unsupported schema '{}'", response.schema),
        ));
    }
    let requested: BTreeSet<&str> = names.iter().map(|name| name.0.as_str()).collect();
    let mut returned = BTreeSet::new();
    for package in &response.packages {
        if !requested.contains(package.requested_as.0.as_str()) {
            return Err(invalid_registry_response(
                "outdated",
                format!("unexpected package '{}'", package.requested_as.0),
            ));
        }
        if !returned.insert(package.requested_as.0.as_str()) {
            return Err(invalid_registry_response(
                "outdated",
                format!("duplicate package '{}'", package.requested_as.0),
            ));
        }
    }
    Ok(())
}

fn validate_info_response(
    response: &InfoResponse,
    name: &PackageSelector,
    target: &Target,
) -> Result<()> {
    if response.schema != "glu.info.v1" {
        return Err(invalid_registry_response(
            "info",
            format!("unsupported schema '{}'", response.schema),
        ));
    }
    if response.requested_as != *name {
        return Err(invalid_registry_response(
            "info",
            "requested name does not match",
        ));
    }
    if !bottle_tag_is_compatible(&response.bottle, target) {
        return Err(invalid_registry_response(
            "info",
            format!(
                "bottle tag '{}' is incompatible with target '{}'",
                response.bottle, target.0
            ),
        ));
    }
    Ok(())
}

fn parse_macos_bottle_tag(value: &str) -> Option<(&str, usize)> {
    const RELEASES: &[&str] = &[
        "big_sur", "monterey", "ventura", "sonoma", "sequoia", "tahoe",
    ];
    RELEASES.iter().enumerate().find_map(|(release, name)| {
        value
            .strip_suffix(name)
            .and_then(|arch| arch.strip_suffix('_'))
            .filter(|arch| !arch.is_empty())
            .map(|arch| (arch, release))
    })
}

pub(crate) fn bottle_tag_is_compatible(tag: &str, target: &Target) -> bool {
    if tag == "all" || tag == target.0 {
        return true;
    }

    matches!(
        (parse_macos_bottle_tag(tag), parse_macos_bottle_tag(&target.0)),
        (Some((tag_arch, tag_release)), Some((target_arch, target_release)))
            if tag_arch == target_arch && tag_release < target_release
    )
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn request_failure_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        // reqwest reports DNS, TCP connection, and TLS handshake failures
        // through the connect category.
        "connect"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}

fn body_failure_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "response_timeout"
    } else {
        "response_body"
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(RETRY_MAX_DELAY));
    }

    let retry_at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let now = chrono::Utc::now().with_timezone(retry_at.offset());
    retry_at
        .signed_duration_since(now)
        .to_std()
        .ok()
        .map(|delay| delay.min(RETRY_MAX_DELAY))
}

fn exponential_delay(retry: u32) -> Duration {
    let exponent = retry.saturating_sub(1).min(6);
    RETRY_BASE_DELAY
        .saturating_mul(1_u32 << exponent)
        .min(RETRY_MAX_DELAY)
}

fn jitter(delay: Duration) -> Duration {
    use ring::rand::SecureRandom;

    let mut random = [0_u8; 8];
    if ring::rand::SystemRandom::new().fill(&mut random).is_err() {
        return delay;
    }
    // Uniformly choose 75%..125%. Keep Retry-After itself unjittered: the
    // server supplied an explicit lower bound rather than a client backoff.
    let fraction = u64::from_le_bytes(random) as f64 / u64::MAX as f64;
    delay.mul_f64(0.75 + fraction * 0.5)
}

fn registry_interrupted() -> anyhow::Error {
    crate::error::InterruptedError {
        operation: "registry",
        message: "interrupted while waiting for the registry (Ctrl+C)",
        trace_path: None,
    }
    .into()
}

impl HttpResolveClient {
    pub fn new(base_url: &str) -> Result<Self> {
        Self::with_cancellation(base_url, tokio_util::sync::CancellationToken::new())
    }

    pub(crate) fn with_cancellation(
        base_url: &str,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        let base_url = Url::parse(base_url).context("invalid registry base URL")?;
        // Central transport policy (config::validate_registry_url): HTTPS
        // always; loopback HTTP only in dev builds. `new` is the only trusted
        // constructor path for registry base URLs, so every caller is covered.
        crate::config::validate_registry_url(&base_url)?;
        Ok(Self {
            base_url,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            diagnostics: Arc::new(Mutex::new(RegistryRequestDiagnostics::default())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            cancellation,
        })
    }

    pub fn diagnostics(&self) -> RegistryRequestDiagnostics {
        self.diagnostics
            .lock()
            .expect("registry diagnostics lock poisoned")
            .clone()
    }

    async fn get_bytes(&self, url: Url, operation: &str) -> Result<(HeaderMap, Vec<u8>)> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut attempt = 1_u32;
        loop {
            let response = match tokio::select! {
                _ = self.cancellation.cancelled() => return Err(registry_interrupted()),
                response = self.http.get(url.clone()).send() => response,
            } {
                Ok(response) => response,
                Err(source) if !source.is_builder() && !source.is_redirect() => {
                    let delay = jitter(exponential_delay(attempt));
                    let failure = request_failure_kind(&source);
                    self.record_retry(RegistryRetryDiagnostic {
                        operation: operation.to_string(),
                        request_id,
                        attempt,
                        failure,
                        status: None,
                        retry_after_seconds: None,
                        backoff_seconds: delay.as_secs_f64(),
                    });
                    attempt = attempt.saturating_add(1);
                    self.wait_to_retry(delay).await?;
                    continue;
                }
                Err(source) => {
                    return Err(RegistryTransportFailure {
                        operation: operation.to_string(),
                        source,
                    }
                    .into())
                }
            };

            let status = response.status();
            if !status.is_success() {
                if retryable_status(status) {
                    let server_delay = retry_after(response.headers());
                    let delay = server_delay.unwrap_or_else(|| jitter(exponential_delay(attempt)));
                    self.record_retry(RegistryRetryDiagnostic {
                        operation: operation.to_string(),
                        request_id,
                        attempt,
                        failure: "http_status",
                        status: Some(status.as_u16()),
                        retry_after_seconds: server_delay.map(|delay| delay.as_secs_f64()),
                        backoff_seconds: delay.as_secs_f64(),
                    });
                    attempt = attempt.saturating_add(1);
                    self.wait_to_retry(delay).await?;
                    continue;
                }
                let body = tokio::select! {
                    _ = self.cancellation.cancelled() => return Err(registry_interrupted()),
                    body = response.bytes() => body,
                };
                match body {
                    Ok(body) => return Err(registry_error(status, &body, operation)),
                    Err(source) => {
                        let delay = jitter(exponential_delay(attempt));
                        let failure = body_failure_kind(&source);
                        self.record_retry(RegistryRetryDiagnostic {
                            operation: operation.to_string(),
                            request_id,
                            attempt,
                            failure,
                            status: Some(status.as_u16()),
                            retry_after_seconds: None,
                            backoff_seconds: delay.as_secs_f64(),
                        });
                        attempt = attempt.saturating_add(1);
                        self.wait_to_retry(delay).await?;
                        continue;
                    }
                }
            }

            let headers = response.headers().clone();
            let body = tokio::select! {
                _ = self.cancellation.cancelled() => return Err(registry_interrupted()),
                body = response.bytes() => body,
            };
            match body {
                Ok(body) => return Ok((headers, body.to_vec())),
                Err(source) => {
                    let delay = jitter(exponential_delay(attempt));
                    let failure = body_failure_kind(&source);
                    self.record_retry(RegistryRetryDiagnostic {
                        operation: operation.to_string(),
                        request_id,
                        attempt,
                        failure,
                        status: None,
                        retry_after_seconds: None,
                        backoff_seconds: delay.as_secs_f64(),
                    });
                    attempt = attempt.saturating_add(1);
                    self.wait_to_retry(delay).await?;
                }
            }
        }
    }

    fn record_retry(&self, diagnostic: RegistryRetryDiagnostic) {
        self.diagnostics
            .lock()
            .expect("registry diagnostics lock poisoned")
            .retries
            .push(diagnostic);
    }

    async fn wait_to_retry(&self, delay: Duration) -> Result<()> {
        tokio::select! {
            _ = self.cancellation.cancelled() => Err(registry_interrupted()),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }

    pub async fn resolve(&self, request: &ResolveRequest) -> Result<InstallManifest> {
        let mut url = self
            .base_url
            .join("/v1/resolve")
            .context("failed to build resolve URL")?;

        {
            let mut query = url.query_pairs_mut();
            for name in &request.names {
                query.append_pair("name", &name.0);
            }
            query.append_pair("target", &request.target.0);
        }

        let (_, body) = self.get_bytes(url, "resolve").await?;
        let manifest = serde_json::from_slice::<InstallManifest>(&body).map_err(|source| {
            RegistryDecodeFailure {
                operation: "resolve".to_string(),
                source,
            }
        })?;
        validate_resolve_response(
            &manifest.schema,
            &manifest.request,
            &manifest.roots,
            request,
            false,
        )?;
        Ok(manifest)
    }

    /// `?slim=true` resolve: the same graph with minimal package records,
    /// for read-only consumers that never install (docs/reference/registry-contract.md,
    /// Slim variant).
    pub async fn resolve_slim(&self, request: &ResolveRequest) -> Result<SlimManifest> {
        let mut url = self
            .base_url
            .join("/v1/resolve")
            .context("failed to build resolve URL")?;

        {
            let mut query = url.query_pairs_mut();
            for name in &request.names {
                query.append_pair("name", &name.0);
            }
            query.append_pair("target", &request.target.0);
            query.append_pair("slim", "true");
        }

        let (_, body) = self.get_bytes(url, "resolve").await?;
        let manifest = serde_json::from_slice::<SlimManifest>(&body).map_err(|source| {
            RegistryDecodeFailure {
                operation: "resolve".to_string(),
                source,
            }
        })?;
        validate_resolve_response(
            &manifest.schema,
            &manifest.request,
            &manifest.roots,
            request,
            true,
        )?;
        Ok(manifest)
    }

    /// `GET /v1/uses`: every registry package that (transitively) depends on
    /// `name`, as slim records (docs/reference/registry-contract.md).
    pub async fn uses(
        &self,
        name: &PackageSelector,
        target: &Target,
        direct: bool,
    ) -> Result<UsesResponse> {
        let mut url = self
            .base_url
            .join("/v1/uses")
            .context("failed to build uses URL")?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("name", &name.0);
            query.append_pair("target", &target.0);
            if direct {
                query.append_pair("direct", "true");
            }
        }

        let (_, body) = self.get_bytes(url, "uses").await?;
        let response: UsesResponse =
            serde_json::from_slice(&body).map_err(|source| RegistryDecodeFailure {
                operation: "uses".to_string(),
                source,
            })?;
        validate_uses_response(&response, name, target, direct)?;
        Ok(response)
    }

    pub async fn outdated(
        &self,
        names: &[PackageSelector],
        target: &Target,
    ) -> Result<(OutdatedResponse, Option<String>)> {
        const BATCH_SIZE: usize = 25;
        if names.len() <= BATCH_SIZE {
            return self.outdated_once(names, target).await;
        }

        let mut packages = Vec::new();
        let mut latest_glu_version = None;
        let mut schema = None;
        for batch in names.chunks(BATCH_SIZE) {
            let (response, batch_latest) = self.outdated_once(batch, target).await?;
            schema.get_or_insert(response.schema);
            if latest_glu_version.is_none() {
                latest_glu_version = batch_latest;
            }
            packages.extend(response.packages);
        }

        Ok((
            OutdatedResponse {
                schema: schema.unwrap_or_else(|| "glu.outdated.v1".to_string()),
                packages,
            },
            latest_glu_version,
        ))
    }

    async fn outdated_once(
        &self,
        names: &[PackageSelector],
        target: &Target,
    ) -> Result<(OutdatedResponse, Option<String>)> {
        let mut url = self
            .base_url
            .join("/v1/outdated")
            .context("failed to build outdated URL")?;

        {
            let mut query = url.query_pairs_mut();
            for name in names {
                query.append_pair("name", &name.0);
            }
            query.append_pair("target", &target.0);
        }

        let (headers, body) = self.get_bytes(url, "outdated").await?;
        let latest_glu_version = latest_glu_version_header(&headers);

        let response = serde_json::from_slice::<OutdatedResponse>(&body).map_err(|source| {
            RegistryDecodeFailure {
                operation: "outdated".to_string(),
                source,
            }
        })?;
        validate_outdated_response(&response, names)?;

        Ok((response, latest_glu_version))
    }

    pub async fn info(&self, name: &PackageSelector, target: &Target) -> Result<InfoResponse> {
        let mut url = self
            .base_url
            .join("/v1/info")
            .context("failed to build info URL")?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("name", &name.0);
            query.append_pair("target", &target.0);
        }

        let (_, body) = self.get_bytes(url, "info").await?;
        let response = serde_json::from_slice::<InfoResponse>(&body).map_err(|source| {
            RegistryDecodeFailure {
                operation: "info".to_string(),
                source,
            }
        })?;
        validate_info_response(&response, name, target)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "dev-registry")]
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[cfg(feature = "dev-registry")]
    fn fault_server(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }

    #[cfg(feature = "dev-registry")]
    fn response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn build(error: &str) -> RegistryErrorBody {
        RegistryErrorBody {
            error: error.to_string(),
            name: None,
            target: None,
            reason: None,
            requested_by: None,
            suggestions: vec![],
        }
    }

    fn err(error: &str, name: Option<&str>, suggestions: Vec<&str>) -> anyhow::Error {
        let mut registry = build(error);
        registry.name = name.map(str::to_string);
        registry.suggestions = suggestions.into_iter().map(str::to_string).collect();
        format_registry_error(&registry, "resolve", None)
    }

    #[test]
    fn latest_glu_version_comes_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_LATEST_GLU_VERSION, " 0.2.0 ".parse().unwrap());
        assert_eq!(
            latest_glu_version_header(&headers).as_deref(),
            Some("0.2.0")
        );

        headers.insert(HEADER_LATEST_GLU_VERSION, "  ".parse().unwrap());
        assert_eq!(latest_glu_version_header(&headers), None);
    }

    #[test]
    fn retry_after_accepts_http_dates() {
        let retry_at = chrono::Utc::now() + chrono::Duration::seconds(2);
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            retry_at
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
                .parse()
                .unwrap(),
        );
        let delay = retry_after(&headers).unwrap();
        assert!(delay > Duration::ZERO);
        assert!(delay <= Duration::from_secs(2));
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn retries_522_then_returns_successful_body() {
        let (base, server) = fault_server(vec![
            response("522 Unknown", "Retry-After: 0\r\n", ""),
            response("200 OK", "", "ok"),
        ]);
        let client = HttpResolveClient::new(&base).unwrap();
        let (_, body) = client
            .get_bytes(Url::parse(&format!("{base}/v1/info")).unwrap(), "info")
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(body, b"ok");
        let diagnostics = client.diagnostics();
        assert_eq!(diagnostics.retries.len(), 1);
        assert_eq!(diagnostics.retries[0].status, Some(522));
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn honors_retry_after_on_429_then_succeeds() {
        let (base, server) = fault_server(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", ""),
            response("200 OK", "", "ok"),
        ]);
        let client = HttpResolveClient::new(&base).unwrap();
        let (_, body) = client
            .get_bytes(Url::parse(&format!("{base}/v1/uses")).unwrap(), "uses")
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(body, b"ok");
        let retry = &client.diagnostics().retries[0];
        assert_eq!(retry.status, Some(429));
        assert_eq!(retry.retry_after_seconds, Some(0.0));
        assert_eq!(retry.backoff_seconds, 0.0);
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn retries_interrupted_response_body() {
        let (base, server) = fault_server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nshort".to_string(),
            response("200 OK", "", "complete"),
        ]);
        let client = HttpResolveClient::new(&base).unwrap();
        let (_, body) = client
            .get_bytes(
                Url::parse(&format!("{base}/v1/resolve")).unwrap(),
                "resolve",
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(body, b"complete");
        assert_eq!(client.diagnostics().retries[0].failure, "response_body");
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn does_not_exhaust_transient_status_retries() {
        let mut responses = (0..10)
            .map(|_| response("503 Service Unavailable", "Retry-After: 0\r\n", ""))
            .collect::<Vec<_>>();
        responses.push(response("200 OK", "", "eventual"));
        let (base, server) = fault_server(responses);
        let client = HttpResolveClient::new(&base).unwrap();
        let (_, body) = client
            .get_bytes(
                Url::parse(&format!("{base}/v1/outdated")).unwrap(),
                "outdated",
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(body, b"eventual");
        assert_eq!(client.diagnostics().retries.len(), 10);
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn outdated_retries_only_the_failed_batch_request() {
        let body = r#"{"schema":"glu.outdated.v1","packages":[]}"#;
        let (base, server) = fault_server(vec![
            response("200 OK", "", body),
            response("503 Service Unavailable", "Retry-After: 0\r\n", ""),
            response("200 OK", "", body),
        ]);
        let client = HttpResolveClient::new(&base).unwrap();
        let names = (0..30)
            .map(|index| PackageSelector(format!("package-{index}")))
            .collect::<Vec<_>>();
        let (result, _) = client
            .outdated(&names, &Target("arm64_sequoia".to_string()))
            .await
            .unwrap();
        server.join().unwrap();

        assert!(result.packages.is_empty());
        let diagnostics = client.diagnostics();
        assert_eq!(diagnostics.retries.len(), 1);
        assert_eq!(diagnostics.retries[0].request_id, 2);
        assert_eq!(diagnostics.retries[0].attempt, 1);
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn permanent_status_is_not_retried() {
        let (base, server) = fault_server(vec![response("404 Not Found", "", "missing")]);
        let client = HttpResolveClient::new(&base).unwrap();
        let error = client
            .get_bytes(Url::parse(&format!("{base}/v1/info")).unwrap(), "info")
            .await
            .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("HTTP 404"));
        assert!(client.diagnostics().retries.is_empty());
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn malformed_success_is_not_retried() {
        let (base, server) = fault_server(vec![response("200 OK", "", "{")]);
        let client = HttpResolveClient::new(&base).unwrap();
        let error = client
            .info(
                &PackageSelector("vips".to_string()),
                &Target("arm64_sequoia".to_string()),
            )
            .await
            .unwrap_err();
        server.join().unwrap();

        assert!(error.downcast_ref::<RegistryDecodeFailure>().is_some());
        assert!(client.diagnostics().retries.is_empty());
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn retry_backoff_future_can_be_cancelled_promptly() {
        let (base, server) = fault_server(vec![response(
            "429 Too Many Requests",
            "Retry-After: 15\r\n",
            "",
        )]);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let client = HttpResolveClient::with_cancellation(&base, cancellation.clone()).unwrap();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancellation.cancel();
        });
        let started = std::time::Instant::now();
        let error = client
            .get_bytes(
                Url::parse(&format!("{base}/v1/resolve")).unwrap(),
                "resolve",
            )
            .await
            .unwrap_err();
        cancel.await.unwrap();
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error
            .downcast_ref::<crate::error::InterruptedError>()
            .is_some());
        assert_eq!(
            client.diagnostics().retries[0].retry_after_seconds,
            Some(15.0)
        );
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test]
    async fn in_flight_request_future_can_be_cancelled_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel_from_server = cancellation.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            cancel_from_server.cancel();
        });

        let base = format!("http://{address}");
        let client = HttpResolveClient::with_cancellation(&base, cancellation).unwrap();
        let started = std::time::Instant::now();
        let error = client
            .get_bytes(
                Url::parse(&format!("{base}/v1/resolve")).unwrap(),
                "resolve",
            )
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error
            .downcast_ref::<crate::error::InterruptedError>()
            .is_some());
    }

    #[test]
    fn resolve_response_must_echo_the_exact_request_and_roots() {
        let request = ResolveRequest {
            names: vec![
                PackageSelector("vips".to_string()),
                PackageSelector("vips".to_string()),
                PackageSelector("zlib".to_string()),
            ],
            target: Target("arm64_sequoia".to_string()),
        };
        let echo = ResolveRequestEcho {
            name: request.names.clone(),
            target: request.target.clone(),
            slim: false,
        };
        let roots = echo
            .name
            .iter()
            .map(|name| PackageSelection {
                requested_as: name.clone(),
                package_key: glu_core::PackageKey(format!("package:{}", name.0)),
                package: glu_core::PackageId(format!("pkg:test/{}@1", name.0)),
            })
            .collect::<Vec<_>>();

        validate_resolve_response("glu.resolve.v1", &echo, &roots, &request, false).unwrap();

        let mut unrelated = roots;
        unrelated[0].requested_as = PackageSelector("other".to_string());
        assert!(
            validate_resolve_response("glu.resolve.v1", &echo, &unrelated, &request, false)
                .is_err()
        );
    }

    #[test]
    fn bottle_tag_compatibility_allows_exact_all_and_older_macos() {
        let target = Target("arm64_sequoia".to_string());
        assert!(bottle_tag_is_compatible("arm64_sequoia", &target));
        assert!(bottle_tag_is_compatible("all", &target));
        assert!(bottle_tag_is_compatible("arm64_sonoma", &target));
        assert!(!bottle_tag_is_compatible("arm64_tahoe", &target));
        assert!(!bottle_tag_is_compatible("x86_64_sonoma", &target));
    }

    #[test]
    fn outdated_response_rejects_wrong_schema_and_unrequested_packages() {
        let names = [PackageSelector("vips".to_string())];
        let mut response = OutdatedResponse {
            schema: "wrong".to_string(),
            packages: Vec::new(),
        };
        assert!(validate_outdated_response(&response, &names).is_err());

        response.schema = "glu.outdated.v1".to_string();
        response.packages.push(glu_core::OutdatedPackage {
            requested_as: PackageSelector("other".to_string()),
            package_key: glu_core::PackageKey("package:other".to_string()),
            name: glu_core::PackageName("other".to_string()),
            update: None,
            latest: glu_core::VersionRevision {
                package: glu_core::PackageId("pkg:test/other@1".to_string()),
                version: "1".to_string(),
                revision: 0,
            },
        });
        assert!(validate_outdated_response(&response, &names).is_err());
    }

    #[test]
    fn not_found_without_suggestions() {
        assert_eq!(
            err("not_found", Some("zzzz"), vec![]).to_string(),
            "package 'zzzz' not found"
        );
    }

    #[test]
    fn not_found_with_single_suggestion() {
        assert_eq!(
            err("not_found", Some("aws-skd-cpp"), vec!["aws-sdk-cpp"]).to_string(),
            "package 'aws-skd-cpp' not found\n       Did you mean 'aws-sdk-cpp'?"
        );
    }

    #[test]
    fn not_found_with_multiple_suggestions() {
        assert_eq!(
            err(
                "not_found",
                Some("aws-skd-cpp"),
                vec!["aws-sdk-cpp", "aws-sdk-cpp-core"]
            )
            .to_string(),
            "package 'aws-skd-cpp' not found\n       Did you mean any of: 'aws-sdk-cpp', 'aws-sdk-cpp-core'?"
        );
    }

    #[test]
    fn not_found_names_the_requiring_root() {
        let mut registry = build("not_found");
        registry.name = Some("alsa-lib".to_string());
        registry.requested_by = Some("gradle".to_string());
        assert_eq!(
            format_registry_error(&registry, "resolve", None).to_string(),
            "package 'alsa-lib' not found (required by 'gradle')"
        );
    }

    #[test]
    fn unavailable_reports_target_and_reason() {
        let mut registry = build("unavailable");
        registry.name = Some("alsa-lib".to_string());
        registry.target = Some("arm64_sequoia".to_string());
        registry.reason = Some("no_supported_bottle_tag".to_string());
        assert_eq!(
            format_registry_error(&registry, "resolve", None).to_string(),
            "package 'alsa-lib' is not available for 'arm64_sequoia' (no bottle for this OS/architecture)"
        );
    }

    #[test]
    fn unavailable_names_the_requiring_root() {
        let mut registry = build("unavailable");
        registry.name = Some("alsa-lib".to_string());
        registry.target = Some("arm64_sequoia".to_string());
        registry.reason = Some("no_supported_bottle_tag".to_string());
        registry.requested_by = Some("gradle".to_string());
        assert_eq!(
            format_registry_error(&registry, "resolve", None).to_string(),
            "package 'alsa-lib' is not available for 'arm64_sequoia' (no bottle for this OS/architecture; required by 'gradle')"
        );
    }

    #[test]
    fn unavailable_skips_tautological_suggestion() {
        let mut registry = build("unavailable");
        registry.name = Some("alsa-lib".to_string());
        registry.target = Some("arm64_sequoia".to_string());
        registry.reason = Some("no_supported_bottle_tag".to_string());
        registry.suggestions = vec!["alsa-lib".to_string()];
        assert_eq!(
            format_registry_error(&registry, "resolve", None).to_string(),
            "package 'alsa-lib' is not available for 'arm64_sequoia' (no bottle for this OS/architecture)"
        );
    }

    #[test]
    fn no_compatible_bottle_reports_target() {
        let mut registry = build("no_compatible_bottle");
        registry.name = Some("example".to_string());
        registry.target = Some("arm64_sequoia".to_string());
        registry.requested_by = Some("vips".to_string());
        assert_eq!(
            format_registry_error(&registry, "resolve", None).to_string(),
            "no compatible bottle for 'example' on 'arm64_sequoia' (required by 'vips')"
        );
    }

    #[test]
    fn unsupported_target() {
        assert_eq!(
            err("unsupported_target", None, vec![]).to_string(),
            "unsupported target 'unknown'"
        );
    }
}
