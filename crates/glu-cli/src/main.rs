mod args;
mod command_model;
mod confirm;
mod diagnostic;
mod events;
mod help;
mod output;
mod package_list;
mod progress;
mod tables;
mod trace_cmd;

use anyhow::anyhow;
use args::{Cli, Command};
use clap::Parser;
use command_model::{
    command_spec_by_id, CleanupConfirmationDetails, CleanupConfirmationRecord, CliError,
    CliErrorDetails, CommandId, CommandOutput, CommandSpec, ErrorCode, ErrorPackageRecord,
    ErrorUpdateRecord, ExitClass, GlobalOptions, InfoManyOutput, InfoOutput, InfoPackageError,
    InfoPackageResult, InstallConfirmationDetails, InvocationInfo, ListOutput, ListScope, ListView,
    OperationErrorDetails, PackagesErrorDetails, PartialInstallDetails, PlannedRemovalsDetails,
    RegistryErrorDetails, RemovalConfirmationDetails, RequirementErrorDetails, ReverseDepsOutput,
    ReverseDepsSource, UnsupportedOptionDetails, UpdateConfirmationDetails, COMMAND_SPECS,
};
use glu_client::{config::ClientConfig, install::InstallOptions, GluClient};
#[cfg(test)]
use glu_core::PackageName;
use glu_core::PackageSelector;
use std::{collections::BTreeMap, future::Future, io::IsTerminal, time::Duration};

#[tokio::main]
async fn main() {
    // Restore the default SIGPIPE disposition so `glu foo | head` exits
    // quietly on a closed pipe instead of panicking (Rust's runtime sets
    // SIGPIPE to ignore by default).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let invocation = InvocationContext::from_args(&args);
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            if error.kind() == clap::error::ErrorKind::DisplayVersion {
                print!("{error}");
            } else {
                match help::help_output(&invocation.command_path, invocation.json_requested, false)
                {
                    Ok(help) => output::render_help_output(&help),
                    Err(error) => {
                        let failure = CliFailure::Runtime(error);
                        if invocation.json_requested {
                            print_json_error(&failure, &invocation);
                        } else {
                            print_cli_failure(&failure);
                        }
                        std::process::exit(failure_exit_class(&failure).code());
                    }
                }
            }
            return;
        }
        Err(error) => {
            let failure = CliFailure::Parse(error);
            if invocation.json_requested {
                print_json_error(&failure, &invocation);
            } else {
                print_cli_failure(&failure);
            }
            std::process::exit(2);
        }
    };

    match run(cli).await {
        Ok((Some(result), globals)) => output::render_command_output(&result, &globals),
        Ok((None, _)) => {}
        Err(error) => {
            if invocation.json_requested {
                print_json_error(&error, &invocation);
            } else {
                print_cli_failure(&error);
            }
            std::process::exit(failure_exit_class(&error).code());
        }
    }
}

#[derive(Debug)]
struct InvocationContext {
    json_requested: bool,
    argv: Vec<String>,
    command: Option<String>,
    command_path: Vec<String>,
    recognized: bool,
}

impl InvocationContext {
    fn from_args(args: &[String]) -> Self {
        let inference = infer_command(args);
        Self {
            json_requested: output_json_requested(args),
            argv: args.to_vec(),
            command: inference.command,
            command_path: inference.command_path,
            recognized: inference.recognized,
        }
    }

    fn info(&self) -> InvocationInfo {
        InvocationInfo {
            argv: self.argv.clone(),
            command_path: self.command_path.clone(),
            recognized: self.recognized,
        }
    }
}

struct CommandInference {
    command: Option<String>,
    command_path: Vec<String>,
    recognized: bool,
}

#[derive(Debug)]
enum CliFailure {
    Parse(clap::Error),
    Structured(Box<CliError>),
    Runtime(anyhow::Error),
}

impl From<anyhow::Error> for CliFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Runtime(error)
    }
}

#[derive(Debug)]
struct ResolutionInterrupted;

impl std::fmt::Display for ResolutionInterrupted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("interrupted while resolving registry request (Ctrl+C)")
    }
}

impl std::error::Error for ResolutionInterrupted {}

/// Runs a registry-bound future with transient first-line feedback. Machine
/// output and non-interactive output await the future without touching the
/// terminal. Dropping the future clears the spinner through its RAII guard.
async fn while_resolving<F>(enabled: bool, future: F) -> Result<F::Output, CliFailure>
where
    F: Future,
{
    if !enabled {
        return Ok(future.await);
    }

    let mut spinner = progress::ResolutionSpinner::new();
    spinner.start();

    let start = tokio::time::Instant::now() + Duration::from_millis(80);
    let mut ticker = tokio::time::interval_at(start, Duration::from_millis(80));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(future);
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            output = &mut future => {
                spinner.clear();
                return Ok(output);
            }
            _ = ticker.tick() => spinner.tick(),
            _ = &mut interrupt => {
                spinner.clear();
                return Err(anyhow::Error::new(ResolutionInterrupted).into());
            }
        }
    }
}

