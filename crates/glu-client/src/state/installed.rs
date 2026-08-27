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
    /// Direct package-key adjacency from the newest installed keg per package.
    outgoing: BTreeMap<PackageKey, Vec<glu_core::RuntimeDependencyRequirement>>,
    /// Reverse package-key adjacency, built once with the snapshot.
    incoming: BTreeMap<PackageKey, Vec<PackageKey>>,
    declared_names: BTreeSet<PackageName>,
    deactivated_names: BTreeSet<PackageName>,
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
        for package in packages {
            let current = package.name.clone();
            insert_selector_mapping(
                &mut selector_index,
                PackageSelector(current.0.clone()),
                &package.package_key,
            )?;
            for selector in package.aliases.iter().chain(&package.oldnames) {
                insert_selector_mapping(
                    &mut selector_index,
                    selector.clone(),
                    &package.package_key,
                )?;
            }
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
        let mut outgoing = BTreeMap::new();
        let mut incoming: BTreeMap<PackageKey, Vec<PackageKey>> = BTreeMap::new();
        for (package_key, kegs) in &by_key {
            let Some(package) = kegs.first() else {
                continue;
            };
            outgoing.insert(package_key.clone(), package.deps.clone());
            for dep in &package.deps {
                if !by_key.contains_key(&dep.package_key) {
                    bail!(
                        "installed package {} references missing package key {}",
                        package.name.0,
                        dep.package_key.0
                    );
                }
                incoming
                    .entry(dep.package_key.clone())
                    .or_default()
                    .push(package_key.clone());
            }
        }
        for dependents in incoming.values_mut() {
            dependents.sort();
            dependents.dedup();
        }
        Ok(Self {
            prefix,
            by_name,
            by_key,
            selector_index,
            outgoing,
            incoming,
            declared_names,
            deactivated_names,
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

    pub fn resolve_selector(&self, selector: &PackageSelector) -> Option<&InstalledPackage> {
        self.selector_index
            .get(selector)
            .and_then(|key| self.find_by_key(key))
    }

    pub fn dependencies(&self, key: &PackageKey) -> &[glu_core::RuntimeDependencyRequirement] {
        self.outgoing.get(key).map_or(&[], Vec::as_slice)
    }

    pub fn dependent_keys(&self, key: &PackageKey) -> &[PackageKey] {
        self.incoming.get(key).map_or(&[], Vec::as_slice)
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
        let names = self.declared_name_set();
        self.list()
            .into_iter()
            .filter(|package| names.contains(&package.name))
            .collect()
    }

    /// Names of declared packages, per the declaration file. Name-level,
    /// matching the reachability semantics of `dangling_packages`.
    pub fn declared_names(&self) -> Vec<PackageName> {
        self.declared_name_set().into_iter().collect()
    }

    /// Names marked deactivated in the declaration, sorted.
    pub fn deactivated_names(&self) -> Vec<PackageName> {
        self.deactivated_names.iter().cloned().collect()
    }

    /// Whether `selector` resolves to a currently deactivated package.
    pub fn is_deactivated(&self, selector: &PackageSelector) -> bool {
        self.resolve_selector(selector)
            .is_some_and(|package| self.deactivated_names.contains(&package.name))
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

    /// Packages no longer required by anything declared: see
    /// `dangling_packages`.
    pub fn dangling(&self) -> Vec<InstalledPackage> {
        let installed = self.list();
        let declared = self.declared_name_set();
        dangling_packages(&installed, &declared)
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
        let roots = self.declared();
        let mut seen: BTreeSet<PackageKey> = BTreeSet::new();
        roots
            .iter()
            .map(|root| {
                let name = root.name.0.clone();
                seen.insert(root.package_key.clone());
                DependencyTreeNode {
                    name,
                    version: root.keg_version.0.clone(),
                    children: dependency_tree_children(root, self, &mut seen),
                    already_shown: false,
                    requires: None,
                }
            })
            .collect()
    }

    /// Like `dependency_tree`, but also roots every dangling package —
    /// installed, not declared, and unreachable from anything declared — so
    /// the tree covers *everything* installed (`glu ls --all --tree`).
    pub fn dependency_tree_all(&self) -> Vec<DependencyTreeNode> {
        let mut nodes = self.dependency_tree();
        let mut seen: BTreeSet<PackageKey> = self
            .declared()
            .into_iter()
            .map(|package| package.package_key)
            .collect();
        for package in self.dangling() {
            if !seen.insert(package.package_key.clone()) {
                continue;
            }
            let Some(installed) = self.find_by_key(&package.package_key) else {
                continue;
            };
            nodes.push(DependencyTreeNode {
                name: package.name.0.clone(),
                version: package.keg_version.0.clone(),
                children: dependency_tree_children(installed, self, &mut seen),
                already_shown: false,
                requires: None,
            });
        }
        nodes.sort_by_key(|node| node.name.to_lowercase());
        nodes
    }

    /// `glu deps`: the dependency subtree of one installed package (the
    /// newest keg of that name) — receipts only, so it answers "what does
    /// this pull in on my system" offline and exact.
    pub fn dependency_tree_for(&self, selector: &PackageSelector) -> Option<DependencyTreeNode> {
        let root = self.resolve_selector(selector)?;
        let mut seen: BTreeSet<PackageKey> = BTreeSet::new();
        seen.insert(root.package_key.clone());
        Some(DependencyTreeNode {
            name: root.name.0.clone(),
            version: root.keg_version.0.clone(),
            children: dependency_tree_children(root, self, &mut seen),
            already_shown: false,
            requires: None,
        })
    }

    /// `glu why`: the reverse dependency tree of one installed package —
    /// the target at the root, its direct dependents as children, and their
    /// dependents recursed (cycle-guarded). `None` when nothing installed
    /// depends on it.
    pub fn reverse_dependents_tree(
        &self,
        selector: &PackageSelector,
    ) -> Option<DependencyTreeNode> {
        let root = self.resolve_selector(selector)?;
        let mut seen: BTreeSet<PackageKey> = BTreeSet::new();
        seen.insert(root.package_key.clone());
        Some(DependencyTreeNode {
            name: root.name.0.clone(),
            version: root.keg_version.0.clone(),
            children: reverse_dependents_children(self, &root.package_key, &mut seen),
            already_shown: false,
            requires: None,
        })
    }
}

/// One node of the `glu ls --all` dependency tree: a package (the newest
/// installed keg of that name), its children, and whether it was already
/// expanded elsewhere (its subtree is shown above, so it renders as a leaf
/// with a `(*)` marker).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DependencyTreeNode {
    pub name: String,
    pub version: String,
    pub children: Vec<DependencyTreeNode>,
    pub already_shown: bool,
    /// The requirement the parent's edge places on this package, rendered
    /// under `--verbose` (e.g. `>= 2.84.3`). `None` for roots.
    pub requires: Option<String>,
}

/// Children of `package` in the `glu ls --all` tree: its declared deps (from
/// the receipt), each carrying the newest installed keg's version. A dep
/// already in `seen` — expanded from another parent, or an ancestor on the
/// current branch (a cycle) — is marked `already_shown` and not re-expanded;
/// otherwise it is inserted and its subtree recursed.
fn dependency_tree_children(
    package: &InstalledPackage,
    state: &InstalledState,
    seen: &mut BTreeSet<PackageKey>,
) -> Vec<DependencyTreeNode> {
    let mut deps: Vec<_> = state.dependencies(&package.package_key).iter().collect();
    deps.sort_by_key(|dep| dep.package_key.0.to_lowercase());
    let mut children = Vec::new();
    for dep in deps {
        let newest = state.find_by_key(&dep.package_key);
        let mut child = DependencyTreeNode {
            name: newest
                .map(|package| package.name.0.clone())
                .unwrap_or_else(|| dep.requested_as.0.clone()),
            version: newest.map(|p| p.keg_version.0.clone()).unwrap_or_default(),
            children: Vec::new(),
            already_shown: false,
            requires: format_requires(&dep.requires),
        };
        if let Some(dep_package) = newest {
            if seen.insert(dep.package_key.clone()) {
                child.children = dependency_tree_children(dep_package, state, seen);
            } else if !dep_package.deps.is_empty() {
                child.already_shown = true;
            }
        }
        children.push(child);
    }
    children
}

/// The `requires` floor of a dependency edge, as a display string —
/// `>= 2.84.3` or `>= 2.84.3_1` for a revisioned floor.
pub(crate) fn format_requires(requires: &glu_core::DependencyRequires) -> Option<String> {
    let mut floor = format!(">= {}", requires.version);
    if requires.revision > 0 {
        floor.push_str(&format!("_{}", requires.revision));
    }
    Some(floor)
}

/// Children of `package` in the reverse dependency tree (`glu why`): every
/// installed package that directly depends on `name`, sorted by name. A
/// dependent already on the ancestry path (a cycle) is marked and not
/// re-expanded.
fn reverse_dependents_children(
    state: &InstalledState,
    package_key: &PackageKey,
    seen: &mut BTreeSet<PackageKey>,
) -> Vec<DependencyTreeNode> {
    let mut parents: Vec<&InstalledPackage> = state
        .dependent_keys(package_key)
        .iter()
        .filter_map(|key| state.find_by_key(key))
        .collect();
    parents.sort_by_key(|parent| parent.name.0.to_lowercase());
    let mut children = Vec::new();
    for parent in parents {
        let mut node = DependencyTreeNode {
            name: parent.name.0.clone(),
            version: parent.keg_version.0.clone(),
            children: Vec::new(),
            already_shown: false,
            requires: None,
        };
        if seen.insert(parent.package_key.clone()) {
            node.children = reverse_dependents_children(state, &parent.package_key, seen);
        } else if has_dependents(state, &parent.package_key) {
            node.already_shown = true;
        }
        children.push(node);
    }
    children
}

fn has_dependents(state: &InstalledState, package_key: &PackageKey) -> bool {
    !state.dependent_keys(package_key).is_empty()
}

/// Whether `start` depends, directly or transitively, on any selected
/// installed package. Selector resolution happens once up front; the walk is
/// exclusively over stable package keys, so independently updated concrete
/// package IDs do not break reachability.
pub(crate) fn depends_on_any(
    installed: &[InstalledPackage],
    start: &InstalledPackage,
    target_names: &BTreeSet<PackageName>,
) -> bool {
    let Some(selector_index) = installed_selector_index(installed) else {
        return true;
    };
    let key_index = installed_key_index(installed);
    let target_keys: BTreeSet<PackageKey> = target_names
        .iter()
        .filter_map(|name| selector_index.get(&PackageSelector(name.0.clone())))
        .cloned()
        .collect();

    let mut visited: BTreeSet<PackageKey> = BTreeSet::new();
    let mut stack: Vec<PackageKey> = start
        .deps
        .iter()
        .map(|dep| dep.package_key.clone())
        .collect();
    while let Some(package_key) = stack.pop() {
        if target_keys.contains(&package_key) {
            return true;
        }
        if !visited.insert(package_key.clone()) {
            continue;
        }
        let Some(package) = key_index
            .get(&package_key)
            .and_then(|ix| installed.get(*ix))
        else {
            continue;
        };
        stack.extend(package.deps.iter().map(|dep| dep.package_key.clone()));
    }
    false
}

fn installed_selector_index(
    installed: &[InstalledPackage],
) -> Option<HashMap<PackageSelector, PackageKey>> {
    let mut index = HashMap::new();
    for package in installed {
        let current = PackageSelector(package.name.0.clone());
        if !insert_slice_selector_mapping(&mut index, current, &package.package_key) {
            return None;
        }
        for selector in package.aliases.iter().chain(&package.oldnames) {
            if !insert_slice_selector_mapping(&mut index, selector.clone(), &package.package_key) {
                return None;
            }
        }
    }
    Some(index)
}

fn installed_key_index(installed: &[InstalledPackage]) -> HashMap<PackageKey, usize> {
    let mut index = HashMap::new();
    for (ix, package) in installed.iter().enumerate() {
        index.entry(package.package_key.clone()).or_insert(ix);
    }
    index
}

fn insert_slice_selector_mapping(
    index: &mut HashMap<PackageSelector, PackageKey>,
    selector: PackageSelector,
    package_key: &PackageKey,
) -> bool {
    if let Some(existing) = index.get(&selector) {
        return existing == package_key;
    }
    index.insert(selector, package_key.clone());
    true
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
    compare_versions(&a.version, &b.version).then(a.revision.cmp(&b.revision))
}

#[cfg(test)]
fn declared_set(names: &[&str]) -> BTreeSet<PackageName> {
    names
        .iter()
        .map(|name| PackageName((*name).to_string()))
        .collect()
}

/// Packages that are no longer required by anything, or that a version bump
/// superseded:
/// - not reachable — directly or transitively — from any declared
///   package's dependency edges (the `declared` seed comes from the
///   declaration file; see `InstalledState::declared_names`), or
/// - an older keg for a reachable package identity. Declaration selectors are
///   resolved once, then reachability uses `PackageKey`. The single-version
///   contract ("One version per package", state-model.md) keeps only the newest
///   keg per identity — the keg dependency satisfaction and edge walking use
///   (`installed` must be sorted newest-first, as `list()` and
///   `simulate_post_install_state` produce). The keg a version bump
///   replaced is therefore dangling too.
///
/// Sync removes these after a version bump drops a dependency (its new
/// bottle no longer declares it) or replaces an installed version;
/// removing them is safe because nothing installed links to them by path —
/// the same stable-path convention that makes version bumps safe for
/// dependents (see docs/explanation/state-model.md, Sync section). Returns every
/// dangling keg, sorted like `list()`.
pub(crate) fn dangling_packages(
    installed: &[InstalledPackage],
    declared: &BTreeSet<PackageName>,
) -> Vec<InstalledPackage> {
    let Some(selector_index) = installed_selector_index(installed) else {
        // If two installed packages claim the same selector, removing anything
        // based on reachability would be a guess. Loaded InstalledState fails
        // earlier; this slice helper is conservative.
        return Vec::new();
    };

    let key_index = installed_key_index(installed);
    let mut reachable: BTreeSet<PackageKey> = BTreeSet::new();
    let mut stack: Vec<PackageKey> = Vec::new();
    for name in declared {
        let Some(package_key) = selector_index.get(&PackageSelector(name.0.clone())) else {
            continue;
        };
        if reachable.insert(package_key.clone()) {
            stack.push(package_key.clone());
        }
    }

    while let Some(package_key) = stack.pop() {
        let Some(package) = key_index
            .get(&package_key)
            .and_then(|ix| installed.get(*ix))
        else {
            continue;
        };
        for dep in &package.deps {
            if reachable.insert(dep.package_key.clone()) {
                stack.push(dep.package_key.clone());
            }
        }
    }

    // Every reachable identity keeps exactly its newest concrete keg.
    let retained: BTreeSet<PackageId> = reachable
        .iter()
        .filter_map(|key| key_index.get(key))
        .filter_map(|ix| installed.get(*ix))
        .map(|package| package.id.clone())
        .collect();

    let mut dangling: Vec<InstalledPackage> = installed
        .iter()
        .filter(|package| !retained.contains(&package.id))
        .cloned()
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

pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let a_parts = version_parts(a);
    let b_parts = version_parts(b);

    for (a, b) in a_parts.iter().zip(b_parts.iter()) {
        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }

    a_parts.len().cmp(&b_parts.len())
}

fn version_parts(version: &str) -> Vec<&str> {
    version
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
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
    use glu_core::{
        ArtifactId, DependencyRequires, KegVersion, PackageId, RuntimeDependencyRequirement,
    };
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
                .map(|dep| RuntimeDependencyRequirement {
                    package_key: glu_core::PackageKey(format!("package:{dep}")),
                    package: PackageId(format!("pkg:test/{dep}@1.0")),
                    requested_as: glu_core::PackageSelector((*dep).to_string()),
                    requires: DependencyRequires {
                        version: "1.0".to_string(),
                        revision: 0,
                    },
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
    fn installed_state_fails_closed_when_oldname_collides_with_current_name() {
        let err = InstalledState::from_loaded_packages(
            vec![
                installed_pkg_with_oldnames("foo", &[], "1.0"),
                installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
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
    fn dangling_packages_resolve_old_dependency_names_through_installed_index() {
        let mut app = installed_pkg_with_oldnames("app", &[], "1.0");
        app.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:bar".to_string()),
            package: PackageId("pkg:test/bar@1.0".to_string()),
            requested_as: glu_core::PackageSelector("foo".to_string()),
            requires: DependencyRequires {
                version: "1.0".to_string(),
                revision: 0,
            },
        }];
        let installed = vec![app, installed_pkg_with_oldnames("bar", &["foo"], "1.0")];
        let declared = declared_set(&["app"]);

        let dangling = dangling_packages(&installed, &declared);

        assert!(dangling.is_empty());
    }

    #[test]
    fn depends_on_any_resolves_old_dependency_names_through_installed_index() {
        let mut app = installed_pkg_with_oldnames("app", &[], "1.0");
        app.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:bar".to_string()),
            package: PackageId("pkg:test/bar@1.0".to_string()),
            requested_as: glu_core::PackageSelector("foo".to_string()),
            requires: DependencyRequires {
                version: "1.0".to_string(),
                revision: 0,
            },
        }];
        let installed = vec![
            app.clone(),
            installed_pkg_with_oldnames("bar", &["foo"], "1.0"),
        ];
        let targets = declared_set(&["bar"]);

        assert!(depends_on_any(&installed, &app, &targets));
    }

    #[test]
    fn dangling_packages_follows_package_key_for_alias_edges() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
            requires: DependencyRequires {
                version: "22.1.8".to_string(),
                revision: 0,
            },
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let installed = vec![rust, llvm];
        let declared = declared_set(&["rust"]);

        let dangling = dangling_packages(&installed, &declared);

        assert!(dangling.is_empty());
    }

    #[test]
    fn depends_on_any_follows_package_key_for_alias_edges() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
            requires: DependencyRequires {
                version: "22.1.8".to_string(),
                revision: 0,
            },
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let installed = vec![rust.clone(), llvm];
        let targets = declared_set(&["llvm"]);

        assert!(depends_on_any(&installed, &rust, &targets));
    }

    #[test]
    fn selectors_share_package_key_for_forward_and_reverse_queries() {
        let mut rust = installed_pkg_with_oldnames("rust", &[], "1.98.0");
        rust.deps = vec![RuntimeDependencyRequirement {
            package_key: glu_core::PackageKey("package:llvm".to_string()),
            package: PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string()),
            requested_as: glu_core::PackageSelector("llvm@22".to_string()),
            requires: DependencyRequires {
                version: "22.1.8".to_string(),
                revision: 0,
            },
        }];
        let mut llvm = installed_pkg_with_oldnames("llvm", &[], "22.1.8");
        llvm.id = PackageId("pkg:homebrew/core/llvm@22.1.8_2".to_string());
        llvm.aliases = vec![glu_core::PackageSelector("llvm@22".to_string())];
        llvm.keg_version = KegVersion("22.1.8_2".to_string());
        let state = InstalledState::from_packages(vec![rust, llvm]);

        let deps = state
            .dependency_tree_for(&PackageSelector("rust".to_string()))
            .unwrap();
        assert_eq!(deps.children[0].name, "llvm");
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

        let dangling = dangling_packages(&installed, &declared_set(&["vips"]));
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

        let dangling = dangling_packages(&installed, &declared_set(&["vips"]));

        assert!(dangling.is_empty());
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

        let dangling = dangling_packages(&installed, &declared_set(&["vips"]));
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
