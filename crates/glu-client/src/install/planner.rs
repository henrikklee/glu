use crate::{
    install::{graph, manifest_lookup::ManifestLookup},
    state::installed::{compare_versions, InstalledState},
};
use anyhow::{bail, Result};
use glu_core::{
    InstallManifest, InstalledPackage, PackageDependency, PackageId, PackageName, ResolvedPackage,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct InstallWorkSet {
    pub satisfied: Vec<PackageId>,
    /// Existing installed packages that already satisfy the resolved package
    /// version but need a local old-name -> current-name transition before
    /// dependents can link against the canonical opt path. The executor node
    /// is a commit-stage dependency anchor; actual rename semantics are added
    /// separately.
    pub rename: Vec<RenameWorkItem>,
    pub install: Vec<PackageId>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenameWorkItem {
    pub package: PackageId,
    pub old_name: PackageName,
    pub old_keg_version: String,
    pub old_version: String,
    pub old_revision: u32,
    pub old_keg_path: PathBuf,
}

/// One Homebrew-style formula-installer expansion. Every dependency visited
/// from this root is checked against the complete flattened requirement map
/// recorded by this root's selected bottle, not the immediate parent's map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct InstallerRootContext {
    package: PackageId,
}

struct WorksetSelection<'a> {
    selected: &'a mut BTreeSet<PackageId>,
    satisfied: &'a mut BTreeSet<PackageId>,
    rename: &'a mut BTreeSet<RenameWorkItem>,
}

/// How a command's requested roots and their dependency closure become
/// install/satisfied work. One public planner boundary owns the modes; private
/// passes may differ where command semantics genuinely require different
/// traversal behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorksetMode {
    /// `glu install`: a root is satisfied by *any* installed version
    /// (`install` is idempotent by name — only `update` changes an
    /// installed package's version), so a satisfied root's dependency edges
    /// are never walked and absent roots flow through the regular
    /// real-checked closure.
    Install,
    /// `glu install --force` / `glu reinstall`: every named root repours
    /// regardless of installed state, and the whole closure is real-checked
    /// below them. "No matter how the system is right now, make it as I say."
    Force,
    /// Named and bare `glu update`: update selected roots when their release
    /// or persisted package facts differ, but retain dependencies that still
    /// satisfy every active installer-root context. Roots are inspected even
    /// when already current so newly required or changed dependency topology
    /// is reconciled.
    UpdateRoots,
    /// `glu update --all`: inspect every package in the resolved declared-root
    /// closure and select every release or persisted package fact that differs
    /// from installed state. Exact matches are not repoured.
    UpdateAll,
    /// `glu install --force --deps` / `glu reinstall --deps`: repour the
    /// full dependency closure of the named roots.
    ReinstallDeps,
}

/// Turns a resolve manifest into install/satisfied work according to
/// `mode`. The `InstallWorkSet` is the contract the DAG consumes: `install`
/// is what gets downloaded/prepared/linked/registered, `satisfied` is what
/// is already present and only needs ordering/presence (their committed
/// markers gate linking).
pub fn compute_workset(
    manifest: &InstallManifest,
    state: &InstalledState,
    mode: WorksetMode,
) -> Result<InstallWorkSet> {
    match mode {
        WorksetMode::Install => {
            let mut satisfied = BTreeSet::new();
            let mut rename = BTreeSet::new();
            let mut selected = BTreeSet::new();
            let mut context_roots = Vec::new();

            for root in &manifest.roots {
                let root_id = &root.package;
                let package = manifest.require_package(root_id)?;
                // Any installed version satisfies a root, not just an exact
                // match: `install` is idempotent by name, `update` is the
                // only path that changes an installed package's version. If
                // the installed package is still under a previous name, plan
                // a commit-stage rename transition instead of downloading.
                match root_match(root_id, package, state)? {
                    PackageMatch::Current => {
                        satisfied.insert(root_id.clone());
                    }
                    PackageMatch::PreviousName(rename_item) => {
                        rename.insert(RenameWorkItem {
                            package: root_id.clone(),
                            ..rename_item
                        });
                    }
                    PackageMatch::Missing => {
                        selected.insert(root_id.clone());
                        context_roots.push(root_id.clone());
                    }
                }
            }

            expand_installer_contexts(
                manifest,
                state,
                &context_roots,
                &mut selected,
                &mut satisfied,
                &mut rename,
            )?;
            let install = graph::order_selected_packages(manifest, &selected)?;

            Ok(finalize_workset(satisfied, rename, install))
        }
        WorksetMode::Force => {
            // Roots repour unconditionally; the walk covers the full
            // closure (all roots, not just absent ones) with contextual
            // dependency satisfaction below them, so a dependency
            // introduced by the version bump is discovered rather than
            // silently missing.
            let mut satisfied = BTreeSet::new();
            let mut rename = BTreeSet::new();
            let roots = manifest.root_package_ids();
            let mut selected = roots.iter().cloned().collect();
            expand_installer_contexts(
                manifest,
                state,
                &roots,
                &mut selected,
                &mut satisfied,
                &mut rename,
            )?;
            let install = graph::order_selected_packages(manifest, &selected)?;
            Ok(finalize_workset(satisfied, rename, install))
        }
        WorksetMode::UpdateRoots => update_roots_workset(manifest, state),
        WorksetMode::UpdateAll => update_all_workset(manifest, state),
        WorksetMode::ReinstallDeps => {
            let install = graph::dependency_closure_order(manifest, &manifest.root_package_ids())?;
            Ok(InstallWorkSet {
                satisfied: vec![],
                rename: Vec::new(),
                install,
            })
        }
    }
}

