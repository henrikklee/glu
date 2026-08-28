use crate::{
    dependency_query::{self, DependencyTreeNode},
    homebrew_version::PackageVersion,
    state::package_graph::InstalledPackageGraph,
};
use anyhow::{bail, Result};
use glu_core::{InstalledPackage, PackageId, PackageKey, PackageName, PackageSelector, Prefix};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

/// Installed state for a prefix, loaded as a **snapshot** per command.
/// Durable reads and writes live in `InstalledStateStore`; this type is the
/// in-memory database built from those records. Every query (`find`, `kegs`,
/// `names`, `list`, `dangling`) hits this index, never the filesystem.
///
/// The index is a snapshot of the receipts at load time, not a live view:
/// a command that mutates receipts must load a new snapshot before any
/// post-mutation query — e.g. the dangling report after an update runs
/// against freshly loaded state, or a dependency the new version dropped
/// would still look reachable via the old receipt's edges.
///
/// The previous `ReceiptInstalledState` re-scanned the whole Cellar and
/// re-decoded every receipt on *every* query (`find_by_name` walked the
/// full tree per dependency edge), which made a single named `update`
/// scan the installed set 3-4× (outdated listing, per-name lookup,
/// cascade, dangling report). Loading once per command collapses that to
/// one scan and O(log n) lookups.
#[derive(Debug, Clone)]
pub struct InstalledState {
    prefix: Prefix,
    /// Every installed keg indexed by its current receipt name; each name's
    /// kegs are sorted newest-first (version, then revision).
    by_name: BTreeMap<PackageName, Vec<InstalledPackage>>,
    /// The same installed kegs indexed by stable package identity for graph
    /// traversal. Names remain a presentation and selector boundary only.
    by_key: BTreeMap<PackageKey, Vec<InstalledPackage>>,
    /// Every selector advertised by installed receipts resolves to one stable
    /// package identity.
    selector_index: HashMap<PackageSelector, PackageKey>,
    /// Receipt-backed dependency topology for the newest installed keg of
    /// each package. Receipts remain the durable source of graph facts.
    package_graph: InstalledPackageGraph,
    declared_names: BTreeSet<PackageName>,
    declared_keys: BTreeSet<PackageKey>,
    deactivated_names: BTreeSet<PackageName>,
    deactivated_keys: BTreeSet<PackageKey>,
}