async fn run(cli: Cli) -> std::result::Result<(Option<CommandOutput>, GlobalOptions), CliFailure> {
    let globals = GlobalOptions::from_args(cli.globals);
    let json = globals.is_json();
    let null = globals.is_null();
    let tree = globals.tree;
    let verbose = globals.verbose;
    let plan = globals.plan;
    let yes = globals.yes;
    let show_resolution = !json && !null && std::io::stdout().is_terminal();
    let Some(command) = cli.command else {
        // Bare JSON is the discoverable machine contract; bare human output
        // remains the grouped overview.
        let result = CommandOutput::Help(help::help_output(&[], json, false)?);
        return Ok((Some(result), globals));
    };
    let command_id = command.id();
    let spec = command_spec_by_id(command_id).expect("every command has a descriptor");
    validate_global_options(command_id, spec, &globals)?;
    if let Command::PostinstallWorker { job, result } = &command {
        // Runs under the parent's platform sandbox. Do not resolve config or
        // take the prefix mutation lock here: the parent install already owns
        // orchestration state, and re-locking would deadlock.
        glu_client::postinstall::sandbox::run_worker(job, result)?;
        return Ok((None, globals));
    }
    let config = ClientConfig::default_for_host();
    // A1: serialize mutating commands against this prefix with a process lock,
    // so two concurrent install/up/rm/upgrade runs can't race on staging
    // dirs, linked markers, or state writes. Read-only queries skip it.
    let mutating = !plan && spec.mutates;
    let _op_lock = if mutating {
        Some(glu_client::state::op_lock::acquire(&config.prefix)?)
    } else {
        None
    };
    if mutating {
        // Recovery is an execution-side mutation. Plan commands deliberately
        // use read-only snapshots and must not clean or rewrite prefix state.
        glu_client::state::recovery::cleanup_interrupted(&config.prefix)?;
    }
    let client = GluClient::new(config);
    let events = events::for_invocation(&globals);
    let final_output;

    match command {
        Command::Install { names, force, deps } => {
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
                // Bare `glu install` = sync the declaration (glu.json):
                // install every declared package that isn't installed yet.
                let declared = client.declaration_names()?;
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
                yes,
                verbose,
                ..Default::default()
            };
            let install_plan =
                while_resolving(show_resolution, client.plan_install(names, options)).await??;
            if plan {
                final_output = Some(CommandOutput::InstallPlan(output::install_plan_output(
                    &install_plan,
                )));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_install_preflight(&install_plan);
            }
            if !json {
                output::render_install_execution_plan(&install_plan, "install", tree);
            }
            if install_plan.requires_confirmation && !yes {
                if json {
                    return Err(install_confirmation_failure(&install_plan, "install"));
                }
                if !confirm::confirm_install(&install_plan, "install")? {
                    return Ok((None, globals));
                }
            }
            let summary = client
                .execute_install(install_plan, options, events.clone())
                .await?;
            final_output = Some(CommandOutput::Install(output::install_output(&summary)));
        }
        Command::Reinstall { deps, names } => {
            let names: Vec<PackageSelector> = names.into_iter().map(PackageSelector).collect();
            client.validate_reinstall_targets(&names)?;
            let options = InstallOptions {
                force: true,
                deps,
                yes,
                verbose: false,
                declared_policy: glu_client::install::DeclaredPolicy::Preserve,
            };
            let install_plan =
                while_resolving(show_resolution, client.plan_install(names, options)).await??;
            if plan {
                final_output = Some(CommandOutput::ReinstallPlan(output::reinstall_plan_output(
                    &install_plan,
                )));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_install_preflight(&install_plan);
            }
            if !json {
                output::render_install_execution_plan(&install_plan, "reinstall", false);
            }
            if install_plan.requires_confirmation && !yes {
                if json {
                    return Err(install_confirmation_failure(&install_plan, "reinstall"));
                }
                if !confirm::confirm_install(&install_plan, "reinstall")? {
                    return Ok((None, globals));
                }
            }
            let summary = client
                .execute_install(install_plan, options, events.clone())
                .await?;
            final_output = Some(CommandOutput::Reinstall(output::reinstall_output(&summary)));
        }
        Command::Autoremove {} => {
            let dangling = client.plan_autoremove()?;
            if plan {
                final_output = Some(CommandOutput::AutoremovePlan(
                    output::autoremove_plan_output(&dangling),
                ));
                return Ok((final_output, globals));
            }
            if dangling.is_empty() {
                final_output = Some(CommandOutput::Autoremove(output::autoremove_output(&[])));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_autoremove_execution_plan(&dangling);
            }
            if !yes {
                if json {
                    let planned_removals = dangling
                        .iter()
                        .map(|package| ErrorPackageRecord {
                            name: package.name.0.clone(),
                            version: package.keg_version.0.clone(),
                        })
                        .collect();
                    return Err(CliFailure::Structured(Box::new(
                        CliError::confirmation_required(
                            "autoremove would remove packages; rerun with --yes to approve this computed plan",
                            vec!["glu autoremove --yes --json".to_string()],
                            Some(CliErrorDetails::PlannedRemovals(
                                PlannedRemovalsDetails { planned_removals },
                            )),
                        ),
                    )));
                }
                if !confirm::confirm_autoremove(&dangling)? {
                    return Ok((None, globals));
                }
            }
            let removed = client.execute_autoremove(&dangling)?;
            final_output = Some(CommandOutput::Autoremove(output::autoremove_output(
                &removed,
            )));
        }
        Command::Cleanup {} => {
            let cleanup_plan = client.plan_cache_cleanup()?;
            if plan {
                final_output = Some(CommandOutput::CleanupPlan(output::cleanup_plan_output(
                    &cleanup_plan,
                )));
                return Ok((final_output, globals));
            }
            if cleanup_plan.bottles().is_empty() {
                final_output = Some(CommandOutput::Cleanup(output::cleanup_output(
                    &glu_client::download::cache::CacheCleanupResult::default(),
                )));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_cleanup_execution_plan(&cleanup_plan);
            }
            if !yes {
                if json {
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
                    return Err(CliFailure::Structured(Box::new(
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
                        ),
                    )));
                }
                if !confirm::confirm_cleanup(&cleanup_plan)? {
                    return Ok((None, globals));
                }
            }
            let cleaned = client.execute_cache_cleanup(&cleanup_plan)?;
            final_output = Some(CommandOutput::Cleanup(output::cleanup_output(&cleaned)));
        }
        Command::Remove { names } => {
            let removal_plan =
                client.plan_removal(names.into_iter().map(PackageSelector).collect())?;
            if plan {
                final_output = Some(CommandOutput::RemovalPlan(output::removal_plan_output(
                    &removal_plan,
                )));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_removal_execution_plan(&removal_plan);
            }
            if !yes && removal_plan.to_remove.len() > removal_plan.named.len() {
                if json {
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
                    return Err(CliFailure::Structured(Box::new(
                        CliError::confirmation_required(
                            "remove would remove packages beyond the named selectors; rerun with --yes to approve this computed plan",
                            vec!["glu remove --yes --json <selector>...".to_string()],
                            Some(CliErrorDetails::RemovalConfirmation(
                                RemovalConfirmationDetails {
                                    named,
                                    planned_removals,
                                },
                            )),
                        ),
                    )));
                }
                if !confirm::confirm_removal(&removal_plan)? {
                    return Ok((None, globals));
                }
            }
            let removed = client.execute_removal(&removal_plan)?;
            // `.bottle`-sourced config files are real copies in the prefix now
            // (install_etc_var) and `rm` deliberately does not delete them —
            // Homebrew parity (uninstall.rb:76-118). Report the same notice/result
            // through the selected renderer so JSON stays a single final document.
            let leftover =
                glu_client::remove::leftover_config_files(&client.config().prefix, &removed);
            final_output = Some(CommandOutput::Removal(output::removal_output(
                &removed,
                &removal_plan.kept,
                &leftover,
            )));
        }
        Command::List {
            declared: explicit_declared,
            installed,
            all,
        } => {
            let query = client.query_state(events.as_ref())?;
            let installed_scope = globals.tree || installed || all;
            let statuses = if null {
                BTreeMap::new()
            } else {
                query.package_statuses()
            };
            let scope = if installed_scope {
                ListScope::Installed
            } else {
                ListScope::Declared
            };
            let view = if globals.tree {
                ListView::Tree(query.list_tree_all())
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
            final_output = Some(CommandOutput::List(ListOutput {
                scope,
                view,
                statuses,
                hidden_dependencies,
                show_dependency_hint: !explicit_declared,
            }));
        }
        Command::Deps {
            name,
            all,
            status,
            online,
        } => {
            let query = client.query_state(events.as_ref())?;
            let target = name.clone();
            let selector = PackageSelector(name);
            let queries_registry = online || query.resolve_selector(&selector).is_none();
            let view = while_resolving(
                show_resolution && queries_registry,
                client.deps(&query, selector, online, !null),
            )
            .await??;
            let direct = !tree && !all;
            final_output = Some(CommandOutput::Deps(output::deps_output(
                view, target, direct, status,
            )));
        }
        Command::Why { all, name } => {
            let query = client.query_state(events.as_ref())?;
            let view = query.why(&PackageSelector(name.clone()), true);
            final_output = Some(CommandOutput::ReverseDeps(ReverseDepsOutput {
                command: CommandId::Why,
                source: ReverseDepsSource::Installed,
                target: name,
                direct: false,
                all: all && !tree,
                root: view.root,
                statuses: view.statuses,
            }));
        }
        Command::Uses { all, name } => {
            let selector = PackageSelector(name.clone());
            let direct = !tree && !all;
            let statuses = if null {
                BTreeMap::new()
            } else {
                client.query_state(events.as_ref())?.package_statuses()
            };
            let Some(root) =
                while_resolving(show_resolution, client.uses(selector, direct)).await??
            else {
                return Err(anyhow!("registry returned nothing for '{name}'").into());
            };
            final_output = Some(CommandOutput::ReverseDeps(ReverseDepsOutput {
                command: CommandId::Uses,
                source: ReverseDepsSource::Registry,
                target: name,
                direct,
                all: all && !tree,
                root: Some(root),
                statuses,
            }));
        }
        Command::Info { names } => {
            let query = client.query_state(events.as_ref())?;
            if names.len() == 1 {
                let requested = PackageSelector(names.into_iter().next().expect("one info name"));
                let (info, installed) =
                    while_resolving(show_resolution, client.info(&query, requested.clone()))
                        .await??;
                let status = query.package_status(&info.package_key);
                final_output = Some(CommandOutput::Info(Box::new(InfoOutput {
                    package: info,
                    installed,
                    declared: status.as_ref().is_some_and(|status| status.declared),
                    deactivated: status.is_some_and(|status| status.deactivated),
                })));
            } else {
                let config = client.config().clone();
                let registry = glu_client::registry::resolve_client::HttpResolveClient::new(
                    &config.registry_base_url,
                )?;
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

                let results = while_resolving(show_resolution, async {
                    let mut results = Vec::with_capacity(handles.len());
                    for handle in handles {
                        results.push(handle.await);
                    }
                    results
                })
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
                final_output = Some(CommandOutput::InfoMany(InfoManyOutput { packages }));
            }
        }
        Command::Deactivate { names } => {
            let names = names.into_iter().map(PackageSelector).collect();
            let results = client.deactivate(names)?;
            final_output = Some(CommandOutput::Deactivation(output::deactivation_output(
                &results,
            )));
        }
        Command::Activate { force, names } => {
            let names = names.into_iter().map(PackageSelector).collect();
            let results = client.activate(names, force)?;
            final_output = Some(CommandOutput::Activation(output::activation_output(
                force, &results,
            )));
        }
        Command::Shellenv { shell } => {
            final_output = Some(CommandOutput::Shellenv(client.shellenv(shell.as_deref())?));
        }
        Command::Setup => {
            final_output = Some(CommandOutput::Setup(client.setup_shells()?));
        }
        Command::Outdated {
            declared,
            installed: _,
            all: _,
        } => {
            let query = client.query_state(events.as_ref())?;
            let mut outdated = while_resolving(show_resolution, client.outdated(&query)).await??;
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
            final_output = Some(CommandOutput::Outdated(output::outdated_output(
                scope, outdated,
            )));
        }
        Command::Update {
            all,
            dependents,
            names,
        } => {
            // Bare `glu up` / `glu up --all` are broad mutations and always
            // present their plan; a named update asks only when it would
            // remove something (a dependency the new version dropped) — the
            // same removal-triggered rule as install/reinstall.
            let is_broad = all || names.is_empty();
            let names = names.into_iter().map(PackageSelector).collect();
            let plan_result =
                while_resolving(show_resolution, client.plan_update(names, all, dependents))
                    .await??;
            if plan {
                final_output = Some(CommandOutput::UpdatePlan(output::update_plan_output(
                    &plan_result,
                    is_broad,
                )));
                return Ok((final_output, globals));
            }
            let plan = plan_result;
            if !json {
                output::render_update_preflight(&plan);
            }
            if plan.to_update.is_empty() && plan.to_remove.is_empty() {
                let summary = client.execute_update(plan, verbose, events.clone()).await?;
                final_output = Some(CommandOutput::Update(output::update_output(&summary)));
                return Ok((final_output, globals));
            }
            if !json {
                output::render_update_execution_plan(&plan, tree);
            }
            if !yes && (is_broad || !plan.to_remove.is_empty()) {
                if json {
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
                    return Err(CliFailure::Structured(Box::new(
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
                        ),
                    )));
                }
                if !confirm::confirm_update(&plan)? {
                    return Ok((None, globals));
                }
            }
            let summary = client.execute_update(plan, verbose, events.clone()).await?;
            final_output = Some(CommandOutput::Update(output::update_output(&summary)));
        }
        Command::Upgrade => {
            final_output = Some(CommandOutput::Upgrade(
                client.upgrade(events.as_ref()).await?,
            ));
        }
        Command::Status => {
            let query = client.query_state(events.as_ref())?;
            final_output = Some(CommandOutput::Status(output::status_output(
                &client, &query,
            )?));
        }
        Command::Trace(cmd) => {
            final_output = Some(trace_cmd::run_trace(&client, cmd, verbose)?);
        }
        Command::Help { command, schemas } => {
            final_output = Some(CommandOutput::Help(help::help_output(
                &command, json, schemas,
            )?));
        }
        Command::PostinstallWorker { .. } => unreachable!("handled before config setup"),
    }

    Ok((final_output, globals))
}

