pub(crate) mod approval;
pub(crate) mod configuration;
pub(crate) mod migration;
pub(crate) mod observability;
pub(crate) mod packages;
pub(crate) mod query;
pub(crate) mod removal;

use crate::command_model::{CommandOutput, GlobalOptions};
use glu_client::{events::ExecutionEvents, install::HostStartupDiagnostics, GluClient};
use std::sync::Arc;

/// Invocation-scoped dependencies shared by command handlers. Process policy
/// such as parsing, capability validation, mutation locking, and recovery
/// remains in the host before this context is constructed.
pub(crate) struct CommandContext<'a> {
    pub(crate) client: &'a GluClient,
    pub(crate) globals: GlobalOptions,
    pub(crate) events: Arc<dyn ExecutionEvents>,
    pub(crate) show_resolution: bool,
    pub(crate) startup_main: std::time::Instant,
    pub(crate) startup: HostStartupDiagnostics,
}

/// A command either owns one final semantic output or was cancelled normally
/// at an interactive approval gate.
pub(crate) struct CommandOutcome {
    output: Option<CommandOutput>,
}

impl CommandOutcome {
    pub(crate) fn cancelled() -> Self {
        Self { output: None }
    }

    pub(crate) fn output(output: CommandOutput) -> Self {
        Self {
            output: Some(output),
        }
    }

    pub(crate) fn into_output(self) -> Option<CommandOutput> {
        self.output
    }
}

pub(crate) type CommandResult = Result<CommandOutcome, crate::CliFailure>;
