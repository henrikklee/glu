use super::{approval, CommandContext, CommandOutcome, CommandResult};
use crate::command_model::{
    CliError, CliErrorDetails, CommandOutput, ErrorPackageRecord, ErrorUpdateRecord,
    InstallConfirmationDetails, RequirementErrorDetails, UpdateConfirmationDetails,
};
use crate::{confirm, output, while_resolving, CliFailure};
use glu_client::install::InstallOptions;
use glu_core::PackageSelector;

pub(crate) async fn install(
    context: &CommandContext<'_>,
    names: Vec<String>,
    force: bool,
    deps: bool,
) -> CommandResult {
    if deps && !force {
        return Err(CliFailure::Structured(Box::new(
            CliError::invalid_flag_combination(
                "--deps requires --force (or use 'reinstall --deps' to reinstall installed packages)",
                "--deps",
                vec![
                    "glu install --force --deps <name>".to_string(),
                    "glu reinstall --deps <name>".to_string(),
                ],
                Some(CliErrorDetails::Requirement(RequirementErrorDetails {
                    requires: "--force",
                })),
            ),
        )));
    }
    let names = if names.is_empty() {
        let declared = context.client.declaration_names()?;
        if declared.is_empty() {
            return Err(CliFailure::Structured(Box::new(
                CliError::empty_declaration(
                    "nothing declared in glu.json — add a package with `glu add <name>`",
                    vec!["glu add <name>".to_string()],
                ),
            )));
        }
        declared
            .into_iter()
            .map(|name| PackageSelector(name.0))
            .collect()
    } else {
        names.into_iter().map(PackageSelector).collect()
    };
    let options = InstallOptions {
        force,
        deps,
        yes: context.globals.yes,
        verbose: context.globals.verbose,
        ..Default::default()
    };
    let mut startup = context.startup;
    startup.main_entry_to_plan_seconds = context.startup_main.elapsed().as_secs_f64();
    let mut install_plan = while_resolving(
        context.show_resolution,
        context.client.plan_install(names, options),
    )
    .await??;
    install_plan.set_host_startup_diagnostics(startup);
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::InstallPlan(
            output::install_plan_output(&install_plan),
        )));
    }
    if !context.globals.is_json() {
        output::render_install_preflight(&install_plan);
        output::render_install_execution_plan(&install_plan, "install", context.globals.tree);
    }
    if install_plan.requires_confirmation {
        let approved = approval::approve(
            context.globals,
            || install_confirmation_error(&install_plan, "install"),
            || confirm::confirm_install(&install_plan, "install"),
        )?;
        if !approved {
            return Ok(CommandOutcome::cancelled());
        }
    }
    let summary = context
        .client
        .execute_install(install_plan, options, context.events.clone())
        .await?;
    Ok(CommandOutcome::output(CommandOutput::Install(
        output::install_output(&summary),
    )))
}

pub(crate) async fn reinstall(
    context: &CommandContext<'_>,
    names: Vec<String>,
    deps: bool,
) -> CommandResult {
    let names: Vec<PackageSelector> = names.into_iter().map(PackageSelector).collect();
    context.client.validate_reinstall_targets(&names)?;
    let options = InstallOptions {
        force: true,
        deps,
        yes: context.globals.yes,
        verbose: false,
        declared_policy: glu_client::install::DeclaredPolicy::Preserve,
    };
    let mut startup = context.startup;
    startup.main_entry_to_plan_seconds = context.startup_main.elapsed().as_secs_f64();
    let mut install_plan = while_resolving(
        context.show_resolution,
        context.client.plan_install(names, options),
    )
    .await??;
    install_plan.set_host_startup_diagnostics(startup);
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::ReinstallPlan(
            output::reinstall_plan_output(&install_plan),
        )));
    }
    if !context.globals.is_json() {
        output::render_install_preflight(&install_plan);
        output::render_install_execution_plan(&install_plan, "reinstall", false);
    }
    if install_plan.requires_confirmation {
        let approved = approval::approve(
            context.globals,
            || install_confirmation_error(&install_plan, "reinstall"),
            || confirm::confirm_install(&install_plan, "reinstall"),
        )?;
        if !approved {
            return Ok(CommandOutcome::cancelled());
        }
    }
    let summary = context
        .client
        .execute_install(install_plan, options, context.events.clone())
        .await?;
    Ok(CommandOutcome::output(CommandOutput::Reinstall(
        output::reinstall_output(&summary),
    )))
}

pub(crate) async fn update(
    context: &CommandContext<'_>,
    names: Vec<String>,
    all: bool,
    dependents: bool,
) -> CommandResult {
    let is_broad = all || names.is_empty();
    let names = names.into_iter().map(PackageSelector).collect();
    let plan = while_resolving(
        context.show_resolution,
        context.client.plan_update(names, all, dependents),
    )
    .await??;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::UpdatePlan(
            output::update_plan_output(&plan, is_broad),
        )));
    }
    if !context.globals.is_json() {
        output::render_update_preflight(&plan);
    }
    if plan.to_update.is_empty() && plan.to_remove.is_empty() {
        let summary = context
            .client
            .execute_update(plan, context.globals.verbose, context.events.clone())
            .await?;
        return Ok(CommandOutcome::output(CommandOutput::Update(
            output::update_output(&summary),
        )));
    }
    if !context.globals.is_json() {
        output::render_update_execution_plan(&plan, context.globals.tree);
    }
    if is_broad || !plan.to_remove.is_empty() {
        let approved = approval::approve(
            context.globals,
            || {
                let planned_updates = plan
                    .to_update
                    .iter()
                    .map(|update| ErrorUpdateRecord {
                        current: update.current.clone(),
                        latest: update.latest.clone(),
                        name: update.name.0.clone(),
                    })
                    .collect();
                let planned_removals = plan
                    .to_remove
                    .iter()
                    .map(|package| ErrorPackageRecord {
                        name: package.name.0.clone(),
                        version: package.keg_version.0.clone(),
                    })
                    .collect();
                CliError::confirmation_required(
                    "update requires confirmation; rerun with --yes to approve this computed plan",
                    vec!["glu update --yes --json".to_string()],
                    Some(CliErrorDetails::UpdateConfirmation(
                        UpdateConfirmationDetails {
                            broad: is_broad,
                            planned_removals,
                            planned_updates,
                        },
                    )),
                )
            },
            || confirm::confirm_update(&plan),
        )?;
        if !approved {
            return Ok(CommandOutcome::cancelled());
        }
    }
    let summary = context
        .client
        .execute_update(plan, context.globals.verbose, context.events.clone())
        .await?;
    Ok(CommandOutcome::output(CommandOutput::Update(
        output::update_output(&summary),
    )))
}

fn install_confirmation_error(
    plan: &glu_client::install::InstallPlan,
    command: &'static str,
) -> CliError {
    let planned_removals = plan
        .would_remove
        .iter()
        .map(|package| ErrorPackageRecord {
            name: package.name.0.clone(),
            version: package.keg_version.0.clone(),
        })
        .collect();
    CliError::confirmation_required(
        format!(
            "{command} would remove unused packages; rerun with --yes to approve this computed plan"
        ),
        vec![format!("glu {command} --yes --json <name>...")],
        Some(CliErrorDetails::InstallConfirmation(
            InstallConfirmationDetails {
                command: command.to_string(),
                planned_removals,
            },
        )),
    )
}