fn install_confirmation_failure(
    plan: &glu_client::install::InstallPlan,
    command: &'static str,
) -> CliFailure {
    let planned_removals = plan
        .would_remove
        .iter()
        .map(|package| ErrorPackageRecord {
            name: package.name.0.clone(),
            version: package.keg_version.0.clone(),
        })
        .collect();
    CliFailure::Structured(Box::new(CliError::confirmation_required(
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
    )))
}

fn validate_global_options(
    command_id: CommandId,
    spec: &CommandSpec,
    globals: &GlobalOptions,
) -> std::result::Result<(), CliFailure> {
    if globals.tree
        && globals.is_json()
        && !globals.plan
        && matches!(command_id, CommandId::Install | CommandId::Update)
    {
        return Err(CliFailure::Structured(Box::new(
            CliError::invalid_flag_combination(
                "--tree with --json is available for install/update plans; add --plan",
                "--tree",
                vec!["add --plan or remove --tree".to_string()],
                Some(CliErrorDetails::Requirement(RequirementErrorDetails {
                    requires: "--plan",
                })),
            ),
        )));
    }

    for (selected, flag) in [
        (globals.is_json(), "--json"),
        (globals.is_null(), "--null"),
        (globals.tree, "--tree"),
        (globals.plan, "--plan"),
        (globals.yes, "--yes"),
        (globals.verbose, "--verbose"),
    ] {
        if selected && !spec.supports_global_option(flag) {
            return Err(CliFailure::Structured(Box::new(
                CliError::invalid_flag_combination(
                    format!("{flag} is not supported by this command"),
                    flag,
                    Vec::new(),
                    Some(CliErrorDetails::UnsupportedOption(
                        UnsupportedOptionDetails {
                            unsupported_option: flag,
                        },
                    )),
                ),
            )));
        }
    }
    Ok(())
}

