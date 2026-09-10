#[cfg(test)]
use crate::events::SilentExecutionEvents;
use crate::{
    dependency_query::{dependency_forest_from_manifest, DependencyTreeNode},
    events::{
        ExecutionEvents, NodeCompletionStatus, OutputStream, ProgressEvent, ProgressFinishStatus,
    },
    install::{
        orchestrator::{execute_install_plan, ExecuteInstallPlanInput},
        scheduler::{ExecutionContext, InstallResult},
        summary::InstallTimingSummary,
    },
    postinstall::structured::PostinstallPlans,
    state::Declaration,
    state::{snapshot::StateSnapshot, store::InstalledStateStore},
    trace::writer::write_install_trace,
    validation::validate_client_support,
    GluClient,
};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

/// How commands update declaration membership. `glu install` marks roots
/// declared, while `glu update` / `glu reinstall` preserve membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeclaredPolicy {
    /// `glu install`: a package becomes declared exactly when it is a root
    /// of this command or was already declared. Installing an automatic
    /// package promotes it.
    #[default]
    Install,
    /// `glu update` / `glu reinstall`: membership is unchanged. New
    /// packages appearing mid-command (e.g. a dependency the new version
    /// introduced) are automatic.
    Preserve,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Repour even if already installed (wipes the existing keg).
    pub force: bool,
    /// Include the full dependency closure (requires `force` on `install`).
    pub deps: bool,
    /// Skip the confirmation prompt when sync would remove dangling
    /// packages (the trailing autoremove of a full sync).
    pub yes: bool,
    pub verbose: bool,
    /// How the command updates declaration membership; see `DeclaredPolicy`.
    pub declared_policy: DeclaredPolicy,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HostStartupDiagnostics {
    pub argument_seconds: f64,
    pub command_validation_seconds: f64,
    pub config_seconds: f64,
    pub operation_lock_seconds: f64,
    pub recovery_seconds: f64,
    pub main_entry_to_plan_seconds: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InstallStartupDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStartupDiagnostics>,
    pub registry_resolve_seconds: f64,
    pub manifest_validation_seconds: f64,
    pub state_snapshot_seconds: f64,
    pub resolve_and_state_wall_seconds: f64,
    pub plan_finalize_seconds: f64,
    pub plan_total_seconds: f64,
    pub execution_setup_seconds: f64,
    pub command_to_execution_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_requests: Option<crate::registry::resolve_client::RegistryRequestDiagnostics>,
}
use glu_core::{InstalledPackage, PackageId, PackageName, PackageSelector, ResolveRequest};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageChangeStatus {
    AlreadyInstalled,
    AlreadyInstalledPromotedToDeclared,
    Installed,
    InstalledOlderThanResolved,
    WouldInstall,
    WouldUpdate,
}

