use crate::{
    homebrew_version::PackageVersion, install::planner::InstallWorkSet,
    state::installed::InstalledState,
};
use anyhow::{bail, Result};
use glu_core::{InstallManifest, InstalledPackage, PackageId, PackageName, PackageSelector};
use std::collections::BTreeSet;

/// Predicted installed package graph after an install-like workset writes its
/// receipts. This has the same ordering shape as `InstalledState::list`, so
/// name-level reachability picks the newly installed version first.
pub(crate) fn simulate_post_install_state(
    state: &InstalledState,
    manifest: &InstallManifest,
    workset: &InstallWorkSet,
) -> Vec<InstalledPackage> {
    let mut simulated: Vec<InstalledPackage> = state.list();
    for rename in &workset.rename {
        let Some(package) = manifest.packages.get(&rename.package) else {
            continue;
        };
        let Some(installed) = simulated
            .iter_mut()
            .find(|installed| installed.keg_path == rename.old_keg_path)
        else {
            continue;
        };
        installed.package_key = package.package_key.clone();
        installed.name = package.name.clone();
        installed.aliases = package.aliases.clone();
        let old_selector = PackageSelector(rename.old_name.0.clone());
        if !installed.oldnames.contains(&old_selector) {
            installed.oldnames.push(old_selector);
        }
        for oldname in &package.oldnames {
            if !installed.oldnames.contains(oldname) {
                installed.oldnames.push(oldname.clone());
            }
        }
    }
    for package_id in &workset.install {
        let Some(package) = manifest.packages.get(package_id) else {
            continue;
        };
        // Reconciliation can rewrite persisted package facts without changing
        // the concrete release ID. Execution replaces that keg in place, so
        // simulation must replace—not duplicate—the corresponding receipt.
        simulated.retain(|installed| &installed.id != package_id);
        simulated.push(InstalledPackage {
            id: package_id.clone(),
            package_key: package.package_key.clone(),
            name: package.name.clone(),
            aliases: package.aliases.clone(),
            oldnames: package.oldnames.clone(),
            version: package.version.clone(),
            revision: package.revision,
            keg_version: package.keg_version.clone(),
            keg_path: std::path::PathBuf::new(),
            opt_path: std::path::PathBuf::new(),
            exposure: package.exposure.clone(),
            linked: false,
            deps: package.deps.clone(),
            dependency_requirements: package.dependency_requirements.clone(),
            download_bytes: manifest
                .artifacts
                .get(&package.artifact)
                .and_then(|artifact| artifact.bytes),
            installed_bytes: None,
        });
    }
    simulated.sort_by(|a, b| {
        a.name
            .0
            .to_lowercase()
            .cmp(&b.name.0.to_lowercase())
            .then_with(|| {
                PackageVersion::new(&b.version, b.revision)
                    .compare(PackageVersion::new(&a.version, a.revision))
            })
    });
    simulated
}

/// Packages that would be dangling after an install/update/reinstall workset
/// completes, judged against the supplied final declaration name set.
pub(crate) fn predicted_dangling_after_workset(
    state: &InstalledState,
    manifest: &InstallManifest,
    workset: &InstallWorkSet,
    declared: &BTreeSet<PackageName>,
) -> Result<Vec<InstalledPackage>> {
    let simulated = simulate_post_install_state(state, manifest, workset);
    Ok(InstalledState::from_simulated_packages(simulated, declared.clone())?.dangling())
}

/// Update may repair a dangling package by installing it. Do not schedule a
/// package from the current workset for the trailing removal pass.
pub(crate) fn without_workset_installs(
    packages: Vec<InstalledPackage>,
    workset: &InstallWorkSet,
) -> Vec<InstalledPackage> {
    let install_ids: BTreeSet<PackageId> = workset.install.iter().cloned().collect();
    packages
        .into_iter()
        .filter(|package| !install_ids.contains(&package.id))
        .collect()
}

