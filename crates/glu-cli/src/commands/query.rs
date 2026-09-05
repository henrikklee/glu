use super::{CommandContext, CommandOutcome, CommandResult};
use crate::command_model::{
    CommandId, CommandOutput, ErrorCode, InfoManyOutput, InfoOutput, InfoPackageError,
    InfoPackageResult, ListOutput, ListScope, ListView, ReverseDepsOutput, ReverseDepsSource,
};
use crate::{output, while_resolving};
use glu_core::PackageSelector;
use std::collections::BTreeMap;

pub(crate) fn list(
    context: &CommandContext<'_>,
    explicit_declared: bool,
    installed: bool,
    all: bool,
) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    let installed_scope = (context.globals.tree && !explicit_declared) || installed || all;
    let statuses = if context.globals.is_null() {
        BTreeMap::new()
    } else {
        query.package_statuses()
    };
    let scope = if installed_scope {
        ListScope::Installed
    } else {
        ListScope::Declared
    };
    let view = if context.globals.tree {
        ListView::Tree(if explicit_declared {
            query.list_tree()
        } else {
            query.list_tree_all()
        })
    } else if installed_scope {
        ListView::Flat(query.list())
    } else {
        ListView::Flat(query.declared())
    };
    let hidden_dependencies = match &view {
        ListView::Flat(packages) if !installed_scope => {
            query.total_kegs().saturating_sub(packages.len())
        }
        _ => 0,
    };
    Ok(CommandOutcome::output(CommandOutput::List(ListOutput {
        scope,
        view,
        statuses,
        hidden_dependencies,
        show_dependency_hint: !explicit_declared,
    })))
}

pub(crate) async fn deps(
    context: &CommandContext<'_>,
    name: String,
    all: bool,
    status: bool,
    online: bool,
) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    let target = name.clone();
    let selector = PackageSelector(name);
    let queries_registry = online || query.resolve_selector(&selector).is_none();
    let view = while_resolving(
        context.show_resolution && queries_registry,
        context.client.cancellation_token(),
        context
            .client
            .deps(&query, selector, online, !context.globals.is_null()),
    )
    .await??;
    let direct = !context.globals.tree && !all;
    Ok(CommandOutcome::output(CommandOutput::Deps(
        output::deps_output(view, target, direct, status),
    )))
}

pub(crate) fn why(context: &CommandContext<'_>, name: String, all: bool) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    let view = query.why(&PackageSelector(name.clone()), true);
    Ok(CommandOutcome::output(CommandOutput::ReverseDeps(
        ReverseDepsOutput {
            command: CommandId::Why,
            source: ReverseDepsSource::Installed,
            target: name,
            direct: false,
            all: all && !context.globals.tree,
            root: view.root,
            statuses: view.statuses,
        },
    )))
}

pub(crate) async fn uses(context: &CommandContext<'_>, name: String, all: bool) -> CommandResult {
    let selector = PackageSelector(name.clone());
    let direct = !context.globals.tree && !all;
    let statuses = if context.globals.is_null() {
        BTreeMap::new()
    } else {
        context
            .client
            .query_state(context.events.as_ref())?
            .package_statuses()
    };
    let Some(root) = while_resolving(
        context.show_resolution,
        context.client.cancellation_token(),
        context.client.uses(selector, direct),
    )
    .await??
    else {
        return Err(anyhow::anyhow!("registry returned nothing for '{name}'").into());
    };
    Ok(CommandOutcome::output(CommandOutput::ReverseDeps(
        ReverseDepsOutput {
            command: CommandId::Uses,
            source: ReverseDepsSource::Registry,
            target: name,
            direct,
            all: all && !context.globals.tree,
            root: Some(root),
            statuses,
        },
    )))
}

pub(crate) async fn info(context: &CommandContext<'_>, names: Vec<String>) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    if names.len() == 1 {
        let requested = PackageSelector(names.into_iter().next().expect("one info name"));
        let (info, installed) = while_resolving(
            context.show_resolution,
            context.client.cancellation_token(),
            context.client.info(&query, requested),
        )
        .await??;
        let status = query.package_status(&info.package_key);
        return Ok(CommandOutcome::output(CommandOutput::Info(Box::new(
            InfoOutput {
                package: info,
                installed,
                declared: status.as_ref().is_some_and(|status| status.declared),
                deactivated: status.is_some_and(|status| status.deactivated),
            },
        ))));
    }

    let config = context.client.config().clone();
    let registry = context.client.registry_client()?;
    let mut handles = Vec::new();
    for name in names {
        let selector = PackageSelector(name.clone());
        let registry = registry.clone();
        let target = config.target.clone();
        handles.push(tokio::spawn(async move {
            let result = registry.info(&selector, &target).await;
            (name, result)
        }));
    }

    let results = while_resolving(
        context.show_resolution,
        context.client.cancellation_token(),
        async {
            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                results.push(handle.await);
            }
            results
        },
    )
    .await?;
    let mut packages = Vec::new();
    for result in results {
        match result {
            Ok((name, Ok(info))) => {
                let installed = query.find_by_key(&info.package_key).cloned();
                let status = query.package_status(&info.package_key);
                packages.push(InfoPackageResult {
                    requested: name,
                    found: true,
                    package: Some(info),
                    installed,
                    declared: status.as_ref().is_some_and(|status| status.declared),
                    deactivated: status.is_some_and(|status| status.deactivated),
                    error: None,
                });
            }
            Ok((_, Err(error)))
                if error
                    .downcast_ref::<glu_client::error::InterruptedError>()
                    .is_some() =>
            {
                return Err(error.into());
            }
            Ok((name, Err(error))) => {
                let selector = PackageSelector(name.clone());
                let installed = query.resolve_selector(&selector).cloned();
                let status = query.package_status_for_selector(&selector);
                packages.push(InfoPackageResult {
                    requested: name,
                    found: false,
                    package: None,
                    installed,
                    declared: status.as_ref().is_some_and(|status| status.declared),
                    deactivated: status.is_some_and(|status| status.deactivated),
                    error: Some(InfoPackageError {
                        code: ErrorCode::PackageInfoFailed,
                        message: error.to_string(),
                    }),
                });
            }
            Err(error) => packages.push(InfoPackageResult {
                requested: "<task>".to_string(),
                found: false,
                package: None,
                installed: None,
                declared: false,
                deactivated: false,
                error: Some(InfoPackageError {
                    code: ErrorCode::PackageInfoTaskFailed,
                    message: error.to_string(),
                }),
            }),
        }
    }
    Ok(CommandOutcome::output(CommandOutput::InfoMany(
        InfoManyOutput { packages },
    )))
}

pub(crate) async fn outdated(context: &CommandContext<'_>, declared: bool) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    let mut outdated = while_resolving(
        context.show_resolution,
        context.client.cancellation_token(),
        context.client.outdated(&query),
    )
    .await??;
    let scope = if declared {
        outdated.packages.retain(|package| {
            query
                .package_status(&package.package_key)
                .is_some_and(|status| status.declared)
        });
        "declared"
    } else {
        "installed"
    };
    Ok(CommandOutcome::output(CommandOutput::Outdated(
        output::outdated_output(scope, outdated),
    )))
}

pub(crate) fn status(context: &CommandContext<'_>) -> CommandResult {
    let query = context.client.query_state(context.events.as_ref())?;
    Ok(CommandOutcome::output(CommandOutput::Status(
        output::status_output(context.client, &query)?,
    )))
}
