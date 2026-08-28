use anyhow::Result;
use glu_core::{InstallManifest, PackageId, RuntimeDependencyRequirement};
use std::collections::BTreeSet;

/// Walks the package dependency graph reachable from `roots`, returning packages in
/// dependency-before-dependent order. `include` is asked, per edge, whether to descend into
/// that dependency; returning `false` prunes the subtree without adding it to the order (the
/// caller can still record it, e.g. as already-satisfied).
///
/// A dependency that is already an in-progress ancestor on the current walk (i.e. a real cycle,
/// such as two bottled libraries that mutually declare a runtime dependency on each other) is
/// skipped rather than re-entered — mirroring Homebrew's own `Dependency.expand`, which tracks
/// an `@expand_stack` for exactly this reason. The ancestor's own frame still adds it to the
/// order once it completes, so nothing is lost; only the back-edge is dropped.
pub fn dependency_order(
    manifest: &InstallManifest,
    roots: &[PackageId],
    mut include: impl FnMut(&RuntimeDependencyRequirement) -> Result<bool>,
) -> Result<Vec<PackageId>> {
    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    let mut in_progress = BTreeSet::new();
    for root in roots {
        if done.contains(root) || in_progress.contains(root) {
            continue;
        }
        visit(
            root,
            manifest,
            &mut include,
            &mut done,
            &mut in_progress,
            &mut order,
        )?;
    }
    Ok(order)
}

fn visit(
    package_id: &PackageId,
    manifest: &InstallManifest,
    include: &mut impl FnMut(&RuntimeDependencyRequirement) -> Result<bool>,
    done: &mut BTreeSet<PackageId>,
    in_progress: &mut BTreeSet<PackageId>,
    order: &mut Vec<PackageId>,
) -> Result<()> {
    let package = manifest
        .packages
        .get(package_id)
        .ok_or_else(|| anyhow::anyhow!("resolve manifest is missing package {}", package_id.0))?;

    in_progress.insert(package_id.clone());
    for dep in &package.deps {
        if done.contains(&dep.package) || in_progress.contains(&dep.package) {
            continue;
        }
        if include(dep)? {
            visit(&dep.package, manifest, include, done, in_progress, order)?;
        } else {
            done.insert(dep.package.clone());
        }
    }
    in_progress.remove(package_id);

    done.insert(package_id.clone());
    order.push(package_id.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, DependencyRequires, InstallManifest, KegVersion, PackageInstallMetadata,
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
                .map(|dep| RuntimeDependencyRequirement {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:homebrew/core/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector(dep.to_string()),
                    requires: DependencyRequires {
                        version: "1.0".to_string(),
                        revision: 0,
                    },
                })
                .collect(),
            min_versions: Default::default(),
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
            dependency_order(&manifest, std::slice::from_ref(&root_id), |_| Ok(true)).unwrap();

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

        let order = dependency_order(&manifest, std::slice::from_ref(&a_id), |_| Ok(true)).unwrap();

        assert_eq!(order.len(), 2);
        assert!(order.contains(&a_id));
        assert!(order.contains(&b_id));
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

        let order = dependency_order(&manifest, std::slice::from_ref(&root_id), |dep| {
            Ok(dep.package != dep_id)
        })
        .unwrap();

        assert_eq!(order, vec![root_id]);
    }
}
