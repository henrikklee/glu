use glu_core::{
    InstallManifest, InstalledPackage, MinimumVersion, PackageDependency, PackageId, PackageKey,
    PackageName, PackageSelector, ResolvedPackage, SlimManifest, SlimPackage, UsesResponse,
};
use std::collections::{BTreeMap, BTreeSet};

/// A typed query-tree edge. `requested_as` is the exact selector recorded by
/// the authority that supplied the topology. Reverse trees retain the same
/// edge fact while traversing it from provider to dependent.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DependencyTreeEdge {
    pub requested_as: PackageSelector,
    pub reversed: bool,
    pub minimum: Option<MinimumVersion>,
}

/// One package occurrence in a projected dependency tree. Package identity is
/// retained through rendering; names and selectors are presentation facts,
/// never graph keys.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DependencyTreeNode {
    pub package_key: PackageKey,
    pub package: PackageId,
    /// Presentation name: exact dependency selector for forward edges,
    /// canonical package name for roots and reverse edges.
    pub name: String,
    pub canonical_name: PackageName,
    pub version: String,
    pub children: Vec<DependencyTreeNode>,
    pub already_shown: bool,
    pub incoming: Option<DependencyTreeEdge>,
}

/// A registry query graph contradicted its own package identity facts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DependencyProjectionError {
    #[error(
        "registry graph assigns package key '{key}' to both '{first_id}' and '{second_id}'",
        key = .package_key.0,
        first_id = .first.0,
        second_id = .second.0
    )]
    DuplicatePackageKey {
        package_key: PackageKey,
        first: PackageId,
        second: PackageId,
    },
    #[error(
        "registry graph dependency '{selector}' from '{dependent_id}' references missing package '{package_id}'",
        selector = .requested_as.0,
        dependent_id = .dependent.0,
        package_id = .package.0
    )]
    MissingDependency {
        dependent: PackageId,
        requested_as: PackageSelector,
        package: PackageId,
    },
    #[error(
        "registry graph dependency '{selector}' from '{dependent_id}' identifies '{package_id}' as '{expected_key}' but that package is '{actual_key}'",
        selector = .requested_as.0,
        dependent_id = .dependent.0,
        package_id = .package.0,
        expected_key = .expected.0,
        actual_key = .actual.0
    )]
    DependencyIdentityMismatch {
        dependent: PackageId,
        requested_as: PackageSelector,
        package: PackageId,
        expected: PackageKey,
        actual: PackageKey,
    },
    #[error(
        "registry graph root '{selector}' references missing package '{package_id}'",
        selector = .requested_as.0,
        package_id = .package.0
    )]
    MissingRoot {
        requested_as: PackageSelector,
        package: PackageId,
    },
    #[error(
        "registry graph root '{selector}' identifies '{package_id}' as '{expected_key}' but that package is '{actual_key}'",
        selector = .requested_as.0,
        package_id = .package.0,
        expected_key = .expected.0,
        actual_key = .actual.0
    )]
    RootIdentityMismatch {
        requested_as: PackageSelector,
        package: PackageId,
        expected: PackageKey,
        actual: PackageKey,
    },
    #[error(
        "registry graph does not select requested root package '{package_id}'",
        package_id = .package.0
    )]
    UnselectedRoot { package: PackageId },
    #[error("registry uses graph returned {count} roots for one selector")]
    UnexpectedUsesRootCount { count: usize },
}

#[derive(Clone, Copy)]
enum RegistryGraphShape {
    /// Resolve manifests contain the complete forward dependency closure.
    Closed,
    /// Uses responses omit unrelated forward dependencies. An omitted edge is
    /// valid unless its claimed stable package key is present under another
    /// concrete package ID.
    SparseReverse,
}

trait ProjectablePackage {
    fn package_key(&self) -> &PackageKey;
    fn name(&self) -> &PackageName;
    fn version(&self) -> &str;
    fn deps(&self) -> &[PackageDependency];
    fn dependency_requirements(&self) -> &BTreeMap<PackageKey, MinimumVersion>;
}

macro_rules! impl_projectable_package {
    ($type:ty) => {
        impl ProjectablePackage for $type {
            fn package_key(&self) -> &PackageKey {
                &self.package_key
            }

            fn name(&self) -> &PackageName {
                &self.name
            }

            fn version(&self) -> &str {
                &self.keg_version.0
            }

            fn deps(&self) -> &[PackageDependency] {
                &self.deps
            }

            fn dependency_requirements(&self) -> &BTreeMap<PackageKey, MinimumVersion> {
                &self.dependency_requirements
            }
        }
    };
}

