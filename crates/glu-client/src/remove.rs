use crate::link::unlink::remove_keg;
use crate::state::installed::InstalledState;
use crate::state::snapshot::StateSnapshot;
use crate::state::store::InstalledStateStore;
use crate::state::Declaration;
use anyhow::{bail, Result};
use glu_core::{InstalledPackage, KegVersion, PackageId, PackageName, Prefix};
use std::collections::BTreeSet;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RemovedPackage {
    pub name: PackageName,
    pub keg_version: KegVersion,
}

mod mutable_files;

pub use mutable_files::{
    execute_mutable_file_cleanup, plan_mutable_file_cleanup, MutableFileArea,
    MutableFileCleanupPlan, MutableFileCleanupResult, MutableFilePlan, RetainedMutableFile,
    RetainedMutableFileReason,
};

#[derive(Debug)]
pub struct RemovalResult {
    pub removed: Vec<RemovedPackage>,
    pub mutable_files: MutableFileCleanupResult,
}

/// A declared package that `glu rm` removed from the declaration but kept
/// installed because other declared packages still need it.
#[derive(Debug)]
pub struct KeptDeclaredPackage {
    pub package: InstalledPackage,
    pub needed_by: Vec<String>,
}

/// The result of planning a removal. Planning may first discard interrupted
/// install trash; execution removes from the declaration, and sync (here:
/// the plan + execute split) reconciles disk state: the named targets plus
/// everything that becomes dangling once they are gone are removed;
/// declared targets that other declared packages still need are demoted and
/// kept. The CLI asks for confirmation when an actual removal's package ID
/// was not among the concrete packages selected by the user's selectors
/// (docs/reference/cli-behavior.md, Confirmation policy).
#[derive(Debug)]
pub struct RemovalPlan {
    /// The kegs the selectors resolved to — what the user explicitly named.
    pub named: Vec<InstalledPackage>,
    /// Every keg that would actually be removed: the named targets that
    /// nothing else declared needs, plus every package that becomes
    /// dangling once those are gone. Sorted like `InstalledState::list`.
    pub to_remove: Vec<InstalledPackage>,
    /// Declared targets that other declared packages still need: kept
    /// installed and reported.
    pub kept: Vec<KeptDeclaredPackage>,
    pub mutable_files: MutableFileCleanupPlan,
    post_declaration: Declaration,
}

impl RemovalPlan {
    /// Whether execution would remove at least one concrete package that the
    /// user's selectors did not resolve to. Identity, rather than vector
    /// cardinality, is the confirmation boundary: a named package may be kept
    /// while a different package becomes dangling, leaving equal-length lists
    /// that do not contain the same packages.
    pub fn has_unnamed_removals(&self) -> bool {
        let named_ids: BTreeSet<PackageId> = self
            .named
            .iter()
            .map(|package| package.id.clone())
            .collect();
        self.to_remove
            .iter()
            .any(|package| !named_ids.contains(&package.id))
    }
}

/// Plans a removal against the installed state on disk. Loads the receipt
/// snapshot, resolves the selectors, and computes the removal set and the
/// kept set. All errors (unresolved selector, target that is neither declared
/// nor dangling) happen here, before the requested removal is touched.
pub fn plan_removal(prefix: &Prefix, selectors: Vec<String>) -> Result<RemovalPlan> {
    let snapshot = StateSnapshot::load(prefix)?;
    let mut plan = plan_removal_from_snapshot(&snapshot, selectors)?;
    plan.mutable_files =
        plan_mutable_file_cleanup(prefix, &snapshot.installed.list(), &plan.to_remove)?;
    Ok(plan)
}

/// The removal-plan logic over an already-loaded installed list and the
/// declared name set — shared with tests so the selector/demote/dangling
/// decisions are exercised without a filesystem.
fn plan_removal_from_snapshot(
    snapshot: &StateSnapshot,
    selectors: Vec<String>,
) -> Result<RemovalPlan> {
    plan_removal_from_parts(&snapshot.installed, snapshot.declaration.clone(), selectors)
}

#[cfg(test)]
fn plan_removal_from_installed(
    installed: &[InstalledPackage],
    declared: &BTreeSet<PackageName>,
    selectors: Vec<String>,
) -> Result<RemovalPlan> {
    let mut declaration = Declaration::default();
    for package in installed {
        if declared.contains(&package.name) {
            declaration
                .dependencies
                .insert(package.name.clone(), package.keg_version.0.clone());
        }
    }
    let state = InstalledState::from_loaded_packages(
        installed.to_vec(),
        Prefix(PathBuf::from("/tmp/glu")),
        declared.clone(),
        BTreeSet::new(),
    )?;
    plan_removal_from_parts(&state, declaration, selectors)
}

