use anyhow::{bail, Result};
use glu_core::{InstalledPackage, PackageKey, PackageSelector};
use std::collections::{BTreeMap, BTreeSet};

/// One installed dependency relationship. The requested selector is the
/// exact spelling persisted by the dependent's receipt; `provider` is the
/// installed package that currently provides that selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDependencyEdge {
    pub dependent: PackageKey,
    pub requested: PackageSelector,
    pub provider: PackageKey,
}

/// The dependency graph of the newest installed package for each stable
/// package identity. It is an in-memory index; receipts remain the durable
/// source of package and dependency facts.
#[derive(Debug, Clone, Default)]
pub struct InstalledPackageGraph {
    nodes: BTreeSet<PackageKey>,
    outgoing: BTreeMap<PackageKey, Vec<InstalledDependencyEdge>>,
    incoming: BTreeMap<PackageKey, Vec<InstalledDependencyEdge>>,
}

impl InstalledPackageGraph {
    pub(crate) fn from_packages(
        packages: &BTreeMap<PackageKey, Vec<InstalledPackage>>,
    ) -> Result<Self> {
        let nodes: BTreeSet<PackageKey> = packages.keys().cloned().collect();
        let mut outgoing = BTreeMap::new();
        let mut incoming: BTreeMap<PackageKey, Vec<InstalledDependencyEdge>> = BTreeMap::new();

        for (dependent, installed_versions) in packages {
            let Some(package) = installed_versions.first() else {
                continue;
            };
            let mut edges = Vec::with_capacity(package.deps.len());
            for dependency in &package.deps {
                if !nodes.contains(&dependency.package_key) {
                    bail!(
                        "installed package {} requires missing provider {}",
                        package.name.0,
                        dependency.requested_as.0
                    );
                }
                let edge = InstalledDependencyEdge {
                    dependent: dependent.clone(),
                    requested: dependency.requested_as.clone(),
                    provider: dependency.package_key.clone(),
                };
                incoming
                    .entry(edge.provider.clone())
                    .or_default()
                    .push(edge.clone());
                edges.push(edge);
            }
            outgoing.insert(dependent.clone(), edges);
        }

        Ok(Self {
            nodes,
            outgoing,
            incoming,
        })
    }

    pub fn contains(&self, package: &PackageKey) -> bool {
        self.nodes.contains(package)
    }

    pub fn dependencies(&self, package: &PackageKey) -> &[InstalledDependencyEdge] {
        self.outgoing.get(package).map_or(&[], Vec::as_slice)
    }

    pub fn dependents(&self, package: &PackageKey) -> &[InstalledDependencyEdge] {
        self.incoming.get(package).map_or(&[], Vec::as_slice)
    }

    pub fn package_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn dependency_count(&self) -> usize {
        self.outgoing.values().map(Vec::len).sum()
    }

    /// Every installed package identity reachable from `roots`, including
    /// roots themselves. Unknown roots are ignored; callers resolve selectors
    /// before entering the graph.
    pub fn reachable_from(
        &self,
        roots: impl IntoIterator<Item = PackageKey>,
    ) -> BTreeSet<PackageKey> {
        let mut reachable = BTreeSet::new();
        let mut stack = Vec::new();
        for root in roots {
            if self.contains(&root) && reachable.insert(root.clone()) {
                stack.push(root);
            }
        }

        while let Some(package) = stack.pop() {
            for dependency in self.dependencies(&package) {
                if reachable.insert(dependency.provider.clone()) {
                    stack.push(dependency.provider.clone());
                }
            }
        }
        reachable
    }

    /// Whether `start` depends directly or transitively on any identity in
    /// `targets`. The starting package itself is not considered a dependency.
    pub fn depends_on_any(&self, start: &PackageKey, targets: &BTreeSet<PackageKey>) -> bool {
        let mut visited = BTreeSet::new();
        let mut stack: Vec<PackageKey> = self
            .dependencies(start)
            .iter()
            .map(|edge| edge.provider.clone())
            .collect();

        while let Some(package) = stack.pop() {
            if targets.contains(&package) {
                return true;
            }
            if !visited.insert(package.clone()) {
                continue;
            }
            stack.extend(
                self.dependencies(&package)
                    .iter()
                    .map(|edge| edge.provider.clone()),
            );
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{KegVersion, PackageDependency, PackageId, PackageName};
    use std::path::PathBuf;

    fn package(name: &str, aliases: &[&str], deps: &[(&str, &str)]) -> InstalledPackage {
        InstalledPackage {
            id: PackageId(format!("pkg:test/{name}@1.0")),
            package_key: PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: aliases
                .iter()
                .map(|alias| PackageSelector((*alias).to_string()))
                .collect(),
            oldnames: Vec::new(),
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            keg_path: PathBuf::from(format!("/prefix/Cellar/{name}/1.0")),
            opt_path: PathBuf::from(format!("/prefix/opt/{name}")),
            keg_only: false,
            linked: true,
            deps: deps
                .iter()
                .map(|(requested, provider)| PackageDependency {
                    package_key: PackageKey(format!("package:{provider}")),
                    package: PackageId(format!("pkg:test/{provider}@1.0")),
                    requested_as: PackageSelector((*requested).to_string()),
                })
                .collect(),
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }

    #[test]
    fn indexes_requested_name_and_current_provider_in_both_directions() {
        let rust = package("rust", &[], &[("llvm@22", "llvm")]);
        let llvm = package("llvm", &["llvm@22"], &[]);
        let packages = BTreeMap::from([
            (rust.package_key.clone(), vec![rust]),
            (llvm.package_key.clone(), vec![llvm]),
        ]);

        let graph = InstalledPackageGraph::from_packages(&packages).unwrap();
        let rust_key = PackageKey("package:rust".to_string());
        let llvm_key = PackageKey("package:llvm".to_string());

        assert_eq!(graph.package_count(), 2);
        assert_eq!(graph.dependency_count(), 1);
        assert_eq!(graph.dependencies(&rust_key)[0].requested.0, "llvm@22");
        assert_eq!(graph.dependencies(&rust_key)[0].provider, llvm_key);
        assert_eq!(graph.dependents(&llvm_key)[0].dependent, rust_key);
    }

    #[test]
    fn reachability_handles_diamonds_and_cycles_once() {
        let app = package("app", &[], &[("left", "left"), ("right", "right")]);
        let left = package("left", &[], &[("shared", "shared")]);
        let right = package("right", &[], &[("shared", "shared")]);
        let shared = package("shared", &[], &[("left", "left")]);
        let packages = [app, left, right, shared]
            .into_iter()
            .map(|package| (package.package_key.clone(), vec![package]))
            .collect();
        let graph = InstalledPackageGraph::from_packages(&packages).unwrap();

        let app = PackageKey("package:app".to_string());
        let shared = PackageKey("package:shared".to_string());
        let reachable = graph.reachable_from([app.clone()]);

        assert_eq!(reachable.len(), 4);
        assert!(reachable.contains(&shared));
        assert!(graph.depends_on_any(&app, &BTreeSet::from([shared])));
    }

    #[test]
    fn graph_rejects_an_edge_to_a_removed_provider() {
        let app = package("app", &[], &[("missing", "missing")]);
        let packages = BTreeMap::from([(app.package_key.clone(), vec![app])]);

        let err = InstalledPackageGraph::from_packages(&packages).unwrap_err();

        assert!(err
            .to_string()
            .contains("requires missing provider missing"));
    }
}
