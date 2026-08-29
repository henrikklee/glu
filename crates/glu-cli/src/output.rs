use crate::command_model::{
    generated_schema, ActivationOutput, AutoremoveOutput, AutoremovePlanOutput,
    CachedDownloadRecord, CleanupOutput, CleanupPlanOutput, CommandId, CommandOutput,
    DeactivationOutput, DepsOutput, DepsSource, ExecutedMode, ExecutionSummaryRecord,
    GlobalOptions, InfoManyOutput, InfoOutput, InstallOutput, InstallPlanOutput,
    InstallPoolStatsRecord, InstallStatsRecord, JsonSuccessEnvelope, KeptPackageRecord, ListOutput,
    ListScope, ListView, MutationPackageRecord, MutationStatus, OutdatedOutput, OutdatedRecord,
    OutputFormat, PlanMode, ReinstallOutput, ReinstallPlanOutput, RemovalOutput, RemovalPlanOutput,
    RenamePackageRecord, ReverseDepsOutput, ReverseDepsSource, StatusOutput, StatusShell,
    TimingBreakdownRecord, UpdateOutput, UpdatePackageRecord, UpdatePlanOutput,
};
use crate::package_list::{self, PackageListItem};
use crate::tables;
use anyhow::Result;
use glu_client::activation::{
    ActivationResult, ActivationStatus, DeactivationResult, DeactivationStatus,
};
use glu_client::dependency_query::DependencyTreeNode;
use glu_client::download::cache::{CacheCleanupPlan, CacheCleanupResult, CachedBottle};
use glu_client::remove::{KeptDeclaredPackage, RemovedPackage};
use glu_client::tree_render::{
    format_minimum_version, render_dependency_tree, render_dependency_tree_with_context, RootStyle,
    TreeRenderOptions,
};
use glu_client::{shell::SetupResult, GluClient, LocalQuery};
#[cfg(test)]
use glu_core::PackageName;
use glu_core::{InfoResponse, InstalledPackage};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

pub(crate) fn render_command_output(output: &CommandOutput, globals: &GlobalOptions) {
    match globals.output {
        OutputFormat::Human => render_human(output, globals),
        OutputFormat::Json => render_json(output, globals),
        OutputFormat::Null => render_null(output, globals),
    }
}

fn render_human(output: &CommandOutput, globals: &GlobalOptions) {
    match output {
        CommandOutput::List(list) => render_list_output(list, globals),
        CommandOutput::Deps(deps) => render_deps_output(deps, globals),
        CommandOutput::ReverseDeps(reverse) => render_reverse_deps_output(reverse, globals),
        CommandOutput::Info(info) => render_info_output(info, globals),
        CommandOutput::InfoMany(info) => render_info_many_output(info, globals),
        CommandOutput::Install(install) => render_install_output(install, globals),
        CommandOutput::InstallPlan(plan) => render_install_plan_output(plan, globals),
        CommandOutput::Reinstall(reinstall) => render_reinstall_output(reinstall, globals),
        CommandOutput::ReinstallPlan(plan) => render_reinstall_plan_output(plan, globals),
        CommandOutput::Update(update) => render_update_output(update, globals),
        CommandOutput::UpdatePlan(plan) => render_update_plan_output(plan, globals),
        CommandOutput::Activation(activation) => render_activation_output(activation, globals),
        CommandOutput::Deactivation(deactivation) => {
            render_deactivation_output(deactivation, globals)
        }
        CommandOutput::Removal(removal) => render_removal_output(removal, globals),
        CommandOutput::RemovalPlan(plan) => render_removal_plan_output(plan, globals),
        CommandOutput::Autoremove(autoremove) => render_autoremove_output(autoremove, globals),
        CommandOutput::AutoremovePlan(plan) => render_autoremove_plan_output(plan, globals),
        CommandOutput::Cleanup(cleanup) => render_cleanup_output(cleanup, globals),
        CommandOutput::CleanupPlan(plan) => render_cleanup_plan_output(plan, globals),
        CommandOutput::Status(status) => render_status_output(status, globals),
        CommandOutput::Outdated(outdated) => render_outdated_output(outdated, globals),
        CommandOutput::TraceView(view) => render_trace_view_output(view),
        CommandOutput::TraceList(list) => crate::trace_cmd::render_trace_list_human(list),
        CommandOutput::TraceSummary(summary) => {
            crate::trace_cmd::render_trace_summary_human(summary)
        }
        CommandOutput::Setup(setup) => render_setup_output(setup),
        CommandOutput::Shellenv(text) => print!("{text}"),
        CommandOutput::Help(help) => render_help_output(help),
        CommandOutput::Upgrade(upgrade) => render_upgrade_output(upgrade),
    }
}

fn render_json(output: &CommandOutput, globals: &GlobalOptions) {
    match output {
        CommandOutput::List(list) => render_list_output(list, globals),
        CommandOutput::Deps(deps) => render_deps_output(deps, globals),
        CommandOutput::ReverseDeps(reverse) => render_reverse_deps_output(reverse, globals),
        CommandOutput::Info(info) => render_info_output(info, globals),
        CommandOutput::InfoMany(info) => render_info_many_output(info, globals),
        CommandOutput::Install(install) => render_install_output(install, globals),
        CommandOutput::InstallPlan(plan) => render_install_plan_output(plan, globals),
        CommandOutput::Reinstall(reinstall) => render_reinstall_output(reinstall, globals),
        CommandOutput::ReinstallPlan(plan) => render_reinstall_plan_output(plan, globals),
        CommandOutput::Update(update) => render_update_output(update, globals),
        CommandOutput::UpdatePlan(plan) => render_update_plan_output(plan, globals),
        CommandOutput::Activation(activation) => render_activation_output(activation, globals),
        CommandOutput::Deactivation(deactivation) => {
            render_deactivation_output(deactivation, globals)
        }
        CommandOutput::Removal(removal) => render_removal_output(removal, globals),
        CommandOutput::RemovalPlan(plan) => render_removal_plan_output(plan, globals),
        CommandOutput::Autoremove(autoremove) => render_autoremove_output(autoremove, globals),
        CommandOutput::AutoremovePlan(plan) => render_autoremove_plan_output(plan, globals),
        CommandOutput::Cleanup(cleanup) => render_cleanup_output(cleanup, globals),
        CommandOutput::CleanupPlan(plan) => render_cleanup_plan_output(plan, globals),
        CommandOutput::Status(status) => render_status_output(status, globals),
        CommandOutput::Outdated(outdated) => render_outdated_output(outdated, globals),
        CommandOutput::TraceList(list) => print_json_success(CommandId::TraceList, list),
        CommandOutput::TraceSummary(summary) => {
            print_json_success(CommandId::TraceSummary, summary)
        }
        CommandOutput::Help(help) => render_help_output(help),
        CommandOutput::TraceView(_)
        | CommandOutput::Setup(_)
        | CommandOutput::Shellenv(_)
        | CommandOutput::Upgrade(_) => unreachable!("output protocol validated before execution"),
    }
}

fn render_null(output: &CommandOutput, globals: &GlobalOptions) {
    match output {
        CommandOutput::List(list) => render_list_output(list, globals),
        CommandOutput::Deps(deps) => render_deps_output(deps, globals),
        CommandOutput::ReverseDeps(reverse) => render_reverse_deps_output(reverse, globals),
        CommandOutput::Info(_)
        | CommandOutput::InfoMany(_)
        | CommandOutput::Install(_)
        | CommandOutput::InstallPlan(_)
        | CommandOutput::Reinstall(_)
        | CommandOutput::ReinstallPlan(_)
        | CommandOutput::Update(_)
        | CommandOutput::UpdatePlan(_)
        | CommandOutput::Activation(_)
        | CommandOutput::Deactivation(_)
        | CommandOutput::Removal(_)
        | CommandOutput::RemovalPlan(_)
        | CommandOutput::Autoremove(_)
        | CommandOutput::AutoremovePlan(_)
        | CommandOutput::Cleanup(_)
        | CommandOutput::CleanupPlan(_)
        | CommandOutput::Status(_)
        | CommandOutput::Outdated(_)
        | CommandOutput::TraceView(_)
        | CommandOutput::TraceList(_)
        | CommandOutput::TraceSummary(_)
        | CommandOutput::Setup(_)
        | CommandOutput::Shellenv(_)
        | CommandOutput::Help(_)
        | CommandOutput::Upgrade(_) => unreachable!("output protocol validated before execution"),
    }
}

pub(crate) fn render_help_output(help: &crate::help::HelpOutput) {
    match help {
        crate::help::HelpOutput::Human(text) => print!("{text}"),
        crate::help::HelpOutput::Manifest(manifest) => {
            print_json_success(CommandId::Help, manifest.as_ref())
        }
    }
}

fn render_trace_view_output(view: &crate::trace_cmd::TraceViewOutput) {
    println!("{}", view.path.display());
    match &view.open {
        crate::trace_cmd::TraceOpenResult::Failed(error) => eprintln!(
            "{}: could not open browser: {error}",
            glu_client::style::dim("glu")
        ),
        crate::trace_cmd::TraceOpenResult::Opened => {}
        crate::trace_cmd::TraceOpenResult::Exited(code) => {
            debug_assert_ne!(*code, Some(0));
        }
    }
}

pub(crate) fn print_json_success<T: serde::Serialize + ?Sized>(command: CommandId, result: &T) {
    let envelope = JsonSuccessEnvelope {
        ok: true,
        command,
        result,
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("serialize json success")
    );
}

#[derive(Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum FlatView {
    Flat,
}

#[derive(Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TreeView {
    Tree,
}