#[derive(Debug, Clone)]
pub struct PackageChange {
    pub package_key: glu_core::PackageKey,
    pub name: PackageName,
    pub version: String,
    pub exposure: glu_core::Exposure,
    pub status: PackageChangeStatus,
    pub installed: Option<bool>,
    pub linked: Option<bool>,
    pub declared: Option<bool>,
    pub deactivated: Option<bool>,
    pub direct: Option<bool>,
    pub transitive: Option<bool>,
    pub cached: Option<bool>,
    pub download_bytes: Option<u64>,
    pub installed_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RenameChange {
    pub package_key: glu_core::PackageKey,
    pub old_name: PackageName,
    pub new_name: PackageName,
    pub version: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorksetExecutionSummary {
    pub trace_path: Option<PathBuf>,
    pub elapsed_seconds: f64,
    pub timing_breakdown: Option<WorksetTimingBreakdown>,
    pub timing_description: Option<String>,
    pub stats: Option<orchestrator::InstallStatsSnapshot>,
    pub pool_stats: Option<orchestrator::InstallPoolStatsSnapshot>,
}

#[derive(Debug, Clone, Copy)]
pub struct WorksetTimingBreakdown {
    pub download_seconds: f64,
    pub cache_rebuild_seconds: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PartialInstallFailure {
    pub message: String,
    pub report: PartialInstallReport,
}

impl std::fmt::Display for PartialInstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PartialInstallFailure {}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PartialInstallReport {
    pub failure_code: crate::error::RuntimeErrorCode,
    pub failed_node_id: Option<String>,
    pub failed_phase: Option<String>,
    pub failed_kind: Option<String>,
    pub failed_error: Option<String>,
    pub installed: Vec<PartialInstallPackage>,
    pub failed: Vec<PartialInstallPackage>,
    pub skipped: Vec<PartialInstallPackage>,
    pub partial: Vec<PartialKeg>,
    pub trace_path: Option<PathBuf>,
    pub trace_id: Option<String>,
    pub suggested_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialInstallStatus {
    Failed,
    Installed,
    Skipped,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PartialInstallPackage {
    pub name: String,
    pub version: String,
    pub package_id: String,
    pub status: PartialInstallStatus,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PartialKeg {
    pub name: String,
    pub version: String,
    pub package_id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub requested: Vec<PackageSelector>,
    pub would_install: Vec<PackageChange>,
    pub satisfied: Vec<PackageChange>,
    pub promoted: Vec<PackageChange>,
    pub renamed: Vec<RenameChange>,
    pub would_remove: Vec<InstalledPackage>,
    pub requires_confirmation: bool,
    pub would_download_bytes: Option<u64>,
    pub(crate) manifest: glu_core::InstallManifest,
    pub(crate) workset: planner::InstallWorkSet,
    pub(crate) declaration_after: Declaration,
    pub(crate) deactivated_after: BTreeSet<PackageName>,
    declared_before: BTreeSet<PackageName>,
    command_start: std::time::Instant,
    startup_diagnostics: InstallStartupDiagnostics,
}

impl InstallPlan {
    pub fn set_host_startup_diagnostics(&mut self, diagnostics: HostStartupDiagnostics) {
        self.startup_diagnostics.host = Some(diagnostics);
    }

    /// The resolved plan graph, kept semantic so presentation layers can
    /// render human or machine tree views without re-resolving.
    pub fn dependency_tree(&self) -> Vec<DependencyTreeNode> {
        dependency_forest_from_manifest(&self.manifest)
    }

    pub fn resolved_root_keys(&self) -> BTreeSet<glu_core::PackageKey> {
        self.manifest
            .roots
            .iter()
            .map(|root| root.package_key.clone())
            .collect()
    }

    pub fn resolved_version(&self, name: &PackageName) -> Option<&str> {
        self.manifest
            .packages
            .values()
            .find(|package| &package.name == name)
            .map(|package| package.version.as_str())
    }

    /// Preserves an external source's explicit unlink intent for newly
    /// declared roots, after resolve has supplied authoritative exposure.
    /// Existing glu declarations win, and isolated packages are not confused
    /// with deactivated global packages merely because they have no public
    /// linked marker in the source package manager.
    pub(crate) fn preserve_deactivation_for_new_global_roots(
        &mut self,
        unlinked_requested_as: &BTreeSet<PackageSelector>,
    ) -> Vec<PackageName> {
        let mut inferred = BTreeSet::new();
        for root in &self.manifest.roots {
            if !unlinked_requested_as.contains(&root.requested_as) {
                continue;
            }
            let Some(package) = self.manifest.packages.get(&root.package) else {
                continue;
            };
            if !matches!(package.exposure, glu_core::Exposure::Global)
                || self.declared_before.contains(&package.name)
            {
                continue;
            }
            self.declaration_after
                .deactivated
                .insert(package.name.clone(), true);
            self.deactivated_after.insert(package.name.clone());
            inferred.insert(package.name.clone());
        }
        inferred.into_iter().collect()
    }
}

#[cfg(test)]
mod migration_tests;

#[derive(Debug, Clone, Default)]
pub struct InstallSummary {
    pub requested: Vec<PackageSelector>,
    pub resolved_root_keys: BTreeSet<glu_core::PackageKey>,
    pub installed: Vec<PackageChange>,
    pub satisfied: Vec<PackageChange>,
    pub promoted: Vec<PackageChange>,
    pub renamed: Vec<RenameChange>,
    pub removed: Vec<crate::remove::RemovedPackage>,
    pub execution: WorksetExecutionSummary,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateSummary {
    pub updates: Vec<PlannedUpdate>,
    pub removed: Vec<crate::remove::RemovedPackage>,
    pub execution: WorksetExecutionSummary,
    pub latest_glu_version: Option<String>,
    pub broad: bool,
}

pub mod dag;
pub mod graph;
mod manifest_lookup;
pub mod orchestrator;
pub mod planner;
pub mod scheduler;
pub mod summary;

#[cfg(test)]
pub(crate) mod fault {
    use anyhow::{bail, Result};
    use glu_core::PackageName;
    use std::sync::Mutex;

    static FAIL_AFTER_PREPARED_RECEIPT_FOR: Mutex<Option<String>> = Mutex::new(None);
    static FAIL_AFTER_COMMIT_FOR: Mutex<Option<String>> = Mutex::new(None);

    pub(crate) struct FaultGuard;

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            *FAIL_AFTER_PREPARED_RECEIPT_FOR.lock().unwrap() = None;
            *FAIL_AFTER_COMMIT_FOR.lock().unwrap() = None;
        }
    }

    pub(crate) fn fail_after_prepared_receipt_for(name: &str) -> FaultGuard {
        *FAIL_AFTER_PREPARED_RECEIPT_FOR.lock().unwrap() = Some(name.to_string());
        FaultGuard
    }

    pub(crate) fn fail_after_commit_for(name: &str) -> FaultGuard {
        *FAIL_AFTER_COMMIT_FOR.lock().unwrap() = Some(name.to_string());
        FaultGuard
    }

    pub(crate) fn after_prepared_receipt(package: &PackageName) -> Result<()> {
        let mut target = FAIL_AFTER_PREPARED_RECEIPT_FOR.lock().unwrap();
        if target.as_deref() == Some(package.0.as_str()) {
            *target = None;
            bail!("injected failure after staging receipt for {}", package.0);
        }
        Ok(())
    }

    pub(crate) fn after_commit(package: &PackageName) -> Result<()> {
        let mut target = FAIL_AFTER_COMMIT_FOR.lock().unwrap();
        if target.as_deref() == Some(package.0.as_str()) {
            *target = None;
            bail!("injected failure after committing {}", package.0);
        }
        Ok(())
    }
}

pub async fn plan_install(
    client: &GluClient,
    names: Vec<PackageSelector>,
    options: InstallOptions,
) -> Result<InstallPlan> {
    let command_start = std::time::Instant::now();
    let requested = names.clone();
    if names.is_empty() {
        bail!("install requires at least one package name");
    }

    let resolve_and_state_started = std::time::Instant::now();
    let state_started = std::time::Instant::now();
    let prefix_for_state = client.config().prefix.clone();
    let state_task = tokio::task::spawn_blocking(move || StateSnapshot::load(&prefix_for_state));
    let ((manifest, resolve_timing), (snapshot, state_snapshot_seconds)) =
        tokio::try_join!(resolve_manifest_timed(client, names), async {
            let snapshot = state_task
                .await
                .map_err(|error| anyhow::anyhow!("state snapshot loader panicked: {error}"))??;
            Ok::<_, anyhow::Error>((snapshot, state_started.elapsed().as_secs_f64()))
        })?;
    let resolve_and_state_wall_seconds = resolve_and_state_started.elapsed().as_secs_f64();
    let plan_finalize_started = std::time::Instant::now();
    let state = snapshot.installed;
    let declaration_before = snapshot.declaration;
    let mut declaration = declaration_before.clone();
    let mode = if options.force && options.deps {
        planner::WorksetMode::ReinstallDeps
    } else if options.force {
        planner::WorksetMode::Force
    } else {
        planner::WorksetMode::Install
    };
    let workset = planner::compute_workset(&manifest, &state, mode)?;
    let depths = manifest_depths(&manifest);
    let change_context = PackageChangeContext {
        manifest: &manifest,
        state: &state,
        declaration: &declaration_before,
        prefix: &client.config().prefix,
        depths: &depths,
    };
    let mut satisfied = Vec::new();
    let mut promoted = Vec::new();

    for root in &manifest.roots {
        let root_id = &root.package;
        if !workset.satisfied.contains(root_id) {
            continue;
        }
        let Some(package) = manifest.packages.get(root_id) else {
            continue;
        };
        let Some(installed) = state.find(&package.name) else {
            continue;
        };
        if options.declared_policy == DeclaredPolicy::Install
            && !declaration.contains(&package.name)
        {
            // Promoting an installed automatic package records it in the
            // declaration during execution. The installed version stands;
            // `glu up foo` is what bumps it.
            declaration
                .dependencies
                .insert(package.name.clone(), installed.keg_version.0.clone());
            if let Some(change) = package_change_for_id(
                &change_context,
                root_id,
                PackageChangeStatus::AlreadyInstalledPromotedToDeclared,
                Some(installed.keg_version.0.clone()),
            ) {
                promoted.push(change);
            }
        } else if installed.id == *root_id {
            if let Some(change) = package_change_for_id(
                &change_context,
                root_id,
                PackageChangeStatus::AlreadyInstalled,
                Some(installed.keg_version.0.clone()),
            ) {
                satisfied.push(change);
            }
        } else if let Some(change) = package_change_for_id(
            &change_context,
            root_id,
            PackageChangeStatus::InstalledOlderThanResolved,
            Some(installed.keg_version.0.clone()),
        ) {
            satisfied.push(change);
        }
    }

    // Record the resolved version for every declared package in this
    // command's closure (a reinstall or --force can land a new version
    // without changing membership).
    for package_id in &workset.install {
        if let Some(package) = manifest.packages.get(package_id) {
            if declaration.contains(&package.name) {
                declaration
                    .dependencies
                    .insert(package.name.clone(), package.keg_version.0.clone());
            }
        }
    }

    // Under Install policy every root enters the declaration, freshly
    // installed or not (a satisfied automatic root was already added by
    // the promotion loop above; or_insert keeps its installed version).
    if options.declared_policy == DeclaredPolicy::Install {
        for root in &manifest.roots {
            if let Some(package) = manifest.packages.get(&root.package) {
                declaration
                    .dependencies
                    .entry(package.name.clone())
                    .or_insert_with(|| package.keg_version.0.clone());
            }
        }
    }

    canonicalize_declaration_renames(&mut declaration, &manifest, &workset);

    // Full sync: plan the trailing autoremove from the predicted *final*
    // installed graph, not the stale pre-command graph. An interrupted earlier
    // install can leave automatic deps with receipts but leave the requested
    // root's receipt unwritten; those deps look dangling before this command,
    // but the root receipt this command will write re-reaches them. Prompting
    // and removal therefore use the simulated final graph below, and execution
    // reloads real state before deleting anything.
    let declared_after: BTreeSet<PackageName> = declaration.names();
    let predicted_dangling = crate::sync::predicted_dangling_after_workset(
        &state,
        &manifest,
        &workset,
        &declared_after,
    )?;
    let deactivated_after = declaration.deactivated_names();
    let would_download_bytes = download_bytes_for_uncached_package_ids(
        &manifest,
        &client.config().prefix,
        workset.install.iter(),
    );

    let mut plan = InstallPlan {
        requested,
        would_install: package_changes_for_ids(
            &manifest,
            &state,
            &declaration_before,
            &client.config().prefix,
            &depths,
            &workset.install,
            PackageChangeStatus::WouldInstall,
        ),
        satisfied,
        promoted,
        renamed: rename_changes_for_workset(&manifest, &workset),
        requires_confirmation: !predicted_dangling.is_empty(),
        would_remove: predicted_dangling,
        would_download_bytes,
        manifest,
        workset,
        declaration_after: declaration,
        deactivated_after,
        declared_before: declaration_before.names(),
        command_start,
        startup_diagnostics: InstallStartupDiagnostics {
            registry_resolve_seconds: resolve_timing.registry_seconds,
            manifest_validation_seconds: resolve_timing.validation_seconds,
            state_snapshot_seconds,
            resolve_and_state_wall_seconds,
            registry_requests: Some(resolve_timing.diagnostics),
            ..Default::default()
        },
    };
    plan.startup_diagnostics.plan_finalize_seconds = plan_finalize_started.elapsed().as_secs_f64();
    plan.startup_diagnostics.plan_total_seconds = command_start.elapsed().as_secs_f64();
    Ok(plan)
}

pub async fn execute_install(
    client: &GluClient,
    plan: InstallPlan,
    options: InstallOptions,
    events: Arc<dyn ExecutionEvents>,
) -> Result<InstallSummary> {
    let command_start = plan.command_start;
    let startup_diagnostics = plan.startup_diagnostics.clone();
    let mut summary = InstallSummary {
        requested: plan.requested.clone(),
        resolved_root_keys: plan.resolved_root_keys(),
        installed: plan
            .would_install
            .iter()
            .map(|package| {
                let mut change = package.clone();
                change.status = PackageChangeStatus::Installed;
                change
            })
            .collect(),
        satisfied: plan.satisfied.clone(),
        promoted: plan.promoted.clone(),
        renamed: plan.renamed.clone(),
        ..Default::default()
    };

    let store = InstalledStateStore::new(client.config().prefix.clone());
    if plan.workset.install.is_empty() && plan.workset.rename.is_empty() {
        store.write_declaration(&plan.declaration_after)?;
        let final_state = store.load_installed_state_with_declaration(&plan.declaration_after)?;
        let dangling = crate::sync::confirmed_final_dangling(
            &plan.would_remove,
            &final_state.dangling(),
            options.yes,
            "install",
        )?;
        summary.removed = remove_dangling(client, &dangling)?;
        return Ok(summary);
    }

    summary.execution = execute_workset_with_startup(
        client,
        plan.manifest.clone(),
        plan.workset.clone(),
        options,
        plan.deactivated_after.clone(),
        command_start,
        Some(startup_diagnostics),
        events,
    )
    .await?;
    store.write_declaration(&plan.declaration_after)?;
    // The install just wrote receipts for everything in `workset.install`;
    // reload so autoremove sees the real final receipt graph. Without `-y`,
    // only remove packages covered by the pre-execution confirmation.
    let final_state = store.load_installed_state_with_declaration(&plan.declaration_after)?;
    let dangling = crate::sync::confirmed_final_dangling(
        &plan.would_remove,
        &final_state.dangling(),
        options.yes,
        "install",
    )?;
    summary.removed = remove_dangling(client, &dangling)?;
    Ok(summary)
}

/// The trailing autoremove of a full sync: removes every dangling package
/// covered by the confirmed plan and returns the removals for presentation.
fn remove_dangling(
    client: &GluClient,
    dangling: &[InstalledPackage],
) -> Result<Vec<crate::remove::RemovedPackage>> {
    crate::remove::remove_installed_packages(&client.config().prefix, dangling)
}

fn download_bytes_for_uncached_package_ids<'a>(
    manifest: &glu_core::InstallManifest,
    prefix: &glu_core::Prefix,
    ids: impl IntoIterator<Item = &'a PackageId>,
) -> Option<u64> {
    let cache = crate::download::cache::ArtifactCache::new(prefix);
    let mut total = 0_u64;
    let mut seen = BTreeSet::new();
    for id in ids {
        let package = manifest.packages.get(id)?;
        if !seen.insert(package.artifact.clone()) {
            continue;
        }
        let artifact = manifest.artifacts.get(&package.artifact)?;
        if cache.path_for_artifact(artifact).exists() {
            continue;
        }
        total = total.checked_add(artifact.bytes?)?;
    }
    Some(total)
}

fn package_changes_for_ids(
    manifest: &glu_core::InstallManifest,
    state: &StateSnapshotInstalled,
    declaration: &Declaration,
    prefix: &glu_core::Prefix,
    depths: &BTreeMap<PackageId, usize>,
    ids: &[PackageId],
    status: PackageChangeStatus,
) -> Vec<PackageChange> {
    let context = PackageChangeContext {
        manifest,
        state,
        declaration,
        prefix,
        depths,
    };
    ids.iter()
        .filter_map(|id| package_change_for_id(&context, id, status, None))
        .collect()
}

type StateSnapshotInstalled = crate::state::installed::InstalledState;

struct PackageChangeContext<'a> {
    manifest: &'a glu_core::InstallManifest,
    state: &'a StateSnapshotInstalled,
    declaration: &'a Declaration,
    prefix: &'a glu_core::Prefix,
    depths: &'a BTreeMap<PackageId, usize>,
}

fn package_change_for_id(
    context: &PackageChangeContext<'_>,
    id: &PackageId,
    status: PackageChangeStatus,
    version_override: Option<String>,
) -> Option<PackageChange> {
    let package = context.manifest.packages.get(id)?;
    let installed = context.state.find_by_key(&package.package_key);
    let artifact = context.manifest.artifacts.get(&package.artifact);
    let cache = crate::download::cache::ArtifactCache::new(context.prefix);
    let depth = context.depths.get(id).copied().unwrap_or(usize::MAX);
    Some(PackageChange {
        package_key: package.package_key.clone(),
        name: package.name.clone(),
        version: version_override.unwrap_or_else(|| package.keg_version.0.clone()),
        exposure: package.exposure.clone(),
        status,
        installed: Some(installed.is_some()),
        linked: installed.map(|package| package.linked),
        declared: Some(context.declaration.contains(&package.name)),
        deactivated: Some(
            context
                .state
                .is_deactivated(&PackageSelector(package.name.0.clone())),
        ),
        direct: Some(depth <= 1),
        transitive: Some(depth > 1),
        cached: artifact.map(|artifact| cache.path_for_artifact(artifact).exists()),
        download_bytes: artifact.and_then(|artifact| artifact.bytes),
        installed_bytes: installed.and_then(|package| package.installed_bytes),
    })
}

fn manifest_depths(manifest: &glu_core::InstallManifest) -> BTreeMap<PackageId, usize> {
    let mut depths = BTreeMap::new();
    let mut queue = std::collections::VecDeque::new();
    for root in &manifest.roots {
        if depths.insert(root.package.clone(), 0).is_none() {
            queue.push_back(root.package.clone());
        }
    }
    while let Some(id) = queue.pop_front() {
        let depth = depths.get(&id).copied().unwrap_or(0);
        let Some(package) = manifest.packages.get(&id) else {
            continue;
        };
        for dep in &package.deps {
            let next_depth = depth + 1;
            let update = depths
                .get(&dep.package)
                .is_none_or(|existing| next_depth < *existing);
            if update {
                depths.insert(dep.package.clone(), next_depth);
                queue.push_back(dep.package.clone());
            }
        }
    }
    depths
}

fn rename_changes_for_workset(
    manifest: &glu_core::InstallManifest,
    workset: &planner::InstallWorkSet,
) -> Vec<RenameChange> {
    workset
        .rename
        .iter()
        .filter_map(|rename| {
            manifest
                .packages
                .get(&rename.package)
                .map(|package| RenameChange {
                    package_key: package.package_key.clone(),
                    old_name: rename.old_name.clone(),
                    new_name: package.name.clone(),
                    version: package.keg_version.0.clone(),
                })
        })
        .collect()
}

/// One planned update: a package being bumped, with its installed and
/// resolved versions. `current` is the installed keg version; `latest` is
/// what resolution produced.
#[derive(Debug, Clone)]
pub struct PlannedUpdate {
    pub package_key: glu_core::PackageKey,
    pub name: PackageName,
    pub current: String,
    pub latest: String,
    pub exposure: glu_core::Exposure,
    pub installed: Option<bool>,
    pub linked: Option<bool>,
    pub declared: Option<bool>,
    pub deactivated: Option<bool>,
    pub direct: Option<bool>,
    pub transitive: Option<bool>,
    pub cached: Option<bool>,
    pub download_bytes: Option<u64>,
    pub installed_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct UpToDatePackage {
    pub name: PackageName,
    pub version: String,
}

/// The result of planning an update. Planning may first discard interrupted
/// install trash; it does not apply the requested update. The CLI uses the
/// command scope and removal set to decide whether to ask for confirmation
/// (docs/reference/cli-behavior.md, Confirmation policy). The resolved manifest, workset, and
/// pre-command declaration travel with the plan so execution needs no
/// re-resolution. `to_update` contains every package selected for install or
/// rename, including dependencies selected by `--all` or required for root
/// satisfaction.
#[derive(Debug)]
pub struct UpdatePlan {
    pub to_update: Vec<PlannedUpdate>,
    pub up_to_date: Vec<UpToDatePackage>,
    pub cascade_added: Vec<PackageName>,
    pub broad: bool,
    /// Download bytes for packages this update would fetch/prepare, excluding already-satisfied packages.
    pub would_download_bytes: Option<u64>,
    pub latest_glu_version: Option<String>,
    /// Packages that become dangling once the new versions land —
    /// dependencies the new bottles dropped. Execution removes them after
    /// the update completes.
    pub to_remove: Vec<InstalledPackage>,
    /// `None` when there was nothing to update (the caller prints the
    /// "already up to date" state and skips execution).
    pub manifest: Option<glu_core::InstallManifest>,
    pub(crate) workset: planner::InstallWorkSet,
    pub(crate) outdated: crate::outdated::OutdatedResult,
    pub(crate) declaration: Declaration,
}

impl UpdatePlan {
    /// The resolved update graph, when the plan contains work.
    pub fn dependency_tree(&self) -> Vec<DependencyTreeNode> {
        self.manifest
            .as_ref()
            .map(dependency_forest_from_manifest)
            .unwrap_or_default()
    }

    pub fn resolved_root_keys(&self) -> BTreeSet<glu_core::PackageKey> {
        self.manifest
            .as_ref()
            .map(|manifest| {
                manifest
                    .roots
                    .iter()
                    .map(|root| root.package_key.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct UpdateTargetSelection {
    names: Vec<PackageName>,
    explicit_keys: BTreeSet<glu_core::PackageKey>,
}

fn select_update_targets(
    state: &crate::state::installed::InstalledState,
    declaration: &Declaration,
    requested: &[PackageSelector],
) -> Result<UpdateTargetSelection> {
    let declared: BTreeSet<PackageName> = declaration.names();
    if requested.is_empty() {
        return Ok(UpdateTargetSelection {
            names: declared.into_iter().collect(),
            explicit_keys: BTreeSet::new(),
        });
    }

    let mut missing = Vec::new();
    let mut automatic = Vec::new();
    let mut names = BTreeSet::new();
    let mut explicit_keys = BTreeSet::new();
    for selector in requested {
        let Some(installed) = state.resolve_selector(selector) else {
            missing.push(PackageName(selector.0.clone()));
            continue;
        };
        if !declared.contains(&installed.name) {
            automatic.push(installed.name.clone());
            continue;
        }
        explicit_keys.insert(installed.package_key.clone());
        names.insert(installed.name.clone());
    }

    if !missing.is_empty() {
        let quoted: Vec<String> = missing.iter().map(|name| format!("'{}'", name.0)).collect();
        let (noun, verb) = if quoted.len() == 1 {
            ("package", "is not installed")
        } else {
            ("packages", "are not installed")
        };
        bail!("{noun} {} {verb}", quoted.join(", "));
    }
    if !automatic.is_empty() {
        let quoted: Vec<String> = automatic
            .iter()
            .map(|name| format!("'{}'", name.0))
            .collect();
        if quoted.len() == 1 {
            bail!(
                "package {} is installed as a dependency; update its declared root or use `glu update --all`",
                quoted[0]
            );
        }
        bail!(
            "packages {} are installed as dependencies; update their declared roots or use `glu update --all`",
            quoted.join(", ")
        );
    }

    Ok(UpdateTargetSelection {
        names: names.into_iter().collect(),
        explicit_keys,
    })
}

/// Plans an update from a read-only state snapshot. Execution hosts perform
/// interrupted-install cleanup before requesting this plan. Target selection:
/// - named: the named declared roots;
/// - bare (`glu up`, no names, no `--all`): every declared root;
/// - `--all`: every declared root, with the complete dependency closure
///   reconciled to registry-selected releases.
///
/// Named and bare update retain dependencies that satisfy the active package
/// requirements. `--dependents` extends a named update to outdated dependents.
///
/// `to_remove` is computed by simulating the post-update installed set
/// from the resolve manifest, so the confirmation can show the removals
/// (dependencies a new version dropped) before anything happens.
pub async fn plan_update(
    client: &GluClient,
    names: Vec<PackageSelector>,
    all: bool,
    dependents: bool,
) -> Result<UpdatePlan> {
    let snapshot = StateSnapshot::load(&client.config().prefix)?;
    let state = snapshot.installed;
    let installed_selectors = state
        .names()
        .into_iter()
        .map(|name| PackageSelector(name.0))
        .collect::<Vec<_>>();
    let (response, latest_glu_version) = client
        .registry_client()?
        .outdated(&installed_selectors, &client.config().target)
        .await?;
    let outdated = crate::outdated::OutdatedResult {
        packages: crate::outdated::outdated_entries(&state, &response.packages),
        latest_glu_version,
    };
    let declaration = snapshot.declaration;
    let broad = all || names.is_empty();
    let mut up_to_date = Vec::new();
    let targets = select_update_targets(&state, &declaration, &names)?;
    let explicit_target_keys = targets.explicit_keys;
    let mut target_names = targets.names;

    if target_names.is_empty() {
        return Ok(UpdatePlan {
            to_update: Vec::new(),
            up_to_date,
            cascade_added: Vec::new(),
            broad,
            would_download_bytes: Some(0),
            latest_glu_version: outdated.latest_glu_version.clone(),
            to_remove: Vec::new(),
            manifest: None,
            workset: planner::InstallWorkSet {
                satisfied: Vec::new(),
                rename: Vec::new(),
                install: Vec::new(),
            },
            outdated,
            declaration,
        });
    }

    let mut cascade_added = Vec::new();
    if dependents && !all && !names.is_empty() {
        let outdated_names: BTreeSet<PackageName> = outdated
            .packages
            .iter()
            .map(|package| package.name.clone())
            .collect();
        let added: BTreeSet<PackageName> =
            planner::cascade_outdated_dependents(&target_names, &outdated_names, &state)
                .into_iter()
                .collect();
        if !added.is_empty() {
            cascade_added.extend(added.iter().cloned());
            target_names.extend(added);
        }
    }

    let manifest = resolve_manifest(
        client,
        target_names
            .into_iter()
            .map(|name| PackageSelector(name.0))
            .collect(),
    )
    .await?;
    let mode = if all {
        planner::WorksetMode::UpdateAll
    } else {
        planner::WorksetMode::UpdateRoots
    };
    let workset = planner::compute_workset(&manifest, &state, mode)?;
    let changed_ids: BTreeSet<PackageId> = workset
        .install
        .iter()
        .cloned()
        .chain(workset.rename.iter().map(|item| item.package.clone()))
        .collect();
    if !explicit_target_keys.is_empty() {
        for root in &manifest.roots {
            if changed_ids.contains(&root.package)
                || !explicit_target_keys.contains(&root.package_key)
            {
                continue;
            }
            let Some(installed) = state.find_by_key(&root.package_key) else {
                continue;
            };
            up_to_date.push(UpToDatePackage {
                name: installed.name.clone(),
                version: installed.keg_version.0.clone(),
            });
        }
    }
    if changed_ids.is_empty() {
        return Ok(UpdatePlan {
            to_update: Vec::new(),
            up_to_date,
            cascade_added,
            broad,
            would_download_bytes: Some(0),
            latest_glu_version: outdated.latest_glu_version.clone(),
            to_remove: Vec::new(),
            manifest: None,
            workset,
            outdated,
            declaration,
        });
    }

    // `update` never changes declaration membership. New dependencies the
    // new versions introduce are automatic.
    let existing_declared: BTreeSet<PackageName> = state.declared_names().into_iter().collect();

    let depths = manifest_depths(&manifest);
    let change_context = PackageChangeContext {
        manifest: &manifest,
        state: &state,
        declaration: &declaration,
        prefix: &client.config().prefix,
        depths: &depths,
    };
    let mut changed_order = workset.install.clone();
    for rename in &workset.rename {
        if !changed_order.contains(&rename.package) {
            changed_order.push(rename.package.clone());
        }
    }
    let mut to_update = Vec::new();
    for package_id in &changed_order {
        let Some(package) = manifest.packages.get(package_id) else {
            continue;
        };
        let current = state
            .find_by_key(&package.package_key)
            .map(|installed| installed.keg_version.0.clone())
            .unwrap_or_default();
        let change = package_change_for_id(
            &change_context,
            package_id,
            PackageChangeStatus::WouldUpdate,
            Some(package.keg_version.0.clone()),
        );
        to_update.push(PlannedUpdate {
            package_key: package.package_key.clone(),
            name: package.name.clone(),
            current,
            latest: package.keg_version.0.clone(),
            exposure: package.exposure.clone(),
            installed: change.as_ref().and_then(|change| change.installed),
            linked: change.as_ref().and_then(|change| change.linked),
            declared: change.as_ref().and_then(|change| change.declared),
            deactivated: change.as_ref().and_then(|change| change.deactivated),
            direct: change.as_ref().and_then(|change| change.direct),
            transitive: change.as_ref().and_then(|change| change.transitive),
            cached: change.as_ref().and_then(|change| change.cached),
            download_bytes: change.as_ref().and_then(|change| change.download_bytes),
            installed_bytes: change.as_ref().and_then(|change| change.installed_bytes),
        });
    }

    // Simulate the post-update installed set: current receipts plus the
    // receipts this update will write, then build the same installed graph
    // as a reloaded snapshot so each updated identity selects its new version
    // (the same way the reloaded state would after
    // execution). Dependencies the new versions dropped surface as dangling
    // here, before execution.
    let to_remove = crate::sync::without_workset_installs(
        crate::sync::predicted_dangling_after_workset(
            &state,
            &manifest,
            &workset,
            &existing_declared,
        )?,
        &workset,
    );

    let would_download_bytes = download_bytes_for_uncached_package_ids(
        &manifest,
        &client.config().prefix,
        workset.install.iter(),
    );

    Ok(UpdatePlan {
        to_update,
        up_to_date,
        cascade_added,
        broad,
        would_download_bytes,
        latest_glu_version: outdated.latest_glu_version.clone(),
        to_remove,
        manifest: Some(manifest),
        workset,
        outdated,
        declaration,
    })
}

/// Executes a planned update: runs the resolved workset (Preserve policy —
/// membership is unchanged) and then removes the packages the plan flagged
/// as dangling, which the confirmation already listed.
pub async fn execute_update(
    client: &GluClient,
    plan: UpdatePlan,
    verbose: bool,
    events: Arc<dyn ExecutionEvents>,
) -> Result<UpdateSummary> {
    let command_start = std::time::Instant::now();
    let broad = plan.broad;
    let Some(manifest) = plan.manifest else {
        return Ok(UpdateSummary {
            updates: plan.to_update,
            latest_glu_version: plan.outdated.latest_glu_version,
            broad,
            ..Default::default()
        });
    };
    let options = InstallOptions {
        verbose,
        ..Default::default()
    };
    let execution = execute_workset(
        client,
        manifest.clone(),
        plan.workset.clone(),
        options,
        plan.declaration.deactivated_names(),
        command_start,
        events,
    )
    .await?;

    let prefix = client.config().prefix.clone();
    let removed = crate::remove::remove_installed_packages(&prefix, &plan.to_remove)?;

    // Sync the declaration: declared packages record the version this
    // update resolved for them. Automatic packages updated via --all or a
    // maintenance bump are not declared and stay out of the file.
    let mut declaration = plan.declaration;
    for update in &plan.to_update {
        if declaration.contains(&update.name) {
            declaration
                .dependencies
                .insert(update.name.clone(), update.latest.clone());
        }
    }
    canonicalize_declaration_renames(&mut declaration, &manifest, &plan.workset);
    InstalledStateStore::new(prefix.clone()).write_declaration(&declaration)?;

    Ok(UpdateSummary {
        updates: plan.to_update,
        removed,
        execution,
        latest_glu_version: plan.outdated.latest_glu_version,
        broad,
    })
}

pub(crate) async fn resolve_manifest(
    client: &GluClient,
    names: Vec<PackageSelector>,
) -> Result<glu_core::InstallManifest> {
    Ok(resolve_manifest_timed(client, names).await?.0)
}

#[derive(Debug)]
struct ResolveManifestTiming {
    registry_seconds: f64,
    validation_seconds: f64,
    diagnostics: crate::registry::resolve_client::RegistryRequestDiagnostics,
}

async fn resolve_manifest_timed(
    client: &GluClient,
    names: Vec<PackageSelector>,
) -> Result<(glu_core::InstallManifest, ResolveManifestTiming)> {
    let resolve = client.registry_client()?;
    let registry_started = std::time::Instant::now();
    let manifest = resolve
        .resolve(&ResolveRequest {
            names,
            target: client.config().target.clone(),
        })
        .await?;
    let registry_seconds = registry_started.elapsed().as_secs_f64();
    let diagnostics = resolve.diagnostics();
    let validation_started = std::time::Instant::now();
    planner::validate_manifest(&manifest)?;
    validate_client_support(&manifest, &client.config().prefix)?;

    Ok((
        manifest,
        ResolveManifestTiming {
            registry_seconds,
            validation_seconds: validation_started.elapsed().as_secs_f64(),
            diagnostics,
        },
    ))
}

fn canonicalize_declaration_renames(
    declaration: &mut Declaration,
    manifest: &glu_core::InstallManifest,
    workset: &planner::InstallWorkSet,
) {
    for (package_id, package) in &manifest.packages {
        let existing_current = declaration.dependencies.get(&package.name).cloned();
        for oldname in &package.oldnames {
            let old_name = PackageName(oldname.0.clone());
            let Some(old_value) = declaration.dependencies.remove(&old_name) else {
                continue;
            };
            let transition_version = workset
                .rename
                .iter()
                .find(|rename| &rename.package == package_id && rename.old_name == old_name)
                .map(|rename| rename.old_keg_version.clone());
            let value = existing_current
                .clone()
                .or(transition_version)
                .unwrap_or(old_value);
            declaration
                .dependencies
                .entry(package.name.clone())
                .or_insert(value);
        }
    }
}

#[cfg(test)]
mod declaration_rename_tests {
    use super::*;
    use glu_core::{
        ArtifactId, InstallManifest, KegVersion, PackageDependency, PackageInstallMetadata,
        ResolveRequestEcho, ResolvedPackage, Target,
    };
    use std::{collections::BTreeMap, path::PathBuf};

    fn resolved_renamed_package() -> (PackageId, ResolvedPackage) {
        let id = PackageId("pkg:homebrew/core/bar@1.1".to_string());
        (
            id,
            ResolvedPackage {
                package_key: glu_core::PackageKey("package:bar".to_string()),
                name: PackageName("bar".to_string()),
                aliases: Vec::new(),
                oldnames: vec![glu_core::PackageSelector("foo".to_string())],
                version: "1.1".to_string(),
                revision: 0,
                keg_version: KegVersion("1.1".to_string()),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
                exposure: glu_core::Exposure::Global,
                artifact: ArtifactId("art:test".to_string()),
                install: PackageInstallMetadata {
                    opt_names: Vec::new(),
                    link_overwrite: Vec::new(),
                    post_install_defined: false,
                    post_install_steps: Vec::new(),
                    postinstall_network_access_allowed: true,
                },
            },
        )
    }

    fn manifest(package_id: PackageId, package: ResolvedPackage) -> InstallManifest {
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("foo".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("foo".to_string()),
                package_key: package.package_key.clone(),
                package: package_id.clone(),
            }],
            packages: BTreeMap::from([(package_id, package)]),
            artifacts: BTreeMap::new(),
        }
    }

    #[test]
    fn dependency_selector_does_not_mutate_package_identity() {
        let rust_id = PackageId("pkg:homebrew/core/rust@1.98.0".to_string());
        let llvm_id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        let mut rust = resolved_renamed_package().1;
        rust.package_key = glu_core::PackageKey("package:rust".to_string());
        rust.name = PackageName("rust".to_string());
        rust.oldnames.clear();
        rust.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: llvm_id.clone(),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
        }];
        let mut llvm = resolved_renamed_package().1;
        llvm.package_key = glu_core::PackageKey("package:llvm".to_string());
        llvm.name = PackageName("llvm".to_string());
        llvm.aliases = vec![glu_core::PackageSelector("llvm@22".to_string())];
        llvm.oldnames.clear();
        let manifest = InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("rust".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("rust".to_string()),
                package_key: glu_core::PackageKey("package:rust".to_string()),
                package: rust_id.clone(),
            }],
            packages: BTreeMap::from([(rust_id.clone(), rust), (llvm_id.clone(), llvm)]),
            artifacts: BTreeMap::from([(
                ArtifactId("art:test".to_string()),
                glu_core::ResolvedArtifact {
                    url: "https://example.invalid/artifact".to_string(),
                    sha256: "0".repeat(64),
                    bytes: Some(1),
                    bottle_tag: "arm64_sequoia".to_string(),
                    cellar: ":any".to_string(),
                    built_on: None,
                },
            )]),
        };

        planner::validate_manifest(&manifest).unwrap();

        let llvm = manifest.packages.get(&llvm_id).unwrap();
        assert_eq!(llvm.name.0, "llvm");
        assert_eq!(
            llvm.aliases,
            vec![glu_core::PackageSelector("llvm@22".to_string())]
        );
        assert_eq!(
            manifest.packages[&rust_id].deps[0].requested_as.0,
            "llvm@22"
        );
    }

    #[test]
    fn declaration_rename_uses_transition_version_when_old_install_is_reused() {
        let (package_id, package) = resolved_renamed_package();
        let manifest = manifest(package_id.clone(), package);
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("foo".to_string()), "1.0".to_string());
        let workset = planner::InstallWorkSet {
            satisfied: Vec::new(),
            rename: vec![planner::RenameWorkItem {
                package: package_id,
                old_name: PackageName("foo".to_string()),
                old_keg_version: "1.0".to_string(),
                old_version: "1.0".to_string(),
                old_revision: 0,
                old_keg_path: PathBuf::from("/prefix/Cellar/foo/1.0"),
            }],
            install: Vec::new(),
        };

        canonicalize_declaration_renames(&mut declaration, &manifest, &workset);

        assert!(!declaration.contains(&PackageName("foo".to_string())));
        assert_eq!(
            declaration
                .dependencies
                .get(&PackageName("bar".to_string()))
                .map(String::as_str),
            Some("1.0")
        );
    }

    #[test]
    fn declaration_rename_dedupes_when_current_name_already_declared() {
        let (package_id, package) = resolved_renamed_package();
        let manifest = manifest(package_id, package);
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("foo".to_string()), "1.0".to_string());
        declaration
            .dependencies
            .insert(PackageName("bar".to_string()), "1.1".to_string());
        let workset = planner::InstallWorkSet {
            satisfied: Vec::new(),
            rename: Vec::new(),
            install: Vec::new(),
        };

        canonicalize_declaration_renames(&mut declaration, &manifest, &workset);

        assert!(!declaration.contains(&PackageName("foo".to_string())));
        assert_eq!(declaration.dependencies.len(), 1);
        assert_eq!(
            declaration
                .dependencies
                .get(&PackageName("bar".to_string()))
                .map(String::as_str),
            Some("1.1")
        );
    }
}

struct NoopExecutionObserver;

impl scheduler::ExecutionObserver for NoopExecutionObserver {
    fn node_started(&self, _node: &dag::ExecNode) {}
}

struct EventExecutionObserver {
    events: Arc<dyn ExecutionEvents>,
}

impl scheduler::ExecutionObserver for EventExecutionObserver {
    fn node_started(&self, node: &dag::ExecNode) {
        self.events
            .progress(ProgressEvent::InstallNodeStarted { node: node.clone() });
    }

