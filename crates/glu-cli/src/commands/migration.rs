use super::{approval, CommandContext, CommandOutcome, CommandResult};
use crate::command_model::{
    CliError, CliErrorDetails, CommandOutput, ErrorPackageRecord, InstallConfirmationDetails,
};
use crate::{confirm, output, while_resolving};
use glu_client::install::InstallOptions;
use std::path::PathBuf;

pub(crate) async fn migrate(context: &CommandContext<'_>, source: PathBuf) -> CommandResult {
    let plan = while_resolving(
        context.show_resolution,
        context.client.plan_migration(source),
    )
    .await??;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::MigratePlan(
            output::migrate_plan_output(&plan),
        )));
    }

    if plan.roots.is_empty() {
        let summary = context
            .client
            .execute_migration(plan, InstallOptions::default(), context.events.clone())
            .await?;
        return Ok(CommandOutcome::output(CommandOutput::Migrate(
            output::migrate_output(&summary),
        )));
    }

    if !context.globals.is_json() {
        output::render_migration_preflight(&plan, context.globals);
    }
    let approved = approval::approve(
        context.globals,
        || migration_confirmation_error(&plan),
        || confirm::confirm_migration(plan.roots.len()),
    )?;
    if !approved {
        return Ok(CommandOutcome::cancelled());
    }

    let options = InstallOptions {
        yes: context.globals.yes,
        verbose: context.globals.verbose,
        ..Default::default()
    };
    let summary = context
        .client
        .execute_migration(plan, options, context.events.clone())
        .await?;
    Ok(CommandOutcome::output(CommandOutput::Migrate(
        output::migrate_output(&summary),
    )))
}

fn migration_confirmation_error(plan: &glu_client::migrate::MigrationPlan) -> CliError {
    let planned_removals = plan
        .install
        .as_ref()
        .into_iter()
        .flat_map(|install| &install.would_remove)
        .map(|package| ErrorPackageRecord {
            name: package.name.0.clone(),
            version: package.keg_version.0.clone(),
        })
        .collect();
    CliError::confirmation_required(
        "migration requires confirmation; rerun with --yes to approve this computed plan",
        vec!["glu migrate --yes --json".to_string()],
        Some(CliErrorDetails::InstallConfirmation(
            InstallConfirmationDetails {
                command: "migrate".to_string(),
                planned_removals,
            },
        )),
    )
}
