use glu_core::PackageName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeErrorCode {
    PackageNotFound,
    PackageUnavailable,
    NotInstalled,
    RegistryUnavailable,
    RegistryError,
    DownloadFailed,
    ChecksumMismatch,
    PrepareFailed,
    PostinstallFailed,
    LinkFailed,
    PartialInstallFailure,
    Interrupted,
    CommandFailed,
}

impl RuntimeErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PackageNotFound => "package_not_found",
            Self::PackageUnavailable => "package_unavailable",
            Self::NotInstalled => "not_installed",
            Self::RegistryUnavailable => "registry_unavailable",
            Self::RegistryError => "registry_error",
            Self::DownloadFailed => "download_failed",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::PrepareFailed => "prepare_failed",
            Self::PostinstallFailed => "postinstall_failed",
            Self::LinkFailed => "link_failed",
            Self::PartialInstallFailure => "partial_install_failure",
            Self::Interrupted => "interrupted",
            Self::CommandFailed => "command_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InterruptedError {
    pub operation: &'static str,
    pub message: &'static str,
    pub trace_path: Option<std::path::PathBuf>,
}

impl std::fmt::Display for InterruptedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)?;
        if let Some(trace_path) = &self.trace_path {
            write!(formatter, "\nTrace: {}", trace_path.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for InterruptedError {}

#[derive(Debug)]
pub struct RegistryPlanningFailure {
    pub trace_path: std::path::PathBuf,
    pub source: anyhow::Error,
}

impl std::fmt::Display for RegistryPlanningFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}\nTrace: {}",
            self.source,
            self.trace_path.display()
        )
    }
}

impl std::error::Error for RegistryPlanningFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct RegistryFailure {
    pub code: RuntimeErrorCode,
    pub message: String,
    pub name: Option<String>,
    pub target: Option<String>,
    pub reason: Option<String>,
    pub requested_by: Option<String>,
    pub suggestions: Vec<String>,
    pub operation: String,
    pub status: Option<u16>,
}

#[derive(Debug, thiserror::Error)]
#[error("{operation} request failed: {source}")]
pub struct RegistryTransportFailure {
    pub operation: String,
    #[source]
    pub source: reqwest::Error,
}

#[derive(Debug, thiserror::Error)]
#[error("failed to decode {operation} response: {source}")]
pub struct RegistryDecodeFailure {
    pub operation: String,
    #[source]
    pub source: serde_json::Error,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct NotInstalledError {
    pub packages: Vec<PackageName>,
    pub message: String,
    pub suggestions: Vec<String>,
}

impl NotInstalledError {
    pub fn packages(packages: Vec<PackageName>, suggestion_command: Option<String>) -> Self {
        let quoted: Vec<String> = packages
            .iter()
            .map(|name| format!("'{}'", name.0))
            .collect();
        let (noun, verb) = if packages.len() == 1 {
            ("package", "is")
        } else {
            ("packages", "are")
        };
        let mut message = format!("{noun} {} {verb} not installed", quoted.join(", "));
        let suggestions = suggestion_command.into_iter().collect::<Vec<_>>();
        if let Some(suggestion) = suggestions.first() {
            message.push_str(&format!(". Install with `{suggestion}`."));
        }
        Self {
            packages,
            message,
            suggestions,
        }
    }
}