fn plan_removal_from_parts(
    state: &InstalledState,
    mut post_declaration: Declaration,
    selectors: Vec<String>,
) -> Result<RemovalPlan> {
    let installed = state.list();
    let declared: BTreeSet<PackageName> = post_declaration.names();
    let current_dangling: BTreeSet<PackageName> = state
        .dangling_for_declared(&declared)
        .into_iter()
        .map(|package| package.name)
        .collect();

    let mut targets: Vec<InstalledPackage> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for selector in &selectors {
        let matches = resolve_selector(&installed, selector);
        if matches.is_empty() {
            missing.push(selector.as_str());
        } else {
            targets.extend(matches.into_iter().cloned());
        }
    }

    if !missing.is_empty() {
        return Err(crate::error::NotInstalledError::packages(
            missing
                .iter()
                .map(|name| PackageName((*name).to_string()))
                .collect(),
            None,
        )
        .into());
    }

    let mut seen: BTreeSet<PackageId> = BTreeSet::new();
    targets.retain(|target| seen.insert(target.id.clone()));

    // Split targets: those still needed (transitively) by a declared
    // package that isn't being removed get demoted and kept; the rest are
    // removed together with whatever becomes dangling once they are gone.
    let remaining_declared: BTreeSet<PackageName> = declared
        .iter()
        .filter(|name| !targets.iter().any(|target| &target.name == *name))
        .cloned()
        .collect();

    let mut kept: Vec<KeptDeclaredPackage> = Vec::new();
    for target in &targets {
        let mut needed_by: BTreeSet<PackageName> = BTreeSet::new();
        for name in &remaining_declared {
            let Some(package) = state.resolve_selector(&glu_core::PackageSelector(name.0.clone()))
            else {
                continue;
            };
            let target_names = BTreeSet::from([target.name.clone()]);
            if state.depends_on_any(package, &target_names) {
                needed_by.insert(package.name.clone());
            }
        }

        // `rm` removes from the declaration. A target that is neither
        // declared nor already dangling is still needed by a declared
        // package: the model has no operation that leaves a declared
        // package with a missing dependency. A dangling target (e.g. a
        // dependency a recent update dropped) is removable — the manual
        // form of autoremove for that package's clump.
        if !declared.contains(&target.name) && !current_dangling.contains(&target.name) {
            let needing: Vec<&str> = needed_by.iter().map(|name| name.0.as_str()).collect();
            bail!(
                "{} is installed as a dependency of {} and can't be removed. It will be removed automatically once nothing needs it.",
                target.name.0,
                needing.join(", ")
            );
        }

        if !needed_by.is_empty() {
            kept.push(KeptDeclaredPackage {
                package: target.clone(),
                needed_by: needed_by.into_iter().map(|name| name.0).collect(),
            });
        }
    }

    // Once named targets leave the declaration, the graph's reachability
    // result is the complete removal set: unreachable targets, their newly
    // dangling dependencies, and superseded kegs. Targets still reached by a
    // remaining declared root are the `kept` set above.
    for target in &targets {
        post_declaration.dependencies.remove(&target.name);
    }
    let to_remove = state.dangling_for_declared(&post_declaration.names());

    Ok(RemovalPlan {
        named: targets,
        to_remove,
        kept,
        mutable_files: MutableFileCleanupPlan::default(),
        post_declaration,
    })
}

/// Executes a planned removal: updates the declaration and removes every keg
/// in `plan.to_remove`. Returns the removed packages, one per keg, in plan
/// order.
pub fn execute_removal(prefix: &Prefix, plan: &RemovalPlan) -> Result<Vec<RemovedPackage>> {
    Ok(execute_removal_with_config(prefix, plan, false)?.removed)
}

pub fn execute_removal_with_config(
    prefix: &Prefix,
    plan: &RemovalPlan,
    remove_modified: bool,
) -> Result<RemovalResult> {
    // `rm` removes from the declaration first: the named targets leave
    // `glu.json` whether they are removed or demoted-and-kept. Written
    // before the keg surgery so an interrupted removal still reflects the
    // intent change (a keg left behind becomes dangling and is cleaned up
    // by the next sync).
    execute_package_removal(
        prefix,
        &plan.to_remove,
        Some(&plan.post_declaration),
        &plan.mutable_files,
        remove_modified,
    )
}

/// The dangling packages of a prefix: installed, not declared, and not
/// reachable from anything declared — the repair set for bad state (a
/// manually corrupted prefix, an interrupted sync). In a healthy system
/// every command already removed these, so this is normally empty. Sorted
/// like `InstalledState::list`. See docs/explanation/state-model.md, autoremove.
pub fn plan_autoremove(prefix: &Prefix) -> Result<Vec<InstalledPackage>> {
    Ok(StateSnapshot::load(prefix)?.installed.dangling())
}

/// Removes every package in `dangling`, in order. Returns the removed
/// packages, one per keg.
pub fn execute_autoremove(
    prefix: &Prefix,
    dangling: &[InstalledPackage],
) -> Result<Vec<RemovedPackage>> {
    let installed = StateSnapshot::load(prefix)?.installed.list();
    let cleanup = plan_mutable_file_cleanup(prefix, &installed, dangling)?;
    Ok(execute_package_removal(prefix, dangling, None, &cleanup, false)?.removed)
}

