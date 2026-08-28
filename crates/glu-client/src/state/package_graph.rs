use anyhow::{bail, Result};
use glu_core::{InstalledPackage, MinimumVersion, PackageKey, PackageSelector};
use std::collections::{BTreeMap, BTreeSet};

/// One installed dependency relationship. The requested selector is the
/// exact spelling persisted by the dependent's receipt; `provider` is the
/// installed package that currently provides that selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDependencyEdge {
    pub dependent: PackageKey,
    pub requested: PackageSelector,
    pub provider: PackageKey,
    pub minimum_version: Option<MinimumVersion>,
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
                    minimum_version: (!dependency.requires.version.is_empty()).then(|| {
                        MinimumVersion {
                            version: dependency.requires.version.clone(),
                            revision: Some(dependency.requires.revision),
                        }
                    }),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        DependencyRequires, KegVersion, PackageId, PackageName, RuntimeDependencyRequirement,
    };
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
                .map(|(requested, provider)| RuntimeDependencyRequirement {
                    package_key: PackageKey(format!("package:{provider}")),
                    package: PackageId(format!("pkg:test/{provider}@1.0")),
                    requested_as: PackageSelector((*requested).to_string()),
                    requires: DependencyRequires {
                        version: String::new(),
                        revision: 0,
                    },
                })
                .collect(),
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
}
