pub mod activation;
pub mod bottle;
pub mod config;
pub mod deps;
pub mod download;
pub mod error;
pub mod events;
pub mod format;
pub mod hash;
pub mod install;
pub mod link;
pub mod outdated;
mod path_component;
pub mod postinstall;
pub mod registry;
pub mod remove;
pub mod shell;
pub mod state;
pub mod style;
mod sync;
pub mod trace;
pub mod tree_render;
pub mod upgrade;
pub mod validation;
mod worker_output;

use anyhow::{Context, Result};
use config::ClientConfig;
use glu_core::{InstalledPackage, PackageKey, PackageName, PackageSelector};
use install::InstallOptions;

/// Immutable local package state loaded once for one CLI invocation.
///
/// Read commands derive every local view from this value. Mutation planning
/// owns separate snapshots and must load again after writing state.
#[derive(Debug, Clone)]
pub struct LocalQuery {
    snapshot: state::snapshot::StateSnapshot,
}

impl LocalQuery {
    pub fn list(&self) -> Vec<InstalledPackage> {
        self.snapshot.installed.list()
    }

    pub fn declared(&self) -> Vec<InstalledPackage> {
        self.snapshot.installed.declared()
    }

    pub fn declared_names(&self) -> Vec<PackageName> {
        self.snapshot.installed.declared_names()
    }

    pub fn deactivated_names(&self) -> Vec<PackageName> {
        self.snapshot.installed.deactivated_names()
    }

    pub fn total_kegs(&self) -> usize {
        self.snapshot.installed.total_kegs()
    }

    pub fn list_tree(&self) -> Vec<state::installed::DependencyTreeNode> {
        self.snapshot.installed.dependency_tree()
    }

    pub fn list_tree_all(&self) -> Vec<state::installed::DependencyTreeNode> {
        self.snapshot.installed.dependency_tree_all()
    }

    pub fn find_by_key(&self, key: &PackageKey) -> Option<&InstalledPackage> {
        self.snapshot.installed.find_by_key(key)
    }

    pub fn resolve_selector(&self, selector: &PackageSelector) -> Option<&InstalledPackage> {
        self.snapshot.installed.resolve_selector(selector)
    }

    pub fn package_statuses(&self) -> std::collections::BTreeMap<PackageName, deps::PackageStatus> {
        deps_statuses(&self.snapshot.installed, &self.snapshot.declaration)
    }

    pub fn why(&self, selector: &PackageSelector, include_statuses: bool) -> deps::ReverseDepsView {
        deps::ReverseDepsView {
            root: self.snapshot.installed.reverse_dependents_tree(selector),
            statuses: if include_statuses {
                self.package_statuses()
            } else {
                std::collections::BTreeMap::new()
            },
        }
    }
}

pub struct GluClient {
    config: ClientConfig,
}