fn execute_package_removal(
    prefix: &Prefix,
    packages: &[InstalledPackage],
    planned_declaration: Option<&Declaration>,
    cleanup: &MutableFileCleanupPlan,
    remove_modified: bool,
) -> Result<RemovalResult> {
    let store = InstalledStateStore::new(prefix.clone());
    let mut declaration = match planned_declaration {
        Some(declaration) => declaration.clone(),
        None => store.load_declaration()?,
    };
    let pruned = prune_deactivated_entries_for_removed_names(&store, &mut declaration, packages)?;
    if planned_declaration.is_some() || pruned {
        store.write_declaration(&declaration)?;
    }

    let removed = remove_installed_packages_raw(prefix, packages)?;
    let mutable_files = execute_mutable_file_cleanup(prefix, cleanup, remove_modified)?;
    Ok(RemovalResult {
        removed,
        mutable_files,
    })
}

fn prune_deactivated_entries_for_removed_names(
    store: &InstalledStateStore,
    declaration: &mut Declaration,
    packages: &[InstalledPackage],
) -> Result<bool> {
    if packages.is_empty() || declaration.deactivated.is_empty() {
        return Ok(false);
    }

    let remove_ids: BTreeSet<PackageId> =
        packages.iter().map(|package| package.id.clone()).collect();
    let remaining_names: BTreeSet<PackageName> = store
        .load_installed_state()?
        .list()
        .into_iter()
        .filter(|package| !remove_ids.contains(&package.id))
        .map(|package| package.name)
        .collect();

    let mut changed = false;
    for package in packages {
        if !remaining_names.contains(&package.name) {
            changed |= declaration.deactivated.remove(&package.name).is_some();
        }
    }
    Ok(changed)
}

pub(crate) fn remove_installed_packages(
    prefix: &Prefix,
    packages: &[InstalledPackage],
) -> Result<Vec<RemovedPackage>> {
    let installed = StateSnapshot::load(prefix)?.installed.list();
    let cleanup = plan_mutable_file_cleanup(prefix, &installed, packages)?;
    let removed = remove_installed_packages_raw(prefix, packages)?;
    execute_mutable_file_cleanup(prefix, &cleanup, false)?;
    Ok(removed)
}

pub(crate) fn remove_installed_packages_raw(
    prefix: &Prefix,
    packages: &[InstalledPackage],
) -> Result<Vec<RemovedPackage>> {
    let mut removed = Vec::new();
    for package in packages {
        remove_keg(prefix, &package.name, &package.keg_path)?;
        removed.push(RemovedPackage {
            name: package.name.clone(),
            keg_version: package.keg_version.clone(),
        });
    }
    Ok(removed)
}

