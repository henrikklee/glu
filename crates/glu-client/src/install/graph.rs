use anyhow::Result;
use glu_core::{InstallManifest, PackageId};
use std::collections::BTreeSet;

/// Returns the complete topology below `roots` in
/// dependency-before-dependent order.
pub fn dependency_closure_order(
    manifest: &InstallManifest,
    roots: &[PackageId],
) -> Result<Vec<PackageId>> {
    let mut selected = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    for root in roots {
        select_closure(root, manifest, &mut selected, &mut expanded)?;
    }

    order_selected_packages(manifest, &selected)
}

/// Orders an already-selected package subset dependency-before-dependent.
/// Traversal continues through unselected packages because installer-root
/// requirement contexts can select a transitive package while reusing its
/// immediate parent. The selected descendant must still precede the selected
/// dependent.
pub fn order_selected_packages(
    manifest: &InstallManifest,
    selected: &BTreeSet<PackageId>,
) -> Result<Vec<PackageId>> {
    let mut order = Vec::new();
    let mut ordered = BTreeSet::new();
    let mut ordering = BTreeSet::new();
    for package in selected {
        order_selected(
            package,
            manifest,
            selected,
            &mut ordered,
            &mut ordering,
            &mut order,
        )?;
    }
    Ok(order)
}

/// The nearest selected packages below `package_id`, walking through
/// unselected intermediates. The execution DAG uses this collapsed frontier
/// to preserve the dependency-before-dependent commit boundary created by
/// installer-root requirement contexts without adding dense transitive edges.
pub(super) fn selected_dependency_frontier(
    manifest: &InstallManifest,
    package_id: &PackageId,
    selected: &BTreeSet<PackageId>,
) -> Result<BTreeSet<PackageId>> {
    let mut dependencies = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    collect_selected_dependency_frontier(
        package_id,
        manifest,
        selected,
        &mut dependencies,
        &mut expanded,
    )?;
    dependencies.remove(package_id);
    Ok(dependencies)
}

fn collect_selected_dependency_frontier(
    package_id: &PackageId,
    manifest: &InstallManifest,
    selected: &BTreeSet<PackageId>,
    dependencies: &mut BTreeSet<PackageId>,
    expanded: &mut BTreeSet<PackageId>,
) -> Result<()> {
    if !expanded.insert(package_id.clone()) {
        return Ok(());
    }
    let package = manifest
        .packages
        .get(package_id)
        .ok_or_else(|| anyhow::anyhow!("resolve manifest is missing package {}", package_id.0))?;
    for dependency in &package.deps {
        if selected.contains(&dependency.package) {
            dependencies.insert(dependency.package.clone());
            continue;
        }
        collect_selected_dependency_frontier(
            &dependency.package,
            manifest,
            selected,
            dependencies,
            expanded,
        )?;
    }
    Ok(())
}

fn select_closure(
    package_id: &PackageId,
    manifest: &InstallManifest,
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
        select_closure(&dependency.package, manifest, selected, expanded)?;
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
        order_selected(
            &dependency.package,
            manifest,
            selected,
            ordered,
            ordering,
            order,
        )?;
    }

    ordering.remove(package_id);
    if ordered.insert(package_id.clone()) && selected.contains(package_id) {
        order.push(package_id.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, InstallManifest, KegVersion, PackageDependency, PackageInstallMetadata,
        PackageName, ResolveRequestEcho, ResolvedArtifact, ResolvedPackage, Target,
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
            exposure: glu_core::Exposure::Global,
            artifact: artifact.clone(),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
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

        let order = dependency_closure_order(&manifest, std::slice::from_ref(&root_id)).unwrap();

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

        let order = dependency_closure_order(&manifest, std::slice::from_ref(&a_id)).unwrap();

        assert_eq!(order.len(), 2);
        assert!(order.contains(&a_id));
        assert!(order.contains(&b_id));
    }

    #[test]
    fn selected_transitive_dependency_precedes_root_through_reused_parent() {
        let (leaf_id, leaf, leaf_artifact) = pkg("leaf", vec![]);
        let (bridge_id, bridge, bridge_artifact) = pkg("bridge", vec!["leaf"]);
        let (root_id, root, root_artifact) = pkg("root", vec!["bridge"]);
        let manifest = manifest(vec![
            (leaf_id.clone(), leaf, leaf_artifact),
            (bridge_id, bridge, bridge_artifact),
            (root_id.clone(), root, root_artifact),
        ]);
        let selected = BTreeSet::from([leaf_id.clone(), root_id.clone()]);

        let order = order_selected_packages(&manifest, &selected).unwrap();

        assert_eq!(order, vec![leaf_id, root_id]);
    }
}