impl GluClient {
    pub fn new(config: ClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    pub fn query_state(&self, events: &dyn events::ExecutionEvents) -> Result<LocalQuery> {
        let snapshot = state::snapshot::StateSnapshot::load(&self.config.prefix)?;
        for warning in &snapshot.warnings {
            events.notice(events::OutputStream::Stderr, warning);
        }
        Ok(LocalQuery { snapshot })
    }

    /// The declared package names from `glu.json` — what bare `glu install`
    /// syncs toward.
    pub fn declaration_names(&self) -> Result<Vec<PackageName>> {
        Ok(
            state::store::InstalledStateStore::new(self.config.prefix.clone())
                .load_declaration()?
                .names()
                .into_iter()
                .collect(),
        )
    }

    pub async fn plan_install(
        &self,
        names: Vec<PackageSelector>,
        options: InstallOptions,
    ) -> Result<install::InstallPlan> {
        install::plan_install(self, names, options).await
    }

    pub async fn execute_install(
        &self,
        plan: install::InstallPlan,
        options: InstallOptions,
        events: std::sync::Arc<dyn events::ExecutionEvents>,
    ) -> Result<install::InstallSummary> {
        install::execute_install(self, plan, options, events).await
    }

    /// Validates reinstall targets before planning/execution. `reinstall`
    /// repours installed packages only; `glu install --force` is the variant
    /// that installs absent packages instead.
    pub fn validate_reinstall_targets(&self, selectors: &[PackageSelector]) -> Result<()> {
        let state = state::snapshot::StateSnapshot::load(&self.config.prefix)?.installed;
        let missing = selectors
            .iter()
            .filter(|selector| state.resolve_selector(selector).is_none())
            .map(|selector| PackageName(selector.0.clone()))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let plain = missing
                .iter()
                .map(|name| name.0.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            return Err(error::NotInstalledError::packages(
                missing,
                Some(format!("glu install {plain}")),
            )
            .into());
        }
        Ok(())
    }

    /// Plans a removal from a read-only state snapshot: resolves the selectors
    /// against the installed set, decides what would be removed (the named
    /// targets plus everything that becomes dangling once they are gone)
    /// and what would be demoted-and-kept because other declared packages
    /// still need it. The CLI uses the difference between `to_remove` and
    /// `named` to decide whether to ask for confirmation. See
    /// `remove::plan_removal`.
    pub fn activate(
        &self,
        names: Vec<PackageSelector>,
        force: bool,
    ) -> Result<Vec<activation::ActivationResult>> {
        activation::activate_packages(&self.config.prefix, names, force)
    }

    pub fn deactivate(
        &self,
        names: Vec<PackageSelector>,
    ) -> Result<Vec<activation::DeactivationResult>> {
        activation::deactivate_packages(&self.config.prefix, names)
    }

    pub fn plan_removal(&self, selectors: Vec<PackageSelector>) -> Result<remove::RemovalPlan> {
        remove::plan_removal(
            &self.config.prefix,
            selectors.into_iter().map(|selector| selector.0).collect(),
        )
    }

    /// Executes a planned removal: demotes the kept packages (their
    /// declaration membership changes) and removes every keg in
    /// `plan.to_remove`. Returns the removed packages, one per keg. See
    /// `remove::execute_removal`.
    pub fn execute_removal(
        &self,
        plan: &remove::RemovalPlan,
    ) -> Result<Vec<remove::RemovedPackage>> {
        remove::execute_removal(&self.config.prefix, plan)
    }

    /// `glu deps <name>`: the forward dependency tree of one package.
    /// Installed → receipts (offline, exact) unless `online` forces the
    /// registry answer; not installed → a slim registry resolve (online,
    /// marked `Resolved` in the view).
    pub async fn deps(
        &self,
        query: &LocalQuery,
        selector: PackageSelector,
        online: bool,
        include_statuses: bool,
    ) -> Result<deps::DepsView> {
        let statuses = if include_statuses {
            query.package_statuses()
        } else {
            std::collections::BTreeMap::new()
        };
        let installed = query.resolve_selector(&selector).is_some();
        if !online {
            if let Some(root) = query.snapshot.installed.dependency_tree_for(&selector) {
                return Ok(deps::DepsView {
                    source: deps::DepsSource::Installed,
                    installed: true,
                    root,
                    statuses,
                });
            }
        }
        let resolve =
            registry::resolve_client::HttpResolveClient::new(&self.config.registry_base_url)?;
        let manifest = resolve
            .resolve_slim(&glu_core::ResolveRequest {
                names: vec![selector.clone()],
                target: self.config.target.clone(),
            })
            .await
            .with_context(|| {
                format!(
                    "'{}' is not installed and its dependencies could not be resolved",
                    selector.0
                )
            })?;
        let root_id = manifest
            .roots
            .first()
            .map(|root| &root.package)
            .ok_or_else(|| anyhow::anyhow!("registry returned nothing for '{}'", selector.0))?;
        let root = install::dependency_tree_from_slim(&manifest, root_id)
            .ok_or_else(|| anyhow::anyhow!("registry result is missing '{}'", selector.0))?;
        Ok(deps::DepsView {
            source: deps::DepsSource::Resolved,
            installed,
            root,
            statuses,
        })
    }

    /// `glu uses <name>`: the reverse dependency tree from the registry —
    /// who (transitively) could install this. Always online; `direct`
    /// limits the query to one-hop dependents. `None` when the registry
    /// returns nothing for the name.
    pub async fn uses(
        &self,
        selector: PackageSelector,
        direct: bool,
    ) -> Result<Option<state::installed::DependencyTreeNode>> {
        let resolve =
            registry::resolve_client::HttpResolveClient::new(&self.config.registry_base_url)?;
        let response = resolve
            .uses(&selector, &self.config.target, direct)
            .await
            .with_context(|| format!("could not look up what depends on '{}'", selector.0))?;
        Ok(install::reverse_tree_from_uses(&response))
    }

    /// The dangling packages of this prefix — installed, not declared, and
    /// not needed by anything declared. The repair set for bad state;
    /// normally empty since every command already removes dangling
    /// packages. See `remove::plan_autoremove`.
    pub fn plan_autoremove(&self) -> Result<Vec<glu_core::InstalledPackage>> {
        remove::plan_autoremove(&self.config.prefix)
    }

    /// Removes every package in `dangling`. See `remove::execute_autoremove`.
    pub fn execute_autoremove(
        &self,
        dangling: &[glu_core::InstalledPackage],
    ) -> Result<Vec<remove::RemovedPackage>> {
        remove::execute_autoremove(&self.config.prefix, dangling)
    }

    /// Registry info for `name`, plus the locally installed keg (if any).
    pub async fn info(
        &self,
        query: &LocalQuery,
        selector: PackageSelector,
    ) -> Result<(glu_core::InfoResponse, Option<glu_core::InstalledPackage>)> {
        let info =
            registry::resolve_client::HttpResolveClient::new(&self.config.registry_base_url)?
                .info(&selector, &self.config.target)
                .await?;
        let installed = query.find_by_key(&info.package_key).cloned();
        Ok((info, installed))
    }

    /// Installed packages with a newer version available in the registry,
    /// sorted by name.
    pub async fn outdated(&self, query: &LocalQuery) -> Result<outdated::OutdatedResult> {
        let state = &query.snapshot.installed;
        let names = state
            .names()
            .into_iter()
            .map(|name| PackageSelector(name.0))
            .collect::<Vec<_>>();
        let (response, latest_glu_version) =
            registry::resolve_client::HttpResolveClient::new(&self.config.registry_base_url)?
                .outdated(&names, &self.config.target)
                .await?;
        Ok(outdated::OutdatedResult {
            packages: outdated::outdated_entries(state, &response.packages),
            latest_glu_version,
        })
    }

    /// Plans an update from a read-only state snapshot: resolves the target
    /// set (bare = all declared, `--all` includes automatic packages,
    /// named = exactly those) and computes what would be removed once the
    /// new versions land (dependencies a new bottle dropped). The CLI uses
    /// `to_update`/`to_remove` to decide whether to ask for confirmation.
    /// See `install::plan_update`.
    pub async fn plan_update(
        &self,
        names: Vec<PackageSelector>,
        all: bool,
        dependents: bool,
    ) -> Result<install::UpdatePlan> {
        install::plan_update(self, names, all, dependents).await
    }

    /// Executes a planned update: runs the resolved workset (membership is
    /// unchanged) and removes the packages the plan flagged as dangling.
    /// See `install::execute_update`.
    pub async fn execute_update(
        &self,
        plan: install::UpdatePlan,
        verbose: bool,
        events: std::sync::Arc<dyn events::ExecutionEvents>,
    ) -> Result<install::UpdateSummary> {
        install::execute_update(self, plan, verbose, events).await
    }

    /// Self-update of the glu client: compares against the registry's latest
    /// glu version header (no download when already current) and replaces the
    /// running binary when newer.
    pub async fn upgrade(
        &self,
        events: &dyn events::ExecutionEvents,
    ) -> Result<upgrade::UpgradeResult> {
        upgrade::upgrade(&self.config, events).await
    }

    pub fn shell_statuses(&self) -> Result<Vec<shell::ShellStatus>> {
        shell::shell_statuses()
    }

    pub fn shellenv(&self, shell_name: Option<&str>) -> Result<String> {
        shell::shellenv(&self.config, shell_name)
    }

    pub fn setup_shells(&self) -> Result<shell::SetupResult> {
        shell::setup_shells(&self.config)
    }
}

fn deps_statuses(
    state: &state::installed::InstalledState,
    declaration: &state::Declaration,
) -> std::collections::BTreeMap<PackageName, deps::PackageStatus> {
    state
        .list()
        .into_iter()
        .map(|package| {
            let declared = declaration.contains(&package.name);
            let deactivated = state.is_deactivated(&PackageSelector(package.name.0.clone()));
            (
                package.name.clone(),
                deps::PackageStatus {
                    installed: true,
                    linked: package.linked,
                    declared,
                    deactivated,
                    download_bytes: package.download_bytes,
                    installed_bytes: package.installed_bytes,
                },
            )
        })
        .collect()
}
