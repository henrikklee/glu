use super::{CommandContext, CommandOutcome, CommandResult};
use crate::{command_model::CommandOutput, output};
use glu_core::PackageSelector;

pub(crate) fn deactivate(context: &CommandContext<'_>, names: Vec<String>) -> CommandResult {
    let names = names.into_iter().map(PackageSelector).collect();
    let results = context.client.deactivate(names)?;
    Ok(CommandOutcome::output(CommandOutput::Deactivation(
        output::deactivation_output(&results),
    )))
}

pub(crate) fn activate(
    context: &CommandContext<'_>,
    names: Vec<String>,
    force: bool,
) -> CommandResult {
    let names = names.into_iter().map(PackageSelector).collect();
    let results = context.client.activate(names, force)?;
    Ok(CommandOutcome::output(CommandOutput::Activation(
        output::activation_output(force, &results),
    )))
}

pub(crate) fn shellenv(context: &CommandContext<'_>, shell: Option<String>) -> CommandResult {
    Ok(CommandOutcome::output(CommandOutput::Shellenv(
        context.client.shellenv(shell.as_deref())?,
    )))
}

pub(crate) fn setup(context: &CommandContext<'_>) -> CommandResult {
    Ok(CommandOutcome::output(CommandOutput::Setup(
        context.client.setup_shells()?,
    )))
}

pub(crate) async fn upgrade(context: &CommandContext<'_>) -> CommandResult {
    Ok(CommandOutcome::output(CommandOutput::Upgrade(
        context.client.upgrade(context.events.as_ref()).await?,
    )))
}
