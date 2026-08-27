use crate::error::{
    RegistryDecodeFailure, RegistryFailure, RegistryTransportFailure, RuntimeErrorCode,
};
use anyhow::{Context, Result};
use glu_core::{
    InfoResponse, InstallManifest, OutdatedResponse, PackageSelector, ResolveRequest, SlimManifest,
    Target, UsesResponse,
};
use reqwest::header::HeaderMap;
use serde::Deserialize;
use url::Url;

const HEADER_LATEST_GLU_VERSION: &str = "x-glu-latest-version";

#[derive(Debug, Clone)]
pub struct HttpResolveClient {
    base_url: Url,
    http: reqwest::Client,
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
async fn registry_error(response: reqwest::Response, operation: &str) -> anyhow::Error {
    let status = response.status();
    match response.text().await {
        Ok(text) => match serde_json::from_str::<RegistryErrorBody>(&text) {
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
        },
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

impl HttpResolveClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url).context("invalid registry base URL")?;
        // Central transport policy (config::validate_registry_url): HTTPS
        // always; loopback HTTP only in dev builds. `new` is the only trusted
        // entry point for registry base URLs, so every caller is covered.
        crate::config::validate_registry_url(&base_url)?;
        Ok(Self {
            base_url,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        })
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

        let response =
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| RegistryTransportFailure {
                    operation: "resolve".to_string(),
                    source,
                })?;

        if !response.status().is_success() {
            return Err(registry_error(response, "resolve").await);
        }

        response.json::<InstallManifest>().await.map_err(|source| {
            RegistryDecodeFailure {
                operation: "resolve".to_string(),
                source,
            }
            .into()
        })
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

        let response =
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| RegistryTransportFailure {
                    operation: "resolve".to_string(),
                    source,
                })?;

        if !response.status().is_success() {
            return Err(registry_error(response, "resolve").await);
        }

        response.json::<SlimManifest>().await.map_err(|source| {
            RegistryDecodeFailure {
                operation: "resolve".to_string(),
                source,
            }
            .into()
        })
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

        let response =
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| RegistryTransportFailure {
                    operation: "uses".to_string(),
                    source,
                })?;

        if !response.status().is_success() {
            return Err(registry_error(response, "uses").await);
        }

        let response: UsesResponse =
            response
                .json()
                .await
                .map_err(|source| RegistryDecodeFailure {
                    operation: "uses".to_string(),
                    source,
                })?;
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

        let response =
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| RegistryTransportFailure {
                    operation: "outdated".to_string(),
                    source,
                })?;

        if !response.status().is_success() {
            return Err(registry_error(response, "outdated").await);
        }

        let latest_glu_version = latest_glu_version_header(response.headers());

        let response = response
            .json::<OutdatedResponse>()
            .await
            .map_err(|source| RegistryDecodeFailure {
                operation: "outdated".to_string(),
                source,
            })?;

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

        let response =
            self.http
                .get(url)
                .send()
                .await
                .map_err(|source| RegistryTransportFailure {
                    operation: "info".to_string(),
                    source,
                })?;

        if !response.status().is_success() {
            return Err(registry_error(response, "info").await);
        }

        response.json::<InfoResponse>().await.map_err(|source| {
            RegistryDecodeFailure {
                operation: "info".to_string(),
                source,
            }
            .into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