/// In interactive mode, the user confirmed `predicted`; after execution we
/// reload real state and only remove final dangling packages whose ids were in
/// that confirmed prediction. If fresh unconfirmed removals appear, refuse
/// rather than silently deleting more than was shown. `yes` confirms the final
/// set wholesale.
pub(crate) fn confirmed_final_dangling(
    predicted: &[InstalledPackage],
    final_dangling: &[InstalledPackage],
    yes: bool,
    command: &str,
) -> Result<Vec<InstalledPackage>> {
    if yes {
        return Ok(final_dangling.to_vec());
    }
    let predicted_ids: BTreeSet<PackageId> = predicted.iter().map(|p| p.id.clone()).collect();
    let mut unexpected = Vec::new();
    let mut confirmed = Vec::new();
    for package in final_dangling {
        if predicted_ids.contains(&package.id) {
            confirmed.push(package.clone());
        } else {
            unexpected.push(format!("{} {}", package.name.0, package.keg_version.0));
        }
    }
    if !unexpected.is_empty() {
        bail!(
            "would remove additional unused packages not shown before {command}: {}; re-run with `glu {command} -y` to confirm",
            unexpected.join(", ")
        );
    }
    Ok(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, InstallManifest, KegVersion, PackageDependency, PackageId,
        PackageInstallMetadata, ResolveRequestEcho, ResolvedPackage, Target,
    };
    use std::{collections::BTreeMap, path::PathBuf};

    fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
        InstalledPackage {
            id: PackageId(format!("pkg:test/{name}@1.0")),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            keg_path: PathBuf::new(),
            opt_path: PathBuf::new(),
            exposure: glu_core::Exposure::Global,
            linked: true,
            deps: deps
                .into_iter()
                .map(|dep| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:test/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector(dep.to_string()),
                })
                .collect(),
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }

    fn resolved_pkg(name: &str, deps: Vec<&str>) -> (PackageId, ResolvedPackage) {
        let id = PackageId(format!("pkg:test/{name}@2.0"));
        (
            id.clone(),
            ResolvedPackage {
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: "2.0".to_string(),
                revision: 0,
                keg_version: KegVersion("2.0".to_string()),
                deps: deps
                    .into_iter()
                    .map(|dep| PackageDependency {
                        package_key: glu_core::PackageKey(format!("package:{dep}")),
                        package: PackageId(format!("pkg:test/{dep}@1.0")),
                        requested_as: glu_core::PackageSelector(dep.to_string()),
                    })
                    .collect(),
                dependency_requirements: Default::default(),
                exposure: glu_core::Exposure::Global,
                artifact: ArtifactId(format!("artifact:{name}")),
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

    fn root_selection(name: &str, package: PackageId) -> glu_core::PackageSelection {
        glu_core::PackageSelection {
            requested_as: glu_core::PackageSelector(name.to_string()),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            package,
        }
    }

    #[test]
    fn predicted_dangling_after_workset_uses_simulated_receipts() {
        let state = InstalledState::from_packages(vec![pkg("dep", vec![]), pkg("old", vec![])]);
        let (app_id, app) = resolved_pkg("app", vec!["dep"]);
        let manifest = InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("app".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![root_selection("app", app_id.clone())],
            packages: BTreeMap::from([(app_id.clone(), app)]),
            artifacts: BTreeMap::new(),
        };
        let workset = InstallWorkSet {
            satisfied: Vec::new(),
            rename: Vec::new(),
            install: vec![app_id],
        };
        let declared: BTreeSet<PackageName> =
            [PackageName("app".to_string())].into_iter().collect();

        let dangling =
            predicted_dangling_after_workset(&state, &manifest, &workset, &declared).unwrap();
        let names: Vec<&str> = dangling
            .iter()
            .map(|package| package.name.0.as_str())
            .collect();

        assert_eq!(names, vec!["old"]);
    }

    #[test]
    fn predicted_dangling_after_a_version_bump_includes_the_superseded_keg() {
        // app 1.0 is installed and declared; the update resolves app 2.0,
        // which drops the dependency the old bottle had. The prediction
        // simulates the post-update graph: the new keg is retained, and both
        // the superseded same-name keg and the dropped dependency are
        // scheduled for removal (so the confirmation can show them).
        let state =
            InstalledState::from_packages(vec![pkg("app", vec!["old"]), pkg("old", vec![])]);
        let (app_id, app) = resolved_pkg("app", vec![]);
        let manifest = InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("app".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![root_selection("app", app_id.clone())],
            packages: BTreeMap::from([(app_id.clone(), app)]),
            artifacts: BTreeMap::new(),
        };
        let workset = InstallWorkSet {
            satisfied: Vec::new(),
            rename: Vec::new(),
            install: vec![app_id],
        };
        let declared: BTreeSet<PackageName> =
            [PackageName("app".to_string())].into_iter().collect();

        let dangling =
            predicted_dangling_after_workset(&state, &manifest, &workset, &declared).unwrap();
        let names: Vec<&str> = dangling
            .iter()
            .map(|package| package.name.0.as_str())
            .collect();

        // The superseded app@1.0 (name "app") sorts before "old".
        assert_eq!(names, vec!["app", "old"]);
        assert_eq!(dangling[0].keg_version.0, "1.0");
    }

    #[test]
    fn same_release_fact_reconciliation_replaces_the_simulated_receipt() {
        let mut installed_app = pkg("app", vec!["old"]);
        installed_app.id = PackageId("pkg:test/app@2.0".to_string());
        installed_app.version = "2.0".to_string();
        installed_app.keg_version = KegVersion("2.0".to_string());
        let state = InstalledState::from_packages(vec![installed_app, pkg("old", vec![])]);
        let (app_id, app) = resolved_pkg("app", vec![]);
        let manifest = InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("app".to_string())],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![root_selection("app", app_id.clone())],
            packages: BTreeMap::from([(app_id.clone(), app)]),
            artifacts: BTreeMap::new(),
        };
        let workset = InstallWorkSet {
            satisfied: Vec::new(),
            rename: Vec::new(),
            install: vec![app_id],
        };
        let declared = BTreeSet::from([PackageName("app".to_string())]);

        let dangling =
            predicted_dangling_after_workset(&state, &manifest, &workset, &declared).unwrap();

        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].name.0, "old");
    }

    #[test]
    fn confirmed_final_dangling_removes_only_what_was_predicted() {
        let predicted = vec![pkg("old-dep", vec![])];
        let final_dangling = vec![pkg("old-dep", vec![])];

        let confirmed =
            confirmed_final_dangling(&predicted, &final_dangling, false, "install").unwrap();

        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].name.0, "old-dep");
    }

    #[test]
    fn confirmed_final_dangling_refuses_unpredicted_removal_without_yes() {
        let err = confirmed_final_dangling(
            &[pkg("old-dep", vec![])],
            &[pkg("old-dep", vec![]), pkg("surprise", vec![])],
            false,
            "install",
        )
        .unwrap_err();

        assert!(err.to_string().contains("surprise 1.0"));
    }

    #[test]
    fn confirmed_final_dangling_yes_accepts_final_state() {
        let confirmed = confirmed_final_dangling(
            &[pkg("old-dep", vec![])],
            &[pkg("surprise", vec![])],
            true,
            "install",
        )
        .unwrap();

        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].name.0, "surprise");
    }
}