/// Returns the argv prefix that clap may interpret as options and commands.
/// Everything after `--` is positional data.
fn args_before_boundary(args: &[String]) -> &[String] {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    &args[..end]
}

/// Bootstrap protocol negotiation for parse failures. This deliberately
/// recognizes only the JSON renderer flag; command identity and behavior are
/// still owned by clap and the command model.
fn output_json_requested(args: &[String]) -> bool {
    args_before_boundary(args).iter().any(|arg| {
        arg == "--json"
            || arg.starts_with("--json=")
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.chars().skip(1).any(|flag| flag == 'j'))
    })
}

fn infer_command(args: &[String]) -> CommandInference {
    let mut words = args_before_boundary(args)
        .iter()
        .filter(|arg| !arg.starts_with('-'));
    let Some(first) = words.next() else {
        return CommandInference {
            command: None,
            command_path: Vec::new(),
            recognized: false,
        };
    };
    if first == "help" {
        return CommandInference {
            command: Some(CommandId::Help.as_str().to_string()),
            command_path: vec![CommandId::Help.as_str().to_string()],
            recognized: true,
        };
    }
    let Some(spec) = resolve_spec_segment(COMMAND_SPECS, first) else {
        return CommandInference {
            command: None,
            command_path: vec![first.to_string()],
            recognized: false,
        };
    };
    if spec.name == "trace" {
        if let Some(second) = words.next() {
            if let Some(sub) = resolve_spec_segment(spec.subcommands, second) {
                return CommandInference {
                    command: Some(sub.id.as_str().to_string()),
                    command_path: vec!["trace".to_string(), sub.name.to_string()],
                    recognized: true,
                };
            }
            return CommandInference {
                command: None,
                command_path: vec!["trace".to_string(), second.to_string()],
                recognized: false,
            };
        }
    }
    CommandInference {
        command: Some(spec.id.as_str().to_string()),
        command_path: vec![spec.name.to_string()],
        recognized: true,
    }
}

fn resolve_spec_segment<'a>(specs: &'a [CommandSpec], segment: &str) -> Option<&'a CommandSpec> {
    specs
        .iter()
        .find(|spec| spec.name == segment || spec.aliases.contains(&segment))
}

fn print_json_error(error: &CliFailure, invocation: &InvocationContext) {
    let cli_error = cli_error_for_failure(error);
    let envelope = cli_error.envelope(invocation.command.clone(), invocation.info());
    eprintln!(
        "{}",
        serde_json::to_string(&envelope).expect("serialize json error")
    );
}

fn cli_error_for_failure(error: &CliFailure) -> CliError {
    match error {
        CliFailure::Parse(error) => CliError::clap_parse_error(error),
        CliFailure::Structured(error) => CliError {
            code: error.code,
            message: error.message.clone(),
            offending_arg: error.offending_arg.clone(),
            usage: error.usage.clone(),
            suggestions: error.suggestions.clone(),
            details: error.details.clone(),
        },
        CliFailure::Runtime(error) => runtime_cli_error(error),
    }
}