pub(crate) fn result_schema(name: &str) -> Option<serde_json::Value> {
    match name {
        "InstallResult" => Some(generated_schema::<InstallResult<'static>>()),
        "ReinstallResult" => Some(generated_schema::<ReinstallResult<'static>>()),
        "UpdateResult" => Some(generated_schema::<UpdateResult<'static>>()),
        "ListResult" => Some(generated_schema::<ListResult>()),
        "DepsResult" => Some(generated_schema::<DepsResult>()),
        "ReverseDepsResult" => Some(generated_schema::<ReverseDepsResult<'static>>()),
        "InfoResult" => Some(generated_schema::<InfoResult<'static>>()),
        "StatusResult" => Some(generated_schema::<StatusOutput>()),
        "OutdatedResult" => Some(generated_schema::<OutdatedOutput>()),
        "ActivationResult" => Some(generated_schema::<ActivationOutput>()),
        "DeactivationResult" => Some(generated_schema::<DeactivationOutput>()),
        "AutoremoveResult" => Some(generated_schema::<AutoremoveResult<'static>>()),
        "CleanupResult" => Some(generated_schema::<CleanupResult<'static>>()),
        "RemovalResult" => Some(generated_schema::<RemovalResult<'static>>()),
        _ => None,
    }
}

fn render_list_output(list: &ListOutput, globals: &GlobalOptions) {
    match &list.view {
        ListView::Tree(tree) if globals.is_json() => {
            print_list_tree_json(list.scope, tree, &list.statuses)
        }
        ListView::Tree(tree) if globals.is_null() => {
            let mut items = Vec::new();
            flatten_tree_unique(tree, false, &mut items);
            for (name, _) in items {
                print!("{name}\0");
            }
        }
        ListView::Tree(tree) => print_list_tree(tree),
        ListView::Flat(packages) if globals.is_json() => {
            print_list_json(list.scope, packages, &list.statuses)
        }
        ListView::Flat(packages) if globals.is_null() => {
            for package in packages {
                print!("{}\0", package.name.0);
            }
        }
        ListView::Flat(packages) => {
            print_list(packages, &list.statuses);
            if list.scope == ListScope::Declared
                && list.hidden_dependencies > 0
                && list.show_dependency_hint
                && std::io::stdout().is_terminal()
            {
                println!();
                println!(
                    "{}{}",
                    glu_client::style::dim("View all dependencies with "),
                    glu_client::style::bold_dim("glu ls --all")
                );
            }
        }
    }
}

pub(crate) fn status_output(client: &GluClient, query: &LocalQuery) -> Result<StatusOutput> {
    use glu_client::config::PrefixSource;

    let config = client.config();
    let source = glu_client::config::host_prefix_source();
    let prefix_source = match source {
        PrefixSource::Env => "env (GLU_PREFIX)",
        PrefixSource::SelfLocated => "self-located",
        PrefixSource::Default => "default",
    }
    .to_string();

    let prefix = &config.prefix.0;
    let deactivated = query.deactivated_names();
    let deactivated_count = deactivated.len();
    let deactivated = deactivated.into_iter().map(|name| name.0).collect();
    let declared_count = query.declared_names().len();
    let shells = client
        .shell_statuses()?
        .into_iter()
        .map(|shell| StatusShell {
            name: shell.name,
            config_path: display_path(&shell.config_path),
            configured: shell.configured,
        })
        .collect();

    Ok(StatusOutput {
        version: env!("CARGO_PKG_VERSION"),
        prefix: display_path(prefix),
        prefix_source,
        prefix_length: prefix.as_os_str().len(),
        fixed_cellar_length: "/opt/homebrew".len(),
        target: config.target.0.clone(),
        registry: config.registry_base_url.clone(),
        distribution: config.distribution_base_url.clone(),
        installed_count: query.total_kegs(),
        declared_count,
        deactivated_count,
        deactivated,
        shells,
    })
}

pub(crate) fn deps_output(
    view: glu_client::deps::DepsView,
    direct: bool,
    status: bool,
) -> DepsOutput {
    let source = match view.source {
        glu_client::deps::DepsSource::Installed => DepsSource::Installed,
        glu_client::deps::DepsSource::Resolved => DepsSource::Resolved,
    };
    DepsOutput {
        source,
        installed: view.installed,
        direct,
        status,
        root: view.root,
        statuses: view.statuses,
    }
}

fn render_deps_output(deps: &DepsOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        let result = if globals.tree {
            let root = if deps.direct {
                direct_only_root(&deps.root)
            } else {
                deps.root.clone()
            };
            DepsResult::Tree(DepsTreeResult {
                source: deps.source,
                installed: deps.installed,
                direct: deps.direct,
                view: TreeView::Tree,
                graph: dependency_graph_json(
                    std::slice::from_ref(&root),
                    deps.direct,
                    &deps.statuses,
                ),
            })
        } else {
            DepsResult::Flat(DepsFlatResult {
                dependencies: dependency_records(&deps.root.children, deps.direct, &deps.statuses),
                direct: deps.direct,
                installed: deps.installed,
                source: deps.source,
                tree: false,
            })
        };
        print_json_success(CommandId::Deps, &result);
        return;
    }

    if globals.is_null() {
        let mut items = Vec::new();
        flatten_tree_unique(&deps.root.children, deps.direct, &mut items);
        for (name, _) in items {
            print!("{name}\0");
        }
        return;
    }

    if globals.tree {
        print_deps_tree(&deps.root, &deps.statuses, deps.direct, globals.verbose);
        if deps.status {
            print_dependency_status(
                &dependency_records(&deps.root.children, deps.direct, &deps.statuses),
                &deps.statuses,
                globals.verbose,
            );
        }
    } else {
        let records = dependency_records(&deps.root.children, deps.direct, &deps.statuses);
        print_deps_flat(&records, &deps.statuses, globals.verbose, deps.status);
    }
    if deps.source == DepsSource::Resolved {
        let note = if deps.installed {
            "resolved from the registry — what a fresh install would pull"
        } else {
            "resolved from the registry — not installed"
        };
        println!("{}", glu_client::style::dim(note));
    }
}

fn render_reverse_deps_output(reverse: &ReverseDepsOutput, globals: &GlobalOptions) {
    let is_why = reverse.command == CommandId::Why;
    if globals.is_json() {
        let result = if globals.tree {
            let (roots, initial_depth) = match &reverse.root {
                Some(root) => (root.children.clone(), 1),
                None => (Vec::new(), 0),
            };
            ReverseDepsResult::Tree(ReverseDepsTreeResult {
                target: &reverse.target,
                source: reverse.source,
                direct: reverse.direct,
                view: TreeView::Tree,
                graph: dependency_graph_json_at_depth(
                    &roots,
                    reverse.direct,
                    initial_depth,
                    &reverse.statuses,
                ),
            })
        } else {
            let dependents = reverse
                .root
                .as_ref()
                .map(|root| {
                    if is_why {
                        why_root_cause_records(root, &reverse.statuses)
                    } else {
                        dependency_records(&root.children, reverse.direct, &reverse.statuses)
                    }
                })
                .unwrap_or_default();
            ReverseDepsResult::Flat(ReverseDepsFlatResult {
                direct: reverse.direct,
                dependents,
                source: reverse.source,
                target: &reverse.target,
                tree: false,
            })
        };
        print_json_success(reverse.command, &result);
        return;
    }

    if globals.is_null() {
        if let Some(root) = &reverse.root {
            if is_why {
                for record in why_root_cause_records(root, &reverse.statuses) {
                    print!("{}\0", record.name);
                }
            } else {
                let mut items = Vec::new();
                flatten_tree_unique(&root.children, reverse.direct, &mut items);
                for (name, _) in items {
                    print!("{name}\0");
                }
            }
        }
        return;
    }

    let Some(root) = &reverse.root else {
        println!("Nothing depends on it.");
        return;
    };
    if root.children.is_empty() {
        println!("Nothing depends on it.");
    } else if globals.tree {
        print_tree_roots(&root.children, reverse.direct, globals.verbose);
    } else {
        let items = if is_why {
            why_root_cause_records(root, &reverse.statuses)
                .into_iter()
                .map(|record| (record.name, record.version))
                .collect()
        } else {
            let mut items = Vec::new();
            flatten_tree_unique(&root.children, reverse.direct, &mut items);
            items
        };
        print_flat(&items);
    }
}

fn why_root_cause_records(
    root: &DependencyTreeNode,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) -> Vec<DependencyRecord> {
    let mut records = Vec::new();
    let mut seen = BTreeMap::new();
    collect_why_nodes(&root.children, 1, statuses, &mut seen, &mut records, true);
    if records.is_empty() {
        collect_why_nodes(&root.children, 1, statuses, &mut seen, &mut records, false);
    }
    records
}

fn collect_why_nodes(
    nodes: &[DependencyTreeNode],
    depth: usize,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    seen: &mut BTreeMap<glu_core::PackageKey, usize>,
    records: &mut Vec<DependencyRecord>,
    declared_only: bool,
) {
    for node in nodes {
        let status = statuses.get(&node.package_key);
        let selected = if declared_only {
            status.is_some_and(|status| status.declared)
        } else {
            node.children.is_empty() && !node.already_shown
        };
        if selected {
            if let Some(index) = seen.get(&node.package_key).copied() {
                if depth == 1 {
                    records[index].direct = true;
                    records[index].transitive = false;
                }
            } else {
                seen.insert(node.package_key.clone(), records.len());
                records.push(dependency_record(node, status, depth));
            }
        }
        if !node.already_shown {
            collect_why_nodes(
                &node.children,
                depth + 1,
                statuses,
                seen,
                records,
                declared_only,
            );
        }
    }
}

fn direct_only_root(root: &DependencyTreeNode) -> DependencyTreeNode {
    let mut root = root.clone();
    for child in &mut root.children {
        child.children.clear();
    }
    root
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum DepsResult {
    Flat(DepsFlatResult),
    Tree(DepsTreeResult),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DepsFlatResult {
    dependencies: Vec<DependencyRecord>,
    direct: bool,
    installed: bool,
    source: DepsSource,
    tree: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ReverseDepsResult<'a> {
    Flat(ReverseDepsFlatResult<'a>),
    Tree(ReverseDepsTreeResult<'a>),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ReverseDepsFlatResult<'a> {
    direct: bool,
    dependents: Vec<DependencyRecord>,
    source: ReverseDepsSource,
    target: &'a str,
    tree: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DependencyRecord {
    package_key: String,
    package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    requested_as: Option<String>,
    reversed: bool,
    name: String,
    version: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    linked: Option<bool>,
    declared: bool,
    deactivated: bool,
    direct: bool,
    transitive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    download_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    installed_bytes: Option<u64>,
}

fn dependency_records(
    nodes: &[DependencyTreeNode],
    direct_only: bool,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) -> Vec<DependencyRecord> {
    let mut seen = BTreeMap::new();
    let mut records = Vec::new();
    collect_dependency_records(nodes, direct_only, statuses, 1, &mut seen, &mut records);
    records
}

fn collect_dependency_records(
    nodes: &[DependencyTreeNode],
    direct_only: bool,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    depth: usize,
    seen: &mut BTreeMap<glu_core::PackageKey, usize>,
    records: &mut Vec<DependencyRecord>,
) {
    for node in nodes {
        if let Some(index) = seen.get(&node.package_key).copied() {
            if depth == 1 {
                records[index].direct = true;
                records[index].transitive = false;
            }
        } else {
            seen.insert(node.package_key.clone(), records.len());
            records.push(dependency_record(
                node,
                statuses.get(&node.package_key),
                depth,
            ));
        }
        if !(direct_only && depth >= 1) && !node.already_shown {
            collect_dependency_records(
                &node.children,
                direct_only,
                statuses,
                depth + 1,
                seen,
                records,
            );
        }
    }
}

fn dependency_record(
    node: &DependencyTreeNode,
    status: Option<&glu_client::deps::PackageStatus>,
    depth: usize,
) -> DependencyRecord {
    DependencyRecord {
        package_key: node.package_key.0.clone(),
        package: node.package.0.clone(),
        requested_as: node
            .incoming
            .as_ref()
            .map(|edge| edge.requested_as.0.clone()),
        reversed: node.incoming.as_ref().is_some_and(|edge| edge.reversed),
        name: node.name.clone(),
        version: node.version.clone(),
        installed: status.is_some_and(|status| status.installed),
        linked: status.map(|status| status.linked),
        declared: status.is_some_and(|status| status.declared),
        deactivated: status.is_some_and(|status| status.deactivated),
        direct: depth == 1,
        transitive: depth > 1,
        download_bytes: status.and_then(|status| status.download_bytes),
        installed_bytes: status.and_then(|status| status.installed_bytes),
    }
}

pub(crate) fn outdated_output(
    scope: &'static str,
    result: glu_client::outdated::OutdatedResult,
) -> OutdatedOutput {
    let packages: Vec<OutdatedRecord> = result
        .packages
        .iter()
        .map(|package| OutdatedRecord {
            name: package.name.0.clone(),
            current: package.installed.clone(),
            update: package.update.clone(),
            latest: package.latest.clone(),
        })
        .collect();
    OutdatedOutput {
        scope,
        packages,
        installed_client_version: env!("CARGO_PKG_VERSION"),
        latest_glu_version: result.latest_glu_version,
        human_packages: result.packages,
    }
}

fn render_outdated_output(outdated: &OutdatedOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Outdated, outdated);
        return;
    }
    tables::print_outdated(&outdated.human_packages);
    if let Some(hint) = glu_client::outdated::glu_update_hint(
        outdated.latest_glu_version.as_deref(),
        env!("CARGO_PKG_VERSION"),
    ) {
        println!("{hint}");
    }
}

pub(crate) fn install_output(summary: &glu_client::install::InstallSummary) -> InstallOutput {
    InstallOutput {
        mode: ExecutedMode::Executed,
        requested: summary
            .requested
            .iter()
            .map(|name| name.0.clone())
            .collect(),
        installed: summary
            .installed
            .iter()
            .map(package_change_record)
            .collect(),
        satisfied: summary
            .satisfied
            .iter()
            .map(package_change_record)
            .collect(),
        promoted: summary.promoted.iter().map(package_change_record).collect(),
        renamed: summary
            .renamed
            .iter()
            .map(|rename| RenamePackageRecord {
                package_key: rename.package_key.0.clone(),
                old_name: rename.old_name.0.clone(),
                new_name: rename.new_name.0.clone(),
                version: rename.version.clone(),
                status: MutationStatus::Renamed,
            })
            .collect(),
        removed: summary.removed.iter().map(removed_package_record).collect(),
        execution: execution_summary_record(&summary.execution),
    }
}

pub(crate) fn install_plan_output(plan: &glu_client::install::InstallPlan) -> InstallPlanOutput {
    InstallPlanOutput {
        mode: PlanMode::Plan,
        requested: plan.requested.iter().map(|name| name.0.clone()).collect(),
        would_install: plan
            .would_install
            .iter()
            .map(package_change_record)
            .collect(),
        satisfied: plan.satisfied.iter().map(package_change_record).collect(),
        would_promote: plan
            .promoted
            .iter()
            .map(|package| {
                let mut record = package_change_record(package);
                record.status = MutationStatus::WouldPromote;
                record
            })
            .collect(),
        would_rename: plan
            .renamed
            .iter()
            .map(|rename| RenamePackageRecord {
                package_key: rename.package_key.0.clone(),
                old_name: rename.old_name.0.clone(),
                new_name: rename.new_name.0.clone(),
                version: rename.version.clone(),
                status: MutationStatus::WouldRename,
            })
            .collect(),
        would_remove: plan
            .would_remove
            .iter()
            .map(installed_package_removal_plan_record)
            .collect(),
        requires_confirmation: plan.requires_confirmation,
        would_download_bytes: plan.would_download_bytes,
        dependency_tree: plan.dependency_tree(),
    }
}

pub(crate) fn reinstall_output(summary: &glu_client::install::InstallSummary) -> ReinstallOutput {
    ReinstallOutput {
        mode: ExecutedMode::Executed,
        requested: summary
            .requested
            .iter()
            .map(|name| name.0.clone())
            .collect(),
        reinstalled: summary
            .installed
            .iter()
            .map(|package| {
                let mut record = package_change_record(package);
                record.status = MutationStatus::Reinstalled;
                record
            })
            .collect(),
        satisfied: summary
            .satisfied
            .iter()
            .map(package_change_record)
            .collect(),
        renamed: summary
            .renamed
            .iter()
            .map(|rename| RenamePackageRecord {
                package_key: rename.package_key.0.clone(),
                old_name: rename.old_name.0.clone(),
                new_name: rename.new_name.0.clone(),
                version: rename.version.clone(),
                status: MutationStatus::Renamed,
            })
            .collect(),
        removed: summary.removed.iter().map(removed_package_record).collect(),
        execution: execution_summary_record(&summary.execution),
    }
}

pub(crate) fn reinstall_plan_output(
    plan: &glu_client::install::InstallPlan,
) -> ReinstallPlanOutput {
    ReinstallPlanOutput {
        mode: PlanMode::Plan,
        requested: plan.requested.iter().map(|name| name.0.clone()).collect(),
        would_reinstall: plan
            .would_install
            .iter()
            .map(|package| {
                let mut record = package_change_record(package);
                record.status = MutationStatus::WouldReinstall;
                record
            })
            .collect(),
        satisfied: plan.satisfied.iter().map(package_change_record).collect(),
        would_rename: plan
            .renamed
            .iter()
            .map(|rename| RenamePackageRecord {
                package_key: rename.package_key.0.clone(),
                old_name: rename.old_name.0.clone(),
                new_name: rename.new_name.0.clone(),
                version: rename.version.clone(),
                status: MutationStatus::WouldRename,
            })
            .collect(),
        would_remove: plan
            .would_remove
            .iter()
            .map(installed_package_removal_plan_record)
            .collect(),
        requires_confirmation: plan.requires_confirmation,
        would_download_bytes: plan.would_download_bytes,
    }
}

pub(crate) fn update_output(summary: &glu_client::install::UpdateSummary) -> UpdateOutput {
    UpdateOutput {
        mode: ExecutedMode::Executed,
        updates: summary
            .updates
            .iter()
            .map(|update| update_package_record(update, MutationStatus::Updated))
            .collect(),
        removed: summary.removed.iter().map(removed_package_record).collect(),
        execution: execution_summary_record(&summary.execution),
        latest_glu_version: summary.latest_glu_version.clone(),
        broad: summary.broad,
    }
}

fn update_package_record(
    update: &glu_client::install::PlannedUpdate,
    status: MutationStatus,
) -> UpdatePackageRecord {
    UpdatePackageRecord {
        package_key: update.package_key.0.clone(),
        name: update.name.0.clone(),
        current: update.current.clone(),
        latest: update.latest.clone(),
        status,
        installed: update.installed,
        linked: update.linked,
        declared: update.declared,
        deactivated: update.deactivated,
        direct: update.direct,
        transitive: update.transitive,
        cached: update.cached,
        download_bytes: update.download_bytes,
        installed_bytes: update.installed_bytes,
    }
}

fn package_change_record(change: &glu_client::install::PackageChange) -> MutationPackageRecord {
    MutationPackageRecord {
        package_key: Some(change.package_key.0.clone()),
        name: change.name.0.clone(),
        version: change.version.clone(),
        status: change.status.into(),
        installed: change.installed,
        linked: change.linked,
        declared: change.declared,
        deactivated: change.deactivated,
        direct: change.direct,
        transitive: change.transitive,
        cached: change.cached,
        download_bytes: change.download_bytes,
        installed_bytes: change.installed_bytes,
    }
}

fn plain_mutation_package_record(
    name: String,
    version: String,
    status: MutationStatus,
) -> MutationPackageRecord {
    MutationPackageRecord {
        package_key: None,
        name,
        version,
        status,
        installed: None,
        linked: None,
        declared: None,
        deactivated: None,
        direct: None,
        transitive: None,
        cached: None,
        download_bytes: None,
        installed_bytes: None,
    }
}

pub(crate) fn update_plan_output(
    plan: &glu_client::install::UpdatePlan,
    broad: bool,
) -> UpdatePlanOutput {
    UpdatePlanOutput {
        mode: PlanMode::Plan,
        would_update: plan
            .to_update
            .iter()
            .map(|update| update_package_record(update, MutationStatus::WouldUpdate))
            .collect(),
        would_remove: plan
            .to_remove
            .iter()
            .map(installed_package_removal_plan_record)
            .collect(),
        requires_confirmation: !plan.to_update.is_empty() && (broad || !plan.to_remove.is_empty()),
        broad,
        would_download_bytes: plan.would_download_bytes,
        latest_glu_version: plan.latest_glu_version.clone(),
        dependency_tree: plan.dependency_tree(),
    }
}

fn removed_package_record(package: &RemovedPackage) -> MutationPackageRecord {
    plain_mutation_package_record(
        package.name.0.clone(),
        package.keg_version.0.clone(),
        MutationStatus::Removed,
    )
}

fn installed_package_removal_plan_record(package: &InstalledPackage) -> MutationPackageRecord {
    plain_mutation_package_record(
        package.name.0.clone(),
        package.keg_version.0.clone(),
        MutationStatus::WouldRemove,
    )
}

fn execution_summary_record(
    summary: &glu_client::install::WorksetExecutionSummary,
) -> ExecutionSummaryRecord {
    ExecutionSummaryRecord {
        trace_path: summary.trace_path.as_ref().map(|path| display_path(path)),
        elapsed_seconds: summary.elapsed_seconds,
        timing_breakdown: summary
            .timing_breakdown
            .map(|breakdown| TimingBreakdownRecord {
                download_seconds: breakdown.download_seconds,
                cache_rebuild_seconds: breakdown.cache_rebuild_seconds,
            }),
        stats: summary.stats.map(|stats| InstallStatsRecord {
            downloaded: stats.downloaded,
            reused: stats.reused,
            prepared: stats.prepared,
            signed_machos: stats.signed_machos,
            linked_files: stats.linked_files,
            global_postinstalls: stats.global_postinstalls,
        }),
        timing_description: summary.timing_description.clone(),
        pool_stats: summary.pool_stats.map(|stats| InstallPoolStatsRecord {
            writer_seconds: stats.writer_seconds,
            writer_workers: stats.writer_workers,
            codesign_seconds: stats.codesign_seconds,
            codesign_workers: stats.codesign_workers,
        }),
    }
}

pub(crate) fn activation_output(force: bool, results: &[ActivationResult]) -> ActivationOutput {
    ActivationOutput {
        force,
        packages: results
            .iter()
            .map(|result| {
                plain_mutation_package_record(
                    result.name.0.clone(),
                    result.keg_version.0.clone(),
                    match result.status {
                        ActivationStatus::Activated => MutationStatus::Activated,
                        ActivationStatus::AlreadyActive => MutationStatus::AlreadyActive,
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn deactivation_output(results: &[DeactivationResult]) -> DeactivationOutput {
    DeactivationOutput {
        packages: results
            .iter()
            .map(|result| {
                plain_mutation_package_record(
                    result.name.0.clone(),
                    result.keg_version.0.clone(),
                    match result.status {
                        DeactivationStatus::Deactivated => MutationStatus::Deactivated,
                        DeactivationStatus::AlreadyDeactivated => {
                            MutationStatus::AlreadyDeactivated
                        }
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn removal_output(
    removed: &[RemovedPackage],
    kept: &[KeptDeclaredPackage],
    leftover_config_files: &[std::path::PathBuf],
) -> RemovalOutput {
    RemovalOutput {
        mode: ExecutedMode::Executed,
        removed: removed
            .iter()
            .map(|result| {
                plain_mutation_package_record(
                    result.name.0.clone(),
                    result.keg_version.0.clone(),
                    MutationStatus::Removed,
                )
            })
            .collect(),
        kept: kept
            .iter()
            .map(|kept| KeptPackageRecord {
                name: kept.package.name.0.clone(),
                version: kept.package.keg_version.0.clone(),
                needed_by: kept.needed_by.clone(),
                status: MutationStatus::KeptNeededByDeclared,
            })
            .collect(),
        leftover_config_files: leftover_config_files
            .iter()
            .map(|path| display_path(path))
            .collect(),
    }
}

pub(crate) fn removal_plan_output(plan: &glu_client::remove::RemovalPlan) -> RemovalPlanOutput {
    RemovalPlanOutput {
        mode: PlanMode::Plan,
        named: plan
            .named
            .iter()
            .map(|package| {
                plain_mutation_package_record(
                    package.name.0.clone(),
                    package.keg_version.0.clone(),
                    MutationStatus::Selected,
                )
            })
            .collect(),
        would_remove: plan
            .to_remove
            .iter()
            .map(installed_package_removal_plan_record)
            .collect(),
        would_keep: plan
            .kept
            .iter()
            .map(|kept| KeptPackageRecord {
                name: kept.package.name.0.clone(),
                version: kept.package.keg_version.0.clone(),
                needed_by: kept.needed_by.clone(),
                status: MutationStatus::WouldKeepNeededByDeclared,
            })
            .collect(),
        requires_confirmation: plan.to_remove.len() > plan.named.len(),
    }
}

pub(crate) fn autoremove_output(results: &[RemovedPackage]) -> AutoremoveOutput {
    AutoremoveOutput {
        mode: ExecutedMode::Executed,
        packages: results
            .iter()
            .map(|result| {
                plain_mutation_package_record(
                    result.name.0.clone(),
                    result.keg_version.0.clone(),
                    MutationStatus::Removed,
                )
            })
            .collect(),
    }
}

pub(crate) fn autoremove_plan_output(dangling: &[InstalledPackage]) -> AutoremovePlanOutput {
    AutoremovePlanOutput {
        mode: PlanMode::Plan,
        would_remove: dangling
            .iter()
            .map(installed_package_removal_plan_record)
            .collect(),
        requires_confirmation: !dangling.is_empty(),
    }
}

struct CachedBottleSummary {
    packages: Vec<CachedDownloadRecord>,
    unassociated_downloads: usize,
    unassociated_bytes: u64,
}

fn summarize_cached_bottles(bottles: &[CachedBottle]) -> CachedBottleSummary {
    let mut packages = BTreeMap::<(String, String), (usize, u64)>::new();
    let mut unassociated_downloads = 0;
    let mut unassociated_bytes = 0_u64;
    for bottle in bottles {
        if let Some(package) = bottle.package() {
            let summary = packages
                .entry((package.name.0.clone(), package.keg_version.0.clone()))
                .or_default();
            summary.0 += 1;
            summary.1 += bottle.bytes();
        } else {
            unassociated_downloads += 1;
            unassociated_bytes += bottle.bytes();
        }
    }
    CachedBottleSummary {
        packages: packages
            .into_iter()
            .map(
                |((name, version), (downloads, bytes))| CachedDownloadRecord {
                    name,
                    version,
                    downloads,
                    bytes,
                },
            )
            .collect(),
        unassociated_downloads,
        unassociated_bytes,
    }
}

pub(crate) fn cleanup_output(result: &CacheCleanupResult) -> CleanupOutput {
    let summary = summarize_cached_bottles(result.removed());
    CleanupOutput {
        mode: ExecutedMode::Executed,
        removed: summary.packages,
        removed_downloads: result.removed().len(),
        unassociated_downloads: summary.unassociated_downloads,
        unassociated_bytes: summary.unassociated_bytes,
        reclaimed_bytes: result.reclaimed_bytes(),
    }
}

pub(crate) fn cleanup_plan_output(plan: &CacheCleanupPlan) -> CleanupPlanOutput {
    let summary = summarize_cached_bottles(plan.bottles());
    CleanupPlanOutput {
        mode: PlanMode::Plan,
        would_remove: summary.packages,
        would_remove_downloads: plan.bottles().len(),
        unassociated_downloads: summary.unassociated_downloads,
        unassociated_bytes: summary.unassociated_bytes,
        requires_confirmation: !plan.bottles().is_empty(),
        would_reclaim_bytes: plan.reclaimable_bytes(),
    }
}

fn render_info_output(info: &InfoOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Info, &InfoResult::One(info));
        return;
    }
    print_info(&info.package, info.installed.as_ref());
}

fn render_info_many_output(info: &InfoManyOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Info, &InfoResult::Many(info));
        return;
    }

    let mut first = true;
    for package in &info.packages {
        if let Some(metadata) = &package.package {
            if !first {
                println!();
            }
            print_info(metadata, package.installed.as_ref());
            first = false;
        }
    }
    let failures: Vec<_> = info
        .packages
        .iter()
        .filter_map(|package| {
            package
                .error
                .as_ref()
                .map(|error| (&package.requested, error))
        })
        .collect();
    if !failures.is_empty() {
        if !first {
            println!();
        }
        for (name, error) in failures {
            crate::diagnostic::print_labeled_error(name, &error.message);
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum InfoResult<'a> {
    One(&'a InfoOutput),
    Many(&'a InfoManyOutput),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum InstallResult<'a> {
    Executed(&'a InstallOutput),
    PlanFlat(&'a InstallPlanOutput),
    PlanTree(InstallPlanTreeResult<'a>),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct InstallPlanTreeResult<'a> {
    edges: BTreeMap<String, Vec<DependencyGraphEdge>>,
    mode: PlanMode,
    nodes: BTreeMap<String, DependencyGraphNode>,
    requested: &'a [String],
    requires_confirmation: bool,
    roots: Vec<String>,
    satisfied: &'a [MutationPackageRecord],
    view: TreeView,
    would_download_bytes: Option<u64>,
    would_install: &'a [MutationPackageRecord],
    would_promote: &'a [MutationPackageRecord],
    would_remove: &'a [MutationPackageRecord],
    would_rename: &'a [RenamePackageRecord],
}

impl<'a> InstallPlanTreeResult<'a> {
    fn new(
        plan: &'a InstallPlanOutput,
        statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    ) -> Self {
        let DependencyGraphJson {
            nodes,
            roots,
            edges,
        } = dependency_graph_json(&plan.dependency_tree, false, statuses);
        Self {
            edges,
            mode: plan.mode,
            nodes,
            requested: &plan.requested,
            requires_confirmation: plan.requires_confirmation,
            roots,
            satisfied: &plan.satisfied,
            view: TreeView::Tree,
            would_download_bytes: plan.would_download_bytes,
            would_install: &plan.would_install,
            would_promote: &plan.would_promote,
            would_remove: &plan.would_remove,
            would_rename: &plan.would_rename,
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ReinstallResult<'a> {
    Executed(&'a ReinstallOutput),
    Plan(&'a ReinstallPlanOutput),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum UpdateResult<'a> {
    Executed(&'a UpdateOutput),
    PlanFlat(&'a UpdatePlanOutput),
    PlanTree(UpdatePlanTreeResult<'a>),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct UpdatePlanTreeResult<'a> {
    broad: bool,
    edges: BTreeMap<String, Vec<DependencyGraphEdge>>,
    latest_glu_version: &'a Option<String>,
    mode: PlanMode,
    nodes: BTreeMap<String, DependencyGraphNode>,
    requires_confirmation: bool,
    roots: Vec<String>,
    view: TreeView,
    would_download_bytes: Option<u64>,
    would_remove: &'a [MutationPackageRecord],
    would_update: &'a [UpdatePackageRecord],
}

impl<'a> UpdatePlanTreeResult<'a> {
    fn new(
        plan: &'a UpdatePlanOutput,
        statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    ) -> Self {
        let DependencyGraphJson {
            nodes,
            roots,
            edges,
        } = dependency_graph_json(&plan.dependency_tree, false, statuses);
        Self {
            broad: plan.broad,
            edges,
            latest_glu_version: &plan.latest_glu_version,
            mode: plan.mode,
            nodes,
            requires_confirmation: plan.requires_confirmation,
            roots,
            view: TreeView::Tree,
            would_download_bytes: plan.would_download_bytes,
            would_remove: &plan.would_remove,
            would_update: &plan.would_update,
        }
    }
}

pub(crate) fn render_install_preflight(plan: &glu_client::install::InstallPlan) {
    let requested = plan.resolved_root_keys();
    let promoted: Vec<_> = plan
        .promoted
        .iter()
        .map(|package| {
            PackageListItem::package(&package.name.0, &package.version)
                .emphasized(requested.contains(&package.package_key))
        })
        .collect();
    package_list::print_labeled_section("Added to your packages", &promoted);

    let satisfied: Vec<_> = plan
        .satisfied
        .iter()
        .filter(|package| {
            package.status == glu_client::install::PackageChangeStatus::AlreadyInstalled
        })
        .map(|package| {
            PackageListItem::package(&package.name.0, &package.version)
                .emphasized(requested.contains(&package.package_key))
        })
        .collect();
    package_list::print_labeled_section("Already installed", &satisfied);

    for package in &plan.satisfied {
        if package.status == glu_client::install::PackageChangeStatus::InstalledOlderThanResolved {
            let resolved = plan
                .resolved_version(&package.name)
                .unwrap_or("the resolved version");
            println!(
                "{} {} is already installed. Run `glu update {}` to update to {}.",
                package.name.0,
                glu_client::style::dim(&package.version),
                package.name.0,
                glu_client::style::dim(resolved)
            );
        }
    }
}

pub(crate) fn render_install_execution_plan(
    plan: &glu_client::install::InstallPlan,
    label: &str,
    tree: bool,
) {
    let requested = plan.resolved_root_keys();
    let total = plan.would_install.len() + plan.renamed.len();
    if total > 0 {
        if tree {
            println!(
                "Will {label} {}:",
                glu_client::format::plural(total, "package")
            );
            let included: BTreeSet<glu_core::PackageKey> = plan
                .would_install
                .iter()
                .map(|package| package.package_key.clone())
                .chain(plan.renamed.iter().map(|rename| rename.package_key.clone()))
                .collect();
            let install_tree = filter_package_tree(&plan.dependency_tree(), &included);
            for line in render_dependency_tree_with_context(
                &install_tree.nodes,
                TreeRenderOptions::decorated(RootStyle::AlwaysLast),
                &install_tree.context,
            ) {
                println!("{line}");
            }
        } else {
            let mut items: Vec<_> = plan
                .renamed
                .iter()
                .map(|rename| {
                    PackageListItem::rename(&rename.old_name.0, &rename.new_name.0, &rename.version)
                        .emphasized(requested.contains(&rename.package_key))
                })
                .collect();
            items.extend(plan.would_install.iter().map(|package| {
                PackageListItem::package(&package.name.0, &package.version)
                    .emphasized(requested.contains(&package.package_key))
            }));
            package_list::print_section(&format!("Will {label}"), &items);
        }
    }
    let removals: Vec<_> = plan
        .would_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_counted_section("Will also remove", "unused package", &removals);
    if total > 0 || !removals.is_empty() {
        println!();
    }
}

pub(crate) fn render_update_preflight(plan: &glu_client::install::UpdatePlan) {
    let up_to_date: Vec<_> = plan
        .up_to_date
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.version))
        .collect();
    package_list::print_labeled_section("Already up to date", &up_to_date);

    let dependents: Vec<_> = plan
        .cascade_added
        .iter()
        .map(|name| PackageListItem::name(&name.0))
        .collect();
    package_list::print_counted_section("Also updating", "outdated dependent", &dependents);
}

pub(crate) fn render_update_execution_plan(plan: &glu_client::install::UpdatePlan, tree: bool) {
    let update_tree = plan.dependency_tree();
    let changes: Vec<_> = plan
        .to_update
        .iter()
        .map(|update| {
            (
                update.package_key.clone(),
                update.current.clone(),
                update.latest.clone(),
            )
        })
        .collect();
    if !(tree && print_update_tree("Will update", &update_tree, &changes)) {
        let items: Vec<_> = plan
            .to_update
            .iter()
            .map(|update| {
                PackageListItem::update(&update.name.0, &update.current, &update.latest)
                    .emphasized(update.direct == Some(true))
            })
            .collect();
        package_list::print_section("Will update", &items);
    }
    let removals: Vec<_> = plan
        .to_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_section("Will also remove", &removals);
}

pub(crate) fn render_removal_execution_plan(plan: &glu_client::remove::RemovalPlan) {
    let removals: Vec<_> = plan
        .to_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_section("Will remove", &removals);
    let retained: Vec<_> = plan
        .kept
        .iter()
        .map(|kept| {
            PackageListItem::package(&kept.package.name.0, &kept.package.keg_version.0)
                .annotated(format!("needed by {}", kept.needed_by.join(", ")))
        })
        .collect();
    package_list::print_section("Will retain", &retained);
}

pub(crate) fn render_autoremove_execution_plan(packages: &[InstalledPackage]) {
    let items: Vec<_> = packages
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_counted_section("Will remove", "unused package", &items);
}

fn render_install_output(install: &InstallOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Install, &InstallResult::Executed(install));
        return;
    }
    render_execution_summary(&install.execution, globals);
}

fn render_install_plan_output(plan: &InstallPlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        let result = if globals.tree {
            let statuses = mutation_plan_statuses(
                plan.would_install
                    .iter()
                    .chain(&plan.satisfied)
                    .chain(&plan.would_promote),
            );
            InstallResult::PlanTree(InstallPlanTreeResult::new(plan, &statuses))
        } else {
            InstallResult::PlanFlat(plan)
        };
        print_json_success(CommandId::Install, &result);
        return;
    }
    let has_changes = !plan.would_install.is_empty()
        || !plan.would_promote.is_empty()
        || !plan.would_rename.is_empty()
        || !plan.would_remove.is_empty();
    if !has_changes {
        println!("Nothing to do.");
    }
    let install_tree = if globals.tree {
        install_only_tree(&plan.dependency_tree, &plan.would_install)
    } else {
        Default::default()
    };
    let requested = dependency_root_keys(&plan.dependency_tree);
    if !install_tree.nodes.is_empty() {
        println!(
            "Would install {}:",
            glu_client::format::plural(plan.would_install.len(), "package")
        );
        print_mutation_tree(&install_tree);
    } else {
        print_install_package_section("Would install", &plan.would_install, &requested);
    }
    print_install_package_section("Already satisfied", &plan.satisfied, &requested);
    print_install_package_section("Would promote to declared", &plan.would_promote, &requested);
    print_install_rename_section("Would rename", &plan.would_rename, &requested);
    print_package_section("Would remove", &plan.would_remove);
    print_would_download(plan.would_download_bytes);
}

fn render_reinstall_output(reinstall: &ReinstallOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Reinstall, &ReinstallResult::Executed(reinstall));
        return;
    }
    render_execution_summary(&reinstall.execution, globals);
}

fn render_reinstall_plan_output(plan: &ReinstallPlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Reinstall, &ReinstallResult::Plan(plan));
        return;
    }
    if plan.would_reinstall.is_empty()
        && plan.would_rename.is_empty()
        && plan.would_remove.is_empty()
    {
        println!("Nothing to do.");
        return;
    }
    print_package_section("Would reinstall", &plan.would_reinstall);
    print_package_section("Already satisfied", &plan.satisfied);
    print_rename_section("Would rename", &plan.would_rename);
    print_package_section("Would remove", &plan.would_remove);
    print_would_download(plan.would_download_bytes);
}

fn render_update_output(update: &UpdateOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Update, &UpdateResult::Executed(update));
        return;
    }
    if update.updates.is_empty() && update.execution.trace_path.is_none() && update.broad {
        println!("Already up to date.");
    }
    render_execution_summary(&update.execution, globals);
    if !update.updates.is_empty() || update.broad {
        if let Some(hint) = glu_client::outdated::glu_update_hint(
            update.latest_glu_version.as_deref(),
            env!("CARGO_PKG_VERSION"),
        ) {
            println!("{hint}");
        }
    }
}

fn render_execution_summary(execution: &ExecutionSummaryRecord, globals: &GlobalOptions) {
    if execution.trace_path.is_none() {
        return;
    }
    if globals.verbose {
        if let Some(pool) = &execution.pool_stats {
            eprintln!(
                "glu: writer pool busy time: {:.3}s across {} workers",
                pool.writer_seconds, pool.writer_workers
            );
            eprintln!(
                "glu: codesign pool busy time: {:.3}s across {} workers",
                pool.codesign_seconds, pool.codesign_workers
            );
        }
        if let Some(stats) = &execution.stats {
            println!(
                "Artifacts: {} downloaded, {} reused",
                stats.downloaded, stats.reused
            );
            println!(
                "Prepared {} kegs ({} Mach-O files signed, {} prefix links)",
                stats.prepared, stats.signed_machos, stats.linked_files
            );
        }
    }
    println!();
    let summary = match &execution.timing_description {
        Some(breakdown) => format!("Done in {:.1}s · {breakdown}", execution.elapsed_seconds),
        None => format!("Done in {:.1}s", execution.elapsed_seconds),
    };
    println!("{}", glu_client::style::dim(&summary));
}

fn render_update_plan_output(plan: &UpdatePlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        let result = if globals.tree {
            let statuses = update_plan_statuses(&plan.would_update);
            UpdateResult::PlanTree(UpdatePlanTreeResult::new(plan, &statuses))
        } else {
            UpdateResult::PlanFlat(plan)
        };
        print_json_success(CommandId::Update, &result);
        return;
    }
    if plan.would_update.is_empty() && plan.would_remove.is_empty() {
        println!("Nothing to do.");
    } else {
        let rendered_tree = globals.tree
            && print_update_tree(
                "Would update",
                &plan.dependency_tree,
                &plan
                    .would_update
                    .iter()
                    .map(|update| {
                        (
                            glu_core::PackageKey(update.package_key.clone()),
                            update.current.clone(),
                            update.latest.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        if !rendered_tree {
            let updates: Vec<_> = plan.would_update.iter().map(update_list_item).collect();
            package_list::print_section("Would update", &updates);
        }
        print_package_section("Would remove", &plan.would_remove);
    }
    print_would_download(plan.would_download_bytes);
}

fn would_download_message(bytes: Option<u64>) -> Option<String> {
    bytes
        .filter(|bytes| *bytes > 0)
        .map(|bytes| format!("Would download: {}", glu_client::format::human_bytes(bytes)))
}

fn print_would_download(bytes: Option<u64>) {
    if let Some(message) = would_download_message(bytes) {
        println!("{message}");
    }
}

fn print_mutation_plan(
    verb: &str,
    packages: &[MutationPackageRecord],
    removals: &[MutationPackageRecord],
) {
    if packages.is_empty() && removals.is_empty() {
        println!("Nothing to do.");
        return;
    }
    print_package_section(&format!("Would {verb}"), packages);
    print_package_section("Would remove", removals);
}

fn update_list_item(update: &UpdatePackageRecord) -> PackageListItem {
    PackageListItem::update(&update.name, &update.current, &update.latest)
        .emphasized(update.direct == Some(true))
}

fn print_package_section(heading: &str, packages: &[MutationPackageRecord]) {
    let items: Vec<_> = packages
        .iter()
        .map(|package| {
            PackageListItem::package(&package.name, &package.version)
                .emphasized(package.direct == Some(true))
        })
        .collect();
    package_list::print_section(heading, &items);
}

fn dependency_root_keys(tree: &[DependencyTreeNode]) -> BTreeSet<String> {
    tree.iter().map(|root| root.package_key.0.clone()).collect()
}

fn print_install_package_section(
    heading: &str,
    packages: &[MutationPackageRecord],
    requested: &BTreeSet<String>,
) {
    package_list::print_section(heading, &install_package_items(packages, requested));
}

fn install_package_items(
    packages: &[MutationPackageRecord],
    requested: &BTreeSet<String>,
) -> Vec<PackageListItem> {
    packages
        .iter()
        .map(|package| {
            PackageListItem::package(&package.name, &package.version).emphasized(
                package
                    .package_key
                    .as_ref()
                    .is_some_and(|package_key| requested.contains(package_key)),
            )
        })
        .collect()
}

fn print_install_rename_section(
    heading: &str,
    renames: &[RenamePackageRecord],
    requested: &BTreeSet<String>,
) {
    let items: Vec<_> = renames
        .iter()
        .map(|rename| {
            PackageListItem::rename(&rename.old_name, &rename.new_name, &rename.version)
                .emphasized(requested.contains(&rename.package_key))
        })
        .collect();
    package_list::print_section(heading, &items);
}

fn print_rename_section(heading: &str, renames: &[RenamePackageRecord]) {
    let items: Vec<_> = renames
        .iter()
        .map(|rename| PackageListItem::rename(&rename.old_name, &rename.new_name, &rename.version))
        .collect();
    package_list::print_section(heading, &items);
}

#[derive(Default)]
struct MutationTree {
    nodes: Vec<DependencyTreeNode>,
    context: BTreeSet<(glu_core::PackageKey, String)>,
}

fn filter_package_tree(
    nodes: &[DependencyTreeNode],
    included: &BTreeSet<glu_core::PackageKey>,
) -> MutationTree {
    let mut result = MutationTree::default();
    for node in nodes {
        let mut children = filter_package_tree(&node.children, included);
        result.context.append(&mut children.context);
        if included.contains(&node.package_key) {
            let mut node = node.clone();
            node.children = children.nodes;
            result.nodes.push(node);
        } else if !children.nodes.is_empty() {
            // Retain the minimal unchanged path needed to preserve factual
            // dependency edges between visible mutation nodes.
            result
                .context
                .insert((node.package_key.clone(), node.version.clone()));
            let mut node = node.clone();
            node.children = children.nodes;
            result.nodes.push(node);
        }
    }
    result
}

fn install_only_tree(
    tree: &[DependencyTreeNode],
    would_install: &[MutationPackageRecord],
) -> MutationTree {
    let included: BTreeSet<glu_core::PackageKey> = would_install
        .iter()
        .filter_map(|package| package.package_key.clone().map(glu_core::PackageKey))
        .collect();
    filter_package_tree(tree, &included)
}

fn print_mutation_tree(tree: &MutationTree) {
    let options = TreeRenderOptions {
        decorated: true,
        direct: false,
        verbose: false,
        show_versions: true,
        version_label: None,
        root_style: RootStyle::Plain,
    };
    for line in render_dependency_tree_with_context(&tree.nodes, options, &tree.context) {
        println!("{line}");
    }
}

fn update_only_tree(
    tree: &[DependencyTreeNode],
    changes: &[(glu_core::PackageKey, String, String)],
) -> MutationTree {
    let changes: BTreeMap<glu_core::PackageKey, (&str, &str)> = changes
        .iter()
        .map(|(package_key, current, latest)| {
            (package_key.clone(), (current.as_str(), latest.as_str()))
        })
        .collect();

    let included: BTreeSet<glu_core::PackageKey> = changes.keys().cloned().collect();
    let mut tree = filter_package_tree(tree, &included);

    fn add_transitions(
        nodes: &mut [DependencyTreeNode],
        changes: &BTreeMap<glu_core::PackageKey, (&str, &str)>,
    ) {
        for node in nodes {
            if let Some((current, latest)) = changes.get(&node.package_key) {
                node.version = format!("{current} → {latest}");
            }
            add_transitions(&mut node.children, changes);
        }
    }
    add_transitions(&mut tree.nodes, &changes);
    tree
}

pub(crate) fn print_update_tree(
    action: &str,
    tree: &[DependencyTreeNode],
    changes: &[(glu_core::PackageKey, String, String)],
) -> bool {
    let tree = update_only_tree(tree, changes);
    if tree.nodes.is_empty() {
        return false;
    }
    println!(
        "{action} {}:",
        glu_client::format::plural(changes.len(), "package")
    );
    print_mutation_tree(&tree);
    true
}

fn mutation_plan_statuses<'a>(
    records: impl IntoIterator<Item = &'a MutationPackageRecord>,
) -> BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus> {
    records
        .into_iter()
        .filter_map(|record| {
            Some((
                glu_core::PackageKey(record.package_key.clone()?),
                glu_client::deps::PackageStatus {
                    installed: record.installed.unwrap_or(false),
                    installed_version: None,
                    linked: record.linked.unwrap_or(false),
                    declared: record.declared.unwrap_or(false),
                    deactivated: record.deactivated.unwrap_or(false),
                    download_bytes: record.download_bytes,
                    installed_bytes: record.installed_bytes,
                },
            ))
        })
        .collect()
}

fn update_plan_statuses(
    records: &[UpdatePackageRecord],
) -> BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus> {
    records
        .iter()
        .map(|record| {
            (
                glu_core::PackageKey(record.package_key.clone()),
                glu_client::deps::PackageStatus {
                    installed: record.installed.unwrap_or(false),
                    installed_version: None,
                    linked: record.linked.unwrap_or(false),
                    declared: record.declared.unwrap_or(false),
                    deactivated: record.deactivated.unwrap_or(false),
                    download_bytes: record.download_bytes,
                    installed_bytes: record.installed_bytes,
                },
            )
        })
        .collect()
}

fn render_activation_output(activation: &ActivationOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Activate, activation);
        return;
    }
    // Activation state is package-level: selectors resolve names/aliases to
    // the newest installed keg, rather than targeting an arbitrary version.
    let activated: Vec<_> = activation
        .packages
        .iter()
        .filter(|package| package.status == MutationStatus::Activated)
        .map(|package| PackageListItem::name(&package.name))
        .collect();
    let unchanged: Vec<_> = activation
        .packages
        .iter()
        .filter(|package| package.status == MutationStatus::AlreadyActive)
        .map(|package| PackageListItem::name(&package.name))
        .collect();
    package_list::print_section("Activated", &activated);
    package_list::print_labeled_section("Already active", &unchanged);
}

fn render_deactivation_output(deactivation: &DeactivationOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Deactivate, deactivation);
        return;
    }
    let deactivated: Vec<_> = deactivation
        .packages
        .iter()
        .filter(|package| package.status == MutationStatus::Deactivated)
        .map(|package| PackageListItem::name(&package.name))
        .collect();
    let unchanged: Vec<_> = deactivation
        .packages
        .iter()
        .filter(|package| package.status == MutationStatus::AlreadyDeactivated)
        .map(|package| PackageListItem::name(&package.name))
        .collect();
    package_list::print_section("Deactivated", &deactivated);
    package_list::print_labeled_section("Already deactivated", &unchanged);
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum RemovalResult<'a> {
    Executed(&'a RemovalOutput),
    Plan(&'a RemovalPlanOutput),
}

fn render_removal_output(removal: &RemovalOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Remove, &RemovalResult::Executed(removal));
        return;
    }
    if !removal.leftover_config_files.is_empty() {
        println!();
        println!(
            "{}",
            glu_client::style::bold_yellow(
                "The following configuration files have not been removed!",
            )
        );
        println!(
            "{}",
            glu_client::style::yellow("If desired, remove them manually with `rm -rf`:")
        );
        for path in &removal.leftover_config_files {
            println!("  {path}");
        }
    }
}

fn render_removal_plan_output(plan: &RemovalPlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Remove, &RemovalResult::Plan(plan));
        return;
    }
    print_mutation_plan("remove", &[], &plan.would_remove);
    let kept: Vec<_> = plan
        .would_keep
        .iter()
        .map(|kept| {
            PackageListItem::package(&kept.name, &kept.version)
                .annotated(format!("needed by {}", kept.needed_by.join(", ")))
        })
        .collect();
    package_list::print_section("Would retain", &kept);
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum AutoremoveResult<'a> {
    Executed(&'a AutoremoveOutput),
    Plan(&'a AutoremovePlanOutput),
}

fn render_autoremove_output(autoremove: &AutoremoveOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(
            CommandId::Autoremove,
            &AutoremoveResult::Executed(autoremove),
        );
        return;
    }
    if autoremove.packages.is_empty() {
        println!("No unused packages to remove.");
    }
}

fn render_autoremove_plan_output(plan: &AutoremovePlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Autoremove, &AutoremoveResult::Plan(plan));
        return;
    }
    print_mutation_plan("remove", &[], &plan.would_remove);
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum CleanupResult<'a> {
    Executed(&'a CleanupOutput),
    Plan(&'a CleanupPlanOutput),
}

fn cached_bottle_items(
    bottles: &[CachedDownloadRecord],
    unassociated_downloads: usize,
    unassociated_bytes: u64,
) -> Vec<PackageListItem> {
    let mut items: Vec<_> = bottles
        .iter()
        .map(|bottle| {
            let annotation = if bottle.downloads == 1 {
                glu_client::format::human_bytes(bottle.bytes)
            } else {
                format!(
                    "{}, {}",
                    glu_client::format::plural(bottle.downloads, "download"),
                    glu_client::format::human_bytes(bottle.bytes)
                )
            };
            PackageListItem::package(&bottle.name, &bottle.version).annotated(annotation)
        })
        .collect();
    if unassociated_downloads > 0 {
        items.push(PackageListItem::name("Unassociated").annotated(format!(
            "{}, {}",
            glu_client::format::plural(unassociated_downloads, "download"),
            glu_client::format::human_bytes(unassociated_bytes)
        )));
    }
    items
}

pub(crate) fn render_cleanup_execution_plan(plan: &CacheCleanupPlan) {
    let plan = cleanup_plan_output(plan);
    print_cleanup_plan(&plan, "Will remove", "Will reclaim");
}

fn render_cleanup_output(cleanup: &CleanupOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Cleanup, &CleanupResult::Executed(cleanup));
        return;
    }
    if cleanup.removed_downloads == 0 {
        println!("No cached downloads to remove.");
        return;
    }
    println!(
        "Reclaimed: {}",
        glu_client::format::human_bytes(cleanup.reclaimed_bytes)
    );
}

fn render_cleanup_plan_output(plan: &CleanupPlanOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Cleanup, &CleanupResult::Plan(plan));
        return;
    }
    print_cleanup_plan(plan, "Would remove", "Would reclaim");
}

fn print_cleanup_plan(plan: &CleanupPlanOutput, heading: &str, reclaim_label: &str) {
    if plan.would_remove_downloads == 0 {
        println!("No cached downloads to remove.");
        return;
    }
    package_list::print_counted_section_with_total(
        heading,
        "cached download",
        plan.would_remove_downloads,
        &cached_bottle_items(
            &plan.would_remove,
            plan.unassociated_downloads,
            plan.unassociated_bytes,
        ),
    );
    println!(
        "{reclaim_label}: {}",
        glu_client::format::human_bytes(plan.would_reclaim_bytes)
    );
}

fn render_status_output(status: &StatusOutput, globals: &GlobalOptions) {
    if globals.is_json() {
        print_json_success(CommandId::Status, status);
        return;
    }

    println!("glu {}", status.version);
    println!(
        "Prefix:       {} {}",
        status.prefix,
        glu_client::style::dim(&format!("({})", status.prefix_source)),
    );
    if status.prefix_length != status.fixed_cellar_length {
        println!(
            "  length:     {}",
            glu_client::style::yellow(&format!(
                "{} bytes; fixed-cellar bottles require {} (same as /opt/homebrew) and will be rejected",
                status.prefix_length, status.fixed_cellar_length
            ))
        );
    }
    println!("Target:       {}", status.target);
    println!("Registry:     {}", status.registry);
    println!("Distribution: {}", status.distribution);
    println!(
        "Installed:    {}",
        glu_client::format::plural(status.installed_count, "package")
    );
    println!(
        "Declared:     {}",
        glu_client::format::plural(status.declared_count, "package")
    );
    if status.deactivated.is_empty() {
        println!("Deactivated:  {}", glu_client::style::dim("none"));
    } else {
        println!("Deactivated:  {}", status.deactivated.join(", "));
    }

    println!("Shell integration:");
    for shell in &status.shells {
        if shell.configured {
            println!(
                "  {} {:<5} {:<38}",
                glu_client::style::green("✓"),
                shell.name,
                shell.config_path
            );
        } else {
            println!(
                "  {} {:<5} {:<38}{}",
                glu_client::style::dim("○"),
                shell.name,
                shell.config_path,
                glu_client::style::dim("not configured")
            );
        }
    }
}

/// `glu list`: plain `name version` rows. TTY output may color versions and
/// state labels, but it does not add headers or bullets, so the command stays
/// a list primitive.
pub(crate) fn print_list(
    packages: &[InstalledPackage],
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) {
    let items: Vec<_> = packages
        .iter()
        .map(|package| {
            let item = PackageListItem::package(&package.name.0, &package.keg_version.0);
            if statuses
                .get(&package.package_key)
                .is_some_and(|status| status.deactivated)
            {
                item.annotated("deactivated")
            } else {
                item
            }
        })
        .collect();
    package_list::print_primitive(&items);
}

/// Flat view of a tree: every stable package identity once, sorted by its
/// displayed spelling. Distinct packages with the same display string remain
/// distinct. `direct` stops after the first level.
pub(crate) fn flatten_tree_unique(
    nodes: &[DependencyTreeNode],
    direct: bool,
    out: &mut Vec<(String, String)>,
) {
    fn walk<'a>(
        nodes: &'a [DependencyTreeNode],
        direct: bool,
        out: &mut Vec<&'a DependencyTreeNode>,
    ) {
        for node in nodes {
            out.push(node);
            if !direct {
                walk(&node.children, direct, out);
            }
        }
    }
    let mut by_identity = std::collections::BTreeMap::new();
    let mut flat = Vec::new();
    walk(nodes, direct, &mut flat);
    for node in flat {
        by_identity
            .entry(node.package_key.clone())
            .or_insert_with(|| (node.name.clone(), node.version.clone()));
    }
    let mut unique: Vec<_> = by_identity.into_values().collect();
    unique.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.1.cmp(&b.1))
    });
    *out = unique;
}

/// Flat query output uses the same bare `name version` primitive as `glu ls`.
pub(crate) fn print_flat(items: &[(String, String)]) {
    let items: Vec<_> = items
        .iter()
        .map(|(name, version)| PackageListItem::package(name, version))
        .collect();
    package_list::print_primitive(&items);
}

fn print_deps_flat(
    records: &[DependencyRecord],
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    verbose: bool,
    show_status: bool,
) {
    let items: Vec<_> = records
        .iter()
        .map(|record| deps_list_item(record, statuses, verbose, show_status))
        .collect();
    package_list::print_primitive(&items);
}

fn print_dependency_status(
    records: &[DependencyRecord],
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    verbose: bool,
) {
    if records.is_empty() {
        return;
    }
    println!();
    let items: Vec<_> = records
        .iter()
        .map(|record| deps_list_item(record, statuses, verbose, true))
        .collect();
    package_list::print_labeled_section("Dependency status", &items);
}

fn deps_list_item(
    record: &DependencyRecord,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    verbose: bool,
    show_status: bool,
) -> PackageListItem {
    let installed_version = statuses
        .get(&glu_core::PackageKey(record.package_key.clone()))
        .and_then(|status| status.installed_version.as_deref());
    let mut annotation = Vec::new();
    if verbose {
        if let Some(version) = installed_version {
            annotation.push(format!("{version} installed"));
        }
    }
    if show_status {
        let mut status = dependency_status_parts(record);
        if verbose && installed_version.is_some() {
            status.remove(0);
        }
        annotation.extend(status.into_iter().map(str::to_string));
    }
    let item = PackageListItem::name(&record.name);
    if annotation.is_empty() {
        item
    } else {
        item.annotated(annotation.join(", "))
    }
}

fn print_deps_tree(
    root: &DependencyTreeNode,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    direct: bool,
    verbose: bool,
) {
    fn local_versions(
        node: &DependencyTreeNode,
        statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
        verbose: bool,
    ) -> DependencyTreeNode {
        let mut node = node.clone();
        node.version = if verbose {
            statuses
                .get(&node.package_key)
                .and_then(|status| status.installed_version.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        node.children = node
            .children
            .iter()
            .map(|child| local_versions(child, statuses, verbose))
            .collect();
        node
    }

    let root = local_versions(root, statuses, verbose);
    let options = TreeRenderOptions {
        decorated: true,
        direct,
        verbose,
        show_versions: verbose,
        version_label: Some("installed"),
        root_style: RootStyle::SiblingBranches,
    };
    for line in render_dependency_tree(std::slice::from_ref(&root), options) {
        println!("{line}");
    }
}

fn dependency_status_parts(record: &DependencyRecord) -> Vec<&'static str> {
    let mut parts = Vec::new();
    parts.push(if record.installed { "installed" } else { "new" });
    parts.push(if record.direct {
        "direct"
    } else {
        "transitive"
    });
    if record.declared {
        parts.push("declared");
    }
    if record.deactivated {
        parts.push("deactivated");
    } else if matches!(record.linked, Some(true)) {
        parts.push("linked");
    } else if matches!(record.linked, Some(false)) {
        parts.push("unlinked");
    }
    parts
}

/// `glu ls --tree`: dependency tree rooted at declared packages; with
/// `--installed`/`--all`, dangling packages are added as roots so the tree
/// covers everything installed. Explicit tree output keeps branch structure
/// even when piped; ANSI styling is still terminal-gated by the style layer.
pub(crate) fn print_list_tree(tree: &[DependencyTreeNode]) {
    let options = TreeRenderOptions {
        decorated: true,
        direct: false,
        verbose: false,
        show_versions: true,
        version_label: None,
        root_style: RootStyle::Plain,
    };
    for line in render_dependency_tree(tree, options) {
        println!("{line}");
    }
}

/// `glu list --tree --json`: normalized graph payload with tree-shaped refs.
pub(crate) fn print_list_tree_json(
    scope: ListScope,
    tree: &[DependencyTreeNode],
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) {
    let result = ListResult::Tree(ListTreeResult {
        scope,
        view: TreeView::Tree,
        graph: dependency_graph_json(tree, false, statuses),
    });
    print_json_success(CommandId::List, &result);
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ListResult {
    Flat(ListFlatResult),
    Tree(ListTreeResult),
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ListTreeResult {
    scope: ListScope,
    view: TreeView,
    #[serde(flatten)]
    graph: DependencyGraphJson,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DepsTreeResult {
    source: DepsSource,
    installed: bool,
    direct: bool,
    view: TreeView,
    #[serde(flatten)]
    graph: DependencyGraphJson,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ReverseDepsTreeResult<'a> {
    target: &'a str,
    source: ReverseDepsSource,
    direct: bool,
    view: TreeView,
    #[serde(flatten)]
    graph: DependencyGraphJson,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DependencyGraphJson {
    nodes: BTreeMap<String, DependencyGraphNode>,
    roots: Vec<String>,
    edges: BTreeMap<String, Vec<DependencyGraphEdge>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DependencyGraphNode {
    package_key: String,
    name: String,
    version: String,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    linked: Option<bool>,
    declared: bool,
    deactivated: bool,
    direct: bool,
    transitive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    download_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    installed_bytes: Option<u64>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DependencyGraphEdge {
    to: String,
    requested_as: String,
    reversed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    requires: Option<String>,
}

fn dependency_graph_json(
    nodes: &[DependencyTreeNode],
    direct: bool,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) -> DependencyGraphJson {
    dependency_graph_json_at_depth(nodes, direct, 0, statuses)
}

fn dependency_graph_json_at_depth(
    nodes: &[DependencyTreeNode],
    direct: bool,
    initial_depth: usize,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) -> DependencyGraphJson {
    let mut graph_nodes = BTreeMap::new();
    let mut edges: BTreeMap<String, Vec<DependencyGraphEdge>> = BTreeMap::new();
    let roots = nodes
        .iter()
        .map(|node| {
            let id = dependency_node_id(node);
            collect_dependency_graph(
                node,
                direct,
                initial_depth,
                statuses,
                &mut graph_nodes,
                &mut edges,
            );
            id
        })
        .collect();
    DependencyGraphJson {
        nodes: graph_nodes,
        roots,
        edges,
    }
}

fn collect_dependency_graph(
    node: &DependencyTreeNode,
    direct: bool,
    depth: usize,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    nodes: &mut BTreeMap<String, DependencyGraphNode>,
    edges: &mut BTreeMap<String, Vec<DependencyGraphEdge>>,
) {
    let id = dependency_node_id(node);
    let graph_node = nodes
        .entry(id.clone())
        .or_insert_with(|| dependency_graph_node(node, statuses, depth));
    if depth == 1 {
        graph_node.direct = true;
        graph_node.transitive = false;
    } else if depth > 1 && !graph_node.direct {
        graph_node.transitive = true;
    }

    if direct && depth >= 1 {
        return;
    }

    for child in &node.children {
        let child_id = dependency_node_id(child);
        let graph_node = nodes
            .entry(child_id.clone())
            .or_insert_with(|| dependency_graph_node(child, statuses, depth + 1));
        if depth == 0 {
            graph_node.direct = true;
            graph_node.transitive = false;
        } else if !graph_node.direct {
            graph_node.transitive = true;
        }
        edges
            .entry(id.clone())
            .or_default()
            .push(DependencyGraphEdge {
                to: child_id,
                requested_as: child
                    .incoming
                    .as_ref()
                    .map_or_else(|| child.name.clone(), |edge| edge.requested_as.0.clone()),
                reversed: child.incoming.as_ref().is_some_and(|edge| edge.reversed),
                requires: child
                    .incoming
                    .as_ref()
                    .and_then(|edge| edge.minimum.as_ref())
                    .map(format_minimum_version),
            });
        if !child.already_shown {
            collect_dependency_graph(child, direct, depth + 1, statuses, nodes, edges);
        }
    }
}

fn dependency_node_id(node: &DependencyTreeNode) -> String {
    node.package.0.clone()
}

fn dependency_graph_node(
    node: &DependencyTreeNode,
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
    depth: usize,
) -> DependencyGraphNode {
    let status = statuses.get(&node.package_key);
    DependencyGraphNode {
        package_key: node.package_key.0.clone(),
        name: node.canonical_name.0.clone(),
        version: node.version.clone(),
        installed: status.is_some_and(|status| status.installed),
        linked: status.map(|status| status.linked),
        declared: status.is_some_and(|status| status.declared),
        deactivated: status.is_some_and(|status| status.deactivated),
        direct: depth == 1,
        transitive: depth > 1,
        download_bytes: status.and_then(|status| status.download_bytes),
        installed_bytes: status.and_then(|status| status.installed_bytes),
    }
}

/// Nested tree entry for dependency output: `direct` stops at one level of
/// children, `verbose` shows each edge's version floor.
pub(crate) fn print_tree_roots(nodes: &[DependencyTreeNode], direct: bool, verbose: bool) {
    print_dependency_tree(nodes, direct, verbose, RootStyle::SiblingBranches);
}

fn print_dependency_tree(
    nodes: &[DependencyTreeNode],
    direct: bool,
    verbose: bool,
    root_style: RootStyle,
) {
    let options = TreeRenderOptions {
        decorated: true,
        direct,
        verbose,
        show_versions: true,
        version_label: None,
        root_style,
    };
    for line in render_dependency_tree(nodes, options) {
        println!("{line}");
    }
}

/// `glu list --json` record. Declaration order is the serialized order —
/// `name` first, then `version`, matching `brew info --json` conventions.
#[derive(serde::Serialize, schemars::JsonSchema)]
struct ListRecord {
    name: String,
    version: String,
    keg_only: bool,
    declared: bool,
    active: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ListFlatResult {
    packages: Vec<ListRecord>,
    scope: ListScope,
    view: FlatView,
}

/// `glu list --json`: stable object-shaped result, never decorated.
pub(crate) fn print_list_json(
    scope: ListScope,
    packages: &[InstalledPackage],
    statuses: &BTreeMap<glu_core::PackageKey, glu_client::deps::PackageStatus>,
) {
    let records: Vec<ListRecord> = packages
        .iter()
        .map(|package| {
            let status = statuses.get(&package.package_key);
            ListRecord {
                name: package.name.0.clone(),
                version: package.keg_version.0.clone(),
                keg_only: package.keg_only,
                declared: status.is_some_and(|status| status.declared),
                active: !status.is_some_and(|status| status.deactivated),
            }
        })
        .collect();
    let result = ListResult::Flat(ListFlatResult {
        packages: records,
        scope,
        view: FlatView::Flat,
    });
    print_json_success(CommandId::List, &result);
}

fn render_upgrade_output(result: &glu_client::upgrade::UpgradeResult) {
    match result.status {
        glu_client::upgrade::UpgradeStatus::AlreadyCurrent => {
            println!("Already up to date (glu {}).", result.version);
        }
        glu_client::upgrade::UpgradeStatus::Updated => {
            println!("glu updated to {}.", result.version);
        }
    }
}

fn render_setup_output(result: &SetupResult) {
    println!("Configured shell integration:");
    for shell in &result.shells {
        println!("  {:<5} {}", shell.name, display_path(&shell.config_path));
    }
    if let Some(rc) = &result.current_rc {
        println!();
        println!("To use glu in this terminal now:");
        println!("  source {}", display_path(rc));
    }
}

pub(crate) fn print_info(info: &InfoResponse, installed: Option<&InstalledPackage>) {
    match &info.desc {
        Some(desc) => println!(
            "{} {}   {desc}",
            info.name.0,
            glu_client::style::dim(&info.version)
        ),
        None => println!("{} {}", info.name.0, glu_client::style::dim(&info.version)),
    }
    match (&info.homepage, &info.license) {
        (Some(homepage), Some(license)) => println!("{homepage} · {license}"),
        (Some(homepage), None) => println!("{homepage}"),
        (None, Some(license)) => println!("{license}"),
        (None, None) => {}
    }
    println!();

    println!(
        "  {:<16} {}",
        "Installed",
        match installed {
            Some(package) => format!("yes ({})", package.version),
            None => "no".to_string(),
        }
    );
    println!(
        "  {:<16} {}  ·  {} with dependencies",
        "Download size",
        fmt_size(info.download_bytes),
        fmt_size(info.download_bytes_with_dependencies)
    );
    println!(
        "  {:<16} {}  ·  {} with dependencies",
        "Installed size",
        fmt_size(info.installed_bytes),
        fmt_size(info.installed_bytes_with_dependencies)
    );
    println!(
        "  {:<16} {} total ({} direct)",
        "Dependencies", info.dependencies.total, info.dependencies.direct
    );
}

fn fmt_size(bytes: Option<u64>) -> String {
    bytes
        .map(glu_client::format::human_bytes)
        .unwrap_or_else(|| "—".to_string())
}

/// Prints an error to stderr in red, with quoted suggestion names in the
/// `Did you mean ...?` lines bolded. Falls back to plain text when styling
/// is disabled (piped / NO_COLOR).
fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, version: &str, children: Vec<DependencyTreeNode>) -> DependencyTreeNode {
        DependencyTreeNode {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            package: glu_core::PackageId(format!("pkg:test/{name}@{version}")),
            name: name.to_string(),
            canonical_name: glu_core::PackageName(name.to_string()),
            version: version.to_string(),
            children,
            already_shown: false,
            incoming: None,
        }
    }

    #[test]
    fn dependency_graph_json_dedupes_nodes_but_keeps_edges() {
        let repeated = DependencyTreeNode {
            already_shown: true,
            incoming: Some(glu_client::dependency_query::DependencyTreeEdge {
                requested_as: glu_core::PackageSelector("shared".to_string()),
                reversed: false,
                minimum: Some(glu_core::MinimumVersion {
                    version: "1.0".to_string(),
                    revision: None,
                }),
            }),
            ..node("shared", "1.0", Vec::new())
        };
        let tree = vec![node(
            "root",
            "2.0",
            vec![node("shared", "1.0", Vec::new()), repeated],
        )];

        let graph = dependency_graph_json(&tree, false, &BTreeMap::new());
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.roots, vec!["pkg:test/root@2.0"]);
        assert!(graph.nodes.contains_key("pkg:test/root@2.0"));
        assert!(graph.nodes.contains_key("pkg:test/shared@1.0"));
        let edges = graph.edges.get("pkg:test/root@2.0").unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].to, "pkg:test/shared@1.0");
        assert_eq!(edges[1].to, "pkg:test/shared@1.0");
        assert_eq!(edges[1].requires.as_deref(), Some(">= 1.0"));
    }

    #[test]
    fn query_outputs_keep_edge_selector_and_dedupe_by_package_identity() {
        let mut alias = node("llvm", "22.1", Vec::new());
        alias.name = "llvm@22".to_string();
        alias.incoming = Some(glu_client::dependency_query::DependencyTreeEdge {
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
            reversed: false,
            minimum: Some(glu_core::MinimumVersion {
                version: "22.1".to_string(),
                revision: None,
            }),
        });
        let tree = vec![node("rust", "1.0", vec![alias])];

        let lines = render_dependency_tree(&tree, TreeRenderOptions::plain());
        assert_eq!(lines, vec!["rust 1.0", "llvm@22 22.1"]);

        let records = dependency_records(&tree[0].children, false, &BTreeMap::new());
        assert_eq!(records[0].package_key, "package:llvm");
        assert_eq!(records[0].package, "pkg:test/llvm@22.1");
        assert_eq!(records[0].name, "llvm@22");
        assert_eq!(records[0].requested_as.as_deref(), Some("llvm@22"));

        let graph = dependency_graph_json(&tree, false, &BTreeMap::new());
        let edge = &graph.edges["pkg:test/rust@1.0"][0];
        assert_eq!(edge.to, "pkg:test/llvm@22.1");
        assert_eq!(edge.requested_as, "llvm@22");
        assert_eq!(graph.nodes["pkg:test/llvm@22.1"].name, "llvm");
        assert_eq!(
            graph.nodes["pkg:test/llvm@22.1"].package_key,
            "package:llvm"
        );
    }

    #[test]
    fn flat_and_nul_projection_does_not_merge_colliding_display_names() {
        let first = node("first", "1.0", Vec::new());
        let mut second = node("second", "2.0", Vec::new());
        second.name = first.name.clone();
        let mut flat = Vec::new();

        flatten_tree_unique(&[first, second], false, &mut flat);

        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].0, "first");
        assert_eq!(flat[1].0, "first");
        assert_eq!(flat[0].1, "1.0");
        assert_eq!(flat[1].1, "2.0");
    }

    #[test]
    fn dependency_graph_json_direct_keeps_only_one_edge_level() {
        let tree = vec![node(
            "root",
            "1.0",
            vec![node(
                "child",
                "1.0",
                vec![node("grandchild", "1.0", Vec::new())],
            )],
        )];

        let graph = dependency_graph_json(&tree, true, &BTreeMap::new());
        assert_eq!(graph.roots, vec!["pkg:test/root@1.0"]);
        assert_eq!(graph.edges.get("pkg:test/root@1.0").unwrap().len(), 1);
        assert_eq!(
            graph.edges.get("pkg:test/root@1.0").unwrap()[0].to,
            "pkg:test/child@1.0"
        );
        assert!(!graph.edges.contains_key("pkg:test/child@1.0"));
        assert!(!graph.nodes.contains_key("pkg:test/grandchild@1.0"));
    }

    #[test]
    fn verbose_deps_uses_local_version_not_resolved_candidate() {
        let record = DependencyRecord {
            package_key: "package:glib".to_string(),
            package: "pkg:test/glib@9.9-candidate".to_string(),
            requested_as: Some("glib@2".to_string()),
            reversed: false,
            name: "glib@2".to_string(),
            version: "9.9-candidate".to_string(),
            installed: true,
            linked: Some(true),
            declared: false,
            deactivated: false,
            direct: true,
            transitive: false,
            download_bytes: None,
            installed_bytes: None,
        };
        let statuses = BTreeMap::from([(
            glu_core::PackageKey("package:glib".to_string()),
            glu_client::deps::PackageStatus {
                installed: true,
                installed_version: Some("2.82.0".to_string()),
                linked: true,
                declared: false,
                deactivated: false,
                download_bytes: None,
                installed_bytes: None,
            },
        )]);

        assert_eq!(
            package_list::render_primitive(&[deps_list_item(&record, &statuses, false, false)]),
            "glib@2"
        );
        assert_eq!(
            package_list::render_primitive(&[deps_list_item(&record, &statuses, true, false)]),
            "glib@2 (2.82.0 installed)"
        );
    }

    #[test]
    fn dependency_json_records_include_status_annotations() {
        let tree = vec![node(
            "direct",
            "1.0",
            vec![node("transitive", "2.0", Vec::new())],
        )];
        let statuses = BTreeMap::from([(
            glu_core::PackageKey("package:direct".to_string()),
            glu_client::deps::PackageStatus {
                installed: true,
                installed_version: Some("1.0".to_string()),
                linked: true,
                declared: false,
                deactivated: false,
                download_bytes: Some(10),
                installed_bytes: Some(20),
            },
        )]);

        let records = dependency_records(&tree, false, &statuses);

        assert_eq!(records.len(), 2);
        assert!(records[0].installed);
        assert_eq!(records[0].linked, Some(true));
        assert!(records[0].direct);
        assert!(!records[0].transitive);
        assert_eq!(records[0].download_bytes, Some(10));
        assert!(!records[1].installed);
        assert!(!records[1].direct);
        assert!(records[1].transitive);
    }

    #[test]
    fn why_flat_reports_declared_root_causes_only() {
        let root = node(
            "nss",
            "3.127",
            vec![node(
                "poppler",
                "26.08.0",
                vec![node("vips", "8.18.6", Vec::new())],
            )],
        );
        let statuses = BTreeMap::from([
            (
                glu_core::PackageKey("package:poppler".to_string()),
                glu_client::deps::PackageStatus {
                    installed: true,
                    declared: false,
                    ..Default::default()
                },
            ),
            (
                glu_core::PackageKey("package:vips".to_string()),
                glu_client::deps::PackageStatus {
                    installed: true,
                    declared: true,
                    ..Default::default()
                },
            ),
        ]);

        let records = why_root_cause_records(&root, &statuses);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "vips");
        assert!(!records[0].direct);
        assert!(records[0].transitive);
    }

    #[test]
    fn why_tree_json_elides_target_with_target_relative_depths() {
        let root = node(
            "nss",
            "3.127",
            vec![node(
                "poppler",
                "26.08.0",
                vec![node("vips", "8.18.6", Vec::new())],
            )],
        );

        let graph = dependency_graph_json_at_depth(&root.children, false, 1, &BTreeMap::new());

        assert_eq!(graph.roots, vec!["pkg:test/poppler@26.08.0"]);
        assert!(!graph.nodes.contains_key("pkg:test/nss@3.127"));
        assert!(graph.nodes["pkg:test/poppler@26.08.0"].direct);
        assert!(graph.nodes["pkg:test/vips@8.18.6"].transitive);
    }

    #[test]
    fn install_plan_emphasizes_only_resolved_roots() {
        fn mutation(name: &str, direct: bool) -> MutationPackageRecord {
            MutationPackageRecord {
                package_key: Some(format!("package:{name}")),
                name: name.to_string(),
                version: "1.0".to_string(),
                status: MutationStatus::WouldInstall,
                installed: None,
                linked: None,
                declared: None,
                deactivated: None,
                direct: Some(direct),
                transitive: Some(!direct),
                cached: None,
                download_bytes: None,
                installed_bytes: None,
            }
        }

        let tree = vec![
            node(
                "requested-a",
                "1.0",
                vec![node("immediate-dependency", "1.0", Vec::new())],
            ),
            node("requested-b", "1.0", Vec::new()),
        ];
        let requested = dependency_root_keys(&tree);
        let items = install_package_items(
            &[
                mutation("requested-a", false),
                mutation("immediate-dependency", true),
                mutation("requested-b", false),
            ],
            &requested,
        );

        assert!(items[0].emphasized);
        assert!(!items[1].emphasized);
        assert!(items[2].emphasized);
    }

    #[test]
    fn tree_plan_json_preserves_graph_and_local_status() {
        let tree = vec![node(
            "root",
            "2.0",
            vec![node("installed-dep", "1.0", Vec::new())],
        )];
        let statuses = BTreeMap::from([(
            glu_core::PackageKey("package:installed-dep".to_string()),
            glu_client::deps::PackageStatus {
                installed: true,
                installed_version: Some("1.0".to_string()),
                linked: true,
                declared: false,
                deactivated: false,
                download_bytes: Some(10),
                installed_bytes: Some(20),
            },
        )]);
        let plan = InstallPlanOutput {
            mode: PlanMode::Plan,
            requested: Vec::new(),
            would_install: Vec::new(),
            satisfied: Vec::new(),
            would_promote: Vec::new(),
            would_rename: Vec::new(),
            would_remove: Vec::new(),
            requires_confirmation: false,
            would_download_bytes: None,
            dependency_tree: tree,
        };
        let value = serde_json::to_value(InstallPlanTreeResult::new(&plan, &statuses)).unwrap();

        assert_eq!(value["view"], "tree");
        assert_eq!(value["roots"], serde_json::json!(["pkg:test/root@2.0"]));
        assert_eq!(value["nodes"]["pkg:test/root@2.0"]["direct"], false);
        assert_eq!(value["nodes"]["pkg:test/installed-dep@1.0"]["direct"], true);
        assert_eq!(
            value["nodes"]["pkg:test/installed-dep@1.0"]["installed"],
            true
        );
    }

    fn assert_result_valid<T: serde::Serialize>(name: &str, result: &T) {
        let schema = result_schema(name).unwrap();
        let value = serde_json::to_value(result).unwrap();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "{name} rejected {value}: {errors:?}");
    }

    fn empty_execution() -> ExecutionSummaryRecord {
        ExecutionSummaryRecord {
            trace_path: None,
            elapsed_seconds: 0.0,
            timing_breakdown: None,
            stats: None,
            timing_description: None,
            pool_stats: None,
        }
    }

    #[test]
    fn generated_result_schemas_accept_every_operational_variant() {
        let tree = vec![node("root", "1.0", Vec::new())];
        let graph = || dependency_graph_json(&tree, false, &BTreeMap::new());

        assert_result_valid(
            "ListResult",
            &ListResult::Flat(ListFlatResult {
                packages: Vec::new(),
                scope: ListScope::Declared,
                view: FlatView::Flat,
            }),
        );
        assert_result_valid(
            "ListResult",
            &ListResult::Tree(ListTreeResult {
                scope: ListScope::Installed,
                view: TreeView::Tree,
                graph: graph(),
            }),
        );

        assert_result_valid(
            "DepsResult",
            &DepsResult::Flat(DepsFlatResult {
                dependencies: Vec::new(),
                direct: false,
                installed: true,
                source: DepsSource::Installed,
                tree: false,
            }),
        );
        assert_result_valid(
            "DepsResult",
            &DepsResult::Tree(DepsTreeResult {
                source: DepsSource::Resolved,
                installed: false,
                direct: false,
                view: TreeView::Tree,
                graph: graph(),
            }),
        );

        assert_result_valid(
            "ReverseDepsResult",
            &ReverseDepsResult::Flat(ReverseDepsFlatResult {
                direct: false,
                dependents: Vec::new(),
                source: ReverseDepsSource::Installed,
                target: "root",
                tree: false,
            }),
        );
        assert_result_valid(
            "ReverseDepsResult",
            &ReverseDepsResult::Tree(ReverseDepsTreeResult {
                target: "root",
                source: ReverseDepsSource::Registry,
                direct: false,
                view: TreeView::Tree,
                graph: graph(),
            }),
        );

        let info = InfoOutput {
            package: glu_core::InfoResponse {
                schema: "glu.info.v1".to_string(),
                requested_as: glu_core::PackageSelector("root".to_string()),
                package_key: glu_core::PackageKey("package:root".to_string()),
                package: glu_core::PackageId("root@1.0".to_string()),
                name: PackageName("root".to_string()),
                version: "1.0".to_string(),
                desc: None,
                homepage: None,
                license: None,
                dependencies: glu_core::InfoDependencies {
                    direct: 0,
                    total: 0,
                },
                download_bytes: None,
                installed_bytes: None,
                download_bytes_with_dependencies: None,
                installed_bytes_with_dependencies: None,
                bottle: "test".to_string(),
            },
            installed: None,
            declared: false,
            deactivated: false,
        };
        assert_result_valid("InfoResult", &InfoResult::One(&info));
        let info_many = InfoManyOutput {
            packages: Vec::new(),
        };
        assert_result_valid("InfoResult", &InfoResult::Many(&info_many));

        let install = InstallOutput {
            mode: ExecutedMode::Executed,
            requested: Vec::new(),
            installed: Vec::new(),
            satisfied: Vec::new(),
            promoted: Vec::new(),
            renamed: Vec::new(),
            removed: Vec::new(),
            execution: empty_execution(),
        };
        assert_result_valid("InstallResult", &InstallResult::Executed(&install));
        let install_plan = InstallPlanOutput {
            mode: PlanMode::Plan,
            requested: Vec::new(),
            would_install: Vec::new(),
            satisfied: Vec::new(),
            would_promote: Vec::new(),
            would_rename: Vec::new(),
            would_remove: Vec::new(),
            requires_confirmation: false,
            would_download_bytes: None,
            dependency_tree: tree.clone(),
        };
        assert_result_valid("InstallResult", &InstallResult::PlanFlat(&install_plan));
        assert_result_valid(
            "InstallResult",
            &InstallResult::PlanTree(InstallPlanTreeResult::new(&install_plan, &BTreeMap::new())),
        );

        let reinstall = ReinstallOutput {
            mode: ExecutedMode::Executed,
            requested: Vec::new(),
            reinstalled: Vec::new(),
            satisfied: Vec::new(),
            renamed: Vec::new(),
            removed: Vec::new(),
            execution: empty_execution(),
        };
        assert_result_valid("ReinstallResult", &ReinstallResult::Executed(&reinstall));
        let reinstall_plan = ReinstallPlanOutput {
            mode: PlanMode::Plan,
            requested: Vec::new(),
            would_reinstall: Vec::new(),
            satisfied: Vec::new(),
            would_rename: Vec::new(),
            would_remove: Vec::new(),
            requires_confirmation: false,
            would_download_bytes: None,
        };
        assert_result_valid("ReinstallResult", &ReinstallResult::Plan(&reinstall_plan));

        let update = UpdateOutput {
            mode: ExecutedMode::Executed,
            updates: Vec::new(),
            removed: Vec::new(),
            execution: empty_execution(),
            latest_glu_version: None,
            broad: false,
        };
        assert_result_valid("UpdateResult", &UpdateResult::Executed(&update));
        let update_plan = UpdatePlanOutput {
            mode: PlanMode::Plan,
            would_update: Vec::new(),
            would_remove: Vec::new(),
            requires_confirmation: false,
            broad: false,
            would_download_bytes: None,
            latest_glu_version: None,
            dependency_tree: tree,
        };
        assert_result_valid("UpdateResult", &UpdateResult::PlanFlat(&update_plan));
        assert_result_valid(
            "UpdateResult",
            &UpdateResult::PlanTree(UpdatePlanTreeResult::new(&update_plan, &BTreeMap::new())),
        );

        let activation = ActivationOutput {
            force: false,
            packages: Vec::new(),
        };
        assert_result_valid("ActivationResult", &activation);
        let deactivation = DeactivationOutput {
            packages: Vec::new(),
        };
        assert_result_valid("DeactivationResult", &deactivation);

        let removal = RemovalOutput {
            mode: ExecutedMode::Executed,
            removed: Vec::new(),
            kept: Vec::new(),
            leftover_config_files: Vec::new(),
        };
        assert_result_valid("RemovalResult", &RemovalResult::Executed(&removal));
        let removal_plan = RemovalPlanOutput {
            mode: PlanMode::Plan,
            named: Vec::new(),
            would_remove: Vec::new(),
            would_keep: Vec::new(),
            requires_confirmation: false,
        };
        assert_result_valid("RemovalResult", &RemovalResult::Plan(&removal_plan));

        let autoremove = AutoremoveOutput {
            mode: ExecutedMode::Executed,
            packages: Vec::new(),
        };
        assert_result_valid("AutoremoveResult", &AutoremoveResult::Executed(&autoremove));
        let autoremove_plan = AutoremovePlanOutput {
            mode: PlanMode::Plan,
            would_remove: Vec::new(),
            requires_confirmation: false,
        };
        assert_result_valid(
            "AutoremoveResult",
            &AutoremoveResult::Plan(&autoremove_plan),
        );

        let cleanup = CleanupOutput {
            mode: ExecutedMode::Executed,
            removed: vec![CachedDownloadRecord {
                name: "demo".to_string(),
                version: "1.0".to_string(),
                downloads: 1,
                bytes: 42,
            }],
            removed_downloads: 2,
            unassociated_downloads: 1,
            unassociated_bytes: 10,
            reclaimed_bytes: 52,
        };
        assert_result_valid("CleanupResult", &CleanupResult::Executed(&cleanup));
        let cleanup_plan = CleanupPlanOutput {
            mode: PlanMode::Plan,
            would_remove: vec![CachedDownloadRecord {
                name: "demo".to_string(),
                version: "1.0".to_string(),
                downloads: 1,
                bytes: 42,
            }],
            would_remove_downloads: 2,
            unassociated_downloads: 1,
            unassociated_bytes: 10,
            requires_confirmation: true,
            would_reclaim_bytes: 52,
        };
        assert_result_valid("CleanupResult", &CleanupResult::Plan(&cleanup_plan));

        let status = StatusOutput {
            version: "0.1.0",
            prefix: "/opt/glustore".to_string(),
            prefix_source: "default".to_string(),
            prefix_length: 13,
            fixed_cellar_length: 13,
            target: "arm64_test".to_string(),
            registry: "https://registry.glu.run".to_string(),
            distribution: "https://example.invalid".to_string(),
            installed_count: 0,
            declared_count: 0,
            deactivated_count: 0,
            deactivated: Vec::new(),
            shells: Vec::new(),
        };
        assert_result_valid("StatusResult", &status);
        let outdated = OutdatedOutput {
            scope: "installed",
            packages: Vec::new(),
            installed_client_version: "0.1.0",
            latest_glu_version: None,
            human_packages: Vec::new(),
        };
        assert_result_valid("OutdatedResult", &outdated);
    }

    #[test]
    fn download_summary_uses_human_units_and_hides_zero() {
        assert_eq!(
            would_download_message(Some(31_513_444)).as_deref(),
            Some("Would download: 31.5 MB")
        );
        assert_eq!(would_download_message(Some(0)), None);
        assert_eq!(would_download_message(None), None);
    }

    #[test]
    fn update_tree_replaces_flat_list_and_omits_unchanged_nodes() {
        let tree = vec![node(
            "root",
            "2.0",
            vec![
                node("updated", "3.0", Vec::new()),
                node("unchanged", "1.0", Vec::new()),
            ],
        )];
        let filtered = update_only_tree(
            &tree,
            &[
                (
                    glu_core::PackageKey("package:root".to_string()),
                    "1.0".to_string(),
                    "2.0".to_string(),
                ),
                (
                    glu_core::PackageKey("package:updated".to_string()),
                    "2.0".to_string(),
                    "3.0".to_string(),
                ),
            ],
        );

        assert_eq!(filtered.nodes.len(), 1);
        assert_eq!(filtered.nodes[0].name, "root");
        assert_eq!(filtered.nodes[0].version, "1.0 → 2.0");
        assert_eq!(filtered.nodes[0].children.len(), 1);
        assert_eq!(filtered.nodes[0].children[0].name, "updated");
        assert_eq!(filtered.nodes[0].children[0].version, "2.0 → 3.0");
        assert!(filtered.context.is_empty());
    }

    #[test]
    fn install_tree_retains_only_required_installed_context() {
        let tree = vec![node(
            "root",
            "1.0",
            vec![
                node("new-direct", "1.0", Vec::new()),
                node(
                    "already-installed",
                    "1.0",
                    vec![node("new-transitive", "1.0", Vec::new())],
                ),
            ],
        )];
        let mut would_install = vec![
            plain_mutation_package_record(
                "root".to_string(),
                "1.0".to_string(),
                MutationStatus::WouldInstall,
            ),
            plain_mutation_package_record(
                "new-direct".to_string(),
                "1.0".to_string(),
                MutationStatus::WouldInstall,
            ),
            plain_mutation_package_record(
                "new-transitive".to_string(),
                "1.0".to_string(),
                MutationStatus::WouldInstall,
            ),
        ];
        for record in &mut would_install {
            record.package_key = Some(format!("package:{}", record.name));
        }

        let filtered = install_only_tree(&tree, &would_install);
        assert_eq!(filtered.nodes.len(), 1);
        assert_eq!(filtered.nodes[0].name, "root");
        assert_eq!(
            filtered.nodes[0]
                .children
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            vec!["new-direct", "already-installed"]
        );
        assert_eq!(
            filtered.nodes[0].children[1].children[0].name,
            "new-transitive"
        );
        assert!(filtered.context.contains(&(
            glu_core::PackageKey("package:already-installed".to_string()),
            "1.0".to_string(),
        )));
    }

    #[test]
    fn package_sections_keep_package_details() {
        let many = [
            PackageListItem::package("vips", "8.18.6"),
            PackageListItem::package("glib", "2.88.3"),
        ];

        assert_eq!(
            package_list::render_section("Will remove", &many),
            "Will remove 2 packages:\n  ▪ glib 2.88.3\n  ▪ vips 8.18.6"
        );
    }

    #[test]
    fn install_json_timing_breakdown_is_structured() {
        let summary = glu_client::install::InstallSummary {
            requested: vec![glu_core::PackageSelector("node".to_string())],
            execution: glu_client::install::WorksetExecutionSummary {
                elapsed_seconds: 6.1,
                timing_breakdown: Some(glu_client::install::WorksetTimingBreakdown {
                    download_seconds: 4.9,
                    cache_rebuild_seconds: 0.0,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let value = serde_json::to_value(install_output(&summary)).unwrap();
        assert_eq!(
            value["execution"]["timing_breakdown"],
            serde_json::json!({
                "download_seconds": 4.9,
                "cache_rebuild_seconds": 0.0,
            })
        );
        assert!(value["execution"]["timing_breakdown"].is_object());
    }
}