impl InstalledState {
    /// Test-only: build state from an already-read package list, without a
    /// prefix on disk. Shares the real index construction used by the store so
    /// fixture states and loaded states can't diverge.
    #[cfg(test)]
    pub(crate) fn from_packages(packages: Vec<InstalledPackage>) -> Self {
        Self::from_loaded_packages(
            packages,
            Prefix(PathBuf::from("/tmp/glu")),
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap()
    }

    #[cfg(test)]
    pub(crate) fn from_packages_declared(
        packages: Vec<InstalledPackage>,
        declared_names: BTreeSet<PackageName>,
    ) -> Self {
        Self::from_loaded_packages(
            packages,
            Prefix(PathBuf::from("/tmp/glu")),
            declared_names,
            BTreeSet::new(),
        )
        .unwrap()
    }

    /// Build a hypothetical installed-state snapshot from predicted receipt
    /// facts. Simulation uses the same selector precedence, current-keg
    /// selection, and package graph as state loaded from disk.
    pub(crate) fn from_simulated_packages(
        packages: Vec<InstalledPackage>,
        declared_names: BTreeSet<PackageName>,
    ) -> Result<Self> {
        Self::from_loaded_packages(
            packages,
            Prefix(PathBuf::new()),
            declared_names,
            BTreeSet::new(),
        )
    }

    /// Index already-read receipts as an in-memory installed-state database.
    /// Rows are grouped by current receipt name; the resolver maps every name
    /// that package owns (current name + old names) to that current group.
    pub(crate) fn from_loaded_packages(
        packages: Vec<InstalledPackage>,
        prefix: Prefix,
        declared_names: BTreeSet<PackageName>,
        deactivated_names: BTreeSet<PackageName>,
    ) -> Result<Self> {
        let mut by_name: BTreeMap<PackageName, Vec<InstalledPackage>> = BTreeMap::new();
        let mut by_key: BTreeMap<PackageKey, Vec<InstalledPackage>> = BTreeMap::new();
        let mut selector_index: HashMap<PackageSelector, PackageKey> = HashMap::new();
        let exact_selectors: BTreeSet<PackageSelector> = packages
            .iter()
            .map(|package| PackageSelector(package.name.0.clone()))
            .collect();
        for package in &packages {
            insert_selector_mapping(
                &mut selector_index,
                PackageSelector(package.name.0.clone()),
                &package.package_key,
            )?;
        }
        for package in &packages {
            for selector in package.aliases.iter().chain(&package.oldnames) {
                if exact_selectors.contains(selector) {
                    continue;
                }
                insert_selector_mapping(
                    &mut selector_index,
                    selector.clone(),
                    &package.package_key,
                )?;
            }
        }
        for package in packages {
            let current = package.name.clone();
            by_key
                .entry(package.package_key.clone())
                .or_default()
                .push(package.clone());
            by_name.entry(current).or_default().push(package);
        }
        for kegs in by_name.values_mut() {
            kegs.sort_by(|a, b| compare_installed(b, a));
        }
        for kegs in by_key.values_mut() {
            kegs.sort_by(|a, b| compare_installed(b, a));
        }
        let package_graph = InstalledPackageGraph::from_packages(&by_key)?;
        let declared_keys = declared_names
            .iter()
            .filter_map(|name| {
                selector_index
                    .get(&PackageSelector(name.0.clone()))
                    .cloned()
            })
            .collect();
        let deactivated_keys = deactivated_names
            .iter()
            .filter_map(|name| {
                selector_index
                    .get(&PackageSelector(name.0.clone()))
                    .cloned()
            })
            .collect();
        Ok(Self {
            prefix,
            by_name,
            by_key,
            selector_index,
            package_graph,
            declared_names,
            declared_keys,
            deactivated_names,
            deactivated_keys,
        })
    }

    /// Newest installed keg whose current receipt name is exactly `name`, if
    /// any. Selector-aware callers must use `resolve_selector` instead.
    pub fn find(&self, name: &PackageName) -> Option<&InstalledPackage> {
        self.by_name.get(name).and_then(|kegs| kegs.first())
    }

    pub fn find_by_key(&self, key: &PackageKey) -> Option<&InstalledPackage> {
        self.by_key.get(key).and_then(|kegs| kegs.first())
    }

    /// The selected installed release for every stable package identity.
    pub fn current_packages(&self) -> impl Iterator<Item = &InstalledPackage> {
        self.by_key.values().filter_map(|kegs| kegs.first())
    }

    pub fn resolve_selector(&self, selector: &PackageSelector) -> Option<&InstalledPackage> {
        self.selector_index
            .get(selector)
            .and_then(|key| self.find_by_key(key))
    }

    pub fn package_graph(&self) -> &InstalledPackageGraph {
        &self.package_graph
    }

    /// Every installed keg whose current receipt name is exactly `name`,
    /// newest first.
    pub fn kegs(&self, name: &PackageName) -> &[InstalledPackage] {
        self.by_name.get(name).map_or(&[], |kegs| kegs.as_slice())
    }

    /// Every installed package name (unique, sorted).
    pub fn names(&self) -> Vec<PackageName> {
        self.by_name.keys().cloned().collect()
    }

    /// Every installed package across all kegs, sorted by name
    /// (case-insensitive) and then by version descending, so the newest
    /// keg of each package comes first. Multiple versions of the same
    /// package are all listed.
    pub fn list(&self) -> Vec<InstalledPackage> {
        let mut installed: Vec<InstalledPackage> =
            self.by_name.values().flatten().cloned().collect();
        installed.sort_by(|a, b| {
            a.name
                .0
                .to_lowercase()
                .cmp(&b.name.0.to_lowercase())
                .then_with(|| compare_installed(b, a))
        });
        installed
    }

    /// Every installed keg whose name is in the declaration (`glu.json`),
    /// sorted like `list()`. The keg-level view of the declaration: what the
    /// user asked for, at the versions they asked for.
    pub fn declared(&self) -> Vec<InstalledPackage> {
        self.list()
            .into_iter()
            .filter(|package| self.declared_keys.contains(&package.package_key))
            .collect()
    }

    /// Names of declared packages, per the declaration file. Name-level,
    /// matching the graph roots used by `dangling`.
    pub fn declared_names(&self) -> Vec<PackageName> {
        self.declared_name_set().into_iter().collect()
    }

    /// Names marked deactivated in the declaration, sorted.
    pub fn deactivated_names(&self) -> Vec<PackageName> {
        self.deactivated_names.iter().cloned().collect()
    }

    /// Whether `selector` resolves to a currently deactivated package.
    pub fn is_declared_key(&self, package_key: &PackageKey) -> bool {
        self.declared_keys.contains(package_key)
    }

    pub fn is_deactivated_key(&self, package_key: &PackageKey) -> bool {
        self.deactivated_keys.contains(package_key)
    }

    pub fn is_deactivated(&self, selector: &PackageSelector) -> bool {
        self.resolve_selector(selector)
            .is_some_and(|package| self.is_deactivated_key(&package.package_key))
            || self
                .deactivated_names
                .contains(&PackageName(selector.0.clone()))
    }

    pub fn is_active(&self, selector: &PackageSelector) -> bool {
        !self.is_deactivated(selector)
    }

    fn declared_name_set(&self) -> BTreeSet<PackageName> {
        self.declared_names.clone()
    }

    /// Packages no longer required by anything declared, plus superseded
    /// kegs for reachable package identities.
    pub fn dangling(&self) -> Vec<InstalledPackage> {
        self.dangling_for_declared(&self.declared_names)
    }

    /// Compute dangling packages against an alternate declaration while using
    /// this snapshot's canonical selector index and package graph. Removal
    /// planning uses this to model declaration changes without rebuilding
    /// topology through a second traversal implementation.
    pub(crate) fn dangling_for_declared(
        &self,
        declared: &BTreeSet<PackageName>,
    ) -> Vec<InstalledPackage> {
        let roots = declared.iter().filter_map(|name| {
            self.selector_index
                .get(&PackageSelector(name.0.clone()))
                .cloned()
        });
        let reachable = self.package_graph.reachable_from(roots);
        let retained: BTreeSet<PackageId> = reachable
            .iter()
            .filter_map(|key| self.find_by_key(key))
            .map(|package| package.id.clone())
            .collect();

        let mut dangling: Vec<InstalledPackage> = self
            .list()
            .into_iter()
            .filter(|package| !retained.contains(&package.id))
            .collect();
        dangling.sort_by(|a, b| {
            a.name
                .0
                .to_lowercase()
                .cmp(&b.name.0.to_lowercase())
                .then_with(|| compare_installed(b, a))
        });
        dangling
    }

    /// Whether `start` depends directly or transitively on any selected
    /// installed package. Selectors resolve once at the graph boundary; graph
    /// traversal then uses stable package identities exclusively.
    pub(crate) fn depends_on_any(
        &self,
        start: &InstalledPackage,
        target_names: &BTreeSet<PackageName>,
    ) -> bool {
        let targets: BTreeSet<PackageKey> = target_names
            .iter()
            .filter_map(|name| {
                self.selector_index
                    .get(&PackageSelector(name.0.clone()))
                    .cloned()
            })
            .collect();
        self.package_graph
            .depends_on_any(&start.package_key, &targets)
    }

    /// Total number of installed kegs across all packages — the `glu ls`
    /// hint counts the hidden dependencies as `total − declared`.
    pub fn total_kegs(&self) -> usize {
        self.by_name.values().map(|kegs| kegs.len()).sum()
    }

    pub fn cellar_path(&self) -> PathBuf {
        self.prefix.0.join("Cellar")
    }

    /// Full `glu ls --all` dependency tree: declared packages at the top,
    /// each expanded by its declared deps (the `deps` recorded in its
    /// receipt). Each package expands once — the first sighting shows the
    /// full subtree, later sightings render as a leaf marked `(*)`
    /// ("dependencies already shown above"), the cargo-tree convention.
    /// This keeps dense graphs readable while staying complete: every
    /// package appears exactly once with its deps.
    pub fn dependency_tree(&self) -> Vec<DependencyTreeNode> {
        dependency_query::installed_forward_forest(
            self.current_packages(),
            self.declared()
                .into_iter()
                .map(|package| package.package_key),
        )
    }

    /// Like `dependency_tree`, but also roots every dangling package —
    /// installed, not declared, and unreachable from anything declared — so
    /// the tree covers *everything* installed (`glu ls --all --tree`).
    pub fn dependency_tree_all(&self) -> Vec<DependencyTreeNode> {
        let mut roots: Vec<PackageKey> = self
            .declared()
            .into_iter()
            .map(|package| package.package_key)
            .chain(
                self.dangling()
                    .into_iter()
                    .map(|package| package.package_key),
            )
            .collect();
        roots.sort();
        roots.dedup();
        let mut nodes = dependency_query::installed_forward_forest(self.current_packages(), roots);
        nodes.sort_by_key(|node| node.name.to_lowercase());
        nodes
    }

    /// `glu deps`: the dependency subtree of one installed package (the
    /// newest keg of that identity) — receipts only, so it answers "what does
    /// this pull in on my system" offline and exact.
    pub fn dependency_tree_for(&self, selector: &PackageSelector) -> Option<DependencyTreeNode> {
        let root = self.resolve_selector(selector)?;
        dependency_query::installed_forward_tree(self.current_packages(), &root.package_key)
    }

    /// `glu why`: the reverse dependency tree of one installed package —
    /// the target at the root, its direct dependents as children, and their
    /// dependents recursed (cycle-guarded). `None` when the selector is not
    /// installed.
    pub fn reverse_dependents_tree(
        &self,
        selector: &PackageSelector,
    ) -> Option<DependencyTreeNode> {
        let root = self.resolve_selector(selector)?;
        dependency_query::installed_reverse_tree(self.current_packages(), &root.package_key)
    }
}

fn insert_selector_mapping(
    index: &mut HashMap<PackageSelector, PackageKey>,
    selector: PackageSelector,
    package_key: &PackageKey,
) -> Result<()> {
    if let Some(existing) = index.get(&selector) {
        if existing != package_key {
            bail!(
                "installed selector '{}' belongs to both {} and {}",
                selector.0,
                existing.0,
                package_key.0
            );
        }
        return Ok(());
    }
    index.insert(selector, package_key.clone());
    Ok(())
}

fn compare_installed(a: &InstalledPackage, b: &InstalledPackage) -> std::cmp::Ordering {
    PackageVersion::new(&a.version, a.revision).compare(PackageVersion::new(&b.version, b.revision))
}

#[cfg(test)]
fn declared_set(names: &[&str]) -> BTreeSet<PackageName> {
    names
        .iter()
        .map(|name| PackageName((*name).to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        receipts::{
            GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
            ReceiptStatus,
        },
        store::InstalledStateStore,
    };
    use glu_core::{ArtifactId, KegVersion, PackageDependency, PackageId};
    use std::fs;

    fn write_receipt(
        prefix: &std::path::Path,
        name: &str,
        version: &str,
        declared: bool,
        keg_only: bool,
    ) {
        write_receipt_status(
            prefix,
            name,
            version,
            declared,
            keg_only,
            ReceiptStatus::Complete,
        )
    }

    fn write_receipt_status(
        prefix: &std::path::Path,
        name: &str,
        version: &str,
        declared: bool,
        keg_only: bool,
        status: ReceiptStatus,
    ) {
        let keg = prefix.join("Cellar").join(name).join(version);
        fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:test/{name}@{version}")),
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
                cellar: "/opt/homebrew/Cellar".to_string(),
            },
            sizes: crate::state::ReceiptSizes::default(),
            paths: ReceiptPaths {
                keg: keg.clone(),
                opt: prefix.join("opt").join(name),
            },
            links: crate::state::ReceiptLinkNames {
                opt_names: Vec::new(),
            },
            install: ReceiptInstall {
                keg_only,
                linked: true,
                link_overwrite: Vec::new(),
                deps: vec![],
                dependency_requirements: Default::default(),
            },
        };
        let path = InstalledStateStore::receipt_path_for_keg(&keg);
        fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        if declared && status == ReceiptStatus::Complete {
            let store = InstalledStateStore::new(Prefix(prefix.to_path_buf()));
            let mut declaration = store.load_declaration().unwrap();
            declaration
                .dependencies
                .insert(PackageName(name.to_string()), version.to_string());
            store.write_declaration(&declaration).unwrap();
        }
    }