    fn node_completed(&self, node: &dag::ExecNode, status: NodeCompletionStatus) {
        self.events.progress(ProgressEvent::InstallNodeCompleted {
            node: node.clone(),
            status,
        });
    }
}

/// Shared execution tail for both install and update: DAG/scheduler/
/// orchestrator run, live progress events, Ctrl+C-safe trace writing, and
/// the final timing summary. Identical regardless of how `workset` was
/// computed.
async fn execute_workset(
    client: &GluClient,
    manifest: glu_core::InstallManifest,
    workset: planner::InstallWorkSet,
    options: InstallOptions,
    deactivated_names: BTreeSet<PackageName>,
    command_start: std::time::Instant,
    events: Arc<dyn ExecutionEvents>,
) -> Result<WorksetExecutionSummary> {
    execute_workset_with_startup(
        client,
        manifest,
        workset,
        options,
        deactivated_names,
        command_start,
        None,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_workset_with_startup(
    client: &GluClient,
    manifest: glu_core::InstallManifest,
    workset: planner::InstallWorkSet,
    options: InstallOptions,
    deactivated_names: BTreeSet<PackageName>,
    command_start: std::time::Instant,
    startup_diagnostics: Option<InstallStartupDiagnostics>,
    events: Arc<dyn ExecutionEvents>,
) -> Result<WorksetExecutionSummary> {
    let execution_setup_started = std::time::Instant::now();
    let prefix = client.config().prefix.clone();
    let postinstall_plans = PostinstallPlans::analyze(&manifest, &workset.install, &prefix)?;
    let execution_plan =
        dag::make_execution_plan(&manifest, &workset, &prefix, &postinstall_plans)?;
    let manifest_for_report = manifest.clone();
    let plan_name = manifest
        .request
        .name
        .iter()
        .map(|name| name.0.as_str())
        .collect::<Vec<_>>()
        .join("+");

    let progress_enabled = events.wants_progress();
    let ctx =
        ExecutionContext::with_events(options.verbose && progress_enabled, Arc::clone(&events));
    if let Some(mut startup) = startup_diagnostics {
        startup.execution_setup_seconds = execution_setup_started.elapsed().as_secs_f64();
        startup.command_to_execution_seconds = command_start.elapsed().as_secs_f64();
        ctx.put_artifact("startup", serde_json::to_value(startup)?);
    }
    let plan_for_trace = execution_plan.clone();
    let download_progress = crate::download::DownloadProgress::default();

    if progress_enabled {
        events.progress(ProgressEvent::InstallStarted {
            plan: execution_plan.clone(),
            install_total: workset.install.len(),
            dynamic: !options.verbose,
            download_progress: download_progress.clone(),
        });
    }

    // Spinner ticker: asks the selected event sink to redraw ~12x/s while
    // execution runs. Silent protocols skip the ticker entirely.
    let ticker_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ticker_notify = Arc::new(tokio::sync::Notify::new());
    let ticker = progress_enabled.then(|| {
        let events = Arc::clone(&events);
        let stop = Arc::clone(&ticker_stop);
        let notify = Arc::clone(&ticker_notify);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        events.progress(ProgressEvent::InstallTick);
                    }
                    _ = notify.notified() => break,
                }
            }
        })
    });

    let observer: Arc<dyn scheduler::ExecutionObserver> = if progress_enabled {
        Arc::new(EventExecutionObserver {
            events: Arc::clone(&events),
        })
    } else {
        Arc::new(NoopExecutionObserver)
    };

    let outcome = tokio::select! {
        result = execute_install_plan(ExecuteInstallPlanInput {
            plan: execution_plan,
            manifest: Arc::new(manifest),
            prefix,
            postinstall_plans,
            options,
            deactivated_names,
            ctx: ctx.clone(),
            observer,
            download_progress,
            events: Arc::clone(&events),
        }) => (result, false),
        _ = tokio::signal::ctrl_c() => (Ok(orchestrator::SchedulerInstallResult {
            execution: InstallResult {
                plan: plan_for_trace,
                events: ctx.events(),
                artifacts: ctx.artifacts(),
                error: Some("interrupted (Ctrl+C)".to_string()),
                failure: None,
            },
            stats: Default::default(),
            pool_stats: Default::default(),
        }), true),
    };
    ticker_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    ticker_notify.notify_waiters();
    if let Some(ticker) = ticker {
        let _ = ticker.await;
    }

    let (outcome, interrupted) = outcome;
    let result = match outcome {
        Ok(result) => result,
        Err(error) => {
            if progress_enabled {
                events.progress(ProgressEvent::InstallFinished {
                    status: ProgressFinishStatus::Failed,
                });
            }
            return Err(error);
        }
    };
    let finish_status = if interrupted {
        ProgressFinishStatus::Interrupted
    } else if result.execution.error.is_some() {
        ProgressFinishStatus::Failed
    } else {
        ProgressFinishStatus::Done
    };
    if progress_enabled {
        events.progress(ProgressEvent::InstallFinished {
            status: finish_status,
        });
    }

    let trace = result.execution.trace_dict(&plan_name);
    let trace_path = match write_install_trace(&client.config().prefix, &plan_name, &trace) {
        Ok(written_trace) => Some(written_trace.path),
        Err(error) => {
            events.notice(
                OutputStream::Stderr,
                &format!("glu: warning: could not write install trace: {error:#}"),
            );
            None
        }
    };

    if interrupted {
        return Err(crate::error::InterruptedError {
            operation: "install",
            message: "interrupted (Ctrl+C)",
            trace_path,
        }
        .into());
    }

    if let Some(error) = &result.execution.error {
        return Err(partial_install_failure(
            &manifest_for_report,
            &workset,
            &result.execution,
            error,
            trace_path,
        )
        .into());
    }

    let stats = result.stats;
    let pool_stats = result.pool_stats;
    let summary = InstallTimingSummary::from_result(&result.execution, command_start.elapsed());
    let timing_description = summary.breakdown();
    let timing_breakdown = timing_breakdown_struct(summary);

    Ok(WorksetExecutionSummary {
        trace_path,
        elapsed_seconds: summary.total.as_secs_f64(),
        timing_breakdown,
        timing_description,
        stats: Some(stats),
        pool_stats: Some(pool_stats),
    })
}