fn runtime_cli_error(error: &anyhow::Error) -> CliError {
    if let Some(unknown) = error.downcast_ref::<help::UnknownHelpCommand>() {
        CliError::runtime(ErrorCode::ParseError, unknown.to_string(), Vec::new(), None)
    } else if let Some(interrupted) = error.downcast_ref::<ResolutionInterrupted>() {
        CliError::runtime(
            ErrorCode::Interrupted,
            interrupted.to_string(),
            Vec::new(),
            Some(CliErrorDetails::Operation(OperationErrorDetails {
                operation: "resolve".to_string(),
            })),
        )
    } else if let Some(interrupted) = error.downcast_ref::<glu_client::error::InterruptedError>() {
        CliError::runtime(
            ErrorCode::Interrupted,
            interrupted.to_string(),
            Vec::new(),
            Some(CliErrorDetails::Operation(OperationErrorDetails {
                operation: interrupted.operation.to_string(),
            })),
        )
    } else if let Some(partial) = error.downcast_ref::<glu_client::install::PartialInstallFailure>()
    {
        CliError::runtime(
            ErrorCode::PartialInstallFailure,
            partial.message.clone(),
            partial.report.suggested_commands.clone(),
            Some(CliErrorDetails::PartialInstall(Box::new(
                PartialInstallDetails::from_report(&partial.report),
            ))),
        )
    } else if let Some(registry) = error.downcast_ref::<glu_client::error::RegistryFailure>() {
        let details = RegistryErrorDetails {
            http_status: registry.status,
            name: registry.name.clone(),
            operation: registry.operation.clone(),
            reason: registry.reason.clone(),
            requested_by: registry.requested_by.clone(),
            target: registry.target.clone(),
        };
        CliError::runtime(
            registry.code.into(),
            registry.message.clone(),
            registry.suggestions.clone(),
            Some(CliErrorDetails::Registry(details)),
        )
    } else if let Some(transport) =
        error.downcast_ref::<glu_client::error::RegistryTransportFailure>()
    {
        CliError::runtime(
            ErrorCode::RegistryUnavailable,
            transport.to_string(),
            Vec::new(),
            Some(CliErrorDetails::Operation(OperationErrorDetails {
                operation: transport.operation.clone(),
            })),
        )
    } else if let Some(decode) = error.downcast_ref::<glu_client::error::RegistryDecodeFailure>() {
        CliError::runtime(
            ErrorCode::RegistryError,
            decode.to_string(),
            Vec::new(),
            Some(CliErrorDetails::Operation(OperationErrorDetails {
                operation: decode.operation.clone(),
            })),
        )
    } else if let Some(not_installed) = error.downcast_ref::<glu_client::error::NotInstalledError>()
    {
        CliError::runtime(
            ErrorCode::NotInstalled,
            not_installed.message.clone(),
            not_installed.suggestions.clone(),
            Some(CliErrorDetails::Packages(PackagesErrorDetails {
                packages: not_installed
                    .packages
                    .iter()
                    .map(|name| name.0.clone())
                    .collect(),
            })),
        )
    } else {
        CliError::command_failed(error)
    }
}

fn failure_exit_class(error: &CliFailure) -> ExitClass {
    match error {
        CliFailure::Parse(_) => ExitClass::Usage,
        CliFailure::Runtime(error)
            if error.downcast_ref::<help::UnknownHelpCommand>().is_some() =>
        {
            ExitClass::Usage
        }
        CliFailure::Runtime(error)
            if error.downcast_ref::<ResolutionInterrupted>().is_some()
                || error
                    .downcast_ref::<glu_client::error::InterruptedError>()
                    .is_some() =>
        {
            ExitClass::Interrupted
        }
        CliFailure::Structured(error)
            if matches!(
                error.code,
                ErrorCode::InvalidFlagCombination | ErrorCode::ParseError
            ) =>
        {
            ExitClass::Usage
        }
        CliFailure::Structured(_) | CliFailure::Runtime(_) => ExitClass::Runtime,
    }
}

