use anyhow::Result;
use glu_core::{InstallManifest, PackageDependency, PackageId, ResolvedPackage};
use std::collections::BTreeSet;

/// Selects the packages that need work and returns the selected subgraph in
/// dependency-before-dependent order.
///
/// Selection and ordering are separate passes. A dependency skipped for one
/// parent may still be selected for another parent with a higher package-level
/// minimum. Once selection is complete, ordering considers every edge between
/// selected packages, so a shared dependency always precedes every selected
/// dependent regardless of which parent selected it.
pub fn dependency_order(
    manifest: &InstallManifest,
    roots: &[PackageId],
    mut should_install: impl FnMut(&ResolvedPackage, &PackageDependency) -> Result<bool>,
) -> Result<Vec<PackageId>> {
    let mut selected = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    for root in roots {
        select_packages(
            root,
            manifest,
            &mut should_install,
            &mut selected,
            &mut expanded,
        )?;
    }

    let mut order = Vec::new();
    let mut ordered = BTreeSet::new();
    let mut ordering = BTreeSet::new();
    for root in roots {
        order_selected(
            root,
            manifest,
            &selected,
            &mut ordered,
            &mut ordering,
            &mut order,
        )?;
    }
    Ok(order)
}

fn select_packages(
    package_id: &PackageId,
    manifest: &InstallManifest,
    should_install: &mut impl FnMut(&ResolvedPackage, &PackageDependency) -> Result<bool>,
    selected: &mut BTreeSet<PackageId>,
    expanded: &mut BTreeSet<PackageId>,
) -> Result<()> {
    selected.insert(package_id.clone());
    if !expanded.insert(package_id.clone()) {
        return Ok(());
    }

    let package = manifest
        .packages
        .get(package_id)
        .ok_or_else(|| anyhow::anyhow!("resolve manifest is missing package {}", package_id.0))?;
    for dependency in &package.deps {
        if selected.contains(&dependency.package) {
            continue;
        }
        if should_install(package, dependency)? {
            select_packages(
                &dependency.package,
                manifest,
                should_install,
                selected,
                expanded,
            )?;
        }
    }
    Ok(())
}

fn order_selected(
    package_id: &PackageId,
    manifest: &InstallManifest,
    selected: &BTreeSet<PackageId>,
    ordered: &mut BTreeSet<PackageId>,
    ordering: &mut BTreeSet<PackageId>,
    order: &mut Vec<PackageId>,
) -> Result<()> {
    if ordered.contains(package_id) || !ordering.insert(package_id.clone()) {
        return Ok(());
    }

    let package = manifest
        .packages
        .get(package_id)
        .ok_or_else(|| anyhow::anyhow!("resolve manifest is missing package {}", package_id.0))?;
    for dependency in &package.deps {
        if selected.contains(&dependency.package) {
            order_selected(
                &dependency.package,
                manifest,
                selected,
                ordered,
                ordering,
                order,
            )?;
        }
    }

    ordering.remove(package_id);
    if ordered.insert(package_id.clone()) {
        order.push(package_id.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, InstallManifest, KegVersion, PackageInstallMetadata, PackageName,
        ResolveRequestEcho, ResolvedArtifact, ResolvedPackage, Target,
    };
    use std::collections::BTreeMap;

    fn pkg(name: &str, deps: Vec<&str>) -> (PackageId, ResolvedPackage, ResolvedArtifact) {
        let id = PackageId(format!("pkg:homebrew/core/{name}@1.0"));
        let artifact = ArtifactId(format!("art:sha256:{name}"));
        let package = ResolvedPackage {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            deps: deps
                .into_iter()
                .map(|dep| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:homebrew/core/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector(dep.to_string()),
                })
                .collect(),
            dependency_requirements: Default::default(),
            artifact: artifact.clone(),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                keg_only: false,
                link_overwrite: vec![],
                post_install_defined: false,
                post_install_steps: vec![],
                postinstall_network_access_allowed: true,
            },
        };
        let resolved_artifact = ResolvedArtifact {
            url: format!("https://ghcr.io/v2/homebrew/core/{name}/blobs/sha256:{name}"),
            sha256: format!("{name:0<64}"),
            bytes: Some(1),
            bottle_tag: "arm64_sequoia".to_string(),
            cellar: ":any".to_string(),
            built_on: None,
        };
        (id, package, resolved_artifact)
    }

    fn manifest(packages: Vec<(PackageId, ResolvedPackage, ResolvedArtifact)>) -> InstallManifest {
        let mut package_map = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for (id, package, artifact) in packages {
            artifacts.insert(package.artifact.clone(), artifact);
            package_map.insert(id, package);
        }
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots: vec![],
            packages: package_map,
            artifacts,
        }
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let (dep_id, dep, dep_artifact) = pkg("dep", vec![]);
        let (root_id, root, root_artifact) = pkg("root", vec!["dep"]);
        let manifest = manifest(vec![
            (dep_id.clone(), dep, dep_artifact),
            (root_id.clone(), root, root_artifact),
        ]);

        let order =
            dependency_order(&manifest, std::slice::from_ref(&root_id), |_, _| Ok(true)).unwrap();

        assert_eq!(order, vec![dep_id, root_id]);
    }

    #[test]
    fn breaks_mutual_cycle_instead_of_overflowing() {
        let (a_id, a, a_artifact) = pkg("a", vec!["b"]);
        let (b_id, b, b_artifact) = pkg("b", vec!["a"]);
        let manifest = manifest(vec![
            (a_id.clone(), a, a_artifact),
            (b_id.clone(), b, b_artifact),
        ]);

        let order =
            dependency_order(&manifest, std::slice::from_ref(&a_id), |_, _| Ok(true)).unwrap();

        assert_eq!(order.len(), 2);
        assert!(order.contains(&a_id));
        assert!(order.contains(&b_id));
    }

    #[test]
    fn shared_dependency_selected_by_later_parent_precedes_all_dependents() {
        let (dependency_id, dependency, dependency_artifact) = pkg("dependency", vec![]);
        let (first_id, first, first_artifact) = pkg("first", vec!["dependency"]);
        let (second_id, second, second_artifact) = pkg("second", vec!["dependency"]);
        let manifest = manifest(vec![
            (dependency_id.clone(), dependency, dependency_artifact),
            (first_id.clone(), first, first_artifact),
            (second_id.clone(), second, second_artifact),
        ]);

        let order = dependency_order(
            &manifest,
            &[first_id.clone(), second_id.clone()],
            |parent, _| Ok(parent.name.0 == "second"),
        )
        .unwrap();

        assert_eq!(order, vec![dependency_id, first_id, second_id]);
    }

    #[test]
    fn include_false_prunes_subtree() {
        let (leaf_id, leaf, leaf_artifact) = pkg("leaf", vec![]);
        let (dep_id, dep, dep_artifact) = pkg("dep", vec!["leaf"]);
        let (root_id, root, root_artifact) = pkg("root", vec!["dep"]);
        let manifest = manifest(vec![
            (leaf_id, leaf, leaf_artifact),
            (dep_id.clone(), dep, dep_artifact),
            (root_id.clone(), root, root_artifact),
        ]);

        let order = dependency_order(&manifest, std::slice::from_ref(&root_id), |_, dep| {
            Ok(dep.package != dep_id)
        })
        .unwrap();

        assert_eq!(order, vec![root_id]);
    }
}
