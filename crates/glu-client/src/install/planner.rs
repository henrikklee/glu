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

/// How a command's requested roots and their dependency closure become
/// install/satisfied work. One function, one mode — previously the mode
/// concept was split across five near-duplicate builders
/// (`compute_workset` / `force_roots_workset` / `reinstall_closure_workset` /
/// `reinstall_roots_workset` / `update_roots_workset`) plus an if/else in
/// the installer front-matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorksetMode {
    /// `glu install`: a root is satisfied by *any* installed version
    /// (`install` is idempotent by name — only `update` changes an
    /// installed package's version), so a satisfied root's dependency edges
    /// are never walked and absent roots flow through the regular
    /// real-checked closure.
    Install,
    /// `glu update`, `glu install --force`, `glu reinstall`: every named
    /// root repours regardless of installed state, and the whole closure is
    /// real-checked below them — a version bump can introduce new
    /// dependencies, which `Install`'s pruned walk (satisfied roots never
    /// walked) would silently miss. "No matter how the system is right now,
    /// make it as I say."
    Force,
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
            let mut direct_roots = Vec::new();

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
                    PackageMatch::Missing => direct_roots.push(root_id.clone()),
                }
            }

            let install =
                graph::dependency_order(manifest, &direct_roots, |parent, dependency| {
                    match dependency_match(parent, dependency, manifest, state)? {
                        PackageMatch::Current => {
                            satisfied.insert(dependency.package.clone());
                            Ok(false)
                        }
                        PackageMatch::PreviousName(rename_item) => {
                            rename.insert(RenameWorkItem {
                                package: dependency.package.clone(),
                                ..rename_item
                            });
                            Ok(false)
                        }
                        PackageMatch::Missing => Ok(true),
                    }
                })?;

            Ok(finalize_workset(satisfied, rename, install))
        }
        WorksetMode::Force => {
            // Roots repour unconditionally; the walk covers the full
            // closure (all roots, not just absent ones) with real
            // per-dependency satisfaction below them, so a dependency
            // introduced by the version bump is discovered rather than
            // silently missing.
            let mut satisfied = BTreeSet::new();
            let mut rename = BTreeSet::new();
            let roots = manifest.root_package_ids();
            let install = graph::dependency_order(manifest, &roots, |parent, dependency| {
                match dependency_match(parent, dependency, manifest, state)? {
                    PackageMatch::Current => {
                        satisfied.insert(dependency.package.clone());
                        Ok(false)
                    }
                    PackageMatch::PreviousName(rename_item) => {
                        rename.insert(RenameWorkItem {
                            package: dependency.package.clone(),
                            ..rename_item
                        });
                        Ok(false)
                    }
                    PackageMatch::Missing => Ok(true),
                }
            })?;
            Ok(finalize_workset(satisfied, rename, install))
        }
        WorksetMode::ReinstallDeps => {
            let install =
                graph::dependency_order(manifest, &manifest.root_package_ids(), |_, _| Ok(true))?;
            Ok(InstallWorkSet {
                satisfied: vec![],
                rename: Vec::new(),
                install,
            })
        }
    }
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
/// needs this — it already covers every outdated package — only a named
/// `update` can miss a dependent that isn't part of the resolve request.
/// Returns just the newly-discovered names (caller extends the target list
/// and re-resolves); a single pass suffices, `depends_on_any`'s walk is
/// already transitive per candidate.
pub fn cascade_outdated_dependents(
    target_names: &[PackageName],
    outdated_names: &BTreeSet<PackageName>,
    installed: &[InstalledPackage],
) -> Vec<PackageName> {
    let target_names_set: BTreeSet<PackageName> = target_names.iter().cloned().collect();
    let mut added = Vec::new();
    for package in installed {
        if target_names_set.contains(&package.name) || !outdated_names.contains(&package.name) {
            continue;
        }
        if crate::state::installed::depends_on_any(installed, package, &target_names_set) {
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
/// requiring package's minimum version. If the satisfying package is still
/// installed under a previous name, emit a rename anchor instead of treating
/// the canonical package as committed.
fn dependency_match(
    requiring_package: &ResolvedPackage,
    dependency: &PackageDependency,
    manifest: &InstallManifest,
    state: &InstalledState,
) -> Result<PackageMatch> {
    let selected_package = manifest.require_package(&dependency.package)?;
    let minimum = requiring_package
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
        let id = PackageId(format!("pkg:homebrew/core/{name}@1.0"));
        let package = ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
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
        let all = vec![glib, vips];
        let outdated: BTreeSet<PackageName> =
            [PackageName("vips".to_string())].into_iter().collect();

        let added =
            cascade_outdated_dependents(&[PackageName("glib".to_string())], &outdated, &all);

        assert_eq!(added, vec![PackageName("vips".to_string())]);
    }

    #[test]
    fn cascade_outdated_dependents_skips_dependent_that_is_not_outdated() {
        let glib = installed_pkg("glib", "2.0", vec![]);
        let vips = installed_pkg("vips", "1.0", vec![("glib", "1.0")]);
        let all = vec![glib, vips];

        let added =
            cascade_outdated_dependents(&[PackageName("glib".to_string())], &BTreeSet::new(), &all);

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
        let app = manifest.packages[&app_id].clone();
        let dependency = app.deps[0].clone();

        let matched = dependency_match(&app, &dependency, &manifest, &state).unwrap();

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