impl_projectable_package!(InstalledPackage);
impl_projectable_package!(ResolvedPackage);
impl_projectable_package!(SlimPackage);

#[derive(Clone, Copy)]
struct ProjectedPackage<'a> {
    package: &'a PackageId,
    value: &'a dyn ProjectablePackage,
}

#[derive(Clone)]
struct IncomingDependency<'a> {
    dependent: PackageKey,
    dependency: &'a PackageDependency,
}

/// Identity-preserving projection shared by installed and registry query
/// sources. Constructors receive one authority at a time; the projection
/// never merges receipt-backed and registry-backed package facts.
struct DependencyProjection<'a> {
    packages: BTreeMap<PackageKey, ProjectedPackage<'a>>,
    packages_by_id: BTreeMap<PackageId, PackageKey>,
    incoming: BTreeMap<PackageKey, Vec<IncomingDependency<'a>>>,
}

impl<'a> DependencyProjection<'a> {
    fn from_packages<P: ProjectablePackage + 'a>(
        packages: impl IntoIterator<Item = (&'a PackageId, &'a P)>,
    ) -> Self {
        let mut projected = BTreeMap::new();
        let mut packages_by_id = BTreeMap::new();
        for (package_id, package) in packages {
            packages_by_id.insert(package_id.clone(), package.package_key().clone());
            projected.insert(
                package.package_key().clone(),
                ProjectedPackage {
                    package: package_id,
                    value: package,
                },
            );
        }

        let mut incoming: BTreeMap<PackageKey, Vec<IncomingDependency<'a>>> = BTreeMap::new();
        for (dependent, package) in &projected {
            for dependency in package.value.deps() {
                if packages_by_id.get(&dependency.package) != Some(&dependency.package_key) {
                    continue;
                }
                incoming
                    .entry(dependency.package_key.clone())
                    .or_default()
                    .push(IncomingDependency {
                        dependent: dependent.clone(),
                        dependency,
                    });
            }
        }

        Self {
            packages: projected,
            packages_by_id,
            incoming,
        }
    }

    fn package_key_for_id(&self, package: &PackageId) -> Option<&PackageKey> {
        self.packages_by_id.get(package)
    }

    fn forward_tree(&self, root: &PackageKey) -> Option<DependencyTreeNode> {
        let mut seen = BTreeSet::from([root.clone()]);
        self.forward_node(root, None, &mut seen)
    }

    fn forward_forest(
        &self,
        roots: impl IntoIterator<Item = PackageKey>,
    ) -> Vec<DependencyTreeNode> {
        let mut seen = BTreeSet::new();
        roots
            .into_iter()
            .filter_map(|root| {
                seen.insert(root.clone());
                self.forward_node(&root, None, &mut seen)
            })
            .collect()
    }

    fn forward_node(
        &self,
        package_key: &PackageKey,
        incoming: Option<DependencyTreeEdge>,
        seen: &mut BTreeSet<PackageKey>,
    ) -> Option<DependencyTreeNode> {
        let package = self.packages.get(package_key)?;
        let mut dependencies: Vec<&PackageDependency> = package.value.deps().iter().collect();
        dependencies.sort_by(|a, b| {
            a.requested_as
                .0
                .to_lowercase()
                .cmp(&b.requested_as.0.to_lowercase())
                .then_with(|| a.package_key.cmp(&b.package_key))
        });

        let mut children = Vec::new();
        for dependency in dependencies {
            if self.package_key_for_id(&dependency.package) != Some(&dependency.package_key) {
                continue;
            }
            let Some(target) = self.packages.get(&dependency.package_key) else {
                continue;
            };
            let edge = DependencyTreeEdge {
                requested_as: dependency.requested_as.clone(),
                reversed: false,
                minimum: package
                    .value
                    .dependency_requirements()
                    .get(&dependency.package_key)
                    .cloned(),
            };
            let already_seen = !seen.insert(dependency.package_key.clone());
            let mut child = self.node(target, Some(edge));
            if already_seen {
                child.already_shown = !target.value.deps().is_empty();
            } else {
                child.children = self.forward_children(&dependency.package_key, seen);
            }
            children.push(child);
        }

        let mut node = self.node(package, incoming);
        node.children = children;
        Some(node)
    }

    fn forward_children(
        &self,
        package_key: &PackageKey,
        seen: &mut BTreeSet<PackageKey>,
    ) -> Vec<DependencyTreeNode> {
        self.forward_node(package_key, None, seen)
            .map(|node| node.children)
            .unwrap_or_default()
    }

    fn reverse_tree(&self, root: &PackageKey) -> Option<DependencyTreeNode> {
        let mut seen = BTreeSet::from([root.clone()]);
        let root_package = self.packages.get(root)?;
        let mut node = self.node(root_package, None);
        node.children = self.reverse_children(root, &mut seen);
        Some(node)
    }

    fn reverse_children(
        &self,
        package_key: &PackageKey,
        seen: &mut BTreeSet<PackageKey>,
    ) -> Vec<DependencyTreeNode> {
        let mut parents = self.incoming.get(package_key).cloned().unwrap_or_default();
        parents.sort_by(|a, b| {
            let a_name = self
                .packages
                .get(&a.dependent)
                .map(|package| package.value.name().0.to_lowercase())
                .unwrap_or_default();
            let b_name = self
                .packages
                .get(&b.dependent)
                .map(|package| package.value.name().0.to_lowercase())
                .unwrap_or_default();
            a_name
                .cmp(&b_name)
                .then_with(|| a.dependent.cmp(&b.dependent))
        });
        parents.dedup_by(|a, b| a.dependent == b.dependent);

        let mut children = Vec::new();
        for parent in parents {
            let Some(parent_package) = self.packages.get(&parent.dependent) else {
                continue;
            };
            let edge = DependencyTreeEdge {
                requested_as: parent.dependency.requested_as.clone(),
                reversed: true,
                minimum: parent_package
                    .value
                    .dependency_requirements()
                    .get(package_key)
                    .cloned(),
            };
            let already_seen = !seen.insert(parent.dependent.clone());
            let mut node = self.node(parent_package, Some(edge));
            if already_seen {
                node.already_shown = self
                    .incoming
                    .get(&parent.dependent)
                    .is_some_and(|dependents| !dependents.is_empty());
            } else {
                node.children = self.reverse_children(&parent.dependent, seen);
            }
            children.push(node);
        }
        children
    }

    fn node(
        &self,
        package: &ProjectedPackage<'a>,
        incoming: Option<DependencyTreeEdge>,
    ) -> DependencyTreeNode {
        let canonical_name = package.value.name().clone();
        let name = incoming.as_ref().filter(|edge| !edge.reversed).map_or_else(
            || canonical_name.0.clone(),
            |edge| edge.requested_as.0.clone(),
        );
        DependencyTreeNode {
            package_key: package.value.package_key().clone(),
            package: package.package.clone(),
            name,
            canonical_name,
            version: package.value.version().to_string(),
            children: Vec::new(),
            already_shown: false,
            incoming,
        }
    }
}

