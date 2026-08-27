mod list;
mod resolve;
mod summary;
mod viewer;

pub(crate) use list::TraceListOutput;
pub(crate) use summary::TraceSummary;

use crate::command_model::{CommandId, CommandOutput};
use anyhow::Result;
use clap::Subcommand;
use glu_client::GluClient;

#[derive(Debug, Subcommand)]
pub(crate) enum TraceCommand {
    /// Render a trace as an HTML timeline + flow viewer and open it.
    /// Defaults to the most recent trace.
    #[command(visible_alias = "open")]
    View {
        /// Trace id (e.g. `a1b2c3`), a package name (the most recent trace
        /// for it), a trace filename, or a path to a trace JSON file.
        /// Omitted -> the most recent trace (`last.json`).
        target: Option<String>,
    },

    /// Summarize trace timings and phase totals.
    Summary {
        /// Trace id (e.g. `a1b2c3`), a package name (the most recent trace
        /// for it), a trace filename, or a path to a trace JSON file.
        /// Omitted -> the most recent trace (`last.json`).
        target: Option<String>,

        /// Show every package row instead of the concise top slice.
        #[arg(short = 'a', long)]
        all: bool,
    },

    /// List traces, newest first.
    #[command(visible_alias = "ls")]
    List {
        /// Only list failed runs.
        #[arg(short = 'f', long)]
        failures: bool,

        /// Show every trace instead of capping at the 20 most recent.
        #[arg(short = 'a', long)]
        all: bool,
    },
}

impl TraceCommand {
    pub(crate) fn id(&self) -> CommandId {
        match self {
            Self::View { .. } => CommandId::TraceView,
            Self::Summary { .. } => CommandId::TraceSummary,
            Self::List { .. } => CommandId::TraceList,
        }
    }
}

pub(crate) struct TraceViewOutput {
    pub(crate) path: std::path::PathBuf,
    pub(crate) open: TraceOpenResult,
}

pub(crate) enum TraceOpenResult {
    Opened,
    Exited(Option<i32>),
    Failed(String),
}

pub(crate) fn result_schema(name: &str) -> Option<serde_json::Value> {
    match name {
        "TraceListResult" => Some(list::result_schema()),
        "TraceSummaryResult" => Some(summary::result_schema()),
        _ => None,
    }
}

/// Execute a trace subcommand without writing its final output.
pub(crate) fn run_trace(
    client: &GluClient,
    cmd: TraceCommand,
    verbose: bool,
) -> Result<CommandOutput> {
    Ok(match cmd {
        TraceCommand::View { target } => {
            let source = resolve::resolve_trace_target(client, target.as_deref())?;
            let path = viewer::render_trace_viewer(&source)?;
            let open = viewer::open_html(&path);
            CommandOutput::TraceView(TraceViewOutput { path, open })
        }
        TraceCommand::Summary { target, all } => CommandOutput::TraceSummary(
            summary::summary_output(client, target.as_deref(), all || verbose)?,
        ),
        TraceCommand::List { failures, all } => {
            CommandOutput::TraceList(list::list_output(client, failures, all)?)
        }
    })
}

pub(crate) fn render_trace_list_human(output: &list::TraceListOutput) {
    list::render_human(output);
}

pub(crate) fn render_trace_summary_human(output: &summary::TraceSummary) {
    summary::render_human(output);
}
