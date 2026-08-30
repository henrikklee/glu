use super::{CommandContext, CommandOutcome, CommandResult};
use crate::{command_model::CommandOutput, help, trace_cmd};

pub(crate) fn trace(
    context: &CommandContext<'_>,
    command: trace_cmd::TraceCommand,
) -> CommandResult {
    Ok(CommandOutcome::output(trace_cmd::run_trace(
        context.client,
        command,
        context.globals.verbose,
    )?))
}

pub(crate) fn help(
    context: &CommandContext<'_>,
    command: Vec<String>,
    schemas: bool,
) -> CommandResult {
    Ok(CommandOutcome::output(CommandOutput::Help(
        help::help_output(&command, context.globals.is_json(), schemas)?,
    )))
}