fn update_roots_workset(
    manifest: &InstallManifest,
    state: &InstalledState,
) -> Result<InstallWorkSet> {
    let mut selected = BTreeSet::new();
    let mut satisfied = BTreeSet::new();
    let mut rename = BTreeSet::new();
    let context_roots = manifest.root_package_ids();

    for root in &manifest.roots {
        let package = manifest.require_package(&root.package)?;
        record_selected_package_match(
            &root.package,
            package,
            state,
            &mut selected,
            &mut satisfied,
            &mut rename,
        )?;
    }

    expand_installer_contexts(
        manifest,
        state,
        &context_roots,
        &mut selected,
        &mut satisfied,
        &mut rename,
    )?;
    let install = graph::order_selected_packages(manifest, &selected)?;
    Ok(finalize_workset(satisfied, rename, install))
}

/// Expands every active installer root with that root's own flattened
/// requirement map. Packages selected for installation become installer
/// roots in turn, matching Homebrew's nested `FormulaInstaller` behavior.
///
/// Selection is monotonic: if any context needs a package, another context
/// cannot turn it back into satisfied work. Each context still walks through
/// satisfied dependencies so their children are checked with the same root
/// requirements (`Dependency::SKIP`, not `PRUNE`).
fn expand_installer_contexts(
    manifest: &InstallManifest,
    state: &InstalledState,
    initial_context_roots: &[PackageId],
    selected: &mut BTreeSet<PackageId>,
    satisfied: &mut BTreeSet<PackageId>,
    rename: &mut BTreeSet<RenameWorkItem>,
) -> Result<()> {
    let mut pending: BTreeSet<PackageId> = initial_context_roots.iter().cloned().collect();
    pending.extend(selected.iter().cloned());
    let mut completed = BTreeSet::new();

    while let Some(context_root) = pending.iter().next().cloned() {
        pending.remove(&context_root);
        if !completed.insert(context_root.clone()) {
            continue;
        }

        let context = InstallerRootContext {
            package: context_root.clone(),
        };
        let mut expanded = BTreeSet::new();
        let mut work = WorksetSelection {
            selected: &mut *selected,
            satisfied: &mut *satisfied,
            rename: &mut *rename,
        };
        expand_dependency_topology(
            &context_root,
            &context,
            manifest,
            state,
            &mut work,
            &mut expanded,
        )?;

        pending.extend(
            work.selected
                .iter()
                .filter(|package| !completed.contains(*package))
                .cloned(),
        );
    }

    Ok(())
}

fn expand_dependency_topology(
    package_id: &PackageId,
    context: &InstallerRootContext,
    manifest: &InstallManifest,
    state: &InstalledState,
    work: &mut WorksetSelection<'_>,
    expanded: &mut BTreeSet<PackageId>,
) -> Result<()> {
    if !expanded.insert(package_id.clone()) {
        return Ok(());
    }

    let package = manifest.require_package(package_id)?;
    for dependency in &package.deps {
        match dependency_match(context, dependency, manifest, state)? {
            PackageMatch::Current => {
                work.satisfied.insert(dependency.package.clone());
            }
            PackageMatch::PreviousName(rename_item) => {
                work.rename.insert(RenameWorkItem {
                    package: dependency.package.clone(),
                    ..rename_item
                });
            }
            PackageMatch::Missing => {
                work.selected.insert(dependency.package.clone());
            }
        }

        // A satisfied dependency is skipped as install work, but its children
        // remain part of this installer root's expansion.
        expand_dependency_topology(
            &dependency.package,
            context,
            manifest,
            state,
            work,
            expanded,
        )?;
    }
    Ok(())
}

fn update_all_workset(
    manifest: &InstallManifest,
    state: &InstalledState,
) -> Result<InstallWorkSet> {
    let mut selected = BTreeSet::new();
    let mut satisfied = BTreeSet::new();
    let mut rename = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    for root in &manifest.roots {
        inspect_complete_closure(
            &root.package,
            manifest,
            state,
            &mut selected,
            &mut satisfied,
            &mut rename,
            &mut expanded,
        )?;
    }
    let install = graph::order_selected_packages(manifest, &selected)?;
    Ok(finalize_workset(satisfied, rename, install))
}

fn inspect_complete_closure(
    package_id: &PackageId,
    manifest: &InstallManifest,
    state: &InstalledState,
    selected: &mut BTreeSet<PackageId>,
    satisfied: &mut BTreeSet<PackageId>,
    rename: &mut BTreeSet<RenameWorkItem>,
    expanded: &mut BTreeSet<PackageId>,
) -> Result<()> {
    if !expanded.insert(package_id.clone()) {
        return Ok(());
    }
    let package = manifest.require_package(package_id)?;
    record_selected_package_match(package_id, package, state, selected, satisfied, rename)?;
    for dependency in &package.deps {
        inspect_complete_closure(
            &dependency.package,
            manifest,
            state,
            selected,
            satisfied,
            rename,
            expanded,
        )?;
    }
    Ok(())
}

fn record_selected_package_match(
    package_id: &PackageId,
    package: &ResolvedPackage,
    state: &InstalledState,
    selected: &mut BTreeSet<PackageId>,
    satisfied: &mut BTreeSet<PackageId>,
    rename: &mut BTreeSet<RenameWorkItem>,
) -> Result<()> {
    match selected_package_match(package_id, package, state)? {
        PackageMatch::Current => {
            satisfied.insert(package_id.clone());
        }
        PackageMatch::PreviousName(rename_item) => {
            rename.insert(rename_item);
        }
        PackageMatch::Missing => {
            selected.insert(package_id.clone());
        }
    }
    Ok(())
}