fn print_cli_failure(error: &CliFailure) {
    match error {
        CliFailure::Parse(error) => {
            let _ = error.print();
        }
        CliFailure::Structured(error) => {
            eprintln!("{}", glu_client::style::red(&error.message));
            for suggestion in &error.suggestions {
                eprintln!("{} {suggestion}", glu_client::style::dim("Try:"));
            }
        }
        CliFailure::Runtime(error) => diagnostic::print_runtime_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error_envelope_valid(value: &serde_json::Value) {
        let help = crate::help::compact_help_json_value(&[]).unwrap();
        let schema = &help["schemas"]["ErrorEnvelope"];
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(schema)
            .unwrap();
        let errors: Vec<_> = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "error envelope failed schema: {errors:?}"
        );
    }

    #[test]
    fn top_level_help_is_agent_sufficient() {
        let help = help::top_help_text();
        for required in [
            "list, ls",
            "List installed packages",
            "glu list [OPTIONS]",
            "Default scope: declared",
            "--installed",
            "install, i, add",
            "without NAMES, sync declared packages",
            "Mutation: may prompt; --plan previews; --yes approves",
            "does not imply --force, --all",
            "trace summary",
        ] {
            assert!(
                help.contains(required),
                "top-level help omitted {required:?}"
            );
        }
        assert!(!help.contains("More info:"));
    }

    #[test]
    fn command_schema_uses_generated_result_identities() {
        let value = serde_json::to_value(&crate::command_model::COMMAND_SCHEMA).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert!(value.get("json_contract").is_none());
        let list = value["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == "list")
            .unwrap();
        assert_eq!(list["default_scope"], "declared");
        assert_eq!(list["result_schema"], "ListResult");
        assert_eq!(
            list["output_protocols"],
            serde_json::json!(["human", "json_envelope", "null_names"])
        );
    }

    #[test]
    fn compact_help_json_is_user_facing_invocation_manifest() {
        let value = crate::help::compact_help_json_value(&[]).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["cli"], "glu");
        assert!(value.get("format").is_none());
        assert!(value.get("json_contract").is_none());
        assert!(value.get("global_options").is_none());
        assert!(value.get("result_schemas").is_none());

        let install = &value["commands"]["install"];
        assert_eq!(install["json_result"], "InstallResult");
        assert_eq!(install["mutates"], true);
        assert_eq!(install["flags"]["--force"]["short"], "-f");
        assert_eq!(
            install["supports_flags"],
            serde_json::json!(["--json", "--tree", "--verbose", "--plan", "--yes"])
        );
        assert_eq!(
            install["output_protocols"],
            serde_json::json!(["human", "json_envelope"])
        );
        assert!(install.get("renderers").is_none());
        assert!(install.get("default_scope").is_none());

        let help = &value["commands"]["help"];
        assert_eq!(help["json_result"], "HelpManifest");
        assert_eq!(
            help["output_protocols"],
            serde_json::json!(["human", "json_envelope"])
        );

        let list = &value["commands"]["list"];
        assert_eq!(
            list["supports_flags"],
            serde_json::json!(["--json", "--null", "--tree"])
        );
        assert_eq!(
            list["flags"]["--declared"]["effect"],
            "list declared packages (the default)"
        );
        assert!(list["flags"]["--declared"].get("scope").is_none());

        assert_eq!(value["json"]["success"]["command"], "string");
        let described_error_codes: std::collections::BTreeSet<_> = value["json"]["error_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|code| code.as_str().unwrap())
            .collect();
        let schema_error_codes: std::collections::BTreeSet<_> = value["schemas"]["ErrorEnvelope"]
            ["$defs"]["ErrorCode"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|code| code.as_str().unwrap())
            .collect();
        assert_eq!(described_error_codes, schema_error_codes);
        assert_eq!(value["schemas"]["InstallResult"]["title"], "InstallResult");
        assert_eq!(
            value["schemas"]["InstallResult"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            value["schemas"]["ListResult"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(value.get("types").is_none());
    }

    #[test]
    fn generated_schema_documents_and_raw_help_validate() {
        let value = crate::help::compact_help_json_value(&[]).unwrap();
        let schemas = value["schemas"].as_object().unwrap();
        for (name, schema) in schemas {
            assert!(
                jsonschema::draft202012::meta::is_valid(schema),
                "invalid generated JSON Schema: {name}"
            );
        }
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schemas["HelpManifest"])
            .unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "raw help failed its schema: {errors:?}");
    }

    #[test]
    fn generated_schemas_distinguish_null_from_omission() {
        let value = crate::help::compact_help_json_value(&[]).unwrap();

        let info_schema = &value["schemas"]["InfoResult"];
        let info = &info_schema["$defs"]["InfoResponse"];
        assert!(info["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "desc"));
        assert!(info["properties"]["desc"]["type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variant| variant == "null"));

        let package = &value["schemas"]["InstallResult"]["$defs"]["MutationPackageRecord"];
        assert!(!package["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "linked"));
        assert_eq!(package["properties"]["linked"]["type"], "boolean");

        let error = &value["schemas"]["ErrorEnvelope"]["$defs"]["CliError"];
        assert!(!error["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "usage"));
        assert_eq!(error["properties"]["usage"]["type"], "string");
    }

    #[test]
    fn compact_command_help_json_includes_relevant_schema_context() {
        let value = crate::help::compact_help_json_value(&["install".to_string()]).unwrap();
        assert_eq!(value["command"]["json_result"], "InstallResult");
        assert!(value["schemas"].get("InstallResult").is_some());
        assert!(value["schemas"].get("ErrorEnvelope").is_some());
        assert!(value["schemas"].get("HelpManifest").is_some());
        assert_eq!(value["schemas"].as_object().unwrap().len(), 3);
        assert!(value["json"]["conventions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|convention| convention
                == "--yes only approves the computed plan; it does not imply --force, --all, or --dependents"));
    }

    #[test]
    fn every_command_id_has_one_descriptor() {
        let mut descriptors = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for id in CommandId::ALL {
            let spec = command_spec_by_id(*id)
                .unwrap_or_else(|| panic!("missing descriptor for {}", id.as_str()));
            assert_eq!(spec.id, *id);
            assert!(
                descriptors.insert(spec as *const CommandSpec as usize),
                "descriptor reused for {}",
                id.as_str()
            );
            assert!(names.insert(id.as_str()), "duplicate command ID name");
        }
        assert_eq!(
            command_spec_by_id(CommandId::Shellenv)
                .unwrap()
                .output_protocols,
            &[crate::command_model::OutputProtocol::RawShellText]
        );
        assert_eq!(
            command_spec_by_id(CommandId::PostinstallWorker)
                .unwrap()
                .output_protocols,
            &[crate::command_model::OutputProtocol::InternalWorker]
        );
    }

    #[test]
    fn descriptor_capability_matrix_rejects_unsupported_globals_before_command_work() {
        for id in CommandId::ALL {
            let spec = command_spec_by_id(*id).expect("every command has a descriptor");
            for flag in ["--json", "--null", "--tree", "--plan", "--yes", "--verbose"] {
                let mut args = crate::args::GlobalArgs::default();
                match flag {
                    "--json" => args.json = true,
                    "--null" => args.null = true,
                    "--tree" => args.tree = true,
                    "--plan" => args.plan = true,
                    "--yes" => args.yes = true,
                    "--verbose" => args.verbose = true,
                    _ => unreachable!(),
                }
                let globals = GlobalOptions::from_args(args);
                let valid = validate_global_options(*id, spec, &globals).is_ok();
                assert_eq!(
                    valid,
                    spec.supports_global_option(flag),
                    "capability drift for {} {flag}",
                    id.as_str()
                );
            }
        }
    }

    #[test]
    fn tree_json_execution_requires_plan_before_command_work() {
        for id in [CommandId::Install, CommandId::Update] {
            let spec = command_spec_by_id(id).unwrap();
            let mut args = crate::args::GlobalArgs {
                json: true,
                tree: true,
                ..Default::default()
            };
            let globals = GlobalOptions::from_args(args);
            assert!(validate_global_options(id, spec, &globals).is_err());

            args.plan = true;
            let globals = GlobalOptions::from_args(args);
            assert!(validate_global_options(id, spec, &globals).is_ok());
        }
    }

    #[test]
    fn command_schema_matches_visible_clap_surface() {
        use clap::CommandFactory;

        let clap = Cli::command();
        for (long, short) in [
            ("--json", 'j'),
            ("--null", '0'),
            ("--tree", 't'),
            ("--verbose", 'v'),
            ("--plan", 'p'),
            ("--yes", 'y'),
        ] {
            assert_clap_has_option(&clap, "glu", long, Some(short));
        }
        assert_command_specs_match_clap(crate::command_model::COMMAND_SPECS, &clap, None);
    }

    fn assert_command_specs_match_clap(
        specs: &[crate::command_model::CommandSpec],
        clap_parent: &clap::Command,
        parent_id: Option<&str>,
    ) {
        use std::collections::BTreeSet;

        let clap_visible_names: BTreeSet<&str> = clap_parent
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
            .map(|command| command.get_name())
            .collect();
        let spec_names: BTreeSet<&str> = specs.iter().map(|spec| spec.name).collect();
        assert_eq!(
            clap_visible_names,
            spec_names,
            "COMMAND_SPECS must mirror clap's visible command surface under {}",
            clap_parent.get_name()
        );

        for spec in specs {
            let expected_id = parent_id
                .map(|parent| format!("{parent} {}", spec.name))
                .unwrap_or_else(|| spec.name.to_string());
            assert_eq!(
                spec.id.as_str(),
                expected_id,
                "typed command identity drift for {}",
                spec.name
            );
            let clap = clap_parent
                .get_subcommands()
                .find(|command| command.get_name() == spec.name)
                .unwrap_or_else(|| panic!("missing clap command for spec {}", spec.name));
            let clap_aliases: Vec<&str> = clap.get_visible_aliases().collect();
            assert_eq!(
                clap_aliases, spec.aliases,
                "alias drift for command {}",
                spec.name
            );
            let clap_positionals: BTreeSet<String> = clap
                .get_positionals()
                .map(|argument| {
                    argument
                        .get_value_names()
                        .and_then(|names| names.first())
                        .map(ToString::to_string)
                        .unwrap_or_else(|| argument.get_id().as_str().to_uppercase())
                })
                .collect();
            let descriptor_arguments: BTreeSet<String> = spec
                .arguments
                .iter()
                .map(|argument| argument.name.to_string())
                .collect();
            assert_eq!(
                clap_positionals, descriptor_arguments,
                "positional argument drift for command {}",
                spec.name
            );
            for option in spec.options {
                if !matches!(
                    option.long,
                    "--json" | "--null" | "--tree" | "--verbose" | "--plan" | "--yes"
                ) {
                    assert_clap_has_option(clap, spec.name, option.long, option.short);
                }
            }
            if spec
                .output_protocols
                .contains(&crate::command_model::OutputProtocol::JsonEnvelope)
            {
                assert!(
                    spec.result_schema.is_some(),
                    "JSON-capable command {} must document result_schema",
                    spec.name
                );
            }
            assert_command_specs_match_clap(spec.subcommands, clap, Some(&expected_id));
        }
    }

    fn assert_clap_has_option(
        clap: &clap::Command,
        command_name: &str,
        long: &str,
        short: Option<char>,
    ) {
        let expected_long = long.trim_start_matches("--");
        let arg = clap
            .get_arguments()
            .find(|arg| arg.get_long() == Some(expected_long))
            .unwrap_or_else(|| {
                panic!("spec option {long} missing from clap command {command_name}")
            });
        assert_eq!(
            arg.get_short(),
            short,
            "short option drift for {command_name} {long}"
        );
    }

    #[test]
    fn invocation_context_keeps_unknown_command_out_of_command_field() {
        let args = vec!["wat".to_string(), "-j".to_string()];
        let invocation = InvocationContext::from_args(&args);
        assert!(invocation.json_requested);
        assert_eq!(invocation.command, None);
        assert_eq!(invocation.command_path, vec!["wat"]);
        assert!(!invocation.recognized);
    }

    #[test]
    fn invocation_bootstrap_stops_at_the_argument_boundary() {
        let args = vec![
            "info".to_string(),
            "--".to_string(),
            "wat".to_string(),
            "--json".to_string(),
        ];
        let invocation = InvocationContext::from_args(&args);
        assert!(!invocation.json_requested);
        assert_eq!(invocation.command.as_deref(), Some("info"));
        assert_eq!(invocation.command_path, vec!["info"]);
        assert!(invocation.recognized);
        assert_eq!(invocation.argv, args);

        let positional_only = vec!["--".to_string(), "wat".to_string(), "-j".to_string()];
        let invocation = InvocationContext::from_args(&positional_only);
        assert!(!invocation.json_requested);
        assert_eq!(invocation.command, None);
        assert!(invocation.command_path.is_empty());
        assert!(!invocation.recognized);
        assert_eq!(invocation.argv, positional_only);
    }

    #[test]
    fn invocation_bootstrap_recognizes_json_before_the_boundary() {
        for flag in ["--json", "--json=true", "-j", "-vj"] {
            let args = vec![flag.to_string(), "wat".to_string()];
            let invocation = InvocationContext::from_args(&args);
            assert!(invocation.json_requested, "did not recognize {flag}");
            assert_eq!(invocation.argv, args);
        }
    }

    #[test]
    fn json_error_envelope_has_stable_shape() {
        let error = anyhow::anyhow!("package not found: nope");
        let value = serde_json::to_value(
            crate::command_model::CliError::command_failed(&error)
                .envelope(Some("list".to_string()), InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["command"], "list");
        assert_eq!(value["error"]["code"], "command_failed");
        assert_eq!(value["error"]["message"], "package not found: nope");
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn json_runtime_errors_use_typed_codes_when_available() {
        let registry = anyhow::Error::new(glu_client::error::RegistryFailure {
            code: glu_client::error::RuntimeErrorCode::PackageNotFound,
            message: "package 'nope' not found".to_string(),
            name: Some("nope".to_string()),
            target: Some("test-target".to_string()),
            reason: None,
            requested_by: None,
            suggestions: vec!["node".to_string()],
            operation: "resolve".to_string(),
            status: Some(404),
        });
        let value = serde_json::to_value(
            runtime_cli_error(&registry).envelope(None, InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "package_not_found");
        assert_eq!(value["error"]["details"]["name"], "nope");
        assert_eq!(value["error"]["details"]["http_status"], 404);
        assert_eq!(value["error"]["suggestions"], serde_json::json!(["node"]));
        assert_error_envelope_valid(&value);

        let not_installed = anyhow::Error::new(glu_client::error::NotInstalledError::packages(
            vec![PackageName("ripgrep".to_string())],
            Some("glu install ripgrep".to_string()),
        ));
        let value = serde_json::to_value(
            runtime_cli_error(&not_installed).envelope(None, InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "not_installed");
        assert_eq!(
            value["error"]["details"]["packages"],
            serde_json::json!(["ripgrep"])
        );
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn typed_interruption_maps_to_json_and_exit_130() {
        let interrupted = anyhow::Error::new(glu_client::error::InterruptedError {
            operation: "install",
            message: "interrupted (Ctrl+C)",
            trace_path: std::path::PathBuf::from("/tmp/trace.json"),
        });
        let failure = CliFailure::Runtime(interrupted);

        assert_eq!(failure_exit_class(&failure), ExitClass::Interrupted);
        assert_eq!(failure_exit_class(&failure).code(), 130);
        let value = serde_json::to_value(
            cli_error_for_failure(&failure).envelope(None, InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "interrupted");
        assert_eq!(value["error"]["details"]["operation"], "install");
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn resolution_interruption_maps_to_exit_130_without_a_trace() {
        let failure = CliFailure::Runtime(anyhow::Error::new(ResolutionInterrupted));

        assert_eq!(failure_exit_class(&failure), ExitClass::Interrupted);
        let value = serde_json::to_value(
            cli_error_for_failure(&failure).envelope(None, InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "interrupted");
        assert_eq!(value["error"]["details"]["operation"], "resolve");
        assert!(!value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Trace:"));
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn generated_error_schema_accepts_every_detail_variant() {
        let package = || crate::command_model::ErrorPackageRecord {
            name: "demo".to_string(),
            version: "1.0".to_string(),
        };
        let details = vec![
            CliErrorDetails::Requirement(RequirementErrorDetails {
                requires: "--force",
            }),
            CliErrorDetails::UnsupportedOption(crate::command_model::UnsupportedOptionDetails {
                unsupported_option: "--json",
            }),
            CliErrorDetails::Declaration(crate::command_model::DeclarationErrorDetails {
                declaration_file: "glu.json",
            }),
            CliErrorDetails::Parse(crate::command_model::ParseErrorDetails {
                clap_error_kind: "UnknownArgument".to_string(),
            }),
            CliErrorDetails::PlannedRemovals(crate::command_model::PlannedRemovalsDetails {
                planned_removals: vec![package()],
            }),
            CliErrorDetails::CleanupConfirmation(
                crate::command_model::CleanupConfirmationDetails {
                    planned_removals: vec![crate::command_model::CleanupConfirmationRecord {
                        name: "demo".to_string(),
                        version: "1.0".to_string(),
                        downloads: 1,
                        bytes: 42,
                    }],
                    planned_downloads: 2,
                    unassociated_downloads: 1,
                    unassociated_bytes: 10,
                    reclaimable_bytes: 52,
                },
            ),
            CliErrorDetails::RemovalConfirmation(
                crate::command_model::RemovalConfirmationDetails {
                    named: vec![package()],
                    planned_removals: vec![package()],
                },
            ),
            CliErrorDetails::UpdateConfirmation(crate::command_model::UpdateConfirmationDetails {
                broad: true,
                planned_removals: vec![package()],
                planned_updates: vec![crate::command_model::ErrorUpdateRecord {
                    current: "1.0".to_string(),
                    latest: "2.0".to_string(),
                    name: "demo".to_string(),
                }],
            }),
            CliErrorDetails::InstallConfirmation(
                crate::command_model::InstallConfirmationDetails {
                    command: "install".to_string(),
                    planned_removals: vec![package()],
                },
            ),
            CliErrorDetails::Registry(crate::command_model::RegistryErrorDetails {
                http_status: Some(503),
                name: Some("demo".to_string()),
                operation: "resolve".to_string(),
                reason: Some("unavailable".to_string()),
                requested_by: Some("root".to_string()),
                target: Some("test-target".to_string()),
            }),
            CliErrorDetails::Operation(crate::command_model::OperationErrorDetails {
                operation: "download".to_string(),
            }),
            CliErrorDetails::Packages(crate::command_model::PackagesErrorDetails {
                packages: vec!["demo".to_string()],
            }),
            CliErrorDetails::PartialInstall(Box::new(
                crate::command_model::PartialInstallDetails {
                    failed: Vec::new(),
                    failed_error: None,
                    failed_kind: None,
                    failed_node_id: None,
                    failed_phase: None,
                    failure_code: ErrorCode::LinkFailed,
                    installed: Vec::new(),
                    partial: Vec::new(),
                    skipped: Vec::new(),
                    suggested_commands: Vec::new(),
                    trace_id: None,
                    trace_path: std::path::PathBuf::from("/tmp/trace.json"),
                },
            )),
        ];

        for details in details {
            let value = serde_json::to_value(
                crate::command_model::CliError::runtime(
                    ErrorCode::CommandFailed,
                    "test error",
                    Vec::new(),
                    Some(details),
                )
                .envelope(None, InvocationInfo::default()),
            )
            .unwrap();
            assert_error_envelope_valid(&value);
        }
    }

    #[test]
    fn partial_install_json_error_includes_structured_state() {
        let partial = anyhow::Error::new(glu_client::install::PartialInstallFailure {
            message: "Install failed at app during keg_link".to_string(),
            report: glu_client::install::PartialInstallReport {
                failure_code: glu_client::error::RuntimeErrorCode::LinkFailed,
                failed_node_id: Some("keg_link:app".to_string()),
                failed_phase: Some("keg_link".to_string()),
                failed_kind: Some("keg_link".to_string()),
                failed_error: Some("boom".to_string()),
                installed: vec![glu_client::install::PartialInstallPackage {
                    name: "dep".to_string(),
                    version: "1.0".to_string(),
                    package_id: "pkg:test/dep@1.0".to_string(),
                    status: glu_client::install::PartialInstallStatus::Installed,
                }],
                failed: vec![glu_client::install::PartialInstallPackage {
                    name: "app".to_string(),
                    version: "1.0".to_string(),
                    package_id: "pkg:test/app@1.0".to_string(),
                    status: glu_client::install::PartialInstallStatus::Failed,
                }],
                skipped: Vec::new(),
                partial: Vec::new(),
                trace_path: std::path::PathBuf::from("/tmp/trace.json"),
                trace_id: Some("abc123".to_string()),
                suggested_commands: vec!["glu trace view abc123".to_string()],
            },
        });
        let value = serde_json::to_value(
            runtime_cli_error(&partial).envelope(None, InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "partial_install_failure");
        assert_eq!(value["error"]["details"]["failure_code"], "link_failed");
        assert_eq!(value["error"]["details"]["installed"][0]["name"], "dep");
        assert_eq!(value["error"]["details"]["failed"][0]["name"], "app");
        assert_eq!(value["error"]["details"]["trace_id"], "abc123");
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn json_invalid_flag_combination_has_stable_code() {
        let value = serde_json::to_value(
            crate::command_model::CliError::invalid_flag_combination(
                "--deps requires --force",
                "--deps",
                vec!["glu install --force --deps <name>".to_string()],
                Some(CliErrorDetails::Requirement(RequirementErrorDetails {
                    requires: "--force",
                })),
            )
            .envelope(Some("install".to_string()), InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "invalid_flag_combination");
        assert_eq!(value["error"]["offending_arg"], "--deps");
        assert_eq!(value["error"]["details"]["requires"], "--force");
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn json_empty_declaration_has_stable_code() {
        let value = serde_json::to_value(
            crate::command_model::CliError::empty_declaration(
                "nothing declared in glu.json",
                vec!["glu add <name>".to_string()],
            )
            .envelope(Some("install".to_string()), InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["error"]["code"], "empty_declaration");
        assert_eq!(value["error"]["details"]["declaration_file"], "glu.json");
        assert_error_envelope_valid(&value);
    }

    #[test]
    fn json_parse_error_envelope_has_parse_code() {
        let value = serde_json::to_value(
            crate::command_model::CliError::parse_error(
                "error: unexpected argument '--nope' found\n\nUsage: glu ls\n".to_string(),
            )
            .envelope(Some("list".to_string()), InvocationInfo::default()),
        )
        .unwrap();
        assert_eq!(value["command"], "list");
        assert_eq!(value["error"]["code"], "parse_error");
        assert_error_envelope_valid(&value);
    }
}
