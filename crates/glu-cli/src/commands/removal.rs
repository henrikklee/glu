use super::{approval, CommandContext, CommandOutcome, CommandResult};
use crate::command_model::{
    CleanupConfirmationDetails, CleanupConfirmationRecord, CliError, CliErrorDetails,
    CommandOutput, ErrorPackageRecord, PlannedRemovalsDetails, PurgeConfirmationDetails,
    RemovalConfirmationDetails,
};
use crate::{confirm, output};
use glu_core::PackageSelector;

pub(crate) fn autoremove(context: &CommandContext<'_>) -> CommandResult {
    let dangling = context.client.plan_autoremove()?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::AutoremovePlan(
            output::autoremove_plan_output(&dangling),
        )));
    }
    if dangling.is_empty() {
        return Ok(CommandOutcome::output(CommandOutput::Autoremove(
            output::autoremove_output(&[]),
        )));
    }
    if !context.globals.is_json() {
        output::render_autoremove_execution_plan(&dangling);
    }
    let approved = approval::approve(
        context.globals,
        || {
            let planned_removals = dangling
                .iter()
                .map(|package| ErrorPackageRecord {
                    name: package.name.0.clone(),
                    version: package.keg_version.0.clone(),
                })
                .collect();
            CliError::confirmation_required(
                "autoremove would remove packages; rerun with --yes to approve this computed plan",
                vec!["glu autoremove --yes --json".to_string()],
                Some(CliErrorDetails::PlannedRemovals(PlannedRemovalsDetails {
                    planned_removals,
                })),
            )
        },
        || confirm::confirm_autoremove(&dangling),
    )?;
    if !approved {
        return Ok(CommandOutcome::cancelled());
    }
    let removed = context.client.execute_autoremove(&dangling)?;
    Ok(CommandOutcome::output(CommandOutput::Autoremove(
        output::autoremove_output(&removed),
    )))
}

pub(crate) fn cleanup(context: &CommandContext<'_>) -> CommandResult {
    let cleanup_plan = context.client.plan_cache_cleanup()?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::CleanupPlan(
            output::cleanup_plan_output(&cleanup_plan),
        )));
    }
    if cleanup_plan.bottles().is_empty() {
        return Ok(CommandOutcome::output(CommandOutput::Cleanup(
            output::cleanup_output(&glu_client::download::cache::CacheCleanupResult::default()),
        )));
    }
    if !context.globals.is_json() {
        output::render_cleanup_execution_plan(&cleanup_plan);
    }
    let approved = approval::approve(
        context.globals,
        || {
            let summary = output::cleanup_plan_output(&cleanup_plan);
            let planned_removals = summary
                .would_remove
                .into_iter()
                .map(|bottle| CleanupConfirmationRecord {
                    name: bottle.name,
                    version: bottle.version,
                    downloads: bottle.downloads,
                    bytes: bottle.bytes,
                })
                .collect();
            CliError::confirmation_required(
                "cleanup would remove cached downloads; rerun with --yes to approve this computed plan",
                vec!["glu cleanup --yes --json".to_string()],
                Some(CliErrorDetails::CleanupConfirmation(
                    CleanupConfirmationDetails {
                        planned_removals,
                        planned_downloads: summary.would_remove_downloads,
                        unassociated_downloads: summary.unassociated_downloads,
                        unassociated_bytes: summary.unassociated_bytes,
                        reclaimable_bytes: summary.would_reclaim_bytes,
                    },
                )),
            )
        },
        || confirm::confirm_cleanup(&cleanup_plan),
    )?;
    if !approved {
        return Ok(CommandOutcome::cancelled());
    }
    let cleaned = context.client.execute_cache_cleanup(&cleanup_plan)?;
    Ok(CommandOutcome::output(CommandOutput::Cleanup(
        output::cleanup_output(&cleaned),
    )))
}

pub(crate) fn purge(context: &CommandContext<'_>, keep_declaration: bool) -> CommandResult {
    let purge_plan = context.client.plan_purge(keep_declaration)?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::PurgePlan(
            output::purge_plan_output(&purge_plan),
        )));
    }
    if !purge_plan.requires_confirmation() {
        return Ok(CommandOutcome::output(CommandOutput::Purge(
            output::purge_output(&purge_plan, &[]),
        )));
    }
    if !context.globals.is_json() {
        output::render_purge_execution_plan(&purge_plan);
    }
    let approved = approval::approve(
        context.globals,
        || {
            let summary = output::purge_plan_output(&purge_plan);
            let planned_removals = purge_plan
                .packages
                .iter()
                .map(|package| ErrorPackageRecord {
                    name: package.name.0.clone(),
                    version: package.keg_version.0.clone(),
                })
                .collect();
            CliError::confirmation_required(
                "purge would remove installed packages or glu.json; rerun with --yes to approve this computed plan",
                vec![if keep_declaration {
                    "glu purge --keep-declaration --yes --json".to_string()
                } else {
                    "glu purge --yes --json".to_string()
                }],
                Some(CliErrorDetails::PurgeConfirmation(
                    PurgeConfirmationDetails {
                        planned_removals,
                        declaration: summary.declaration,
                        declared_packages: summary.declared_packages,
                        reclaimable_bytes: summary.would_reclaim_bytes,
                    },
                )),
            )
        },
        || confirm::confirm_purge(&purge_plan),
    )?;
    if !approved {
        return Ok(CommandOutcome::cancelled());
    }
    let removed = context.client.execute_purge(&purge_plan)?;
    let leftover =
        glu_client::remove::leftover_config_files(&context.client.config().prefix, &removed);
    Ok(CommandOutcome::output(CommandOutput::Purge(
        output::purge_output(&purge_plan, &leftover),
    )))
}

pub(crate) fn remove(context: &CommandContext<'_>, names: Vec<String>) -> CommandResult {
    let removal_plan = context
        .client
        .plan_removal(names.into_iter().map(PackageSelector).collect())?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::RemovalPlan(
            output::removal_plan_output(&removal_plan),
        )));
    }
    if !context.globals.is_json() {
        output::render_removal_execution_plan(&removal_plan);
    }
    if removal_plan.to_remove.len() > removal_plan.named.len() {
        let approved = approval::approve(
            context.globals,
            || {
                let named = removal_plan
                    .named
                    .iter()
                    .map(|package| ErrorPackageRecord {
                        name: package.name.0.clone(),
                        version: package.keg_version.0.clone(),
                    })
                    .collect();
                let planned_removals = removal_plan
                    .to_remove
                    .iter()
                    .map(|package| ErrorPackageRecord {
                        name: package.name.0.clone(),
                        version: package.keg_version.0.clone(),
                    })
                    .collect();
                CliError::confirmation_required(
                    "remove would remove packages beyond the named selectors; rerun with --yes to approve this computed plan",
                    vec!["glu remove --yes --json <selector>...".to_string()],
                    Some(CliErrorDetails::RemovalConfirmation(
                        RemovalConfirmationDetails {
                            named,
                            planned_removals,
                        },
                    )),
                )
            },
            || confirm::confirm_removal(&removal_plan),
        )?;
        if !approved {
            return Ok(CommandOutcome::cancelled());
        }
    }
    let removed = context.client.execute_removal(&removal_plan)?;
    let leftover =
        glu_client::remove::leftover_config_files(&context.client.config().prefix, &removed);
    Ok(CommandOutcome::output(CommandOutput::Removal(
        output::removal_output(&removed, &removal_plan.kept, &leftover),
    )))
}