fn validate_registry_graph<P: ProjectablePackage>(
    packages: &BTreeMap<PackageId, P>,
    roots: &[glu_core::PackageSelection],
    shape: RegistryGraphShape,
) -> Result<(), DependencyProjectionError> {
    let mut package_ids_by_key = BTreeMap::new();
    for (package_id, package) in packages {
        if let Some(first) = package_ids_by_key.insert(package.package_key().clone(), package_id) {
            return Err(DependencyProjectionError::DuplicatePackageKey {
                package_key: package.package_key().clone(),
                first: first.clone(),
                second: package_id.clone(),
            });
        }
    }

    for (dependent_id, dependent) in packages {
        for dependency in dependent.deps() {
            match packages.get(&dependency.package) {
                Some(provider) if provider.package_key() != &dependency.package_key => {
                    return Err(DependencyProjectionError::DependencyIdentityMismatch {
                        dependent: dependent_id.clone(),
                        requested_as: dependency.requested_as.clone(),
                        package: dependency.package.clone(),
                        expected: dependency.package_key.clone(),
                        actual: provider.package_key().clone(),
                    });
                }
                Some(_) => {}
                None if matches!(shape, RegistryGraphShape::SparseReverse)
                    && !package_ids_by_key.contains_key(&dependency.package_key) => {}
                None => {
                    return Err(DependencyProjectionError::MissingDependency {
                        dependent: dependent_id.clone(),
                        requested_as: dependency.requested_as.clone(),
                        package: dependency.package.clone(),
                    });
                }
            }
        }
    }

    for root in roots {
        let Some(package) = packages.get(&root.package) else {
            return Err(DependencyProjectionError::MissingRoot {
                requested_as: root.requested_as.clone(),
                package: root.package.clone(),
            });
        };
        if package.package_key() != &root.package_key {
            return Err(DependencyProjectionError::RootIdentityMismatch {
                requested_as: root.requested_as.clone(),
                package: root.package.clone(),
                expected: root.package_key.clone(),
                actual: package.package_key().clone(),
            });
        }
    }

    Ok(())
}

