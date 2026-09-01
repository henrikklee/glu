use super::{approval, CommandContext, CommandOutcome, CommandResult};
use crate::command_model::{
    CleanupConfirmationDetails, CleanupConfirmationRecord, CliError, CliErrorDetails,
    CommandOutput, ErrorPackageRecord, ModifiedConfigConfirmationDetails, PlannedRemovalsDetails,
    PurgeConfirmationDetails, RemovalConfirmationDetails,
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

pub(crate) fn purge(
    context: &CommandContext<'_>,
    keep_declaration: bool,
    remove_config: bool,
) -> CommandResult {
    let purge_plan = context.client.plan_purge(keep_declaration)?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::PurgePlan(
            output::purge_plan_output(&purge_plan, remove_config),
        )));
    }
    if !purge_plan.requires_confirmation() {
        return Ok(CommandOutcome::output(CommandOutput::Purge(
            output::purge_output(
                &purge_plan,
                &glu_client::remove::RemovalResult {
                    removed: Vec::new(),
                    mutable_files: Default::default(),
                },
            ),
        )));
    }
    if !context.globals.is_json() {
        output::render_purge_execution_plan(&purge_plan, remove_config, context.globals.verbose);
    }
    let purge_hint = match (keep_declaration, remove_config) {
        (true, true) => "glu purge --keep-declaration --remove-config --yes --json",
        (true, false) => "glu purge --keep-declaration --yes --json",
        (false, true) => "glu purge --remove-config --yes --json",
        (false, false) => "glu purge --yes --json",
    };
    let approved = approval::approve(
        context.globals,
        || {
            let summary = output::purge_plan_output(&purge_plan, remove_config);
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
                vec![purge_hint.to_string()],
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
        || confirm::confirm_purge(&purge_plan, remove_config),
    )?;
    if !approved {
        return Ok(CommandOutcome::cancelled());
    }
    let remove_modified = approve_modified_config_cleanup(
        context,
        &purge_plan.mutable_files.modified,
        remove_config,
        "glu purge --remove-config --yes --json",
    )?;
    let result = context.client.execute_purge(&purge_plan, remove_modified)?;
    Ok(CommandOutcome::output(CommandOutput::Purge(
        output::purge_output(&purge_plan, &result),
    )))
}

pub(crate) fn remove(
    context: &CommandContext<'_>,
    names: Vec<String>,
    remove_config: bool,
) -> CommandResult {
    let removal_plan = context
        .client
        .plan_removal(names.into_iter().map(PackageSelector).collect())?;
    if context.globals.plan {
        return Ok(CommandOutcome::output(CommandOutput::RemovalPlan(
            output::removal_plan_output(&removal_plan, remove_config),
        )));
    }
    if !context.globals.is_json() {
        output::render_removal_execution_plan(
            &removal_plan,
            remove_config,
            context.globals.verbose,
        );
    }
    if removal_plan.has_unnamed_removals() {
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
                let hint = if remove_config {
                    "glu remove --remove-config --yes --json <selector>..."
                } else {
                    "glu remove --yes --json <selector>..."
                };
                CliError::confirmation_required(
                    "remove would remove packages beyond the named selectors; rerun with --yes to approve this computed plan",
                    vec![hint.to_string()],
                    Some(CliErrorDetails::RemovalConfirmation(
                        RemovalConfirmationDetails {
                            named,
                            planned_removals,
                        },
                    )),
                )
            },
            || confirm::confirm_removal(&removal_plan, remove_config),
        )?;
        if !approved {
            return Ok(CommandOutcome::cancelled());
        }
    }
    let remove_modified = approve_modified_config_cleanup(
        context,
        &removal_plan.mutable_files.modified,
        remove_config,
        "glu remove --remove-config --yes --json <selector>...",
    )?;
    let result = context
        .client
        .execute_removal(&removal_plan, remove_modified)?;
    Ok(CommandOutcome::output(CommandOutput::Removal(
        output::removal_output(&result, &removal_plan.kept),
    )))
}

fn approve_modified_config_cleanup(
    context: &CommandContext<'_>,
    modified: &[glu_client::remove::MutableFilePlan],
    requested: bool,
    command_hint: &str,
) -> Result<bool, crate::CliFailure> {
    if modified.is_empty() {
        return Ok(false);
    }
    if !requested {
        if context.globals.yes || context.globals.is_json() {
            return Ok(false);
        }
        return confirm::offer_modified_config_cleanup(modified.len())
            .map_err(crate::CliFailure::from);
    }
    approval::approve(
        context.globals,
        || {
            CliError::confirmation_required(
                "remove would delete modified configuration files; rerun with --remove-config --yes to approve this computed plan",
                vec![command_hint.to_string()],
                Some(CliErrorDetails::ModifiedConfigConfirmation(
                    ModifiedConfigConfirmationDetails {
                        modified_config_files: modified
                            .iter()
                            .map(|file| file.path.to_string_lossy().into_owned())
                            .collect(),
                    },
                )),
            )
        },
        || confirm::confirm_modified_config_cleanup(modified.len()),
    )
}