fn finalize_workset(
    mut satisfied: BTreeSet<PackageId>,
    mut rename: BTreeSet<RenameWorkItem>,
    install: Vec<PackageId>,
) -> InstallWorkSet {
    let installing: BTreeSet<_> = install.iter().cloned().collect();
    satisfied.retain(|package| !installing.contains(package));
    rename.retain(|item| !installing.contains(&item.package));
    InstallWorkSet {
        satisfied: satisfied.into_iter().collect(),
        rename: rename.into_iter().collect(),
        install,
    }
}

/// `update`: expands the target set to also cover any installed,
/// already-outdated package that depends — directly or transitively — on
/// one of `target_names`. Mirrors Homebrew's `upgrade_dependents` "also
/// upgrade outdated runtime dependents" pass (`upgrade.rb`). `--all` never
/// needs this because it reconciles the complete declared-root closure; only
/// a named `update` can miss a dependent outside its resolve request.
/// Returns just the newly-discovered names (caller extends the target list
/// and re-resolves); a single pass suffices, `depends_on_any`'s walk is
/// already transitive per candidate.
pub fn cascade_outdated_dependents(
    target_names: &[PackageName],
    outdated_names: &BTreeSet<PackageName>,
    state: &InstalledState,
) -> Vec<PackageName> {
    let target_names_set: BTreeSet<PackageName> = target_names.iter().cloned().collect();
    let mut added = Vec::new();
    for package in state.list() {
        if target_names_set.contains(&package.name) || !outdated_names.contains(&package.name) {
            continue;
        }
        if state.depends_on_any(&package, &target_names_set) {
            added.push(package.name.clone());
        }
    }
    added
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PackageMatch {
    Current,
    PreviousName(RenameWorkItem),
    Missing,
}

fn selected_package_match(
    package_id: &PackageId,
    package: &ResolvedPackage,
    state: &InstalledState,
) -> Result<PackageMatch> {
    if let Some(installed) = state.find_by_key(&package.package_key) {
        return Ok(
            if installed_package_facts_match(installed, package_id, package) {
                PackageMatch::Current
            } else {
                PackageMatch::Missing
            },
        );
    }
    previous_name_match(package_id, package, state, |installed| {
        &installed.id == package_id
    })
}

fn installed_package_facts_match(
    installed: &InstalledPackage,
    package_id: &PackageId,
    package: &ResolvedPackage,
) -> bool {
    let mut installed_aliases: Vec<_> = installed.aliases.iter().collect();
    installed_aliases.sort();
    let mut resolved_aliases: Vec<_> = package.aliases.iter().collect();
    resolved_aliases.sort();
    let mut installed_oldnames: Vec<_> = installed.oldnames.iter().collect();
    installed_oldnames.sort();
    let mut resolved_oldnames: Vec<_> = package.oldnames.iter().collect();
    resolved_oldnames.sort();
    let mut installed_topology: Vec<_> = installed
        .deps
        .iter()
        .map(|dependency| (&dependency.package_key, &dependency.requested_as))
        .collect();
    installed_topology.sort();
    let mut resolved_topology: Vec<_> = package
        .deps
        .iter()
        .map(|dependency| (&dependency.package_key, &dependency.requested_as))
        .collect();
    resolved_topology.sort();

    &installed.id == package_id
        && installed.package_key == package.package_key
        && installed.name == package.name
        && installed.version == package.version
        && installed.revision == package.revision
        && installed.keg_version == package.keg_version
        && installed_aliases == resolved_aliases
        && installed_oldnames == resolved_oldnames
        && installed_topology == resolved_topology
        && installed.dependency_requirements == package.dependency_requirements
        && installed.keg_only == package.install.keg_only
}

fn root_match(
    package_id: &PackageId,
    package: &ResolvedPackage,
    state: &InstalledState,
) -> Result<PackageMatch> {
    if state.find_by_key(&package.package_key).is_some() {
        return Ok(PackageMatch::Current);
    }
    previous_name_match(package_id, package, state, |_| true)
}

/// A dependency is satisfied by the selected package itself, or by another
/// installed version of the same stable package identity that meets the
/// active installer root's minimum version. The lookup uses the dependency's
/// exact stable key; aliases and old names are not requirement-map keys. If
/// the satisfying package is still installed under a previous name, emit a
/// rename anchor instead of treating the canonical package as committed.
fn dependency_match(
    context: &InstallerRootContext,
    dependency: &PackageDependency,
    manifest: &InstallManifest,
    state: &InstalledState,
) -> Result<PackageMatch> {
    let selected_package = manifest.require_package(&dependency.package)?;
    let installer_root = manifest.require_package(&context.package)?;
    let minimum = installer_root
        .dependency_requirements
        .get(&dependency.package_key);

    let satisfies = |installed: &InstalledPackage| {
        dependency_requirement_satisfied(installed, &dependency.package, minimum)
    };

    if let Some(installed) = state.find_by_key(&dependency.package_key) {
        return Ok(if satisfies(installed) {
            PackageMatch::Current
        } else {
            PackageMatch::Missing
        });
    }

    previous_name_match(&dependency.package, selected_package, state, satisfies)
}

fn previous_name_match(
    package_id: &PackageId,
    package: &ResolvedPackage,
    state: &InstalledState,
    satisfies: impl Fn(&InstalledPackage) -> bool,
) -> Result<PackageMatch> {
    let mut matched = Vec::new();
    for oldname in &package.oldnames {
        let old_name = PackageName(oldname.0.clone());
        let Some(installed) = state.find(&old_name) else {
            continue;
        };
        if satisfies(installed) {
            matched.push(old_name);
        }
    }
    match matched.as_slice() {
        [] => Ok(PackageMatch::Missing),
        [old_name] => {
            let installed = state
                .find(old_name)
                .expect("matched old name must still resolve exactly");
            Ok(PackageMatch::PreviousName(RenameWorkItem {
                package: package_id.clone(),
                old_name: old_name.clone(),
                old_keg_version: installed.keg_version.0.clone(),
                old_version: installed.version.clone(),
                old_revision: installed.revision,
                old_keg_path: installed.keg_path.clone(),
            }))
        }
        _ => bail!(
            "resolved package {} has multiple installed previous names: {}",
            package_id.0,
            matched
                .iter()
                .map(|name| name.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn dependency_requirement_satisfied(
    installed: &InstalledPackage,
    selected_package: &PackageId,
    minimum: Option<&glu_core::MinimumVersion>,
) -> bool {
    if &installed.id == selected_package {
        return true;
    }

    let Some(minimum) = minimum else {
        return false;
    };
    match compare_versions(&installed.version, &minimum.version) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => minimum
            .revision
            .is_none_or(|revision| installed.revision >= revision),
    }
}

pub fn validate_manifest(manifest: &InstallManifest) -> Result<()> {
    if manifest.schema != "glu.resolve.v1" {
        bail!("unsupported resolve schema {}", manifest.schema);
    }

    let mut packages_by_key: BTreeMap<&glu_core::PackageKey, &PackageId> = BTreeMap::new();
    let mut selector_index: BTreeMap<glu_core::PackageSelector, &glu_core::PackageKey> =
        BTreeMap::new();
    for (package_id, package) in &manifest.packages {
        if let Some(existing) = packages_by_key.insert(&package.package_key, package_id) {
            bail!(
                "package key {} belongs to both {} and {}",
                package.package_key.0,
                existing.0,
                package_id.0
            );
        }
        let selectors = std::iter::once(glu_core::PackageSelector(package.name.0.clone()))
            .chain(package.aliases.iter().cloned())
            .chain(package.oldnames.iter().cloned());
        for selector in selectors {
            if let Some(existing) = selector_index.insert(selector.clone(), &package.package_key) {
                if existing != &package.package_key {
                    bail!(
                        "package selector {} belongs to both {} and {}",
                        selector.0,
                        existing.0,
                        package.package_key.0
                    );
                }
            }
        }
    }

    for root in &manifest.roots {
        let package = manifest.require_package(&root.package)?;
        if root.package_key != package.package_key {
            bail!(
                "root selector {} targets {} but package {} advertises {}",
                root.requested_as.0,
                root.package_key.0,
                root.package.0,
                package.package_key.0
            );
        }
        if selector_index.get(&root.requested_as) != Some(&&root.package_key) {
            bail!(
                "root selector {} does not resolve to {}",
                root.requested_as.0,
                root.package_key.0
            );
        }
    }

    for (id, package) in &manifest.packages {
        if !manifest.artifacts.contains_key(&package.artifact) {
            bail!(
                "package {} references missing artifact {}",
                id.0,
                package.artifact.0
            );
        }
        for dep in &package.deps {
            let target = manifest.require_package(&dep.package)?;
            if dep.package_key != target.package_key {
                bail!(
                    "dependency {} targets {} but package {} advertises {}",
                    dep.requested_as.0,
                    dep.package_key.0,
                    dep.package.0,
                    target.package_key.0
                );
            }
            if selector_index.get(&dep.requested_as) != Some(&&dep.package_key) {
                bail!(
                    "dependency selector {} does not resolve to {}",
                    dep.requested_as.0,
                    dep.package_key.0
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store::InstalledStateStore;
    use glu_core::{
        ArtifactId, KegVersion, PackageDependency, PackageInstallMetadata, PackageName,
        ResolveRequestEcho, ResolvedArtifact, ResolvedPackage, Target,
    };
    use std::collections::BTreeMap;

    fn pkg(name: &str, deps: Vec<&str>) -> (PackageId, ResolvedPackage, ResolvedArtifact) {
        pkg_at(name, "1.0", deps)
    }

    fn pkg_at(
        name: &str,
        version: &str,
        deps: Vec<&str>,
    ) -> (PackageId, ResolvedPackage, ResolvedArtifact) {
        let id = PackageId(format!("pkg:homebrew/core/{name}@{version}"));
        let package = ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: version.to_string(),
            revision: 0,
            keg_version: KegVersion(version.to_string()),
            deps: deps
                .iter()
                .map(|dep| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:homebrew/core/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector((*dep).to_string()),
                })
                .collect(),
            dependency_requirements: deps
                .into_iter()
                .map(|dep| {
                    (
                        glu_core::PackageKey(format!("package:{dep}")),
                        glu_core::MinimumVersion {
                            version: "1.0".to_string(),
                            revision: Some(0),
                        },
                    )
                })
                .collect(),
            artifact: ArtifactId(format!("art:sha256:{name}")),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                keg_only: false,
                link_overwrite: vec![],
                post_install_defined: false,
                post_install_steps: vec![],
                postinstall_network_access_allowed: true,
            },
        };
        let artifact = ResolvedArtifact {
            url: format!("https://ghcr.io/v2/homebrew/core/{name}/blobs/sha256:{name}"),
            sha256: format!("{name:0<64}"),
            bytes: Some(1),
            bottle_tag: "arm64_sequoia".to_string(),
            cellar: ":any".to_string(),
            built_on: None,
        };
        (id, package, artifact)
    }

    fn manifest(roots: Vec<&str>, packages: Vec<(&str, Vec<&str>)>) -> InstallManifest {
        let mut package_map = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for (name, deps) in packages {
            let (id, package, artifact) = pkg(name, deps);
            artifacts.insert(package.artifact.clone(), artifact);
            package_map.insert(id, package);
        }
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: roots
                    .iter()
                    .map(|name| glu_core::PackageSelector((*name).to_string()))
                    .collect(),
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: roots
                .into_iter()
                .map(|name| glu_core::PackageSelection {
                    requested_as: glu_core::PackageSelector(name.to_string()),
                    package_key: glu_core::PackageKey(format!("package:{name}")),
                    package: PackageId(format!("pkg:homebrew/core/{name}@1.0")),
                })
                .collect(),
            packages: package_map,
            artifacts,
        }
    }

    fn manifest_from_parts(
        packages: Vec<(PackageId, ResolvedPackage, ResolvedArtifact)>,
        roots: Vec<PackageId>,
    ) -> InstallManifest {
        let mut package_map = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for (id, package, artifact) in packages {
            artifacts.insert(package.artifact.clone(), artifact);
            package_map.insert(id, package);
        }
        let roots = roots
            .into_iter()
            .map(|package_id| {
                let package = &package_map[&package_id];
                glu_core::PackageSelection {
                    requested_as: glu_core::PackageSelector(package.name.0.clone()),
                    package_key: package.package_key.clone(),
                    package: package_id,
                }
            })
            .collect::<Vec<_>>();
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: roots
                    .iter()
                    .filter_map(|root| {
                        package_map
                            .get(&root.package)
                            .map(|package| glu_core::PackageSelector(package.name.0.clone()))
                    })
                    .collect(),
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots,
            packages: package_map,
            artifacts,
        }
    }

    fn id(name: &str) -> PackageId {
        PackageId(format!("pkg:homebrew/core/{name}@1.0"))
    }

    fn minimum(version: &str) -> glu_core::MinimumVersion {
        glu_core::MinimumVersion {
            version: version.to_string(),
            revision: Some(0),
        }
    }

    fn installed_pkg(
        name: &str,
        version: &str,
        dependencies: Vec<(&str, &str)>,
    ) -> InstalledPackage {
        let mut deps = Vec::new();
        let mut dependency_requirements = BTreeMap::new();
        for (dependency, minimum_version) in dependencies {
            let package_key = glu_core::PackageKey(format!("package:{dependency}"));
            deps.push(PackageDependency {
                package_key: package_key.clone(),
                package: id(dependency),
                requested_as: glu_core::PackageSelector(dependency.to_string()),
            });
            dependency_requirements.insert(
                package_key,
                glu_core::MinimumVersion {
                    version: minimum_version.to_string(),
                    revision: Some(0),
                },
            );
        }

        InstalledPackage {
            id: PackageId(format!("pkg:homebrew/core/{name}@{version}")),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: version.to_string(),
            revision: 0,
            keg_version: KegVersion(version.to_string()),
            keg_path: std::path::PathBuf::from(format!("/prefix/Cellar/{name}/{version}")),
            opt_path: std::path::PathBuf::from(format!("/prefix/opt/{name}")),
            keg_only: false,
            linked: true,
            deps,
            dependency_requirements,
            download_bytes: None,
            installed_bytes: None,
        }
    }

    #[test]
    fn cascade_outdated_dependents_includes_outdated_dependent() {
        let glib = installed_pkg("glib", "2.0", vec![]);
        let vips = installed_pkg("vips", "1.0", vec![("glib", "1.0")]);
        let state = InstalledState::from_packages(vec![glib, vips]);
        let outdated: BTreeSet<PackageName> =
            [PackageName("vips".to_string())].into_iter().collect();

        let added =
            cascade_outdated_dependents(&[PackageName("glib".to_string())], &outdated, &state);

        assert_eq!(added, vec![PackageName("vips".to_string())]);
    }

    #[test]
    fn cascade_outdated_dependents_skips_dependent_that_is_not_outdated() {
        let glib = installed_pkg("glib", "2.0", vec![]);
        let vips = installed_pkg("vips", "1.0", vec![("glib", "1.0")]);
        let state = InstalledState::from_packages(vec![glib, vips]);

        let added = cascade_outdated_dependents(
            &[PackageName("glib".to_string())],
            &BTreeSet::new(),
            &state,
        );

        assert!(added.is_empty());
    }

    #[test]
    fn compute_workset_treats_any_installed_version_as_satisfied() {
        use crate::state::{
            GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
            ReceiptStatus,
        };
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let keg = prefix.0.join("Cellar/node/0.9");
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId("pkg:homebrew/core/node@0.9".to_string()),
                package_key: glu_core::PackageKey("package:node".to_string()),
                name: PackageName("node".to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "0.9".to_string(),
                revision: 0,
                keg_version: KegVersion("0.9".to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.0.join("opt/node"),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                keg_only: false,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                dependency_requirements: Default::default(),
            },
        };
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();

        // Resolve returns node@1.0 (newer than the installed 0.9) — should be
        // treated as satisfied (any installed version counts), so its own
        // deps (libuv) are never walked either, exactly like an exact-version
        // match would behave.
        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.satisfied, vec![id("node")]);
        assert!(workset.install.is_empty());
    }

    #[test]
    fn force_mode_discovers_a_new_dependency_from_the_version_bump() {
        use crate::state::{
            GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
            ReceiptStatus,
        };
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let keg = prefix.0.join("Cellar/node/0.9");
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId("pkg:homebrew/core/node@0.9".to_string()),
                package_key: glu_core::PackageKey("package:node".to_string()),
                name: PackageName("node".to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "0.9".to_string(),
                revision: 0,
                keg_version: KegVersion("0.9".to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: ":any".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.0.join("opt/node"),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                keg_only: false,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                dependency_requirements: Default::default(),
            },
        };
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();

        // node@1.0 needs libuv, which 0.9 (installed) didn't and isn't
        // installed at all. `Install` alone would never notice (node is
        // satisfied-by-name, so its deps are never walked) — this is
        // exactly the bug `WorksetMode::Force` exists to fix.
        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Force).unwrap();

        assert!(workset.install.contains(&id("libuv")));
        assert!(workset.install.contains(&id("node")));
    }

    /// Writes a glu install receipt for `name@version` into the prefix, the
    /// same shape the existing tests build inline.
    fn write_receipt(prefix: &glu_core::Prefix, name: &str, version: &str) {
        use crate::state::{
            GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
            ReceiptStatus,
        };
        let keg = prefix.0.join("Cellar").join(name).join(version);
        std::fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:homebrew/core/{name}@{version}")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: KegVersion(version.to_string()),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
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
                keg_only: false,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                dependency_requirements: Default::default(),
            },
        };
        std::fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn installed_previous_name_root_plans_rename_not_download() {
        let mut state_pkg = installed_pkg("foo", "1.0", vec![]);
        state_pkg.id = PackageId("pkg:homebrew/core/foo@1.0".to_string());
        let state = InstalledState::from_packages(vec![state_pkg]);
        let (bar_id, mut bar, bar_artifact) = pkg("bar", vec![]);
        bar.oldnames = vec![glu_core::PackageSelector("foo".to_string())];
        let manifest = manifest_from_parts(
            vec![(bar_id.clone(), bar, bar_artifact)],
            vec![bar_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert!(workset.satisfied.is_empty());
        assert!(workset.install.is_empty());
        assert_eq!(workset.rename.len(), 1);
        assert_eq!(workset.rename[0].package, bar_id);
        assert_eq!(workset.rename[0].old_name.0, "foo");
        assert_eq!(workset.rename[0].old_keg_version, "1.0");
    }

    #[test]
    fn installed_previous_name_dependency_plans_rename_anchor_not_download() {
        let state = InstalledState::from_packages(vec![installed_pkg("foo", "1.0", vec![])]);
        let (bar_id, mut bar, bar_artifact) = pkg("bar", vec![]);
        bar.oldnames = vec![glu_core::PackageSelector("foo".to_string())];
        let (app_id, app, app_artifact) = pkg("app", vec!["bar"]);
        let manifest = manifest_from_parts(
            vec![
                (bar_id.clone(), bar, bar_artifact),
                (app_id.clone(), app, app_artifact),
            ],
            vec![app_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![app_id]);
        assert!(workset.satisfied.is_empty());
        assert_eq!(workset.rename.len(), 1);
        assert_eq!(workset.rename[0].package, bar_id);
        assert_eq!(workset.rename[0].old_name.0, "foo");
        assert_eq!(workset.rename[0].old_keg_version, "1.0");
    }

    #[test]
    fn too_old_previous_name_dependency_installs_current_package() {
        let state = InstalledState::from_packages(vec![installed_pkg("foo", "0.9", vec![])]);
        let (bar_id, mut bar, bar_artifact) = pkg("bar", vec![]);
        bar.oldnames = vec![glu_core::PackageSelector("foo".to_string())];
        let (app_id, app, app_artifact) = pkg("app", vec!["bar"]);
        let manifest = manifest_from_parts(
            vec![
                (bar_id.clone(), bar, bar_artifact),
                (app_id.clone(), app, app_artifact),
            ],
            vec![app_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![bar_id, app_id]);
        assert!(workset.rename.is_empty());
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn dep_is_satisfied_when_installed_at_or_above_the_resolved_candidate() {
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "libuv", "1.0");

        // node@1.0 resolves with dep libuv@1.0; the installed libuv@1.0 is at
        // the resolved candidate, so it is reused, not installed.
        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![id("node")]);
        assert_eq!(workset.satisfied, vec![id("libuv")]);
    }

    #[test]
    fn dependency_match_uses_stable_package_identity_for_alias_selectors() {
        let mut installed = installed_pkg("bar", "1.0", vec![]);
        installed.aliases = vec![glu_core::PackageSelector("foo".to_string())];
        let state = InstalledState::from_packages(vec![installed]);
        let mut manifest = manifest(vec!["app"], vec![("app", vec!["bar"]), ("bar", vec![])]);
        let app_id = id("app");
        manifest.packages.get_mut(&app_id).unwrap().deps[0].requested_as =
            glu_core::PackageSelector("foo".to_string());
        let dependency = manifest.packages[&app_id].deps[0].clone();
        let context = InstallerRootContext { package: app_id };

        let matched = dependency_match(&context, &dependency, &manifest, &state).unwrap();

        assert_eq!(matched, PackageMatch::Current);
    }

    #[test]
    fn dep_below_the_resolved_candidate_is_installed() {
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "libuv", "0.9");

        // node@1.0 resolves with dep libuv@1.0; installed libuv@0.9 is below
        // the resolved candidate, so the resolved libuv is installed.
        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![id("libuv"), id("node")]);
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn stricter_shared_requirement_installs_dependency_once_for_all_parents() {
        let state = InstalledState::from_packages(vec![installed_pkg("shared", "0.9", vec![])]);
        let mut manifest = manifest(
            vec!["first", "second"],
            vec![
                ("first", vec!["shared"]),
                ("second", vec!["shared"]),
                ("shared", vec![]),
            ],
        );
        let shared_key = glu_core::PackageKey("package:shared".to_string());
        manifest
            .packages
            .get_mut(&id("first"))
            .unwrap()
            .dependency_requirements
            .get_mut(&shared_key)
            .unwrap()
            .version = "0.9".to_string();

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(
            workset.install,
            vec![id("shared"), id("first"), id("second")]
        );
        assert!(workset.satisfied.is_empty());
        assert!(workset.rename.is_empty());
    }

    #[test]
    fn installer_root_floor_applies_through_a_satisfied_intermediate() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("bridge", "1.0", vec![("leaf", "1.0")]),
            installed_pkg("leaf", "1.0", vec![]),
        ]);
        let (leaf_id, leaf, leaf_artifact) = pkg_at("leaf", "2.0", vec![]);
        let (bridge_id, mut bridge, bridge_artifact) = pkg("bridge", vec!["leaf"]);
        bridge.deps[0].package = leaf_id.clone();
        let (root_id, mut root, root_artifact) = pkg("root", vec!["bridge"]);
        root.dependency_requirements.insert(
            glu_core::PackageKey("package:leaf".to_string()),
            minimum("2.0"),
        );
        let manifest = manifest_from_parts(
            vec![
                (leaf_id.clone(), leaf, leaf_artifact),
                (bridge_id.clone(), bridge, bridge_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![leaf_id, root_id]);
        assert_eq!(workset.satisfied, vec![bridge_id]);
    }

    #[test]
    fn selected_dependency_expands_again_with_its_own_requirement_context() {
        let state = InstalledState::from_packages(vec![installed_pkg("leaf", "1.0", vec![])]);
        let (leaf_id, leaf, leaf_artifact) = pkg_at("leaf", "2.0", vec![]);
        let (bridge_id, mut bridge, bridge_artifact) = pkg("bridge", vec!["leaf"]);
        bridge.deps[0].package = leaf_id.clone();
        bridge.dependency_requirements.insert(
            glu_core::PackageKey("package:leaf".to_string()),
            minimum("2.0"),
        );
        let (root_id, mut root, root_artifact) = pkg("root", vec!["bridge"]);
        root.dependency_requirements.insert(
            glu_core::PackageKey("package:leaf".to_string()),
            minimum("1.0"),
        );
        let manifest = manifest_from_parts(
            vec![
                (leaf_id.clone(), leaf, leaf_artifact),
                (bridge_id.clone(), bridge, bridge_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![leaf_id, bridge_id, root_id]);
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn selected_concrete_dependency_self_satisfies_an_impossible_floor() {
        let (leaf_id, leaf, leaf_artifact) = pkg_at("leaf", "2.0", vec![]);
        let state = InstalledState::from_packages(vec![installed_pkg("leaf", "2.0", vec![])]);
        let (root_id, mut root, root_artifact) = pkg("root", vec!["leaf"]);
        root.deps[0].package = leaf_id.clone();
        root.dependency_requirements.insert(
            glu_core::PackageKey("package:leaf".to_string()),
            minimum("99.0"),
        );
        let manifest = manifest_from_parts(
            vec![
                (leaf_id.clone(), leaf, leaf_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![root_id]);
        assert_eq!(workset.satisfied, vec![leaf_id]);
    }

    #[test]
    fn requirement_context_does_not_guess_alias_keys() {
        let (leaf_id, mut leaf, leaf_artifact) = pkg_at("leaf", "2.0", vec![]);
        leaf.aliases = vec![glu_core::PackageSelector("legacy-leaf".to_string())];
        let mut installed = installed_pkg("leaf", "1.0", vec![]);
        installed.aliases = leaf.aliases.clone();
        let state = InstalledState::from_packages(vec![installed]);
        let (root_id, mut root, root_artifact) = pkg("root", vec!["leaf"]);
        root.deps[0].package = leaf_id.clone();
        root.dependency_requirements
            .remove(&glu_core::PackageKey("package:leaf".to_string()));
        root.dependency_requirements.insert(
            glu_core::PackageKey("package:legacy-leaf".to_string()),
            minimum("1.0"),
        );
        let manifest = manifest_from_parts(
            vec![
                (leaf_id.clone(), leaf, leaf_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();

        assert_eq!(workset.install, vec![leaf_id, root_id]);
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn shared_contexts_keep_the_stricter_floor_independent_of_root_order() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("bridge", "1.0", vec![("shared", "1.0")]),
            installed_pkg("shared", "1.0", vec![]),
        ]);
        let (shared_id, shared, shared_artifact) = pkg_at("shared", "2.0", vec![]);
        let (bridge_id, mut bridge, bridge_artifact) = pkg("bridge", vec!["shared"]);
        bridge.deps[0].package = shared_id.clone();
        let (first_id, mut first, first_artifact) = pkg("first", vec!["bridge"]);
        first.dependency_requirements.insert(
            glu_core::PackageKey("package:shared".to_string()),
            minimum("1.0"),
        );
        let (second_id, mut second, second_artifact) = pkg("second", vec!["bridge"]);
        second.dependency_requirements.insert(
            glu_core::PackageKey("package:shared".to_string()),
            minimum("2.0"),
        );
        let manifest = manifest_from_parts(
            vec![
                (shared_id.clone(), shared, shared_artifact),
                (bridge_id.clone(), bridge, bridge_artifact),
                (first_id.clone(), first, first_artifact),
                (second_id.clone(), second, second_artifact),
            ],
            vec![first_id.clone(), second_id.clone()],
        );
        let mut reversed = manifest.clone();
        reversed.roots.reverse();

        let forward = compute_workset(&manifest, &state, WorksetMode::Install).unwrap();
        let backward = compute_workset(&reversed, &state, WorksetMode::Install).unwrap();

        assert_eq!(forward.install, vec![shared_id, first_id, second_id]);
        assert_eq!(forward.satisfied, vec![bridge_id]);
        assert_eq!(forward.install, backward.install);
        assert_eq!(forward.satisfied, backward.satisfied);
    }

    #[test]
    fn update_roots_updates_root_but_keeps_a_satisfying_older_dependency() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("app", "1.0", vec![("dep", "1.0")]),
            installed_pkg("dep", "1.0", vec![]),
        ]);
        let (dep_id, dep, dep_artifact) = pkg_at("dep", "2.0", vec![]);
        let (app_id, mut app, app_artifact) = pkg_at("app", "2.0", vec!["dep"]);
        app.deps[0].package = dep_id.clone();
        let manifest = manifest_from_parts(
            vec![
                (dep_id.clone(), dep, dep_artifact),
                (app_id.clone(), app, app_artifact),
            ],
            vec![app_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::UpdateRoots).unwrap();

        assert_eq!(workset.install, vec![app_id]);
        assert_eq!(workset.satisfied, vec![dep_id]);
    }

    #[test]
    fn update_roots_installs_an_insufficient_dependency_before_the_root() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("app", "1.0", vec![("dep", "0.9")]),
            installed_pkg("dep", "0.9", vec![]),
        ]);
        let (dep_id, dep, dep_artifact) = pkg_at("dep", "2.0", vec![]);
        let (app_id, mut app, app_artifact) = pkg_at("app", "2.0", vec!["dep"]);
        app.deps[0].package = dep_id.clone();
        let manifest = manifest_from_parts(
            vec![
                (dep_id.clone(), dep, dep_artifact),
                (app_id.clone(), app, app_artifact),
            ],
            vec![app_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::UpdateRoots).unwrap();

        assert_eq!(workset.install, vec![dep_id, app_id]);
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn update_roots_updates_each_changed_declared_root_without_repours() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("first", "1.0", vec![]),
            installed_pkg("second", "2.0", vec![]),
        ]);
        let (first_id, first, first_artifact) = pkg_at("first", "2.0", vec![]);
        let (second_id, second, second_artifact) = pkg_at("second", "2.0", vec![]);
        let manifest = manifest_from_parts(
            vec![
                (first_id.clone(), first, first_artifact),
                (second_id.clone(), second, second_artifact),
            ],
            vec![first_id.clone(), second_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::UpdateRoots).unwrap();

        assert_eq!(workset.install, vec![first_id]);
        assert_eq!(workset.satisfied, vec![second_id]);
    }

    #[test]
    fn update_all_updates_a_satisfying_but_older_dependency() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("app", "1.0", vec![("dep", "1.0")]),
            installed_pkg("dep", "1.0", vec![]),
        ]);
        let (dep_id, dep, dep_artifact) = pkg_at("dep", "2.0", vec![]);
        let (app_id, mut app, app_artifact) = pkg("app", vec!["dep"]);
        app.deps[0].package = dep_id.clone();
        let manifest = manifest_from_parts(
            vec![
                (dep_id.clone(), dep, dep_artifact),
                (app_id.clone(), app, app_artifact),
            ],
            vec![app_id.clone()],
        );

        let workset = compute_workset(&manifest, &state, WorksetMode::UpdateAll).unwrap();

        assert_eq!(workset.install, vec![dep_id]);
        assert_eq!(workset.satisfied, vec![app_id]);
    }

    #[test]
    fn update_modes_do_not_repour_exact_package_facts() {
        let state = InstalledState::from_packages(vec![
            installed_pkg("app", "1.0", vec![("dep", "1.0")]),
            installed_pkg("dep", "1.0", vec![]),
        ]);
        let manifest = manifest(vec!["app"], vec![("app", vec!["dep"]), ("dep", vec![])]);

        for mode in [WorksetMode::UpdateRoots, WorksetMode::UpdateAll] {
            let workset = compute_workset(&manifest, &state, mode).unwrap();
            assert!(workset.install.is_empty());
            assert!(workset.rename.is_empty());
        }
    }

    #[test]
    fn update_roots_reconciles_new_topology_even_at_the_same_release() {
        let state = InstalledState::from_packages(vec![installed_pkg("app", "1.0", vec![])]);
        let manifest = manifest(vec!["app"], vec![("app", vec!["dep"]), ("dep", vec![])]);

        let workset = compute_workset(&manifest, &state, WorksetMode::UpdateRoots).unwrap();

        assert_eq!(workset.install, vec![id("dep"), id("app")]);
    }

    #[test]
    fn force_mode_repours_an_installed_root_and_keeps_satisfied_deps() {
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_receipt(&prefix, "node", "1.0");
        write_receipt(&prefix, "libuv", "1.0");

        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Force).unwrap();
        assert_eq!(workset.install, vec![id("node")]);
        assert_eq!(workset.satisfied, vec![id("libuv")]);
    }

    #[test]
    fn force_mode_with_absent_root_installs_the_closure() {
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());

        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::Force).unwrap();
        assert_eq!(workset.install, vec![id("libuv"), id("node")]);
        assert!(workset.satisfied.is_empty());
    }

    #[test]
    fn reinstall_deps_mode_repours_everything() {
        use glu_core::Prefix;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        let manifest = manifest(
            vec!["node"],
            vec![("node", vec!["libuv"]), ("libuv", vec![])],
        );
        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();

        let workset = compute_workset(&manifest, &state, WorksetMode::ReinstallDeps).unwrap();
        assert_eq!(workset.install, vec![id("libuv"), id("node")]);
        assert!(workset.satisfied.is_empty());
    }
}