pub(crate) fn validate_install_manifest_graph(
    manifest: &InstallManifest,
) -> Result<(), DependencyProjectionError> {
    validate_registry_graph(
        &manifest.packages,
        &manifest.roots,
        RegistryGraphShape::Closed,
    )
}

pub(crate) fn installed_forward_forest<'a>(
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    roots: impl IntoIterator<Item = PackageKey>,
) -> Vec<DependencyTreeNode> {
    let projection = DependencyProjection::from_packages(
        packages.into_iter().map(|package| (&package.id, package)),
    );
    projection.forward_forest(roots)
}

pub(crate) fn installed_forward_tree<'a>(
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    root: &PackageKey,
) -> Option<DependencyTreeNode> {
    let projection = DependencyProjection::from_packages(
        packages.into_iter().map(|package| (&package.id, package)),
    );
    projection.forward_tree(root)
}

pub(crate) fn installed_reverse_tree<'a>(
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    root: &PackageKey,
) -> Option<DependencyTreeNode> {
    let projection = DependencyProjection::from_packages(
        packages.into_iter().map(|package| (&package.id, package)),
    );
    projection.reverse_tree(root)
}

fn registry_forward_tree<P: ProjectablePackage>(
    packages: &BTreeMap<PackageId, P>,
    root_id: &PackageId,
) -> Option<DependencyTreeNode> {
    let projection = DependencyProjection::from_packages(packages.iter());
    let root = projection.package_key_for_id(root_id)?.clone();
    projection.forward_tree(&root)
}

pub(crate) fn dependency_forest_from_manifest(
    manifest: &InstallManifest,
) -> Vec<DependencyTreeNode> {
    let projection = DependencyProjection::from_packages(manifest.packages.iter());
    manifest
        .roots
        .iter()
        .filter_map(|root| {
            let package_key = projection.package_key_for_id(&root.package)?;
            (package_key == &root.package_key)
                .then(|| projection.forward_tree(package_key))
                .flatten()
        })
        .collect()
}

pub fn dependency_tree_from_slim(
    manifest: &SlimManifest,
    root_id: &PackageId,
) -> Result<DependencyTreeNode, DependencyProjectionError> {
    validate_registry_graph(
        &manifest.packages,
        &manifest.roots,
        RegistryGraphShape::Closed,
    )?;
    if !manifest.roots.iter().any(|root| &root.package == root_id) {
        return Err(DependencyProjectionError::UnselectedRoot {
            package: root_id.clone(),
        });
    }
    Ok(registry_forward_tree(&manifest.packages, root_id)
        .expect("validated slim root must be present"))
}

