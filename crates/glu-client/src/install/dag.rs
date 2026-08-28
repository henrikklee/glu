use crate::{
    download::{cache::ArtifactCache, ghcr::repo_for_blob_url},
    install::{manifest_lookup::ManifestLookup, planner::InstallWorkSet},
    postinstall::{
        deferral::{global_flush_order, global_postinstall_label},
        structured,
    },
};
use anyhow::{bail, Result};
use glu_core::{InstallManifest, PackageId, PackageName, Prefix, ResolvedArtifact};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

const PREPARE_SUBPHASES: &[&str] = &[
    "extract",
    "writer_wait",
    "text_relocate",
    "fixed_prefix_relocate",
    "macho_patch",
    "codesign",
];

/// The kind of work an execution node performs. Typed (rather than a
/// stringly-matched kind) so the three consumers — plan construction,
/// the scheduler's message/priority logic, and the orchestrator's
/// executor — are exhaustive over the same set; a typo in a kind is a
/// compile error instead of a runtime `unsupported install node kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeKind {
    GhcrAuth,
    GhcrBottleDownload,
    BottlePrepare,
    KegLink,
    KegLinkExisting,
    KegRenameExisting,
    FormulaPostinstall,
    RegistryWrite,
    CachePostinstall,
}

/// Closed set of scheduler token pools used by the package-level install DAG.
/// Trace output still uses the historical lowercase strings via `as_str()`, but
/// plan construction and scheduling use this enum so adding a node kind requires
/// a conscious pool decision instead of another ad hoc string literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecPool {
    Setup,
    Download,
    Prepare,
    Commit,
    Postinstall,
    Registry,
}

impl ExecPool {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecPool::Setup => "setup",
            ExecPool::Download => "download",
            ExecPool::Prepare => "prepare",
            ExecPool::Commit => "commit",
            ExecPool::Postinstall => "postinstall",
            ExecPool::Registry => "registry",
        }
    }
}