    #[test]
    fn loaded_state_builds_package_graph_from_receipt_dependencies_and_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        write_receipt(tmp.path(), "rust", "1.0", true, false);
        write_receipt(tmp.path(), "llvm", "1.0", false, false);

        let rust_keg = tmp.path().join("Cellar/rust/1.0");
        let rust_path = InstalledStateStore::receipt_path_for_keg(&rust_keg);
        let mut rust: GluInstallReceipt =
            serde_json::from_slice(&fs::read(&rust_path).unwrap()).unwrap();
        rust.install.deps = vec![PackageSelector("llvm@22".to_string())];
        fs::write(&rust_path, serde_json::to_vec(&rust).unwrap()).unwrap();

        let llvm_keg = tmp.path().join("Cellar/llvm/1.0");
        let llvm_path = InstalledStateStore::receipt_path_for_keg(&llvm_keg);
        let mut llvm: GluInstallReceipt =
            serde_json::from_slice(&fs::read(&llvm_path).unwrap()).unwrap();
        llvm.package.aliases = vec![PackageSelector("llvm@22".to_string())];
        fs::write(&llvm_path, serde_json::to_vec(&llvm).unwrap()).unwrap();

        let state = InstalledStateStore::new(Prefix(tmp.path().to_path_buf()))
            .load_installed_state()
            .unwrap();
        let graph = state.package_graph();
        let rust_key = PackageKey("package:rust".to_string());
        let llvm_key = PackageKey("package:llvm".to_string());