pub fn reverse_tree_from_uses(
    uses: &UsesResponse,
) -> Result<Option<DependencyTreeNode>, DependencyProjectionError> {
    if uses.roots.len() > 1 {
        return Err(DependencyProjectionError::UnexpectedUsesRootCount {
            count: uses.roots.len(),
        });
    }
    validate_registry_graph(
        &uses.packages,
        &uses.roots,
        RegistryGraphShape::SparseReverse,
    )?;
    let Some(selection) = uses.roots.first() else {
        return Ok(None);
    };
    let projection = DependencyProjection::from_packages(uses.packages.iter());
    let root = projection
        .package_key_for_id(&selection.package)
        .expect("validated uses root must be present");
    Ok(projection.reverse_tree(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{KegVersion, PackageSelection, ResolveRequestEcho, Target, UsesRequestEcho};

    fn slim(name: &str, version: &str, dependencies: &[(&str, &str)]) -> (PackageId, SlimPackage) {
        let package_key = PackageKey(format!("package:{name}"));
        let package_id = PackageId(format!("pkg:test/{name}@{version}"));
        let deps = dependencies
            .iter()
            .map(|(requested_as, provider)| PackageDependency {
                package_key: PackageKey(format!("package:{provider}")),
                package: PackageId(format!("pkg:test/{provider}@1.0")),
                requested_as: PackageSelector((*requested_as).to_string()),
            })
            .collect();
        (
            package_id,
            SlimPackage {
                package_key,
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision: 0,
                keg_version: KegVersion(version.to_string()),
                deps,
                dependency_requirements: dependencies
                    .iter()
                    .map(|(_, provider)| {
                        (
                            PackageKey(format!("package:{provider}")),
                            MinimumVersion {
                                version: "1.0".to_string(),
                                revision: Some(0),
                            },
                        )
                    })
                    .collect(),
            },
        )
    }

    fn slim_manifest(root: &str, packages: Vec<(PackageId, SlimPackage)>) -> SlimManifest {
        let packages: BTreeMap<_, _> = packages.into_iter().collect();
        let root_package = packages
            .iter()
            .find(|(_, package)| package.name.0 == root)
            .unwrap();
        SlimManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector(root.to_string())],
                target: Target("arm64_test".to_string()),
                slim: true,
            },
            roots: vec![PackageSelection {
                requested_as: PackageSelector(root.to_string()),
                package_key: root_package.1.package_key.clone(),
                package: root_package.0.clone(),
            }],
            packages,
        }
    }

    #[test]
    fn forward_projection_keeps_selector_and_both_package_identities() {
        let (root_id, root) = slim("rust", "1.0", &[("llvm@22", "llvm")]);
        let (llvm_id, mut llvm) = slim("llvm", "1.0", &[]);
        llvm.aliases = vec![PackageSelector("llvm@22".to_string())];
        let manifest = slim_manifest(
            "rust",
            vec![(root_id.clone(), root), (llvm_id.clone(), llvm)],
        );

        let tree = dependency_tree_from_slim(&manifest, &root_id)
            .expect("valid slim graph should project its selected root");
        let dependency = &tree.children[0];

        assert_eq!(dependency.name, "llvm@22");
        assert_eq!(dependency.canonical_name.0, "llvm");
        assert_eq!(dependency.package_key.0, "package:llvm");
        assert_eq!(dependency.package, llvm_id);
        assert_eq!(
            dependency.incoming.as_ref().unwrap().requested_as.0,
            "llvm@22"
        );
        assert_eq!(
            dependency
                .incoming
                .as_ref()
                .and_then(|edge| edge.minimum.as_ref())
                .map(|minimum| minimum.version.as_str()),
            Some("1.0")
        );
    }

    #[test]
    fn forward_projection_preserves_old_name_spelling() {
        let (root_id, root) = slim("app", "1.0", &[("old-lib", "new-lib")]);
        let (library_id, mut library) = slim("new-lib", "1.0", &[]);
        library.oldnames = vec![PackageSelector("old-lib".to_string())];
        let manifest = slim_manifest("app", vec![(root_id.clone(), root), (library_id, library)]);

        let dependency = &dependency_tree_from_slim(&manifest, &root_id)
            .expect("valid slim graph should project its selected root")
            .children[0];

        assert_eq!(dependency.name, "old-lib");
        assert_eq!(dependency.canonical_name.0, "new-lib");
        assert_eq!(dependency.package_key.0, "package:new-lib");
    }

    #[test]
    fn uses_projection_displays_dependent_and_retains_reversed_selector() {
        let (target_id, target) = slim("llvm", "1.0", &[]);
        let (rust_id, rust) = slim("rust", "1.0", &[("llvm@22", "llvm")]);
        let manifest = slim_manifest("llvm", vec![(target_id.clone(), target), (rust_id, rust)]);
        let uses = UsesResponse {
            schema: "glu.uses.v1".to_string(),
            request: UsesRequestEcho {
                name: PackageSelector("llvm@22".to_string()),
                target: Target("arm64_test".to_string()),
                direct: false,
            },
            roots: manifest.roots,
            packages: manifest.packages,
        };

        let tree = reverse_tree_from_uses(&uses)
            .expect("valid uses graph")
            .expect("uses graph should contain its selected root");
        let dependent = &tree.children[0];

        assert_eq!(dependent.name, "rust");
        assert_eq!(dependent.package_key.0, "package:rust");
        assert!(dependent.incoming.as_ref().unwrap().reversed);
        assert_eq!(
            dependent.incoming.as_ref().unwrap().requested_as.0,
            "llvm@22"
        );
    }

    #[test]
    fn slim_projection_rejects_a_missing_dependency_provider() {
        let (root_id, root) = slim("rust", "1.0", &[("llvm@22", "llvm")]);
        let manifest = slim_manifest("rust", vec![(root_id.clone(), root)]);

        let error = dependency_tree_from_slim(&manifest, &root_id).unwrap_err();

        assert!(matches!(
            error,
            DependencyProjectionError::MissingDependency {
                dependent,
                requested_as,
                package,
            } if dependent == root_id
                && requested_as.0 == "llvm@22"
                && package.0 == "pkg:test/llvm@1.0"
        ));
    }

    #[test]
    fn slim_projection_rejects_a_dependency_identity_mismatch() {
        let (root_id, root) = slim("rust", "1.0", &[("llvm@22", "llvm")]);
        let (llvm_id, mut llvm) = slim("llvm", "1.0", &[]);
        llvm.package_key = PackageKey("package:not-llvm".to_string());
        let manifest = slim_manifest("rust", vec![(root_id.clone(), root), (llvm_id, llvm)]);

        let error = dependency_tree_from_slim(&manifest, &root_id).unwrap_err();

        assert!(matches!(
            error,
            DependencyProjectionError::DependencyIdentityMismatch {
                expected,
                actual,
                ..
            } if expected.0 == "package:llvm" && actual.0 == "package:not-llvm"
        ));
    }

    #[test]
    fn uses_projection_allows_omitted_unrelated_forward_dependencies() {
        let (target_id, target) = slim("llvm", "1.0", &[]);
        let (rust_id, rust) = slim("rust", "1.0", &[("llvm@22", "llvm"), ("zstd", "zstd")]);
        let manifest = slim_manifest("llvm", vec![(target_id, target), (rust_id, rust)]);
        let uses = UsesResponse {
            schema: "glu.uses.v1".to_string(),
            request: UsesRequestEcho {
                name: PackageSelector("llvm@22".to_string()),
                target: Target("arm64_test".to_string()),
                direct: false,
            },
            roots: manifest.roots,
            packages: manifest.packages,
        };

        let tree = reverse_tree_from_uses(&uses)
            .expect("valid sparse uses graph")
            .expect("uses graph should contain its selected root");

        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "rust");
    }

    #[test]
    fn uses_projection_rejects_a_hidden_edge_to_an_included_package() {
        let (target_id, target) = slim("llvm", "1.0", &[]);
        let (rust_id, mut rust) = slim("rust", "1.0", &[("llvm@22", "llvm")]);
        rust.deps[0].package = PackageId("pkg:test/missing-llvm@1.0".to_string());
        let manifest = slim_manifest("llvm", vec![(target_id, target), (rust_id.clone(), rust)]);
        let uses = UsesResponse {
            schema: "glu.uses.v1".to_string(),
            request: UsesRequestEcho {
                name: PackageSelector("llvm@22".to_string()),
                target: Target("arm64_test".to_string()),
                direct: false,
            },
            roots: manifest.roots,
            packages: manifest.packages,
        };

        let error = reverse_tree_from_uses(&uses).unwrap_err();

        assert!(matches!(
            error,
            DependencyProjectionError::MissingDependency {
                dependent,
                requested_as,
                ..
            } if dependent == rust_id && requested_as.0 == "llvm@22"
        ));
    }

    #[test]
    fn uses_projection_rejects_multiple_roots() {
        let (target_id, target) = slim("llvm", "1.0", &[]);
        let manifest = slim_manifest("llvm", vec![(target_id, target)]);
        let mut uses = UsesResponse {
            schema: "glu.uses.v1".to_string(),
            request: UsesRequestEcho {
                name: PackageSelector("llvm".to_string()),
                target: Target("arm64_test".to_string()),
                direct: false,
            },
            roots: manifest.roots,
            packages: manifest.packages,
        };
        uses.roots.push(uses.roots[0].clone());

        assert_eq!(
            reverse_tree_from_uses(&uses).unwrap_err(),
            DependencyProjectionError::UnexpectedUsesRootCount { count: 2 }
        );
    }
}