/// Three-tier resolution against installed state, most specific match
/// winning at each tier:
///
/// 1. `selector` is a literal installed package name — every keg under it.
///    Handles names that legitimately contain `@` themselves (versioned
///    package names like `postgresql@14`, `python@3.11`), which a naive
///    split would otherwise misparse as "postgresql at version 14".
/// 2. Split on the last `@`; the suffix matches the bare `version` field,
///    any revision (`vips@8.19.0`) — every keg at that version. Checked
///    before tier 3 deliberately: when only a revision-0 keg exists its
///    `keg_version` *equals* the bare version string, so this tier already
///    subsumes that common case, and — the reason it must come first —
///    checking `keg_version` before `version` would let a coincidental
///    revision-0 match short-circuit before a same-version, different-
///    revision sibling (e.g. `8.19.0` alongside `8.19.0_1`) is found. A
///    revision-only bottle rebuild can leave exactly that pair installed at
///    once, and a user typing the version they actually think in shouldn't
///    need to know a revision suffix exists.
/// 3. The suffix exactly matches `keg_version` (`vips@8.19.0_1`) — that one
///    keg. Only reached when tier 2 found nothing, i.e. the suffix isn't a
///    bare version at all (a `version` field never contains the revision
///    suffix), so this is exactly the "user explicitly named a revision"
///    case.
fn resolve_selector<'a>(
    installed: &'a [InstalledPackage],
    selector: &str,
) -> Vec<&'a InstalledPackage> {
    let literal: Vec<&InstalledPackage> = installed
        .iter()
        .filter(|package| package.name.0 == selector)
        .collect();
    if !literal.is_empty() {
        return literal;
    }

    let Some(at) = selector.rfind('@') else {
        return Vec::new();
    };
    let name = &selector[..at];
    let suffix = &selector[at + 1..];

    let by_version: Vec<&InstalledPackage> = installed
        .iter()
        .filter(|package| package.name.0 == name && package.version == suffix)
        .collect();
    if !by_version.is_empty() {
        return by_version;
    }

    installed
        .iter()
        .filter(|package| package.name.0 == name && package.keg_version.0 == suffix)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store::InstalledStateStore;
    use crate::state::{
        GluInstallReceipt, ReceiptArtifact, ReceiptInstall, ReceiptPackage, ReceiptPaths,
        ReceiptStatus,
    };
    use glu_core::{ArtifactId, PackageDependency};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn installed(
        id: &str,
        name: &str,
        version: &str,
        revision: u32,
        deps: Vec<(&str, &str)>,
    ) -> InstalledPackage {
        let keg_version = if revision == 0 {
            version.to_string()
        } else {
            format!("{version}_{revision}")
        };
        InstalledPackage {
            id: PackageId(id.to_string()),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: version.to_string(),
            revision,
            keg_version: KegVersion(keg_version.clone()),
            keg_path: PathBuf::from(format!("/prefix/Cellar/{name}/{keg_version}")),
            opt_path: PathBuf::from(format!("/prefix/opt/{name}")),
            exposure: glu_core::Exposure::Global,
            linked: true,
            deps: deps
                .into_iter()
                .map(|(dep_name, dep_id)| PackageDependency {
                    package_key: glu_core::PackageKey(format!("package:{dep_name}")),
                    package: PackageId(dep_id.to_string()),
                    requested_as: glu_core::PackageSelector(dep_name.to_string()),
                })
                .collect(),
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }

    fn declared_names(names: &[&str]) -> BTreeSet<PackageName> {
        names
            .iter()
            .map(|name| PackageName((*name).to_string()))
            .collect()
    }

    fn write_fixture_receipt(
        prefix: &Prefix,
        name: &str,
        version: &str,
        revision: u32,
        declared: bool,
        dependencies: Vec<&str>,
    ) {
        let keg_version = if revision == 0 {
            version.to_string()
        } else {
            format!("{version}_{revision}")
        };
        let keg = prefix.0.join("Cellar").join(name).join(&keg_version);
        fs::create_dir_all(keg.join(".glu")).unwrap();
        let receipt = GluInstallReceipt {
            schema: "glu.install-receipt.v1".to_string(),
            status: ReceiptStatus::Complete,
            package: ReceiptPackage {
                id: PackageId(format!("pkg:test/{name}@{keg_version}")),
                package_key: glu_core::PackageKey(format!("package:{name}")),
                name: PackageName(name.to_string()),
                aliases: Vec::new(),
                oldnames: Vec::new(),
                version: version.to_string(),
                revision,
                keg_version: KegVersion(keg_version),
            },
            artifact: ReceiptArtifact {
                id: ArtifactId("art:test".to_string()),
                sha256: "deadbeef".to_string(),
                bottle_tag: "arm64_test".to_string(),
                cellar: prefix.0.join("Cellar").to_string_lossy().to_string(),
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
                exposure: glu_core::Exposure::Global,
                linked: false,
                link_overwrite: Vec::new(),
                deps: dependencies
                    .into_iter()
                    .map(|dependency| glu_core::PackageSelector(dependency.to_string()))
                    .collect(),
                dependency_requirements: Default::default(),
            },
        };
        fs::write(
            InstalledStateStore::receipt_path_for_keg(&keg),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        if declared {
            let store = InstalledStateStore::new(prefix.clone());
            let mut declaration = store.load_declaration().unwrap();
            declaration.dependencies.insert(
                PackageName(name.to_string()),
                receipt.package.keg_version.0.clone(),
            );
            store.write_declaration(&declaration).unwrap();
        }
    }

    fn mark_deactivated(prefix: &Prefix, names: &[&str]) {
        let store = InstalledStateStore::new(prefix.clone());
        let mut declaration = store.load_declaration().unwrap();
        for name in names {
            declaration
                .deactivated
                .insert(PackageName((*name).to_string()), true);
        }
        store.write_declaration(&declaration).unwrap();
    }

    #[test]
    fn resolve_selector_bare_name_matches_every_version() {
        let v1 = installed("pkg:vips@1.0", "vips", "1.0", 0, vec![]);
        let v2 = installed("pkg:vips@2.0", "vips", "2.0", 0, vec![]);
        let all = vec![v1, v2];

        let matches = resolve_selector(&all, "vips");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn resolve_selector_literal_name_containing_at_wins_over_split() {
        // A real versioned package name, not a selector.
        let pg14 = installed("pkg:postgresql@14@14.0", "postgresql@14", "14.0", 0, vec![]);
        let all = vec![pg14];

        let matches = resolve_selector(&all, "postgresql@14");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name.0, "postgresql@14");
    }

    #[test]
    fn resolve_selector_exact_keg_version_matches_one_revision() {
        let r0 = installed("pkg:vips@8.19.0", "vips", "8.19.0", 0, vec![]);
        let r1 = installed("pkg:vips@8.19.0_1", "vips", "8.19.0", 1, vec![]);
        let all = vec![r0, r1.clone()];

        let matches = resolve_selector(&all, "vips@8.19.0_1");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].keg_version, r1.keg_version);
    }

    #[test]
    fn resolve_selector_bare_version_matches_every_revision() {
        let r0 = installed("pkg:vips@8.19.0", "vips", "8.19.0", 0, vec![]);
        let r1 = installed("pkg:vips@8.19.0_1", "vips", "8.19.0", 1, vec![]);
        let all = vec![r0, r1];

        let matches = resolve_selector(&all, "vips@8.19.0");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn resolve_selector_no_match_returns_empty() {
        let v1 = installed("pkg:vips@1.0", "vips", "1.0", 0, vec![]);
        let all = vec![v1];

        assert!(resolve_selector(&all, "vips@9.9.9").is_empty());
        assert!(resolve_selector(&all, "node").is_empty());
    }

    #[test]
    fn plan_removal_errors_when_target_is_needed_and_undeclared() {
        let glib = installed("pkg:glib@2.0", "glib", "2.0", 0, vec![]);
        let vips = installed(
            "pkg:vips@1.0",
            "vips",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let all = vec![glib, vips];

        let err =
            plan_removal_from_installed(&all, &declared_names(&["vips"]), vec!["glib".to_string()])
                .unwrap_err();
        assert!(err
            .to_string()
            .contains("installed as a dependency of vips"));
        assert!(err.to_string().contains("can't be removed"));
    }

    #[test]
    fn plan_removal_demotes_declared_package_other_declared_still_needs() {
        let glib = installed("pkg:glib@2.0", "glib", "2.0", 0, vec![]);
        let vips = installed(
            "pkg:vips@1.0",
            "vips",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let all = vec![glib, vips];

        let plan = plan_removal_from_installed(
            &all,
            &declared_names(&["glib", "vips"]),
            vec!["glib".to_string()],
        )
        .unwrap();

        assert!(plan.to_remove.is_empty());
        assert_eq!(plan.kept.len(), 1);
        assert_eq!(plan.kept[0].package.name.0, "glib");
        assert_eq!(plan.kept[0].needed_by, vec!["vips"]);
        assert!(!plan.has_unnamed_removals());
    }

    #[test]
    fn confirmation_uses_package_identity_when_named_and_removal_counts_match() {
        let app = installed(
            "pkg:app@1.0",
            "app",
            "1.0",
            0,
            vec![("shared", "pkg:shared@1.0")],
        );
        let shared = installed("pkg:shared@1.0", "shared", "1.0", 0, vec![]);
        let tool = installed(
            "pkg:tool@1.0",
            "tool",
            "1.0",
            0,
            vec![("leaf", "pkg:leaf@1.0")],
        );
        let leaf = installed("pkg:leaf@1.0", "leaf", "1.0", 0, vec![]);
        let all = vec![app, shared, tool, leaf];

        let plan = plan_removal_from_installed(
            &all,
            &declared_names(&["app", "shared", "tool"]),
            vec!["shared".to_string(), "tool".to_string()],
        )
        .unwrap();

        assert_eq!(plan.named.len(), 2);
        assert_eq!(plan.to_remove.len(), 2);
        assert_eq!(plan.kept.len(), 1);
        assert_eq!(plan.kept[0].package.name.0, "shared");
        assert_eq!(
            plan.to_remove
                .iter()
                .map(|package| package.name.0.as_str())
                .collect::<Vec<_>>(),
            vec!["leaf", "tool"]
        );
        assert!(plan.has_unnamed_removals());
    }

    #[test]
    fn confirmation_tracks_concrete_versions_selected_by_the_selector() {
        let v1 = installed("pkg:vips@1.0", "vips", "1.0", 0, vec![]);
        let v2 = installed("pkg:vips@2.0", "vips", "2.0", 0, vec![]);
        let all = vec![v1, v2];
        let declared = declared_names(&["vips"]);

        let all_versions = plan_removal_from_installed(
            &all,
            &declared,
            vec!["vips".to_string(), "vips".to_string()],
        )
        .unwrap();
        assert_eq!(all_versions.named.len(), 2);
        assert_eq!(all_versions.to_remove.len(), 2);
        assert!(!all_versions.has_unnamed_removals());

        let one_version =
            plan_removal_from_installed(&all, &declared, vec!["vips@2.0".to_string()]).unwrap();
        assert_eq!(one_version.named.len(), 1);
        assert_eq!(one_version.to_remove.len(), 2);
        assert!(one_version.has_unnamed_removals());
    }

    #[test]
    fn plan_removal_removes_target_with_its_dangling_closure() {
        let vips = installed(
            "pkg:vips@1.0",
            "vips",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let glib = installed(
            "pkg:glib@2.0",
            "glib",
            "2.0",
            0,
            vec![("pcre2", "pkg:pcre2@1.0")],
        );
        let pcre2 = installed("pkg:pcre2@1.0", "pcre2", "1.0", 0, vec![]);
        let all = vec![vips, glib, pcre2];

        let plan =
            plan_removal_from_installed(&all, &declared_names(&["vips"]), vec!["vips".to_string()])
                .unwrap();

        assert_eq!(plan.named.len(), 1);
        let names: Vec<&str> = plan.to_remove.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["glib", "pcre2", "vips"]);
        assert!(plan.has_unnamed_removals());
    }

    #[test]
    fn plan_removal_keeps_shared_dependencies() {
        let vips = installed(
            "pkg:vips@1.0",
            "vips",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let ffmpeg = installed(
            "pkg:ffmpeg@1.0",
            "ffmpeg",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let glib = installed("pkg:glib@2.0", "glib", "2.0", 0, vec![]);
        let all = vec![vips, ffmpeg, glib];

        let plan = plan_removal_from_installed(
            &all,
            &declared_names(&["vips", "ffmpeg"]),
            vec!["vips".to_string()],
        )
        .unwrap();

        let names: Vec<&str> = plan.to_remove.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["vips"]);
        assert!(!plan.has_unnamed_removals());
    }

    #[test]
    fn plan_removal_allows_dangling_undeclared_target() {
        // glib was dropped by a previous update: nothing declared needs it,
        // but it is still installed, with its own dependency pcre2.
        let vips = installed("pkg:vips@1.0", "vips", "1.0", 0, vec![]);
        let glib = installed(
            "pkg:glib@2.0",
            "glib",
            "2.0",
            0,
            vec![("pcre2", "pkg:pcre2@1.0")],
        );
        let pcre2 = installed("pkg:pcre2@1.0", "pcre2", "1.0", 0, vec![]);
        let all = vec![vips, glib, pcre2];

        let plan =
            plan_removal_from_installed(&all, &declared_names(&["vips"]), vec!["glib".to_string()])
                .unwrap();

        let names: Vec<&str> = plan.to_remove.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["glib", "pcre2"]);
        assert_eq!(plan.named.len(), 1);
        assert!(plan.has_unnamed_removals());
    }

    #[test]
    fn plan_removal_removing_dependent_and_dependency_together() {
        let vips = installed(
            "pkg:vips@1.0",
            "vips",
            "1.0",
            0,
            vec![("glib", "pkg:glib@2.0")],
        );
        let glib = installed("pkg:glib@2.0", "glib", "2.0", 0, vec![]);
        let all = vec![vips, glib];

        let plan = plan_removal_from_installed(
            &all,
            &declared_names(&["vips", "glib"]),
            vec!["vips".to_string(), "glib".to_string()],
        )
        .unwrap();

        assert!(plan.kept.is_empty());
        let names: Vec<&str> = plan.to_remove.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["glib", "vips"]);
        assert!(!plan.has_unnamed_removals());
    }

    #[test]
    fn execute_removal_demotes_kept_and_removes_rest() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "8.19.0", 0, true, vec!["glib"]);
        write_fixture_receipt(&prefix, "glib", "2.0", 0, false, vec![]);
        write_fixture_receipt(&prefix, "pcre2", "1.0", 0, false, vec![]);
        mark_deactivated(&prefix, &["vips", "glib", "pcre2"]);

        let plan = plan_removal(&prefix, vec!["vips".to_string()]).unwrap();
        assert_eq!(plan.named.len(), 1);

        let removed = execute_removal(&prefix, &plan).unwrap();
        let names: Vec<&str> = removed.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["glib", "pcre2", "vips"]);

        // vips' keg is gone; nothing remains, so removed names are also
        // cleared from deactivation intent.
        let store = InstalledStateStore::new(prefix.clone());
        let state = store.load_installed_state().unwrap();
        assert!(state.list().is_empty());
        assert!(store
            .load_declaration()
            .unwrap()
            .deactivated_names()
            .is_empty());

        // glib declared, vips declared needs glib. `rm glib` keeps glib
        // installed because vips still needs it.
        write_fixture_receipt(&prefix, "vips", "8.19.0", 0, true, vec!["glib"]);
        write_fixture_receipt(&prefix, "glib", "2.0", 0, true, vec![]);
        mark_deactivated(&prefix, &["glib"]);

        let plan = plan_removal(&prefix, vec!["glib".to_string()]).unwrap();
        assert!(plan.to_remove.is_empty());
        assert_eq!(plan.kept.len(), 1);

        let removed = execute_removal(&prefix, &plan).unwrap();
        assert!(removed.is_empty());

        let state = InstalledStateStore::new(prefix.clone())
            .load_installed_state()
            .unwrap();
        assert!(state.find(&PackageName("glib".to_string())).is_some());

        // The declaration lost glib but kept vips.
        let declaration = InstalledStateStore::new(prefix.clone())
            .read_declaration()
            .unwrap()
            .unwrap();
        assert!(!declaration.contains(&PackageName("glib".to_string())));
        assert!(declaration.contains(&PackageName("vips".to_string())));
        assert!(declaration
            .deactivated_names()
            .contains(&PackageName("glib".to_string())));
    }

    #[test]
    fn plan_autoremove_returns_only_dangling_packages() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "1.0", 0, true, vec!["glib"]);
        write_fixture_receipt(&prefix, "glib", "2.0", 0, false, vec![]);
        // foo is not declared and not reachable from vips — dangling.
        write_fixture_receipt(&prefix, "foo", "1.0", 0, false, vec![]);

        let dangling = plan_autoremove(&prefix).unwrap();
        let names: Vec<&str> = dangling.iter().map(|p| p.name.0.as_str()).collect();

        assert_eq!(names, vec!["foo"]);
    }

    #[test]
    fn execute_autoremove_removes_only_the_dangling_kegs() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "1.0", 0, true, vec!["glib"]);
        write_fixture_receipt(&prefix, "glib", "2.0", 0, false, vec![]);
        write_fixture_receipt(&prefix, "foo", "1.0", 0, false, vec![]);
        mark_deactivated(&prefix, &["foo", "vips"]);

        let dangling = plan_autoremove(&prefix).unwrap();
        let removed = execute_autoremove(&prefix, &dangling).unwrap();
        let names: Vec<&str> = removed.iter().map(|p| p.name.0.as_str()).collect();

        assert_eq!(names, vec!["foo"]);
        assert!(!prefix.0.join("Cellar/foo").exists());
        assert!(prefix.0.join("Cellar/glib/2.0").exists());
        assert!(prefix.0.join("Cellar/vips/1.0").exists());
        let deactivated = InstalledStateStore::new(prefix.clone())
            .load_declaration()
            .unwrap()
            .deactivated_names();
        assert!(!deactivated.contains(&PackageName("foo".to_string())));
        assert!(deactivated.contains(&PackageName("vips".to_string())));
    }

    #[test]
    fn plan_and_execute_bare_name_removes_every_installed_version() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "8.18.5", 1, true, vec![]);
        write_fixture_receipt(&prefix, "vips", "8.19.0", 0, true, vec![]);

        let plan = plan_removal(&prefix, vec!["vips".to_string()]).unwrap();
        let removed = execute_removal(&prefix, &plan).unwrap();

        assert_eq!(removed.len(), 2);
        assert!(!prefix.0.join("Cellar/vips").exists());
    }

    #[test]
    fn plan_and_execute_qualified_selector_syncs_the_declared_name() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "8.18.5", 1, true, vec![]);
        write_fixture_receipt(&prefix, "vips", "8.19.0", 0, true, vec![]);

        let plan = plan_removal(&prefix, vec!["vips@8.18.5_1".to_string()]).unwrap();
        let removed = execute_removal(&prefix, &plan).unwrap();

        assert_eq!(removed.len(), 2);
        let versions: Vec<&str> = removed
            .iter()
            .map(|package| package.keg_version.0.as_str())
            .collect();
        assert_eq!(versions, vec!["8.19.0", "8.18.5_1"]);
        assert!(!prefix.0.join("Cellar/vips").exists());
    }

    #[test]
    fn plan_removal_not_installed_selector_mutates_nothing() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "vips", "8.19.0", 0, true, vec![]);

        let err = plan_removal(&prefix, vec!["node".to_string()]).unwrap_err();

        assert!(err.to_string().contains("'node'"));
        assert!(prefix.0.join("Cellar/vips/8.19.0").exists());
    }

    fn write_mutable_default(
        prefix: &Prefix,
        name: &str,
        version: &str,
        relative: &str,
        default: &[u8],
        live: &[u8],
    ) -> PathBuf {
        let source = prefix
            .0
            .join("Cellar")
            .join(name)
            .join(version)
            .join(".bottle")
            .join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, default).unwrap();
        let destination = prefix.0.join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, live).unwrap();
        destination
    }

    #[test]
    fn removal_automatically_deletes_unique_unchanged_defaults() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "fontconfig", "2.0", 0, true, vec![]);
        let config = write_mutable_default(
            &prefix,
            "fontconfig",
            "2.0",
            "etc/fonts/fonts.conf",
            b"default\n",
            b"default\n",
        );

        let plan = plan_removal(&prefix, vec!["fontconfig".to_string()]).unwrap();
        assert_eq!(plan.mutable_files.unchanged.len(), 1);
        assert!(plan.mutable_files.modified.is_empty());

        let result = execute_removal_with_config(&prefix, &plan, false).unwrap();
        assert_eq!(result.mutable_files.removed, vec![config.clone()]);
        assert!(!config.exists());
        assert!(!prefix.0.join("etc/fonts").exists());
    }

    #[test]
    fn removal_preserves_modified_defaults_unless_explicitly_approved() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "gnupg", "2.5.0", 0, true, vec![]);
        let config = write_mutable_default(
            &prefix,
            "gnupg",
            "2.5.0",
            "etc/gnupg/gpg.conf",
            b"default\n",
            b"user config\n",
        );

        let plan = plan_removal(&prefix, vec!["gnupg".to_string()]).unwrap();
        assert_eq!(plan.mutable_files.modified.len(), 1);
        let result = execute_removal_with_config(&prefix, &plan, false).unwrap();
        assert_eq!(result.mutable_files.retained_modified, vec![config.clone()]);
        assert!(config.is_file());

        // A fresh package verifies explicit modified-file cleanup separately.
        write_fixture_receipt(&prefix, "gnupg", "2.5.1", 0, true, vec![]);
        let config = write_mutable_default(
            &prefix,
            "gnupg",
            "2.5.1",
            "etc/gnupg/gpg.conf",
            b"new default\n",
            b"user config\n",
        );
        let plan = plan_removal(&prefix, vec!["gnupg".to_string()]).unwrap();
        let result = execute_removal_with_config(&prefix, &plan, true).unwrap();
        assert_eq!(result.mutable_files.removed, vec![config.clone()]);
        assert!(!config.exists());
    }

    #[test]
    fn retained_package_keeps_a_shared_mutable_file() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "one", "1.0", 0, true, vec![]);
        write_fixture_receipt(&prefix, "two", "1.0", 0, true, vec![]);
        let shared = write_mutable_default(
            &prefix,
            "one",
            "1.0",
            "etc/shared.conf",
            b"default\n",
            b"default\n",
        );
        write_mutable_default(
            &prefix,
            "two",
            "1.0",
            "etc/shared.conf",
            b"default\n",
            b"default\n",
        );

        let plan = plan_removal(&prefix, vec!["one".to_string()]).unwrap();
        assert_eq!(plan.mutable_files.retained.len(), 1);
        assert_eq!(
            plan.mutable_files.retained[0].reason,
            RetainedMutableFileReason::Shared
        );
        let result = execute_removal_with_config(&prefix, &plan, true).unwrap();
        assert_eq!(result.mutable_files.retained_shared, vec![shared.clone()]);
        assert!(shared.is_file());
    }

    #[test]
    fn path_claimed_only_by_removed_packages_is_deleted_once() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "one", "1.0", 0, true, vec![]);
        write_fixture_receipt(&prefix, "two", "1.0", 0, true, vec![]);
        let shared = write_mutable_default(
            &prefix,
            "one",
            "1.0",
            "etc/shared.conf",
            b"default\n",
            b"default\n",
        );
        write_mutable_default(
            &prefix,
            "two",
            "1.0",
            "etc/shared.conf",
            b"default\n",
            b"default\n",
        );

        let plan = plan_removal(&prefix, vec!["one".to_string(), "two".to_string()]).unwrap();
        assert_eq!(plan.mutable_files.unchanged.len(), 1);
        assert!(plan.mutable_files.retained.is_empty());
        let result = execute_removal_with_config(&prefix, &plan, false).unwrap();

        assert_eq!(result.mutable_files.removed, vec![shared.clone()]);
        assert!(!shared.exists());
    }

    #[test]
    fn cleanup_removes_matching_default_but_keeps_unknown_descendants() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "server", "1.0", 0, true, vec![]);
        let config = write_mutable_default(
            &prefix,
            "server",
            "1.0",
            "var/lib/server/config",
            b"new default\n",
            b"user value\n",
        );
        let default = PathBuf::from(format!("{}.default", config.display()));
        fs::write(&default, b"new default\n").unwrap();
        let runtime = prefix.0.join("var/lib/server/database");
        fs::write(&runtime, b"runtime data\n").unwrap();

        let plan = plan_removal(&prefix, vec!["server".to_string()]).unwrap();
        assert_eq!(plan.mutable_files.unchanged.len(), 1);
        assert_eq!(plan.mutable_files.modified.len(), 1);
        let result = execute_removal_with_config(&prefix, &plan, false).unwrap();

        assert_eq!(result.mutable_files.removed, vec![default]);
        assert!(config.is_file());
        assert!(runtime.is_file());
        assert!(prefix.0.join("var/lib/server").is_dir());
    }

    #[test]
    fn cleanup_retains_a_file_that_changes_after_planning() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "one", "1.0", 0, true, vec![]);
        let config = write_mutable_default(
            &prefix,
            "one",
            "1.0",
            "etc/one.conf",
            b"default\n",
            b"default\n",
        );
        let plan = plan_removal(&prefix, vec!["one".to_string()]).unwrap();
        fs::write(&config, b"changed after plan\n").unwrap();

        let result = execute_removal_with_config(&prefix, &plan, false).unwrap();

        assert_eq!(result.mutable_files.retained_changed, vec![config.clone()]);
        assert_eq!(fs::read(config).unwrap(), b"changed after plan\n");
    }

    #[test]
    fn cleanup_prunes_a_uniquely_owned_empty_bottled_directory() {
        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "dbus", "1.0", 0, true, vec![]);
        fs::create_dir_all(prefix.0.join("Cellar/dbus/1.0/.bottle/etc/dbus/session.d")).unwrap();
        let live = prefix.0.join("etc/dbus/session.d");
        fs::create_dir_all(&live).unwrap();

        let plan = plan_removal(&prefix, vec!["dbus".to_string()]).unwrap();
        execute_removal_with_config(&prefix, &plan, false).unwrap();

        assert!(!live.exists());
        assert!(!prefix.0.join("etc/dbus").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_follows_a_live_destination_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "one", "1.0", 0, true, vec![]);
        let source = prefix.0.join("Cellar/one/1.0/.bottle/etc/one.conf");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(source, b"default\n").unwrap();
        let external = tmp.path().join("external");
        fs::write(&external, b"keep\n").unwrap();
        let destination = prefix.0.join("etc/one.conf");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        symlink(&external, &destination).unwrap();

        let plan = plan_removal(&prefix, vec!["one".to_string()]).unwrap();
        assert_eq!(
            plan.mutable_files.retained[0].reason,
            RetainedMutableFileReason::UnsafeType
        );
        execute_removal_with_config(&prefix, &plan, true).unwrap();

        assert!(destination.is_symlink());
        assert_eq!(fs::read(external).unwrap(), b"keep\n");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_traverses_a_symlinked_mutable_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().to_path_buf());
        write_fixture_receipt(&prefix, "one", "1.0", 0, true, vec![]);
        let source = prefix.0.join("Cellar/one/1.0/.bottle/etc/nested/one.conf");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(source, b"default\n").unwrap();
        fs::create_dir_all(prefix.0.join("etc")).unwrap();
        fs::write(external.path().join("one.conf"), b"default\n").unwrap();
        symlink(external.path(), prefix.0.join("etc/nested")).unwrap();

        let plan = plan_removal(&prefix, vec!["one".to_string()]).unwrap();
        assert_eq!(
            plan.mutable_files.retained[0].reason,
            RetainedMutableFileReason::UnsafeType
        );
        execute_removal_with_config(&prefix, &plan, true).unwrap();

        assert_eq!(
            fs::read(external.path().join("one.conf")).unwrap(),
            b"default\n"
        );
    }
}