        assert_eq!(graph.package_count(), 2);
        assert_eq!(graph.dependencies(&rust_key)[0].requested.0, "llvm@22");
        assert_eq!(graph.dependencies(&rust_key)[0].provider, llvm_key);
        assert_eq!(graph.dependents(&llvm_key)[0].dependent, rust_key);
    }

    fn installed_pkg_with_oldnames(
        name: &str,
        oldnames: &[&str],
        version: &str,
    ) -> InstalledPackage {
        InstalledPackage {
            id: PackageId(format!("pkg:test/{name}@{version}")),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: oldnames
                .iter()
                .map(|oldname| glu_core::PackageSelector((*oldname).to_string()))
                .collect(),
            version: version.to_string(),
            revision: 0,
            keg_version: KegVersion(version.to_string()),
            keg_path: std::path::PathBuf::from(format!("/prefix/Cellar/{name}/{version}")),
            opt_path: std::path::PathBuf::from(format!("/prefix/opt/{name}")),
            keg_only: false,
            linked: true,
            deps: Vec::new(),
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }

    fn graph_pkg(name: &str, version: &str, deps: &[&str]) -> InstalledPackage {
        InstalledPackage {
            id: PackageId(format!("pkg:test/{name}@{version}")),
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
            deps: deps
                .iter()
                .map(|dep| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:test/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector((*dep).to_string()),
                })
                .collect(),
            dependency_requirements: deps
                .iter()
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
            download_bytes: None,
            installed_bytes: None,
        }
    }

    #[test]
    fn installed_state_resolves_current_name_and_oldnames_to_same_package() {
        let state = InstalledState::from_packages(vec![installed_pkg_with_oldnames(
            "bar",
            &["foo"],
            "1.0",
        )]);

        let by_current = state.find(&PackageName("bar".to_string())).unwrap();
        let exact_oldname = state.find(&PackageName("foo".to_string()));
        let resolved_oldname = state
            .resolve_selector(&PackageSelector("foo".to_string()))
            .unwrap();

        assert_eq!(by_current.name.0, "bar");
        assert!(exact_oldname.is_none());
        assert_eq!(resolved_oldname.name.0, "bar");
        assert_eq!(by_current.keg_path, resolved_oldname.keg_path);
    }

    #[test]
    fn installed_state_oldname_lookup_returns_newest_current_keg() {
        let state = InstalledState::from_packages(vec![
            installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
            installed_pkg_with_oldnames("bar", &["foo"], "2.0"),
        ]);

        let installed = state
            .resolve_selector(&PackageSelector("foo".to_string()))
            .unwrap();

        assert_eq!(installed.name.0, "bar");
        assert_eq!(installed.version, "2.0");
    }

    #[test]
    fn installed_state_fails_closed_on_ambiguous_selector_claims() {
        let err = InstalledState::from_loaded_packages(
            vec![
                installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
                installed_pkg_with_oldnames("baz", &["foo"], "1.0"),
            ],
            Prefix(std::path::PathBuf::from("/tmp/glu")),
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("installed selector 'foo' belongs to both"));
    }

    #[test]
    fn installed_exact_name_wins_over_another_packages_oldname() {
        let state = InstalledState::from_loaded_packages(
            vec![
                installed_pkg_with_oldnames("foo", &[], "1.0"),
                installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
            ],
            Prefix(std::path::PathBuf::from("/tmp/glu")),
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap();

        let selected = state
            .resolve_selector(&PackageSelector("foo".to_string()))
            .unwrap();
        assert_eq!(selected.package_key.0, "package:foo");
    }

    #[test]
    fn dangling_packages_resolve_old_dependency_names_through_installed_index() {
        let mut app = installed_pkg_with_oldnames("app", &[], "1.0");
        app.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:bar".to_string()),
            package: PackageId("pkg:test/bar@1.0".to_string()),
            requested_as: glu_core::PackageSelector("foo".to_string()),
        }];
        let state = InstalledState::from_packages_declared(
            vec![app, installed_pkg_with_oldnames("bar", &["foo"], "1.0")],
            declared_set(&["app"]),
        );

        assert!(state.dangling().is_empty());
    }

    #[test]
    fn depends_on_any_resolves_old_dependency_names_through_installed_index() {
        let mut app = installed_pkg_with_oldnames("app", &[], "1.0");
        app.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:bar".to_string()),
            package: PackageId("pkg:test/bar@1.0".to_string()),
            requested_as: glu_core::PackageSelector("foo".to_string()),
        }];
        let state = InstalledState::from_packages(vec![
            app,
            installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
        ]);
        let app = state.find(&PackageName("app".to_string())).unwrap();
        let targets = declared_set(&["bar"]);

        assert!(state.depends_on_any(app, &targets));
    }

    #[test]
    fn dangling_packages_follows_package_key_for_alias_edges() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let state =
            InstalledState::from_packages_declared(vec![rust, llvm], declared_set(&["rust"]));

        assert!(state.dangling().is_empty());
    }

    #[test]
    fn depends_on_any_follows_package_key_for_alias_edges() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let state = InstalledState::from_packages(vec![rust, llvm]);
        let rust = state.find(&PackageName("rust".to_string())).unwrap();
        let targets = declared_set(&["llvm"]);

        assert!(state.depends_on_any(rust, &targets));
    }

    #[test]
    fn selectors_share_package_key_for_forward_and_reverse_queries() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![PackageDependency {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.aliases = vec![glu_core::PackageSelector("llvm@22".to_string())];
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let state = InstalledState::from_packages(vec![rust, llvm]);

        let deps = state
            .dependency_tree_for(&PackageSelector("rust".to_string()))
            .unwrap();
        assert_eq!(deps.children[0].name, "llvm@22");
        assert_eq!(deps.children[0].version, "22.1.8_2");

        let canonical = state
            .reverse_dependents_tree(&PackageSelector("llvm".to_string()))
            .unwrap();
        let alias = state
            .reverse_dependents_tree(&PackageSelector("llvm@22".to_string()))
            .unwrap();
        assert_eq!(canonical.name, "llvm");
        assert_eq!(alias.name, "llvm");
        assert_eq!(canonical.children[0].name, "rust");
        assert_eq!(alias.children[0].name, "rust");
    }

    #[test]
    fn dangling_packages_are_unreachable_undeclared_packages() {
        fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, "1.0", &deps)
        }

        // vips (declared) -> glib -> pcre2; foo was a dep of the old vips
        // the update dropped — unreachable from the declared closure and
        // not itself declared, so it is dangling. vips/glib/pcre2 stay.
        let installed = vec![
            pkg("vips", vec!["glib"]),
            pkg("glib", vec!["pcre2"]),
            pkg("pcre2", vec![]),
            pkg("foo", vec![]),
        ];
        let state = InstalledState::from_packages_declared(installed, declared_set(&["vips"]));

        let dangling = state.dangling();
        let names: Vec<&str> = dangling.iter().map(|p| p.name.0.as_str()).collect();

        assert_eq!(names, vec!["foo"]);
    }

    #[test]
    fn dangling_packages_keeps_declared_and_still_reachable() {
        fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, "1.0", &deps)
        }

        // foo is still a dependency of a declared package (via vips) —
        // not dangling even though it is automatic. And vips itself is
        // declared, so it can never be dangling.
        let installed = vec![
            pkg("vips", vec!["glib"]),
            pkg("glib", vec!["foo"]),
            pkg("foo", vec![]),
        ];
        let state = InstalledState::from_packages_declared(installed, declared_set(&["vips"]));

        assert!(state.dangling().is_empty());
    }

    #[test]
    fn dangling_packages_keeps_only_newest_keg_of_a_reachable_name() {
        fn pkg(name: &str, version: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, version, &deps)
        }

        // vips 2.0 is declared; vips 1.0 is the keg a version bump
        // superseded, and foo is a dependency only the old vips had. The
        // single-version contract keeps only the newest keg under each
        // reachable name, so both the superseded keg and the dropped
        // dependency are dangling.
        let installed = vec![
            pkg("vips", "2.0", vec![]),
            pkg("vips", "1.0", vec!["foo"]),
            pkg("foo", "1.0", vec![]),
        ];
        let state = InstalledState::from_packages_declared(installed, declared_set(&["vips"]));

        let dangling = state.dangling();
        let names: Vec<&str> = dangling.iter().map(|p| p.name.0.as_str()).collect();

        assert_eq!(names, vec!["foo", "vips"]);
        assert_eq!(dangling[1].keg_version.0, "1.0");
    }

    #[test]
    fn dependency_tree_expands_declared_tops_cycle_guarded() {
        fn pkg(name: &str, deps: &[&str]) -> InstalledPackage {
            graph_pkg(name, "1.0", deps)
        }
        fn flat<'a>(nodes: &'a [DependencyTreeNode], out: &mut Vec<&'a str>) {
            for node in nodes {
                out.push(&node.name);
                flat(&node.children, out);
            }
        }
        let state = InstalledState::from_packages_declared(
            vec![
                pkg("app", &["liba", "libb", "tiny"]),
                pkg("tool", &["libb", "tiny"]),
                pkg("liba", &["libc"]),
                pkg("libb", &["libc"]),
                // Cycle: libc -> libb; the seen set must cut the walk.
                pkg("libc", &["libb"]),
                // Zero-dep package shared by both declared tops.
                pkg("tiny", &[]),
                pkg("unrelated", &[]),
            ],
            declared_set(&["app", "tool"]),
        );
        let tree = state.dependency_tree();
        // Tops are only the declared packages, sorted by name.
        assert_eq!(
            tree.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["app", "tool"]
        );
        // app's direct deps, sorted.
        let app = &tree[0];
        assert_eq!(
            app.children
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["liba", "libb", "tiny"]
        );
        // Dedup: the first sighting of a package (depth-first) expands its
        // subtree; later sightings — from another parent, or an ancestor on
        // the current branch (the cycle) — are marked `already_shown` and
        // not re-expanded.
        let liba = &app.children[0];
        assert_eq!(liba.children[0].name, "libc");
        let libc = &liba.children[0];
        // libb's first sighting is under libc (depth-first); its dep libc is
        // already seen → `(*)` leaf, cycle cut, no infinite recursion.
        let libb_under_libc = &libc.children[0];
        assert_eq!(libb_under_libc.name, "libb");
        assert!(!libb_under_libc.already_shown);
        assert_eq!(libb_under_libc.children[0].name, "libc");
        assert!(libb_under_libc.children[0].already_shown);
        // app's direct libb is a second sighting → marker, not re-expanded.
        let libb_under_app = &app.children[1];
        assert_eq!(libb_under_app.name, "libb");
        assert!(libb_under_app.already_shown);
        // tiny under app is the first sighting — a plain leaf, no marker.
        let tiny_under_app = &app.children[2];
        assert_eq!(tiny_under_app.name, "tiny");
        assert!(!tiny_under_app.already_shown);
        // tool's deps: libb (shared, has deps) gets the marker; tiny (shared
        // but zero-dep) is a leaf anyway — NO marker.
        let tool = &tree[1];
        assert_eq!(tool.children[0].name, "libb");
        assert!(tool.children[0].already_shown);
        assert_eq!(tool.children[1].name, "tiny");
        assert!(!tool.children[1].already_shown);
        // Automatic package not reachable from any declared top appears
        // nowhere in the tree.
        let mut names = Vec::new();
        flat(&tree, &mut names);
        assert!(!names.contains(&"unrelated"));
        // libc (automatic) shows as a nested child but never as a top.
        assert!(names.contains(&"libc"));
    }

    #[test]
    fn dependency_tree_for_walks_receipt_edges_transitively() {
        fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, "1.0", &deps)
        }

        let state = InstalledState::from_packages(vec![
            pkg("vips", vec!["glib"]),
            pkg("glib", vec!["pcre2"]),
            pkg("pcre2", vec![]),
        ]);

        let tree = state
            .dependency_tree_for(&PackageSelector("vips".to_string()))
            .unwrap();
        assert_eq!(tree.name, "vips");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "glib");
        assert_eq!(tree.children[0].children.len(), 1);
        assert_eq!(tree.children[0].children[0].name, "pcre2");
        // The edge floor is recorded for --verbose.
        assert!(tree.children[0].requires.is_some());

        assert!(state
            .dependency_tree_for(&PackageSelector("absent".to_string()))
            .is_none());
    }

    #[test]
    fn reverse_dependents_tree_walks_upward_transitively() {
        fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, "1.0", &deps)
        }

        let state = InstalledState::from_packages(vec![
            pkg("vips", vec!["glib"]),
            pkg("ffmpeg", vec!["glib"]),
            pkg("glib", vec!["pcre2"]),
            pkg("pcre2", vec![]),
        ]);

        let tree = state
            .reverse_dependents_tree(&PackageSelector("pcre2".to_string()))
            .unwrap();
        assert_eq!(tree.name, "pcre2");
        let names: Vec<&str> = tree.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["glib"]);
        let parents: Vec<&str> = tree.children[0]
            .children
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(parents, vec!["ffmpeg", "vips"]);

        // Nothing depends on the declared roots.
        assert!(state
            .reverse_dependents_tree(&PackageSelector("vips".to_string()))
            .unwrap()
            .children
            .is_empty());
    }

    #[test]
    fn dependency_tree_all_roots_dangling_packages_too() {
        fn pkg(name: &str, deps: Vec<&str>) -> InstalledPackage {
            graph_pkg(name, "1.0", &deps)
        }

        // vips (declared) -> glib; foo is dangling.
        let state = InstalledState::from_packages_declared(
            vec![
                pkg("vips", vec!["glib"]),
                pkg("glib", vec![]),
                pkg("foo", vec![]),
            ],
            declared_set(&["vips"]),
        );

        let all = state.dependency_tree_all();
        let roots: Vec<&str> = all.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(roots, vec!["foo", "vips"]);
    }

    #[test]
    fn declared_names_is_name_level_any_keg_counts() {
        fn pkg(name: &str, version: &str) -> InstalledPackage {
            graph_pkg(name, version, &[])
        }

        let state = InstalledState::from_packages_declared(
            vec![
                pkg("vips", "8.19.0"),
                pkg("vips", "8.18.5"),
                pkg("glib", "2.0"),
                pkg("foo", "1.0"),
            ],
            declared_set(&["glib", "vips"]),
        );

        let declared: Vec<String> = state
            .declared_names()
            .into_iter()
            .map(|name| name.0)
            .collect();

        assert_eq!(declared, vec!["glib".to_string(), "vips".to_string()]);
    }

    #[test]
    fn declared_lists_only_declared_kegs() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "zlib", "1.3.0", false, false);
        write_receipt(&prefix, "node", "26.7.0", true, false);
        write_receipt(&prefix, "node", "24.0.0", true, false);
        write_receipt(&prefix, "vips", "8.19.0", false, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let declared = state.declared();
        let names: Vec<&str> = declared.iter().map(|p| p.name.0.as_str()).collect();
        let versions: Vec<&str> = declared.iter().map(|p| p.version.as_str()).collect();

        // Only declared kegs, sorted like `list()` (newest first per name).
        assert_eq!(names, vec!["node", "node"]);
        assert_eq!(versions, vec!["26.7.0", "24.0.0"]);

        // Name-level view collapses both node kegs to one declared name.
        let declared_names: Vec<String> = state.declared_names().into_iter().map(|n| n.0).collect();
        assert_eq!(declared_names, vec!["node".to_string()]);
    }

    #[test]
    fn deactivated_state_resolves_current_name_and_oldnames() {
        let deactivated = declared_set(&["bar"]);
        let state = InstalledState::from_loaded_packages(
            vec![installed_pkg_with_oldnames("bar", &["foo"], "1.0")],
            Prefix(std::path::PathBuf::from("/tmp/glu")),
            BTreeSet::new(),
            deactivated,
        )
        .unwrap();

        assert!(state.is_deactivated(&PackageSelector("bar".to_string())));
        assert!(state.is_deactivated(&PackageSelector("foo".to_string())));
        assert!(!state.is_active(&PackageSelector("foo".to_string())));
        assert_eq!(
            state.deactivated_names(),
            vec![PackageName("bar".to_string())]
        );
    }

    #[test]
    fn declared_names_marks_a_name_declared_by_any_keg() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "vips", "8.19.0", false, false);
        write_receipt(&prefix, "vips", "8.18.5", true, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let names: Vec<String> = state.declared_names().into_iter().map(|n| n.0).collect();

        assert_eq!(names, vec!["vips".to_string()]);
    }

    #[test]
    fn list_is_empty_without_cellar() {
        let dir = tempfile::tempdir().unwrap();
        let state = InstalledStateStore::new(Prefix(dir.path().to_path_buf()))
            .load_installed_state()
            .unwrap();
        assert!(state.list().is_empty());
    }

    #[test]
    fn incomplete_receipts_are_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        write_receipt_status(
            &prefix,
            "node",
            "1.0",
            true,
            false,
            ReceiptStatus::Incomplete,
        );

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();

        assert!(state.list().is_empty());
    }

    #[test]
    fn list_sorts_by_name_then_version_descending() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "zlib", "1.3.0", false, false);
        write_receipt(&prefix, "node", "24.0.0", false, false);
        write_receipt(&prefix, "node", "26.7.0", true, false);
        write_receipt(&prefix, "Node", "20.0.0", false, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let list = state.list();
        let names: Vec<&str> = list.iter().map(|p| p.name.0.as_str()).collect();
        let versions: Vec<&str> = list.iter().map(|p| p.version.as_str()).collect();

        // Case-insensitive name sort, all kegs listed, newest first per name.
        assert_eq!(names, vec!["node", "node", "Node", "zlib"]);
        assert_eq!(versions, vec!["26.7.0", "24.0.0", "20.0.0", "1.3.0"]);
    }

    #[test]
    fn list_exposes_keg_only() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "icu4c@78", "78.3", false, true);
        write_receipt(&prefix, "node", "26.7.0", true, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let list = state.list();
        assert_eq!(list[0].name.0, "icu4c@78");
        assert!(list[0].keg_only);
        assert!(!list[1].keg_only);
    }

    #[test]
    fn find_returns_the_newest_keg() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "node", "24.0.0", false, false);
        write_receipt(&prefix, "node", "26.7.0", true, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let newest = state.find(&PackageName("node".to_string())).unwrap();
        assert_eq!(newest.version, "26.7.0");
        assert!(state.find(&PackageName("absent".to_string())).is_none());
    }

    #[test]
    fn newest_keg_uses_homebrew_mixed_alphanumeric_ordering() {
        let state = InstalledState::from_packages(vec![
            installed_pkg_with_oldnames("jpeg", &[], "9d"),
            installed_pkg_with_oldnames("jpeg", &[], "10"),
        ]);

        let newest = state.find(&PackageName("jpeg".to_string())).unwrap();

        assert_eq!(newest.version, "10");
    }

    #[test]
    fn names_are_unique_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().to_path_buf();
        write_receipt(&prefix, "node", "24.0.0", false, false);
        write_receipt(&prefix, "node", "26.7.0", true, false);
        write_receipt(&prefix, "zlib", "1.3.0", false, false);

        let state = InstalledStateStore::new(Prefix(prefix))
            .load_installed_state()
            .unwrap();
        let names: Vec<String> = state.names().iter().map(|n| n.0.clone()).collect();
        assert_eq!(names, vec!["node", "zlib"]);
    }
}