impl NodeKind {
    /// The stable kind string used in node ids and trace output (kept
    /// stable so existing traces and tests don't churn).
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::GhcrAuth => "ghcr_auth",
            NodeKind::GhcrBottleDownload => "ghcr_bottle_download",
            NodeKind::BottlePrepare => "bottle_prepare",
            NodeKind::KegLink => "keg_link",
            NodeKind::KegLinkExisting => "keg_link_existing",
            NodeKind::KegRenameExisting => "keg_rename_existing",
            NodeKind::FormulaPostinstall => "formula_postinstall",
            NodeKind::RegistryWrite => "registry_write",
            NodeKind::CachePostinstall => "cache_postinstall",
        }
    }

    pub fn pool(self) -> ExecPool {
        match self {
            NodeKind::GhcrAuth => ExecPool::Setup,
            NodeKind::GhcrBottleDownload => ExecPool::Download,
            NodeKind::BottlePrepare => ExecPool::Prepare,
            NodeKind::KegLink
            | NodeKind::KegLinkExisting
            | NodeKind::KegRenameExisting
            | NodeKind::FormulaPostinstall => ExecPool::Commit,
            NodeKind::RegistryWrite => ExecPool::Registry,
            NodeKind::CachePostinstall => ExecPool::Postinstall,
        }
    }

    /// The node id convention: `<kind>:<percent-encoded part>`.
    pub fn node_id(self, part: &str) -> String {
        format!("{}:{}", self.as_str(), percent_encode_node_part(part))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecNode {
    pub id: String,
    pub kind: NodeKind,
    pub pool: ExecPool,
    /// Concurrency token (slot) this node occupied in its pool, assigned by
    /// the scheduler at dispatch time (trace: which parallel worker ran it).
    /// `None` for pools that run with a single token, or before dispatch.
    pub slot: Option<usize>,
    pub package_id: Option<PackageId>,
    pub formula: Option<PackageName>,
    /// Display-only label for nodes whose stable id is intentionally opaque
    /// (for example cache postinstall nodes keyed by percent-encoded paths).
    /// Consumers must still join by `id`; this is presentation metadata.
    pub label: Option<String>,
    pub inputs: Value,
    pub outputs: Value,
    pub subphases: Vec<String>,
    pub priority: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecEdge {
    pub source: String,
    pub target: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionPlan {
    pub nodes: Vec<ExecNode>,
    pub edges: Vec<ExecEdge>,
    pub pools: BTreeMap<ExecPool, usize>,
}

impl ExecutionPlan {
    pub fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        let mut duplicates = BTreeSet::new();
        for node in &self.nodes {
            if !ids.insert(node.id.clone()) {
                duplicates.insert(node.id.clone());
            }
        }
        if !duplicates.is_empty() {
            bail!(
                "duplicate execution node id(s): {}",
                duplicates.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        for edge in &self.edges {
            if !ids.contains(&edge.source) {
                bail!("edge source does not exist: {}", edge.source);
            }
            if !ids.contains(&edge.target) {
                bail!("edge target does not exist: {}", edge.target);
            }
        }
        check_acyclic(&ids, &self.edges)
    }

    pub fn node(&self, id: &str) -> Option<&ExecNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn as_trace_graph(&self) -> Value {
        json!({
            "nodes": self.nodes.iter().map(|node| json!({
                "id": node.id,
                "kind": node.kind.as_str(),
                "pool": node.pool.as_str(),
                "formula": node.formula.as_ref().map(|name| name.0.as_str()),
                "label": node.label.as_deref(),
                "package_id": node.package_id.as_ref().map(|id| id.0.as_str()),
                "inputs": node.inputs,
                "outputs": node.outputs,
                "subphases": node.subphases,
                "priority": node.priority,
            })).collect::<Vec<_>>(),
            "edges": self.edges.iter().map(|edge| json!({
                "from": edge.source,
                "to": edge.target,
                "reason": edge.reason,
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn make_execution_plan(
    manifest: &InstallManifest,
    workset: &InstallWorkSet,
    prefix: &Prefix,
    postinstall_plans: &structured::PostinstallPlans,
) -> Result<ExecutionPlan> {
    let satisfied = workset.satisfied.iter().cloned().collect::<BTreeSet<_>>();
    let uncached = uncached_packages(manifest, &workset.install, prefix)?;
    let priorities = package_priorities(manifest, &workset.install);

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    if !uncached.is_empty() {
        let repos = uncached
            .iter()
            .map(|package_id| {
                let package = manifest.require_package(package_id)?;
                let artifact = manifest.require_artifact(&package.artifact)?;
                repo_for_blob_url(&artifact.url)
            })
            .collect::<Result<BTreeSet<_>>>()?;
        nodes.push(ExecNode {
            id: "ghcr_auth:install".to_string(),
            kind: NodeKind::GhcrAuth,
            pool: NodeKind::GhcrAuth.pool(),
            slot: None,
            package_id: None,
            formula: None,
            label: None,
            inputs: json!({ "registry": "ghcr.io", "repos": repos }),
            outputs: json!({ "auth_ref": "ghcr_install_token" }),
            subphases: vec![],
            priority: 0.0,
        });
    }

    for package_id in &workset.install {
        let package = manifest.require_package(package_id)?;
        let artifact = manifest.require_artifact(&package.artifact)?;
        let download_id = NodeKind::GhcrBottleDownload.node_id(&package.name.0);
        let repo = repo_for_blob_url(&artifact.url)?;
        let cache_path = artifact_cache_path(prefix, artifact).display().to_string();
        nodes.push(ExecNode {
            id: download_id.clone(),
            kind: NodeKind::GhcrBottleDownload,
            pool: NodeKind::GhcrBottleDownload.pool(),
            slot: None,
            package_id: Some(package_id.clone()),
            formula: Some(package.name.clone()),
            label: None,
            inputs: json!({
                "package_id": package_id.0,
                "formula_name": package.name.0,
                "artifact_id": package.artifact.0,
                "repo": repo,
                "url": artifact.url,
                "sha256": artifact.sha256,
                "size": artifact.bytes,
                "cache_path": cache_path.clone(),
                "cached": !uncached.contains(package_id),
                "tag": artifact.bottle_tag,
                "version": package.keg_version.0,
            }),
            outputs: json!({ "artifact": package.artifact.0, "tarball": cache_path }),
            subphases: vec![],
            priority: priorities.get(package_id).copied().unwrap_or(0.0),
        });
        if uncached.contains(package_id) {
            edges.push(ExecEdge::new("ghcr_auth:install", &download_id, "auth"));
        }
    }

    // One no-op anchor node per already-satisfied package. These carry no
    // work — the orchestrator's KegLinkExisting arm is `Ok(())` — but they
    // are load-bearing structure: dependents' `formula_dependency_committed`
    // edges (see add_dependency_edges) point at them, the scheduler propagates
    // known downstream costs through those edges, and the trace would otherwise
    // hide satisfied dependencies entirely. Do not
    // remove.
    for package_id in &workset.satisfied {
        let package = manifest.require_package(package_id)?;
        let existing_id = NodeKind::KegLinkExisting.node_id(&package.name.0);
        nodes.push(ExecNode {
            id: existing_id,
            kind: NodeKind::KegLinkExisting,
            pool: NodeKind::KegLinkExisting.pool(),
            slot: None,
            package_id: Some(package_id.clone()),
            formula: Some(package.name.clone()),
            label: None,
            inputs: json!({
                "package_id": package_id.0,
                "formula_name": package.name.0,
                "keg": keg_path(prefix, &package.name, &package.keg_version.0),
                "version": package.keg_version.0,
                "keg_only": package.install.keg_only,
            }),
            outputs: json!({ "committed_formula": package.name.0 }),
            subphases: vec![],
            priority: priorities.get(package_id).copied().unwrap_or(0.0),
        });
    }

    for rename in &workset.rename {
        let package = manifest.require_package(&rename.package)?;
        let rename_id = NodeKind::KegRenameExisting.node_id(&package.name.0);
        nodes.push(ExecNode {
            id: rename_id,
            kind: NodeKind::KegRenameExisting,
            pool: NodeKind::KegRenameExisting.pool(),
            slot: None,
            package_id: Some(rename.package.clone()),
            formula: Some(package.name.clone()),
            label: Some(format!("{} -> {}", rename.old_name.0, package.name.0)),
            inputs: json!({
                "package_id": rename.package.0,
                "formula_name": package.name.0,
                "old_name": rename.old_name.0,
                "new_name": package.name.0,
                "old_keg": rename.old_keg_path,
                "old_keg_version": rename.old_keg_version,
                "old_version": rename.old_version,
                "old_revision": rename.old_revision,
                "version": rename.old_keg_version,
            }),
            outputs: json!({ "committed_formula": package.name.0 }),
            subphases: vec![],
            priority: priorities.get(&rename.package).copied().unwrap_or(0.0),
        });
    }

    for package_id in &workset.install {
        let package = manifest.require_package(package_id)?;
        let artifact = manifest.require_artifact(&package.artifact)?;
        let keg = keg_path(prefix, &package.name, &package.keg_version.0);
        let prepare_id = NodeKind::BottlePrepare.node_id(&package.name.0);
        let link_id = NodeKind::KegLink.node_id(&package.name.0);
        let postinstall_id = NodeKind::FormulaPostinstall.node_id(&package.name.0);
        let registry_id = NodeKind::RegistryWrite.node_id(&package.name.0);
        let priority = priorities.get(package_id).copied().unwrap_or(0.0);

        nodes.extend([
            ExecNode {
                id: prepare_id.clone(),
                kind: NodeKind::BottlePrepare,
                pool: NodeKind::BottlePrepare.pool(),
                slot: None,
                package_id: Some(package_id.clone()),
                formula: Some(package.name.clone()),
                label: None,
                inputs: json!({
                    "package_id": package_id.0,
                    "formula_name": package.name.0,
                    "artifact_id": package.artifact.0,
                    "tarball_ref": NodeKind::GhcrBottleDownload.node_id(&package.name.0),
                    "tarball": artifact_cache_path(prefix, artifact),
                    "cellar": artifact.cellar,
                    "keg": keg,
                }),
                outputs: json!({ "keg": keg }),
                subphases: PREPARE_SUBPHASES.iter().map(|s| s.to_string()).collect(),
                priority,
            },
            ExecNode {
                id: link_id.clone(),
                kind: NodeKind::KegLink,
                pool: NodeKind::KegLink.pool(),
                slot: None,
                package_id: Some(package_id.clone()),
                formula: Some(package.name.clone()),
                label: None,
                inputs: json!({
                    "package_id": package_id.0,
                    "formula_name": package.name.0,
                    "keg": keg,
                    "keg_only": package.install.keg_only,
                }),
                outputs: json!({ "linked_formula": package.name.0 }),
                subphases: vec![],
                priority,
            },
            ExecNode {
                id: postinstall_id.clone(),
                kind: NodeKind::FormulaPostinstall,
                pool: NodeKind::FormulaPostinstall.pool(),
                slot: None,
                package_id: Some(package_id.clone()),
                formula: Some(package.name.clone()),
                label: None,
                inputs: json!({
                    "package_id": package_id.0,
                    "formula_name": package.name.0,
                    "keg": keg,
                    "has_postinstall_steps": !package.install.post_install_steps.is_empty(),
                }),
                outputs: json!({
                    "committed_formula": package.name.0,
                    "cache_postinstall_requests": true,
                }),
                subphases: vec![],
                priority,
            },
            ExecNode {
                id: registry_id.clone(),
                kind: NodeKind::RegistryWrite,
                pool: NodeKind::RegistryWrite.pool(),
                slot: None,
                package_id: Some(package_id.clone()),
                formula: Some(package.name.clone()),
                label: None,
                inputs: json!({
                    "package_id": package_id.0,
                    "formula_name": package.name.0,
                    "keg": keg,
                    "linked": !package.install.keg_only,
                }),
                outputs: json!({ "installed_record": package.name.0 }),
                subphases: vec![],
                priority,
            },
        ]);

        edges.extend([
            ExecEdge::new(
                &NodeKind::GhcrBottleDownload.node_id(&package.name.0),
                &prepare_id,
                "phase_order",
            ),
            ExecEdge::new(&prepare_id, &link_id, "phase_order"),
            ExecEdge::new(&link_id, &postinstall_id, "phase_order"),
            ExecEdge::new(&postinstall_id, &registry_id, "registry_after_postinstall"),
        ]);
    }

    add_dependency_edges(
        manifest,
        &workset.install,
        &satisfied,
        &workset
            .rename
            .iter()
            .map(|rename| rename.package.clone())
            .collect::<BTreeSet<_>>(),
        &mut edges,
    )?;
    let cache_nodes = cache_postinstall_nodes(manifest, &workset.install, postinstall_plans)?;
    let mut cache_ids_by_formula = BTreeMap::<String, Vec<String>>::new();
    for node in &cache_nodes {
        for contributor in node.inputs["expected_contributors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            cache_ids_by_formula
                .entry(contributor.to_string())
                .or_default()
                .push(node.id.clone());
        }
    }
    nodes.extend(cache_nodes);

    for (formula, cache_ids) in &cache_ids_by_formula {
        let postinstall_id = NodeKind::FormulaPostinstall.node_id(formula);
        let registry_id = NodeKind::RegistryWrite.node_id(formula);
        for cache_id in cache_ids {
            edges.push(ExecEdge::new(
                &postinstall_id,
                cache_id,
                "cache_postinstall_contributor",
            ));
            edges.push(ExecEdge::new(
                cache_id,
                &registry_id,
                "registry_after_cache_postinstall",
            ));
        }
    }

    for pair in workset.install.windows(2) {
        let prev = manifest.require_package(&pair[0])?;
        let curr = manifest.require_package(&pair[1])?;
        edges.push(ExecEdge::new(
            &NodeKind::RegistryWrite.node_id(&prev.name.0),
            &NodeKind::RegistryWrite.node_id(&curr.name.0),
            "registry_write_order",
        ));
    }

    let edges = dedupe_edges(edges);
    let plan = ExecutionPlan {
        nodes,
        edges,
        pools: base_pools(workset.install.len(), workset.install.len()),
    };
    plan.validate()?;
    Ok(plan)
}

impl ExecEdge {
    fn new(source: &str, target: &str, reason: &str) -> Self {
        Self {
            source: source.to_string(),
            target: target.to_string(),
            reason: reason.to_string(),
        }
    }
}

fn percent_encode_node_part(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '_' | '-' | '+' | '=') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn keg_path(prefix: &Prefix, name: &PackageName, keg_version: &str) -> PathBuf {
    prefix.0.join("Cellar").join(&name.0).join(keg_version)
}

fn artifact_cache_path(prefix: &Prefix, artifact: &ResolvedArtifact) -> PathBuf {
    ArtifactCache::new(prefix).path_for_artifact(artifact)
}

fn uncached_packages(
    manifest: &InstallManifest,
    package_ids: &[PackageId],
    prefix: &Prefix,
) -> Result<BTreeSet<PackageId>> {
    let mut out = BTreeSet::new();
    for package_id in package_ids {
        let package = manifest.require_package(package_id)?;
        let artifact = manifest.require_artifact(&package.artifact)?;
        if !artifact_cache_path(prefix, artifact).exists() {
            out.insert(package_id.clone());
        }
    }
    Ok(out)
}

fn base_pools(downloads: usize, missing: usize) -> BTreeMap<ExecPool, usize> {
    BTreeMap::from([
        (ExecPool::Setup, 1),
        (ExecPool::Download, downloads.clamp(1, 16)),
        (ExecPool::Prepare, missing.clamp(1, 4)),
        (ExecPool::Commit, 1),
        (ExecPool::Postinstall, 4),
        (ExecPool::Registry, 1),
    ])
}

fn add_dependency_edges(
    manifest: &InstallManifest,
    install: &[PackageId],
    satisfied: &BTreeSet<PackageId>,
    rename: &BTreeSet<PackageId>,
    edges: &mut Vec<ExecEdge>,
) -> Result<()> {
    let position = install
        .iter()
        .enumerate()
        .map(|(index, id)| (id, index))
        .collect::<BTreeMap<_, _>>();
    for package_id in install {
        let package = manifest.require_package(package_id)?;
        let target = NodeKind::KegLink.node_id(&package.name.0);
        for dep in &package.deps {
            if let Some(&dep_position) = position.get(&dep.package) {
                // A cyclic pair (e.g. two bottled libraries that mutually declare a runtime
                // dependency on each other) can only be ordered one way. `install`'s order
                // already resolved that (see graph::dependency_order); keep only the edge that
                // agrees with it and drop the other, instead of feeding check_acyclic a real
                // cycle.
                if dep_position >= position[package_id] {
                    continue;
                }
                let dep_package = manifest.require_package(&dep.package)?;
                edges.push(ExecEdge::new(
                    &NodeKind::FormulaPostinstall.node_id(&dep_package.name.0),
                    &target,
                    "formula_dependency_committed",
                ));
            } else if satisfied.contains(&dep.package) {
                let dep_package = manifest.require_package(&dep.package)?;
                edges.push(ExecEdge::new(
                    &NodeKind::KegLinkExisting.node_id(&dep_package.name.0),
                    &target,
                    "formula_dependency_committed",
                ));
            } else if rename.contains(&dep.package) {
                let dep_package = manifest.require_package(&dep.package)?;
                edges.push(ExecEdge::new(
                    &NodeKind::KegRenameExisting.node_id(&dep_package.name.0),
                    &target,
                    "formula_dependency_committed",
                ));
            }
        }
    }
    Ok(())
}

fn cache_postinstall_nodes(
    manifest: &InstallManifest,
    install: &[PackageId],
    postinstall_plans: &structured::PostinstallPlans,
) -> Result<Vec<ExecNode>> {
    let mut expected = BTreeMap::<(String, Vec<String>), BTreeSet<String>>::new();
    for package_id in install {
        manifest.require_package(package_id)?;
        let analysis = postinstall_plans.analysis_for(package_id)?;
        for (key, contributors) in analysis.deferred_contributions() {
            expected
                .entry(key.clone())
                .or_default()
                .extend(contributors.iter().cloned());
        }
    }

    let mut items = expected.into_iter().collect::<Vec<_>>();
    items.sort_by_key(|((kind, key), _)| (global_flush_order(kind), kind.clone(), key.clone()));
    Ok(items
        .into_iter()
        .map(|((kind, key), contributors)| ExecNode {
            id: cache_node_id(&kind, &key),
            kind: NodeKind::CachePostinstall,
            pool: NodeKind::CachePostinstall.pool(),
            slot: None,
            package_id: None,
            formula: None,
            label: Some(global_postinstall_label(&kind)),
            inputs: json!({
                "kind": kind,
                "key": key,
                "expected_contributors": contributors,
            }),
            outputs: json!({ "global_postinstall": { "kind": kind, "key": key } }),
            subphases: vec![],
            priority: 0.0,
        })
        .collect())
}

fn cache_node_id(kind: &str, key: &[String]) -> String {
    let mut id = NodeKind::CachePostinstall.node_id(kind);
    for part in key {
        id.push(':');
        id.push_str(&percent_encode_node_part(part));
    }
    id
}

/// Computes, in one linear pass, both the blended per-package scheduling priority (used for
/// `ExecNode.priority`) and the raw normalized critical-path score keyed by formula name (used
/// by the scheduler's live download-ranking heuristic). `install` must be in dependency-before-
/// dependent order (as produced by `graph::dependency_order`), which lets the critical path be
/// computed by a single reverse walk instead of memoized recursion: by the time a package is
/// visited (back to front), every package that depends on it has already been visited.
fn package_priorities(
    manifest: &InstallManifest,
    install: &[PackageId],
) -> BTreeMap<PackageId, f64> {
    let position = install
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut dependents = BTreeMap::<PackageId, BTreeSet<PackageId>>::new();
    let mut sizes = BTreeMap::<PackageId, u64>::new();
    for package_id in install {
        let Ok(pkg) = manifest.require_package(package_id) else {
            continue;
        };
        let size = manifest
            .artifacts
            .get(&pkg.artifact)
            .and_then(|artifact| artifact.bytes)
            .unwrap_or(0);
        sizes.insert(package_id.clone(), size);
        dependents.entry(package_id.clone()).or_default();
        for dep in &pkg.deps {
            // Only the edge direction the plan actually kept (see add_dependency_edges)
            // contributes; the dropped side of a cyclic pair must not be treated as a real
            // "downstream" edge here either, or the reverse pass below would read an
            // as-yet-unvisited entry.
            if let Some(&dep_position) = position.get(&dep.package) {
                if dep_position < position[package_id] {
                    dependents
                        .entry(dep.package.clone())
                        .or_default()
                        .insert(package_id.clone());
                }
            }
        }
    }

    let mut critical = BTreeMap::<PackageId, f64>::new();
    for package_id in install.iter().rev() {
        let own = ((*sizes.get(package_id).unwrap_or(&0) as f64) + 1.0).ln();
        let downstream = dependents
            .get(package_id)
            .into_iter()
            .flatten()
            .map(|dependent| critical.get(dependent).copied().unwrap_or(0.0))
            .fold(0.0, f64::max);
        critical.insert(package_id.clone(), own + downstream);
    }

    let max_critical = critical.values().copied().fold(0.0, f64::max).max(1.0);
    let max_size = sizes.values().copied().max().unwrap_or(1).max(1) as f64;

    let mut priority = BTreeMap::new();
    for package_id in install {
        let normalized_critical = critical.get(package_id).copied().unwrap_or(0.0) / max_critical;
        let normalized_size = *sizes.get(package_id).unwrap_or(&0) as f64 / max_size;
        priority.insert(
            package_id.clone(),
            (0.75 * normalized_critical) + (0.25 * normalized_size),
        );
    }
    priority
}

fn dedupe_edges(edges: Vec<ExecEdge>) -> Vec<ExecEdge> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for edge in edges {
        let key = (
            edge.source.clone(),
            edge.target.clone(),
            edge.reason.clone(),
        );
        if seen.insert(key) {
            out.push(edge);
        }
    }
    out
}

fn check_acyclic(ids: &BTreeSet<String>, edges: &[ExecEdge]) -> Result<()> {
    let mut outgoing = ids
        .iter()
        .map(|id| (id.clone(), Vec::<String>::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in edges {
        outgoing
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
    }
    let mut state = BTreeMap::<String, u8>::new();
    for id in ids {
        visit(id, &outgoing, &mut state, &mut Vec::new())?;
    }
    Ok(())
}

fn visit(
    node: &str,
    outgoing: &BTreeMap<String, Vec<String>>,
    state: &mut BTreeMap<String, u8>,
    stack: &mut Vec<String>,
) -> Result<()> {
    match state.get(node).copied().unwrap_or(0) {
        2 => return Ok(()),
        1 => bail!("dependency cycle: {} -> {node}", stack.join(" -> ")),
        _ => {}
    }
    state.insert(node.to_string(), 1);
    stack.push(node.to_string());
    for child in outgoing.get(node).into_iter().flatten() {
        visit(child, outgoing, state, stack)?;
    }
    stack.pop();
    state.insert(node.to_string(), 2);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        ArtifactId, DependencyRequires, KegVersion, PackageInstallMetadata, ResolveRequestEcho,
        ResolvedArtifact, ResolvedPackage, RuntimeDependencyRequirement, Target,
    };

    #[test]
    fn node_kinds_have_explicit_pools() {
        assert_eq!(NodeKind::GhcrAuth.pool(), ExecPool::Setup);
        assert_eq!(NodeKind::GhcrBottleDownload.pool(), ExecPool::Download);
        assert_eq!(NodeKind::BottlePrepare.pool(), ExecPool::Prepare);
        assert_eq!(NodeKind::KegLink.pool(), ExecPool::Commit);
        assert_eq!(NodeKind::KegLinkExisting.pool(), ExecPool::Commit);
        assert_eq!(NodeKind::KegRenameExisting.pool(), ExecPool::Commit);
        assert_eq!(NodeKind::FormulaPostinstall.pool(), ExecPool::Commit);
        assert_eq!(NodeKind::RegistryWrite.pool(), ExecPool::Registry);
        assert_eq!(NodeKind::CachePostinstall.pool(), ExecPool::Postinstall);
    }

    fn pkg(
        name: &str,
        deps: Vec<&str>,
        steps: Vec<Value>,
        bytes: u64,
    ) -> (PackageId, ResolvedPackage, ResolvedArtifact) {
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
                post_install_defined: !steps.is_empty(),
                post_install_steps: steps,
                postinstall_network_access_allowed: true,
            },
        };
        let resolved_artifact = ResolvedArtifact {
            url: format!("https://ghcr.io/v2/homebrew/core/{name}/blobs/sha256:{name}"),
            sha256: format!("{name:0<64}"),
            bytes: Some(bytes),
            bottle_tag: "arm64_sequoia".to_string(),
            cellar: ":any".to_string(),
            built_on: None,
        };
        (id, package, resolved_artifact)
    }

    fn manifest(
        packages: Vec<(PackageId, ResolvedPackage, ResolvedArtifact)>,
        roots: Vec<PackageId>,
    ) -> InstallManifest {
        let mut package_map = BTreeMap::new();
        let mut artifacts = BTreeMap::new();
        for (id, package, artifact) in packages {
            artifacts.insert(package.artifact.clone(), artifact);
            package_map.insert(id, package);
        }
        let roots = roots
            .into_iter()
            .map(|package_id| {
                let package = &package_map[&package_id];
                glu_core::PackageSelection {
                    requested_as: glu_core::PackageSelector(package.name.0.clone()),
                    package_key: package.package_key.clone(),
                    package: package_id,
                }
            })
            .collect();
        InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![],
                target: Target("arm64_sequoia".to_string()),
                slim: false,
            },
            roots,
            packages: package_map,
            artifacts,
        }
    }

    fn execution_plan(
        manifest: &InstallManifest,
        workset: &InstallWorkSet,
        prefix: &Prefix,
    ) -> ExecutionPlan {
        let postinstall_plans =
            structured::PostinstallPlans::analyze(manifest, &workset.install, prefix).unwrap();
        make_execution_plan(manifest, workset, prefix, &postinstall_plans).unwrap()
    }

    #[test]
    fn graph_has_phase_and_dependency_edges() {
        let (dep_id, dep, dep_artifact) = pkg("dep", vec![], vec![], 10);
        let (root_id, root, root_artifact) = pkg("root", vec!["dep"], vec![], 20);
        let manifest = manifest(
            vec![
                (dep_id.clone(), dep, dep_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );
        let workset = InstallWorkSet {
            satisfied: vec![],
            rename: Vec::new(),
            install: vec![dep_id, root_id],
        };

        let plan = execution_plan(&manifest, &workset, &Prefix(PathBuf::from("/tmp/glu")));

        assert!(plan.node("bottle_prepare:root").is_some());
        assert!(plan.edges.contains(&ExecEdge::new(
            "formula_postinstall:dep",
            "keg_link:root",
            "formula_dependency_committed"
        )));
        // root's registry_write must still transitively depend on dep's postinstall (via
        // formula_postinstall:dep -> keg_link:root -> formula_postinstall:root ->
        // registry_write:root), but NOT via a direct fan-in edge — that's the bug this test
        // guards against: a direct edge here would mean any unrelated package failing its
        // postinstall blocks every other package's registry_write too.
        assert!(!plan
            .edges
            .iter()
            .any(|edge| edge.reason == "registry_after_all_missing_committed"));
        assert!(plan.edges.contains(&ExecEdge::new(
            "keg_link:root",
            "formula_postinstall:root",
            "phase_order"
        )));
        assert!(plan.edges.contains(&ExecEdge::new(
            "formula_postinstall:root",
            "registry_write:root",
            "registry_after_postinstall"
        )));
    }

    #[test]
    fn rename_existing_is_commit_anchor_and_does_not_gate_download_or_prepare() {
        let (dep_id, dep, dep_artifact) = pkg("bar", vec![], vec![], 10);
        let (root_id, root, root_artifact) = pkg("app", vec!["bar"], vec![], 20);
        let manifest = manifest(
            vec![
                (dep_id.clone(), dep, dep_artifact),
                (root_id.clone(), root, root_artifact),
            ],
            vec![root_id.clone()],
        );
        let workset = InstallWorkSet {
            satisfied: vec![],
            rename: vec![crate::install::planner::RenameWorkItem {
                package: dep_id,
                old_name: PackageName("foo".to_string()),
                old_keg_version: "1.0".to_string(),
                old_version: "1.0".to_string(),
                old_revision: 0,
                old_keg_path: PathBuf::from("/tmp/glu/Cellar/foo/1.0"),
            }],
            install: vec![root_id],
        };

        let plan = execution_plan(&manifest, &workset, &Prefix(PathBuf::from("/tmp/glu")));

        assert_eq!(
            plan.node("keg_rename_existing:bar").map(|node| node.pool),
            Some(ExecPool::Commit)
        );
        assert!(plan.edges.contains(&ExecEdge::new(
            "keg_rename_existing:bar",
            "keg_link:app",
            "formula_dependency_committed"
        )));
        assert!(!plan.edges.iter().any(|edge| {
            edge.source == "keg_rename_existing:bar"
                && matches!(
                    edge.target.as_str(),
                    "ghcr_bottle_download:app" | "bottle_prepare:app"
                )
        }));
    }

    #[test]
    fn cache_postinstall_edges_do_not_gate_unrelated_registry_writes() {
        let step = json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        });
        let (with_cache_id, with_cache, with_cache_artifact) =
            pkg("with-cache", vec![], vec![step], 10);
        let (plain_id, plain, plain_artifact) = pkg("plain", vec![], vec![], 10);
        let manifest = manifest(
            vec![
                (with_cache_id.clone(), with_cache, with_cache_artifact),
                (plain_id.clone(), plain, plain_artifact),
            ],
            vec![with_cache_id.clone(), plain_id.clone()],
        );
        let workset = InstallWorkSet {
            satisfied: vec![],
            rename: Vec::new(),
            install: vec![with_cache_id, plain_id],
        };

        let plan = execution_plan(&manifest, &workset, &Prefix(PathBuf::from("/tmp/glu")));

        let cache_id =
            "cache_postinstall:compile_gsettings_schemas:%2Ftmp%2Fglu%2Fshare%2Fglib-2.0%2Fschemas";
        assert!(plan.edges.contains(&ExecEdge::new(
            cache_id,
            "registry_write:with-cache",
            "registry_after_cache_postinstall"
        )));
        assert!(!plan.edges.contains(&ExecEdge::new(
            cache_id,
            "registry_write:plain",
            "registry_after_cache_postinstall"
        )));
    }

    #[test]
    fn graph_coalesces_gtk3_legacy_cache_runs_with_gtk4_structured_cache_steps() {
        let gtk3_steps = vec![
            json!({
                "type": "compile_gsettings_schemas",
                "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
            }),
            json!({
                "type": "run",
                "command": {"base": "bin", "path": "gtk3-update-icon-cache"},
                "args": ["-f", "-t", "{{HOMEBREW_PREFIX}}/share/icons/hicolor"]
            }),
            json!({
                "type": "run",
                "command": {"base": "bin", "path": "gtk-query-immodules-3.0"},
                "stdout_path": {"path": "{{HOMEBREW_PREFIX}}/lib/gtk-3.0/3.0.0/immodules.cache"}
            }),
        ];
        let gtk4_steps = vec![json!({
            "type": "gtk_update_icon_cache",
            "path": {"base": "homebrew_prefix", "path": "share/icons/hicolor"}
        })];
        let (gtk3_id, gtk3, gtk3_artifact) = pkg("gtk+3", vec![], gtk3_steps, 10);
        let (gtk4_id, gtk4, gtk4_artifact) = pkg("gtk4", vec![], gtk4_steps, 10);
        let manifest = manifest(
            vec![
                (gtk3_id.clone(), gtk3, gtk3_artifact),
                (gtk4_id.clone(), gtk4, gtk4_artifact),
            ],
            vec![gtk3_id.clone(), gtk4_id.clone()],
        );
        let workset = InstallWorkSet {
            satisfied: vec![],
            rename: Vec::new(),
            install: vec![gtk3_id, gtk4_id],
        };

        let plan = execution_plan(&manifest, &workset, &Prefix(PathBuf::from("/tmp/glu")));

        let icon_cache_id =
            "cache_postinstall:gtk_update_icon_cache:%2Ftmp%2Fglu%2Fshare%2Ficons%2Fhicolor";
        assert!(plan.node(icon_cache_id).is_some());
        assert!(plan.edges.contains(&ExecEdge::new(
            "formula_postinstall:gtk+3",
            icon_cache_id,
            "cache_postinstall_contributor"
        )));
        assert!(plan.edges.contains(&ExecEdge::new(
            "formula_postinstall:gtk4",
            icon_cache_id,
            "cache_postinstall_contributor"
        )));
        assert!(plan.node(
            "cache_postinstall:gtk_query_immodules_3:%2Ftmp%2Fglu%2Flib%2Fgtk-3.0%2F3.0.0%2Fimmodules.cache"
        ).is_some());
    }

    #[test]
    fn graph_models_deferred_global_postinstall_fan_in() {
        let step = json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        });
        let (pkg_id, package, artifact) = pkg("glib-user", vec![], vec![step], 10);
        let manifest = manifest(
            vec![(pkg_id.clone(), package, artifact)],
            vec![pkg_id.clone()],
        );
        let workset = InstallWorkSet {
            satisfied: vec![],
            rename: Vec::new(),
            install: vec![pkg_id],
        };

        let plan = execution_plan(&manifest, &workset, &Prefix(PathBuf::from("/tmp/glu")));

        let cache_id =
            "cache_postinstall:compile_gsettings_schemas:%2Ftmp%2Fglu%2Fshare%2Fglib-2.0%2Fschemas";
        assert!(plan.node(cache_id).is_some());
        assert!(plan.edges.contains(&ExecEdge::new(
            "formula_postinstall:glib-user",
            cache_id,
            "cache_postinstall_contributor"
        )));
        assert!(plan.edges.contains(&ExecEdge::new(
            cache_id,
            "registry_write:glib-user",
            "registry_after_cache_postinstall"
        )));
    }
}