fn partial_install_failure(
    manifest: &glu_core::InstallManifest,
    workset: &planner::InstallWorkSet,
    execution: &InstallResult,
    error: &str,
    trace_path: Option<PathBuf>,
) -> PartialInstallFailure {
    let registry_ok_nodes = execution
        .events
        .iter()
        .filter(|event| event.status == "ok")
        .filter(|event| {
            execution
                .plan
                .node(&event.node_id)
                .is_some_and(|node| node.kind == dag::NodeKind::RegistryWrite)
        })
        .map(|event| event.node_id.as_str())
        .collect::<BTreeSet<_>>();
    let installed_ids = workset
        .install
        .iter()
        .filter(|package_id| {
            manifest
                .packages
                .get(*package_id)
                .map(|package| {
                    registry_ok_nodes.contains(
                        dag::NodeKind::RegistryWrite
                            .node_id(&package.name.0)
                            .as_str(),
                    )
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let failed_id = execution
        .failure
        .as_ref()
        .and_then(|failure| failure.package_id.as_ref())
        .map(|id| PackageId(id.clone()));

    let mut installed = Vec::new();
    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    for package_id in &workset.install {
        if installed_ids.contains(package_id) {
            if let Some(package) =
                partial_package_record(manifest, package_id, PartialInstallStatus::Installed)
            {
                installed.push(package);
            }
        } else if failed_id.as_ref() == Some(package_id) {
            if let Some(package) =
                partial_package_record(manifest, package_id, PartialInstallStatus::Failed)
            {
                failed.push(package);
            }
        } else if let Some(package) =
            partial_package_record(manifest, package_id, PartialInstallStatus::Skipped)
        {
            skipped.push(package);
        }
    }

    let partial = partial_kegs(manifest, workset, execution, &installed_ids);
    let trace_id = trace_path.as_deref().and_then(trace_id_from_path);
    let failed_phase = execution
        .failure
        .as_ref()
        .map(|failure| failure.phase.clone());
    let failed_at = execution
        .failure
        .as_ref()
        .and_then(|failure| failure.package.clone())
        .or_else(|| {
            execution
                .failure
                .as_ref()
                .map(|failure| failure.node_id.clone())
        })
        .unwrap_or_else(|| "install".to_string());
    let phase_label = failed_phase.as_deref().unwrap_or("execution");
    let message = format!("Install failed at {failed_at} during {phase_label}: {error}");
    let retry = failed
        .first()
        .map(|package| format!("glu install {} --verbose", package.name));
    let mut suggested_commands = Vec::new();
    if let Some(retry) = retry {
        suggested_commands.push(retry);
    }
    suggested_commands.push("glu autoremove".to_string());
    if let Some(trace_id) = trace_id.as_deref() {
        suggested_commands.push(format!("glu trace view {trace_id}"));
    } else if let Some(trace_path) = trace_path.as_deref() {
        suggested_commands.push(format!("glu trace view {}", trace_path.display()));
    }

    PartialInstallFailure {
        message,
        report: PartialInstallReport {
            failure_code: execution
                .failure
                .as_ref()
                .map(|failure| failure.code)
                .unwrap_or(crate::error::RuntimeErrorCode::PartialInstallFailure),
            failed_node_id: execution
                .failure
                .as_ref()
                .map(|failure| failure.node_id.clone()),
            failed_phase,
            failed_kind: execution
                .failure
                .as_ref()
                .map(|failure| failure.kind.clone()),
            failed_error: execution
                .failure
                .as_ref()
                .map(|failure| failure.error.clone()),
            installed,
            failed,
            skipped,
            partial,
            trace_path,
            trace_id,
            suggested_commands,
        },
    }
}

fn partial_package_record(
    manifest: &glu_core::InstallManifest,
    package_id: &PackageId,
    status: PartialInstallStatus,
) -> Option<PartialInstallPackage> {
    let package = manifest.packages.get(package_id)?;
    Some(PartialInstallPackage {
        name: package.name.0.clone(),
        version: package.keg_version.0.clone(),
        package_id: package_id.0.clone(),
        status,
    })
}

fn partial_kegs(
    manifest: &glu_core::InstallManifest,
    workset: &planner::InstallWorkSet,
    execution: &InstallResult,
    installed_ids: &BTreeSet<PackageId>,
) -> Vec<PartialKeg> {
    workset
        .install
        .iter()
        .filter(|id| !installed_ids.contains(*id))
        .filter_map(|package_id| {
            let package = manifest.packages.get(package_id)?;
            let path = execution
                .plan
                .nodes
                .iter()
                .filter(|node| node.package_id.as_ref() == Some(package_id))
                .find_map(|node| node.inputs.get("keg").and_then(serde_json::Value::as_str))
                .map(PathBuf::from)?;
            if !path.exists() {
                return None;
            }
            Some(PartialKeg {
                name: package.name.0.clone(),
                version: package.keg_version.0.clone(),
                package_id: package_id.0.clone(),
                path,
            })
        })
        .collect()
}

fn trace_id_from_path(path: &std::path::Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit('-').next())
        .filter(|id| id.len() == 6 && id.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(ToOwned::to_owned)
}

fn timing_breakdown_struct(summary: InstallTimingSummary) -> Option<WorksetTimingBreakdown> {
    let download_seconds = summary.download.as_secs_f64();
    let cache_rebuild_seconds = summary.cache_rebuild.as_secs_f64();
    if download_seconds == 0.0 && cache_rebuild_seconds == 0.0 {
        None
    } else {
        Some(WorksetTimingBreakdown {
            download_seconds,
            cache_rebuild_seconds,
        })
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::state::installed::InstalledState;
    use glu_core::{
        KegVersion, PackageDependency, PackageId, PackageName, SlimPackage, Target,
        UsesRequestEcho, UsesResponse,
    };
    use std::collections::BTreeMap;

    fn slim(
        name: &str,
        version: &str,
        dependencies: Vec<(&str, &str)>,
    ) -> (PackageId, SlimPackage) {
        let id = PackageId(format!("pkg:test/{name}@{version}"));
        let mut deps = Vec::new();
        let mut dependency_requirements = BTreeMap::new();
        for (requested_as, package_id) in dependencies {
            let package_key = glu_core::PackageKey(format!("package:{requested_as}"));
            deps.push(PackageDependency {
                package_key: package_key.clone(),
                package: PackageId(format!("pkg:test/{package_id}")),
                requested_as: glu_core::PackageSelector(requested_as.to_string()),
            });
            dependency_requirements.insert(
                package_key,
                glu_core::MinimumVersion {
                    version: "1.0".to_string(),
                    revision: Some(0),
                },
            );
        }

        (
            id.clone(),
            SlimPackage {
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: KegVersion(version.to_string()),
                deps,
                dependency_requirements,
            },
        )
    }

    fn uses_response(packages: Vec<(PackageId, SlimPackage)>, name: &str) -> UsesResponse {
        let packages: BTreeMap<_, _> = packages.into_iter().collect();
        let package = packages
            .iter()
            .find_map(|(id, package)| (package.name.0 == name).then_some(id.clone()))
            .unwrap_or_else(|| PackageId(format!("pkg:test/{name}@missing")));
        UsesResponse {
            schema: "glu.uses.v1".to_string(),
            request: UsesRequestEcho {
                name: PackageSelector(name.to_string()),
                target: Target("arm64_sequoia".to_string()),
                direct: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector(name.to_string()),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                package,
            }],
            packages,
        }
    }

    #[test]
    fn reverse_tree_from_uses_walks_upward() {
        // pcre2 <- glib <- vips; pcre2 <- direct. The response contains the
        // target plus the transitive dependents.
        let (pcre2_id, pcre2) = slim("pcre2", "10.47", vec![]);
        let (glib_id, glib) = slim("glib", "2.88", vec![("pcre2", "pcre2@10.47")]);
        let (vips_id, vips) = slim("vips", "8.19", vec![("glib", "glib@2.88")]);
        let (direct_id, direct) = slim("direct", "1.0", vec![("pcre2", "pcre2@10.47")]);
        let response = uses_response(
            vec![
                (pcre2_id, pcre2),
                (glib_id, glib),
                (vips_id, vips),
                (direct_id, direct),
            ],
            "pcre2",
        );

        let tree = crate::dependency_query::reverse_tree_from_uses(&response)
            .expect("valid uses graph")
            .expect("uses graph should contain its selected root");
        assert_eq!(tree.name, "pcre2");
        let children: Vec<&str> = tree.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(children, vec!["direct", "glib"]);
        // vips depends on glib, which depends on pcre2.
        let glib_node = &tree.children[1];
        assert_eq!(glib_node.children.len(), 1);
        assert_eq!(glib_node.children[0].name, "vips");
        // The displayed floor comes from the dependent's selected artifact.
        assert!(glib_node
            .incoming
            .as_ref()
            .is_some_and(|edge| edge.minimum.is_some()));
    }

    #[test]
    fn reverse_tree_from_uses_none_when_response_has_no_root() {
        let (_id, glib) = slim("glib", "2.88", vec![]);
        let mut response = uses_response(vec![(_id, glib)], "pcre2");
        response.roots.clear();
        assert!(crate::dependency_query::reverse_tree_from_uses(&response)
            .expect("valid empty uses graph")
            .is_none());
    }

    #[test]
    fn dependency_tree_from_slim_matches_full_manifest_shape() {
        let (pcre2_id, pcre2) = slim("pcre2", "10.47", vec![]);
        let (vips_id, vips) = slim("vips", "8.19", vec![("pcre2", "pcre2@10.47")]);
        let mut packages = BTreeMap::new();
        packages.insert(pcre2_id.clone(), pcre2);
        packages.insert(vips_id.clone(), vips);
        let manifest = glu_core::SlimManifest {
            schema: "glu.resolve.v1".to_string(),
            request: glu_core::ResolveRequestEcho {
                name: vec![PackageSelector("vips".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: true,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("vips".to_string()),
                package_key: glu_core::PackageKey("package:vips".to_string()),
                package: vips_id,
            }],
            packages,
        };

        let tree = crate::dependency_query::dependency_tree_from_slim(
            &manifest,
            &manifest.roots[0].package,
        )
        .expect("valid slim graph should project its selected root");
        assert_eq!(tree.name, "vips");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "pcre2");
        assert!(tree.children[0]
            .incoming
            .as_ref()
            .is_some_and(|edge| edge.minimum.is_some()));
    }

    /// Regression test for the interrupted-install autoremove bug: run 1 is
    /// Ctrl+C'd while a declared package's receipt is still unwritten (the
    /// serial registry_write chain blocked on a hung download), so its deps
    /// look dangling to the pre-command state. Run 2 installs the declared
    /// package again, writes its receipt (which re-reaches those deps), and
    /// its trailing autoremove must then keep them — judged on the final
    /// receipt graph, never the pre-command one.
    #[test]
    fn reloaded_final_state_keeps_deps_reached_by_a_just_written_receipt() {
        // Pre-command receipts: only ada-url and brotli have kegs (run 1
        // wrote just these before dying); node's receipt is missing.
        let pre_state = InstalledState::from_packages(vec![
            pkg("ada-url", vec![]),
            pkg("brotli", vec![]),
            pkg("fmt", vec![]),
        ]);
        // Nothing declared yet (node was never registered): all three look
        // dangling to the pre-command state — the bug's prompt state.
        let pre_declared: BTreeSet<PackageName> = BTreeSet::new();
        let pre_dangling = pre_state.dangling_for_declared(&pre_declared);
        assert_eq!(pre_dangling.len(), 3);

        // Run 2 installs node and writes its receipt, which declares
        // ada-url and brotli (but not fmt) as deps.
        let final_state = InstalledState::from_packages(vec![
            pkg("ada-url", vec![]),
            pkg("brotli", vec![]),
            pkg("fmt", vec![]),
            pkg("node", vec!["ada-url", "brotli"]),
        ]);
        // The declaration is node.
        let final_declared: BTreeSet<PackageName> =
            [PackageName("node".to_string())].into_iter().collect();
        let final_dangling = final_state.dangling_for_declared(&final_declared);
        // ada-url and brotli are reached by node's receipt — kept. Only the
        // genuinely-unused fmt is removed.
        let names: Vec<&str> = final_dangling.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["fmt"]);
    }

    #[test]
    fn bare_update_selects_every_declared_root_only() {
        let state =
            InstalledState::from_packages(vec![pkg("app", vec!["dep"]), pkg("dep", vec![])]);
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("app".to_string()), "1.0".to_string());

        let targets = select_update_targets(&state, &declaration, &[]).unwrap();

        assert_eq!(targets.names, vec![PackageName("app".to_string())]);
        assert!(targets.explicit_keys.is_empty());
    }

    #[test]
    fn named_update_resolves_alias_to_a_declared_root() {
        let mut app = pkg("app", vec![]);
        app.aliases = vec![PackageSelector("application".to_string())];
        let state = InstalledState::from_packages(vec![app]);
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("app".to_string()), "1.0".to_string());

        let targets = select_update_targets(
            &state,
            &declaration,
            &[PackageSelector("application".to_string())],
        )
        .unwrap();

        assert_eq!(targets.names, vec![PackageName("app".to_string())]);
        assert_eq!(
            targets.explicit_keys,
            BTreeSet::from([glu_core::PackageKey("package:app".to_string())])
        );
    }

    #[test]
    fn named_update_rejects_an_automatic_dependency() {
        let state =
            InstalledState::from_packages(vec![pkg("app", vec!["dep"]), pkg("dep", vec![])]);
        let mut declaration = Declaration::default();
        declaration
            .dependencies
            .insert(PackageName("app".to_string()), "1.0".to_string());

        let err =
            select_update_targets(&state, &declaration, &[PackageSelector("dep".to_string())])
                .unwrap_err();

        assert!(err.to_string().contains("installed as a dependency"));
        assert!(err.to_string().contains("update --all"));
    }

    /// Installed keg helper for the autoremove regression test (mirrors the
    /// `pkg` builders in state/installed.rs).
    fn pkg(name: &str, deps: Vec<&str>) -> glu_core::InstalledPackage {
        glu_core::InstalledPackage {
            id: glu_core::PackageId(format!("pkg:test/{name}@1.0")),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            keg_path: std::path::PathBuf::from(format!("/prefix/Cellar/{name}/1.0")),
            opt_path: std::path::PathBuf::from(format!("/prefix/opt/{name}")),
            exposure: glu_core::Exposure::Global,
            linked: true,
            deps: deps
                .into_iter()
                .map(|dep| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: glu_core::PackageId(format!("pkg:test/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector(dep.to_string()),
                })
                .collect(),
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }
}

#[cfg(test)]
mod interrupted_install_tests {
    use super::*;
    use crate::{
        config::ClientConfig,
        download::cache::ArtifactCache,
        events::{RecordedExecutionEvent, RecordingExecutionEvents},
        state::{
            snapshot::StateSnapshot, store::InstalledStateStore, GluInstallReceipt,
            ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths, ReceiptStatus,
        },
    };
    use flate2::{write::GzEncoder, Compression};
    use glu_core::{
        ArtifactId, InstallManifest, KegVersion, PackageDependency, PackageInstallMetadata, Prefix,
        ResolvedArtifact, ResolvedPackage, Target,
    };
    use std::collections::BTreeMap;
    #[cfg(feature = "dev-registry")]
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    static FAULT_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    #[cfg(feature = "dev-registry")]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_preplan_resolution_does_not_write_trace() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for response in [
                "HTTP/1.1 522 Unknown\r\nContent-Length: 0\r\nRetry-After: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let temp = TempDir::new().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let client = GluClient::new(ClientConfig {
            prefix: prefix.clone(),
            target: Target("test-target".to_string()),
            registry_base_url: format!("http://{address}"),
            distribution_base_url: "https://example.invalid/releases".to_string(),
        });
        let error = resolve_manifest_timed(&client, vec![PackageSelector("vips".to_string())])
            .await
            .unwrap_err();
        server.join().unwrap();

        assert!(error
            .downcast_ref::<crate::error::RegistryDecodeFailure>()
            .is_some());
        assert!(!prefix.0.join("var/glu/traces").exists());
    }

    #[cfg(feature = "dev-registry")]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_preplan_resolution_preserves_typed_error_without_trace() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let temp = TempDir::new().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let client = GluClient::new(ClientConfig {
            prefix,
            target: Target("test-target".to_string()),
            registry_base_url: format!("http://{address}"),
            distribution_base_url: "https://example.invalid/releases".to_string(),
        });
        let cancellation = client.cancellation_token();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            cancellation.cancel();
        });

        let error = resolve_manifest_timed(&client, vec![PackageSelector("vips".to_string())])
            .await
            .unwrap_err();
        server.join().unwrap();

        let interrupted = error
            .downcast_ref::<crate::error::InterruptedError>()
            .expect("cancellation retains its typed error");
        assert!(interrupted.trace_path.is_none());
        assert!(!client.config().prefix.0.join("var/glu/traces").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_name_dependency_renames_existing_keg_without_downloading_dependency() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let client = client(prefix.clone());
        write_installed_receipt(&prefix, "foo", "1.0");
        let manifest = rename_manifest_with_app(&prefix);
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::Install).unwrap();

        assert_eq!(
            workset.install,
            vec![PackageId("pkg:test/app@1.0".to_string())]
        );
        assert_eq!(workset.rename.len(), 1);
        assert_eq!(workset.rename[0].old_name.0, "foo");

        let events = Arc::new(RecordingExecutionEvents::default());
        let execution = execute_workset_with_startup(
            &client,
            manifest,
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Some(InstallStartupDiagnostics {
                host: Some(HostStartupDiagnostics {
                    config_seconds: 0.5,
                    ..Default::default()
                }),
                registry_resolve_seconds: 1.25,
                ..Default::default()
            }),
            events.clone(),
        )
        .await
        .unwrap();

        let trace: serde_json::Value = serde_json::from_slice(
            &std::fs::read(execution.trace_path.expect("trace was written")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            trace["diagnostics"]["startup"]["registry_resolve_seconds"],
            1.25
        );
        assert_eq!(
            trace["diagnostics"]["startup"]["host"]["config_seconds"],
            0.5
        );

        let recorded = events.events();
        assert!(matches!(
            recorded.first(),
            Some(RecordedExecutionEvent::Progress(event))
                if matches!(event.as_ref(), ProgressEvent::InstallStarted { .. })
        ));
        assert!(matches!(
            recorded.last(),
            Some(RecordedExecutionEvent::Progress(event))
                if matches!(
                    event.as_ref(),
                    ProgressEvent::InstallFinished { status: ProgressFinishStatus::Done }
                )
        ));
        assert!(!recorded
            .iter()
            .any(|event| matches!(event, RecordedExecutionEvent::Notice { .. })));

        let bar_keg = prefix.0.join("Cellar/bar/1.0");
        let app_keg = prefix.0.join("Cellar/app/1.0");
        assert!(!prefix.0.join("Cellar/foo/1.0").exists());
        assert_eq!(receipt_status(&bar_keg), Some(ReceiptStatus::Complete));
        assert_eq!(receipt_status(&app_keg), Some(ReceiptStatus::Complete));
        let bar_receipt = InstalledStateStore::read_receipt_at_keg(&bar_keg).unwrap();
        assert_eq!(bar_receipt.package.name.0, "bar");
        assert!(bar_receipt
            .package
            .oldnames
            .contains(&glu_core::PackageSelector("foo".to_string())));
        assert_eq!(
            prefix.0.join("opt/bar").canonicalize().unwrap(),
            bar_keg.canonicalize().unwrap()
        );
        assert_eq!(
            prefix.0.join("opt/foo").canonicalize().unwrap(),
            bar_keg.canonicalize().unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_write_failure_does_not_prevent_install_declaration_commit() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::Install).unwrap();
        let mut declaration_after = Declaration::default();
        declaration_after
            .dependencies
            .insert(PackageName("app".to_string()), "1.0".to_string());
        let plan = InstallPlan {
            requested: vec![PackageSelector("app".to_string())],
            would_install: Vec::new(),
            satisfied: Vec::new(),
            promoted: Vec::new(),
            renamed: Vec::new(),
            would_remove: Vec::new(),
            requires_confirmation: false,
            would_download_bytes: Some(0),
            manifest,
            workset,
            declaration_after,
            deactivated_after: BTreeSet::new(),
            declared_before: BTreeSet::new(),
            command_start: std::time::Instant::now(),
            startup_diagnostics: InstallStartupDiagnostics::default(),
        };
        block_trace_writes(&prefix);
        let events = Arc::new(RecordingExecutionEvents::default());

        let summary = execute_install(
            &client,
            plan,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            events.clone(),
        )
        .await
        .unwrap();

        assert!(summary.execution.trace_path.is_none());
        let declaration = InstalledStateStore::new(prefix.clone())
            .load_declaration()
            .unwrap();
        assert_eq!(
            declaration
                .dependencies
                .get(&PackageName("app".to_string()))
                .map(String::as_str),
            Some("1.0")
        );
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/1.0")),
            Some(ReceiptStatus::Complete)
        );
        let warnings = events
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    RecordedExecutionEvent::Notice {
                        stream: OutputStream::Stderr,
                        message,
                    } if message.contains("could not write install trace")
                )
            })
            .count();
        assert_eq!(warnings, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_write_failure_does_not_mask_partial_install_failure() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::Install).unwrap();
        block_trace_writes(&prefix);
        let events = Arc::new(RecordingExecutionEvents::default());
        let _fault = fault::fail_after_commit_for("app");

        let error = execute_workset(
            &client,
            manifest,
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            events.clone(),
        )
        .await
        .unwrap_err();

        let partial = error.downcast_ref::<PartialInstallFailure>().unwrap();
        assert!(partial
            .message
            .contains("injected failure after committing app"));
        assert!(partial.report.trace_path.is_none());
        assert!(partial.report.trace_id.is_none());
        assert!(partial
            .report
            .suggested_commands
            .iter()
            .all(|command| !command.starts_with("glu trace view ")));
        let warnings = events
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    RecordedExecutionEvent::Notice {
                        stream: OutputStream::Stderr,
                        message,
                    } if message.contains("could not write install trace")
                )
            })
            .count();
        assert_eq!(warnings, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_staging_is_cleaned_before_retry() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());

        let first_state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let first_workset =
            planner::compute_workset(&manifest, &first_state, planner::WorksetMode::Install)
                .unwrap();
        let _fault = fault::fail_after_prepared_receipt_for("dep");
        let err = execute_workset(
            &client,
            manifest.clone(),
            first_workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("injected failure after staging receipt for dep"));
        assert!(staging_entries(&prefix) > 0);

        let retry_snapshot = StateSnapshot::load_for_mutation(&prefix).unwrap();
        assert_eq!(staging_entries(&prefix), 0);
        let retry_workset = planner::compute_workset(
            &manifest,
            &retry_snapshot.installed,
            planner::WorksetMode::Install,
        )
        .unwrap();
        let to_install: Vec<&str> = retry_workset
            .install
            .iter()
            .map(|id| id.0.as_str())
            .collect();
        assert_eq!(to_install, vec!["pkg:test/dep@1.0", "pkg:test/app@1.0"]);

        execute_workset(
            &client,
            manifest.clone(),
            retry_workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap();

        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/dep/1.0")),
            Some(ReceiptStatus::Complete)
        );
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/1.0")),
            Some(ReceiptStatus::Complete)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deactivated_package_commits_without_public_projection() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::Install).unwrap();

        execute_workset(
            &client,
            manifest,
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            [PackageName("app".to_string())].into_iter().collect(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap();

        let app_keg = prefix.0.join("Cellar/app/1.0");
        let dep_keg = prefix.0.join("Cellar/dep/1.0");
        assert!(app_keg.exists());
        assert!(dep_keg.exists());
        assert_eq!(
            prefix.0.join("opt/app").canonicalize().unwrap(),
            app_keg.canonicalize().unwrap()
        );
        assert!(!prefix.0.join("bin/app").exists());
        assert!(!prefix.0.join("var/homebrew/linked/app").exists());
        assert!(prefix.0.join("bin/dep").exists());
        assert!(prefix.0.join("var/homebrew/linked/dep").exists());
        let app_receipt = InstalledStateStore::read_receipt_at_keg(&app_keg).unwrap();
        let dep_receipt = InstalledStateStore::read_receipt_at_keg(&dep_keg).unwrap();
        assert!(!app_receipt.install.linked);
        assert!(dep_receipt.install.linked);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn named_version_bump_removes_the_superseded_keg() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        // app 0.9 is installed and declared; the update bumps it to 1.0 and
        // also installs its dep. The plan must flag the superseded 0.9 keg
        // for removal, and execution must delete it after the new keg
        // commits — never before.
        write_installed_receipt(&prefix, "app", "0.9");
        let client = client(prefix.clone());
        let manifest = manifest_with_dep(&prefix);
        let declared: BTreeSet<PackageName> =
            [PackageName("app".to_string())].into_iter().collect();

        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        assert_eq!(state.list().len(), 1);
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::UpdateRoots).unwrap();
        let predicted =
            crate::sync::predicted_dangling_after_workset(&state, &manifest, &workset, &declared)
                .unwrap();
        let names: Vec<&str> = predicted
            .iter()
            .map(|package| package.name.0.as_str())
            .collect();
        assert_eq!(names, vec!["app"]);
        assert_eq!(predicted[0].keg_version.0, "0.9");

        // Install the new version. The old keg must still be present: the
        // removal pass runs after the commit, so an interrupted update can
        // never lose the working version before the new one is installed.
        execute_workset(
            &client,
            manifest,
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap();
        assert!(prefix.0.join("Cellar/app/0.9").exists());
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/1.0")),
            Some(ReceiptStatus::Complete)
        );

        // Update's trailing removal deletes exactly the predicted
        // superseded keg, and the final state is single-version.
        let removed = crate::remove::remove_installed_packages(&prefix, &predicted).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!prefix.0.join("Cellar/app/0.9").exists());

        let final_state = StateSnapshot::load(&prefix).unwrap().installed;
        let final_list = final_state.list();
        let final_names: Vec<&str> = final_list.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(final_names, vec!["app", "dep"]);
        assert_eq!(
            final_state
                .find(&PackageName("app".to_string()))
                .unwrap()
                .keg_version
                .0,
            "1.0"
        );
        assert!(final_state.dangling_for_declared(&declared).is_empty());
        assert_eq!(
            prefix.0.join("opt/app").canonicalize().unwrap(),
            prefix.0.join("Cellar/app/1.0").canonicalize().unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_install_failure_reports_installed_failed_and_partial_packages() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::Install).unwrap();
        let _fault = fault::fail_after_commit_for("app");

        let err = execute_workset(
            &client,
            manifest,
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap_err();
        let partial = err.downcast_ref::<PartialInstallFailure>().unwrap();
        assert_eq!(
            partial.report.failure_code,
            crate::error::RuntimeErrorCode::LinkFailed
        );
        assert_eq!(
            partial
                .report
                .installed
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            vec!["dep"]
        );
        assert_eq!(
            partial
                .report
                .failed
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app"]
        );
        assert!(partial.report.skipped.is_empty());
        assert_eq!(partial.report.partial.len(), 1);
        assert_eq!(partial.report.partial[0].name, "app");
        assert!(partial.report.partial[0].path.ends_with("Cellar/app/1.0"));
        assert_eq!(partial.report.failed_phase.as_deref(), Some("keg_link"));
        assert!(partial
            .report
            .trace_path
            .as_ref()
            .is_some_and(|path| path.exists()));
        assert!(partial.report.trace_id.is_some());
        assert!(partial
            .report
            .suggested_commands
            .contains(&"glu autoremove".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_bump_keeps_old_keg_and_the_next_run_cleans_up() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        write_installed_receipt(&prefix, "app", "0.9");
        let client = client(prefix.clone());
        let manifest = manifest_with_dep(&prefix);
        let declared: BTreeSet<PackageName> =
            [PackageName("app".to_string())].into_iter().collect();

        // Ctrl+C after the new keg committed but before its final receipt
        // and before any removal.
        let state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let workset =
            planner::compute_workset(&manifest, &state, planner::WorksetMode::UpdateRoots).unwrap();
        let _fault = fault::fail_after_commit_for("app");
        let err = execute_workset(
            &client,
            manifest.clone(),
            workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("injected failure after committing app"));

        // Crash safety: the superseded keg is still installed and complete;
        // the new keg committed but never got its final receipt.
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/0.9")),
            Some(ReceiptStatus::Complete)
        );
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/1.0")),
            Some(ReceiptStatus::Incomplete)
        );

        // The next mutating command discards the uncommitted keg first, then
        // replans the bump with the superseded keg as the removal set.
        let recovered = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        assert!(!prefix.0.join("Cellar/app/1.0").exists());
        // The failed run did fully install dep before the fault hit app's
        // commit; the point of the crash-safety check is that app still has
        // exactly its old keg, never a half-committed extra.
        assert_eq!(recovered.kegs(&PackageName("app".to_string())).len(), 1);
        let retry_workset =
            planner::compute_workset(&manifest, &recovered, planner::WorksetMode::UpdateRoots)
                .unwrap();
        let predicted = crate::sync::predicted_dangling_after_workset(
            &recovered,
            &manifest,
            &retry_workset,
            &declared,
        )
        .unwrap();
        assert_eq!(predicted.len(), 1);
        assert_eq!(predicted[0].keg_version.0, "0.9");

        // Re-run completes the cleanup: only the new version remains.
        execute_workset(
            &client,
            manifest,
            retry_workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap();
        crate::remove::remove_installed_packages(&prefix, &predicted).unwrap();
        assert!(!prefix.0.join("Cellar/app/0.9").exists());
        assert_eq!(
            receipt_status(&prefix.0.join("Cellar/app/1.0")),
            Some(ReceiptStatus::Complete)
        );
        assert_eq!(
            prefix.0.join("opt/app").canonicalize().unwrap(),
            prefix.0.join("Cellar/app/1.0").canonicalize().unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_committed_root_is_cleaned_and_reinstalled_without_removing_dep() {
        let _serial = FAULT_TEST_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let manifest = manifest_with_dep(&prefix);
        let client = client(prefix.clone());

        let first_state = StateSnapshot::load_for_mutation(&prefix).unwrap().installed;
        let first_workset =
            planner::compute_workset(&manifest, &first_state, planner::WorksetMode::Install)
                .unwrap();
        let _fault = fault::fail_after_commit_for("app");
        let err = execute_workset(
            &client,
            manifest.clone(),
            first_workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("injected failure after committing app"));

        let app_keg = prefix.0.join("Cellar/app/1.0");
        let dep_keg = prefix.0.join("Cellar/dep/1.0");
        assert_eq!(receipt_status(&app_keg), Some(ReceiptStatus::Incomplete));
        assert_eq!(receipt_status(&dep_keg), Some(ReceiptStatus::Complete));

        let second_snapshot = StateSnapshot::load_for_mutation(&prefix).unwrap();
        assert!(!app_keg.exists());
        assert!(dep_keg.exists());
        let second_workset = planner::compute_workset(
            &manifest,
            &second_snapshot.installed,
            planner::WorksetMode::Install,
        )
        .unwrap();
        let to_install: Vec<&str> = second_workset
            .install
            .iter()
            .map(|id| id.0.as_str())
            .collect();
        assert_eq!(to_install, vec!["pkg:test/app@1.0"]);

        execute_workset(
            &client,
            manifest.clone(),
            second_workset,
            InstallOptions {
                yes: true,
                ..Default::default()
            },
            BTreeSet::new(),
            std::time::Instant::now(),
            Arc::new(SilentExecutionEvents),
        )
        .await
        .unwrap();

        assert_eq!(receipt_status(&app_keg), Some(ReceiptStatus::Complete));
        assert_eq!(receipt_status(&dep_keg), Some(ReceiptStatus::Complete));
    }

    fn client(prefix: Prefix) -> GluClient {
        GluClient::new(ClientConfig {
            prefix,
            target: Target("test-target".to_string()),
            registry_base_url: "https://registry.glu.invalid".to_string(),
            distribution_base_url: "https://example.invalid/releases".to_string(),
        })
    }

    fn rename_manifest_with_app(prefix: &Prefix) -> InstallManifest {
        let bar_id = PackageId("pkg:test/bar@1.0".to_string());
        let app_id = PackageId("pkg:test/app@1.0".to_string());
        let app_artifact = artifact(prefix, "app");
        let mut bar = package("bar", ArtifactId("art:test/bar".to_string()), Vec::new());
        bar.oldnames = vec![glu_core::PackageSelector("foo".to_string())];
        let mut packages = BTreeMap::new();
        packages.insert(bar_id.clone(), bar);
        packages.insert(
            app_id.clone(),
            package(
                "app",
                app_artifact.0.clone(),
                vec![PackageDependency {
                    package_key: glu_core::PackageKey("package:bar".to_string()),
                    package: bar_id,
                    requested_as: glu_core::PackageSelector("bar".to_string()),
                }],
            ),
        );
        let mut artifacts = BTreeMap::new();
        artifacts.insert(app_artifact.0, app_artifact.1);
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: glu_core::ResolveRequestEcho {
                name: vec![PackageSelector("app".to_string())],
                target: Target("test-target".to_string()),
                slim: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("app".to_string()),
                package_key: glu_core::PackageKey("package:app".to_string()),
                package: app_id,
            }],
            packages,
            artifacts,
        }
    }

    fn write_installed_receipt(prefix: &Prefix, name: &str, version: &str) {
        let keg = prefix.0.join("Cellar").join(name).join(version);
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(keg.join("bin").join(name), b"tool").unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:test/{name}@{version}")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: KegVersion(version.to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId(format!("art:test/{name}")),
                sha256: "deadbeef".to_string(),
                bottle_tag: "test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.0.join("opt").join(name),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                exposure: glu_core::Exposure::Global,
                linked: true,
                link_overwrite: Vec::new(),
                deps: Vec::new(),
                dependency_requirements: Default::default(),
            },
        };
        InstalledStateStore::write_receipt_at_keg(&keg, &receipt).unwrap();
    }

    fn manifest_with_dep(prefix: &Prefix) -> InstallManifest {
        let dep_id = PackageId("pkg:test/dep@1.0".to_string());
        let app_id = PackageId("pkg:test/app@1.0".to_string());
        let dep_artifact = artifact(prefix, "dep");
        let app_artifact = artifact(prefix, "app");
        let mut packages = BTreeMap::new();
        packages.insert(
            dep_id.clone(),
            package("dep", dep_artifact.0.clone(), Vec::new()),
        );
        packages.insert(
            app_id.clone(),
            package(
                "app",
                app_artifact.0.clone(),
                vec![PackageDependency {
                    package_key: glu_core::PackageKey("package:dep".to_string()),
                    package: dep_id,
                    requested_as: glu_core::PackageSelector("dep".to_string()),
                }],
            ),
        );
        let mut artifacts = BTreeMap::new();
        artifacts.insert(dep_artifact.0, dep_artifact.1);
        artifacts.insert(app_artifact.0, app_artifact.1);
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: glu_core::ResolveRequestEcho {
                name: vec![PackageSelector("app".to_string())],
                target: Target("test-target".to_string()),
                slim: false,
            },
            roots: vec![glu_core::PackageSelection {
                requested_as: glu_core::PackageSelector("app".to_string()),
                package_key: glu_core::PackageKey("package:app".to_string()),
                package: app_id,
            }],
            packages,
            artifacts,
        }
    }

    fn package(name: &str, artifact: ArtifactId, deps: Vec<PackageDependency>) -> ResolvedPackage {
        let dependency_requirements = deps
            .iter()
            .map(|dependency| {
                (
                    dependency.package_key.clone(),
                    glu_core::MinimumVersion {
                        version: "1.0".to_string(),
                        revision: Some(0),
                    },
                )
            })
            .collect();

        ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            deps,
            dependency_requirements,
            exposure: glu_core::Exposure::Global,
            artifact,
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                link_overwrite: Vec::new(),
                post_install_defined: false,
                post_install_steps: Vec::new(),
                postinstall_network_access_allowed: true,
            },
        }
    }

    fn artifact(prefix: &Prefix, name: &str) -> (ArtifactId, ResolvedArtifact) {
        let bytes = bottle_bytes(name);
        let sha256 = crate::hash::sha256_hex(&bytes);
        let cache_path = ArtifactCache::new(prefix).path_for_sha256(&sha256);
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        (
            ArtifactId(format!("art:test/{name}")),
            ResolvedArtifact {
                url: format!("https://ghcr.io/v2/test/{name}/blobs/sha256:{sha256}"),
                sha256,
                bytes: None,
                bottle_tag: "test".to_string(),
                cellar: ":any_skip_relocation".to_string(),
                built_on: None,
            },
        )
    }

    fn bottle_bytes(name: &str) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let path = format!("{name}/1.0/bin/{name}");
        let contents = format!("#!/bin/sh\necho {name}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, path, contents.as_bytes())
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn block_trace_writes(prefix: &Prefix) {
        let glu_var = prefix.0.join("var/glu");
        std::fs::create_dir_all(&glu_var).unwrap();
        std::fs::write(glu_var.join("traces"), b"not a directory").unwrap();
    }

    fn receipt_status(keg: &std::path::Path) -> Option<ReceiptStatus> {
        let bytes = std::fs::read(InstalledStateStore::receipt_path_for_keg(keg)).ok()?;
        let receipt: GluInstallReceipt = serde_json::from_slice(&bytes).ok()?;
        Some(receipt.status)
    }

    fn staging_entries(prefix: &Prefix) -> usize {
        std::fs::read_dir(prefix.0.join("var/glu/staging"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}
