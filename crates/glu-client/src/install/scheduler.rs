use crate::{
    error::RuntimeErrorCode,
    events::{ExecutionEvents, NodeCompletionStatus, OutputStream, SilentExecutionEvents},
    hash::Sha256Mismatch,
    install::dag::{ExecNode, ExecPool, ExecutionPlan, NodeKind},
};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::task::JoinSet;

#[derive(Debug, Clone)]
pub struct RuntimeEvent {
    pub node_id: String,
    pub phase: Option<String>,
    pub start: f64,
    pub end: f64,
    pub status: String,
    /// Pipeline stage this event belongs to (the token pool it ran in):
    /// `setup` | `download` | `prepare` | `commit` | `postinstall` | `registry`.
    pub pool: String,
    /// Concurrency token index within `pool` (which parallel slot ran it),
    /// assigned by the scheduler at dispatch. `None` only when unknowable
    /// (e.g. subphases recorded without a slot during testing).
    pub slot: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeSubphase<'a> {
    pub node_id: &'a str,
    pub phase: &'a str,
    pub start: f64,
    pub end: f64,
    pub status: &'a str,
    pub pool: &'a str,
    pub slot: Option<usize>,
}

#[derive(Clone)]
pub struct ExecutionContext {
    inner: Arc<ExecutionContextInner>,
}

struct ExecutionContextInner {
    started: Instant,
    verbose: bool,
    output_events: Arc<dyn ExecutionEvents>,
    events: Mutex<Vec<RuntimeEvent>>,
    artifacts: Mutex<BTreeMap<String, Value>>,
}

impl ExecutionContext {
    pub fn new(verbose: bool) -> Self {
        Self::with_events(verbose, Arc::new(SilentExecutionEvents))
    }

    pub fn with_events(verbose: bool, output_events: Arc<dyn ExecutionEvents>) -> Self {
        Self {
            inner: Arc::new(ExecutionContextInner {
                started: Instant::now(),
                verbose,
                output_events,
                events: Mutex::new(Vec::new()),
                artifacts: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub fn now(&self) -> f64 {
        self.inner.started.elapsed().as_secs_f64()
    }

    pub fn log(&self, message: &str) {
        if self.inner.verbose {
            self.inner
                .output_events
                .notice(OutputStream::Stderr, &format!("glu: {message}"));
        }
    }

    pub fn record_event(&self, event: RuntimeEvent) {
        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .push(event);
    }

    pub fn record_subphase(&self, subphase: RuntimeSubphase<'_>) {
        self.record_event(RuntimeEvent {
            node_id: subphase.node_id.to_string(),
            phase: Some(subphase.phase.to_string()),
            start: subphase.start,
            end: subphase.end,
            status: subphase.status.to_string(),
            pool: subphase.pool.to_string(),
            slot: subphase.slot,
        });
    }

    pub fn put_artifact(&self, key: impl Into<String>, value: Value) {
        self.inner
            .artifacts
            .lock()
            .expect("artifacts lock poisoned")
            .insert(key.into(), value);
    }

    pub fn artifact(&self, key: &str) -> Option<Value> {
        self.inner
            .artifacts
            .lock()
            .expect("artifacts lock poisoned")
            .get(key)
            .cloned()
    }

    pub fn events(&self) -> Vec<RuntimeEvent> {
        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .clone()
    }

    pub fn artifacts(&self) -> BTreeMap<String, Value> {
        self.inner
            .artifacts
            .lock()
            .expect("artifacts lock poisoned")
            .clone()
    }
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self::new(false)
    }
}

pub trait InstallOperations: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        node: &'a ExecNode,
        ctx: ExecutionContext,
    ) -> BoxFuture<'a, Result<()>>;
}

pub trait ExecutionObserver: Send + Sync + 'static {
    fn node_started(&self, node: &ExecNode);

    fn node_completed(&self, node: &ExecNode, status: NodeCompletionStatus) {
        let _ = (node, status);
    }
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub plan: ExecutionPlan,
    pub events: Vec<RuntimeEvent>,
    pub artifacts: BTreeMap<String, Value>,
    /// Full chain (via `{:?}`) of whatever node failed, if any. Kept as data on `InstallResult`
    /// rather than propagated as `Err` so a trace can always be built and written regardless of
    /// how the run ended — including when it was interrupted (see `install/mod.rs`, which races
    /// this against `tokio::signal::ctrl_c()` using the same `ExecutionContext`).
    pub error: Option<String>,
    pub failure: Option<ExecutionFailure>,
}

#[derive(Debug, Clone)]
pub struct ExecutionFailure {
    pub node_id: String,
    pub kind: String,
    pub phase: String,
    pub package_id: Option<String>,
    pub package: Option<String>,
    pub code: RuntimeErrorCode,
    pub error: String,
}

impl InstallResult {
    pub fn trace_dict(&self, plan_name: &str) -> Value {
        let graph = self.plan.as_trace_graph();
        serde_json::json!({
            "schema_version": 3,
            "status": if self.error.is_some() {
                "failed"
            } else {
                "ok"
            },
            "error": self.error,
            "plan": plan_name,
            "nodes": graph["nodes"],
            "edges": graph["edges"],
            "events": self.events.iter().map(|event| serde_json::json!({
                "node_id": event.node_id,
                "phase": event.phase,
                "start": event.start,
                "end": event.end,
                "status": event.status,
                "pool": event.pool,
                "slot": event.slot,
            })).collect::<Vec<_>>(),
        })
    }
}

/// A node failed mid-run: no new work is dispatched, in-flight tasks drain,
/// and the error names what failed. `what` is the human label — `package
/// <name>` for per-package nodes, `node <id>` for setup/cache nodes. `source`
/// is absent when the failure's stderr was already surfaced as a labeled
/// notice — the headline must not re-describe the cause.
#[derive(Debug, thiserror::Error)]
#[error("installing {what} failed")]
pub struct InstallExecutionError {
    pub what: String,
    pub failure: ExecutionFailure,
    #[source]
    pub source: Option<anyhow::Error>,
}

#[derive(Debug, thiserror::Error)]
#[error("install execution stalled with no ready/running nodes; pending: {pending}")]
pub struct InstallExecutionStall {
    pub pending: String,
}

pub async fn execute_plan(
    plan: ExecutionPlan,
    operations: Arc<dyn InstallOperations>,
    observer: Option<Arc<dyn ExecutionObserver>>,
    ctx: ExecutionContext,
) -> Result<InstallResult> {
    plan.validate()?;
    let mut runner = PlanRunner::new(plan.clone(), operations, ctx.clone(), observer);
    let (error, failure) = match runner.run().await {
        Ok(()) => (None, None),
        Err(err) => {
            let error = format!("{err:?}");
            let failure = err
                .downcast_ref::<InstallExecutionError>()
                .map(|install_error| install_error.failure.clone());
            (Some(error), failure)
        }
    };
    Ok(InstallResult {
        plan,
        events: ctx.events(),
        artifacts: ctx.artifacts(),
        error,
        failure,
    })
}

struct PlanRunner {
    plan: ExecutionPlan,
    operations: Arc<dyn InstallOperations>,
    ctx: ExecutionContext,
    observer: Option<Arc<dyn ExecutionObserver>>,
    nodes: BTreeMap<String, ExecNode>,
    deps_by_node: BTreeMap<String, BTreeSet<String>>,
    dependents_by_node: BTreeMap<String, BTreeSet<String>>,
    download_tail_cost: BTreeMap<String, u64>,
    max_download_tail_cost: u64,
    node_index: BTreeMap<String, usize>,
}

impl PlanRunner {
    fn new(
        plan: ExecutionPlan,
        operations: Arc<dyn InstallOperations>,
        ctx: ExecutionContext,
        observer: Option<Arc<dyn ExecutionObserver>>,
    ) -> Self {
        let nodes = plan
            .nodes
            .iter()
            .cloned()
            .map(|node| (node.id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let mut deps_by_node = plan
            .nodes
            .iter()
            .map(|node| (node.id.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let mut dependents_by_node = plan
            .nodes
            .iter()
            .map(|node| (node.id.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for edge in &plan.edges {
            deps_by_node
                .entry(edge.target.clone())
                .or_default()
                .insert(edge.source.clone());
            dependents_by_node
                .entry(edge.source.clone())
                .or_default()
                .insert(edge.target.clone());
        }

        let node_index = plan
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index))
            .collect::<BTreeMap<_, _>>();

        let mut tail_costs = BTreeMap::new();
        for node in &plan.nodes {
            known_downstream_tail(&node.id, &nodes, &dependents_by_node, &mut tail_costs);
        }
        let download_tail_cost = plan
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::GhcrBottleDownload)
            .map(|node| {
                (
                    node.id.clone(),
                    tail_costs.get(&node.id).copied().unwrap_or(0),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let max_download_tail_cost = download_tail_cost.values().copied().max().unwrap_or(0);

        Self {
            plan,
            operations,
            ctx,
            observer,
            nodes,
            deps_by_node,
            dependents_by_node,
            download_tail_cost,
            max_download_tail_cost,
            node_index,
        }
    }

    async fn run(&mut self) -> Result<()> {
        let pool_limits = self.pool_limits();
        let mut active_by_pool = pool_limits
            .keys()
            .map(|pool| (*pool, 0_usize))
            .collect::<BTreeMap<_, _>>();
        // Which concurrency token (slot) each pool has currently busy. Assigning
        // the lowest free token mirrors the scheduler's dispatch order and lets
        // the trace record exactly which parallel slot ran each node.
        let mut busy_slots: BTreeMap<ExecPool, BTreeSet<usize>> = BTreeMap::new();
        let mut slot_by_node: BTreeMap<String, (ExecPool, usize)> = BTreeMap::new();
        let mut done = BTreeSet::<String>::new();
        let mut remaining_deps = self
            .deps_by_node
            .iter()
            .map(|(id, deps)| (id.clone(), deps.len()))
            .collect::<BTreeMap<_, _>>();
        let mut ready_ids = remaining_deps
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        let mut active = BTreeSet::<String>::new();
        let mut tasks = JoinSet::<(String, Result<()>)>::new();
        let mut failure: Option<(String, anyhow::Error)> = None;

        while done.len() < self.nodes.len() {
            if failure.is_none() {
                for node in self.ready(&ready_ids, &active_by_pool, &pool_limits) {
                    if active_by_pool.get(&node.pool).copied().unwrap_or(0)
                        >= pool_limits.get(&node.pool).copied().unwrap_or(1)
                    {
                        continue;
                    }
                    let limit = pool_limits.get(&node.pool).copied().unwrap_or(1).max(1);
                    let free = busy_slots
                        .get(&node.pool)
                        .map(|busy| (0..limit).find(|s| !busy.contains(s)));
                    let slot = free.flatten().unwrap_or(0);
                    busy_slots.entry(node.pool).or_default().insert(slot);
                    slot_by_node.insert(node.id.clone(), (node.pool, slot));
                    // Stamp the slot on the node so the node-level event and any
                    // subphases (via the orchestrator) can attribute it.
                    let mut node = node;
                    node.slot = Some(slot);
                    ready_ids.remove(&node.id);
                    active.insert(node.id.clone());
                    *active_by_pool.entry(node.pool).or_default() += 1;
                    let operations = self.operations.clone();
                    let observer = self.observer.clone();
                    let ctx = self.ctx.clone();
                    tasks.spawn(async move { run_node(node, operations, observer, ctx).await });
                }
            }

            if tasks.is_empty() {
                if let Some((node_id, source)) = failure.take() {
                    return Err(self.install_error(node_id, source));
                }
                let pending = self
                    .nodes
                    .keys()
                    .filter(|id| !done.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(InstallExecutionStall { pending }.into());
            }

            let Some(joined) = tasks.join_next().await else {
                continue;
            };
            let (node_id, result) = joined?;
            done.insert(node_id.clone());
            active.remove(&node_id);
            if let Some(node) = self.nodes.get(&node_id) {
                if let Some(active) = active_by_pool.get_mut(&node.pool) {
                    *active = active.saturating_sub(1);
                }
                // Free the concurrency token this node occupied.
                if let Some((pool, slot)) = slot_by_node.get(&node_id) {
                    if let Some(busy) = busy_slots.get_mut(pool) {
                        busy.remove(slot);
                    }
                }
            }
            for dependent in self.dependents_by_node.get(&node_id).into_iter().flatten() {
                if let Some(count) = remaining_deps.get_mut(dependent) {
                    *count = count.saturating_sub(1);
                    if *count == 0 && !done.contains(dependent) && !active.contains(dependent) {
                        ready_ids.insert(dependent.clone());
                    }
                }
            }
            if let Err(err) = result {
                if failure.is_none() {
                    failure = Some((node_id, err));
                }
            }
        }

        if let Some((node_id, source)) = failure.take() {
            return Err(self.install_error(node_id, source));
        }
        Ok(())
    }

    /// Human label for the failing node: `package <name>` when the node is
    /// per-package (formula set), `node <id>` otherwise (setup, cache).
    fn install_error(&self, node_id: String, source: anyhow::Error) -> anyhow::Error {
        let what = self
            .nodes
            .get(&node_id)
            .and_then(|node| node.formula.as_ref())
            .map(|name| format!("package {}", name.0))
            .unwrap_or_else(|| format!("node {node_id}"));
        let source_text = format!("{source:?}");
        let failure = if let Some(node) = self.nodes.get(&node_id) {
            ExecutionFailure {
                node_id: node_id.clone(),
                kind: node.kind.as_str().to_string(),
                phase: node.kind.as_str().to_string(),
                package_id: node.package_id.as_ref().map(|id| id.0.clone()),
                package: node.formula.as_ref().map(|name| name.0.clone()),
                code: classify_install_node_error(node.kind, &source),
                error: source_text,
            }
        } else {
            ExecutionFailure {
                node_id: node_id.clone(),
                kind: "unknown".to_string(),
                phase: "unknown".to_string(),
                package_id: None,
                package: None,
                code: RuntimeErrorCode::PartialInstallFailure,
                error: source_text,
            }
        };
        // A postinstall failure whose stderr was already surfaced as a labeled
        // notice is reported by that notice; keep the headline to just what
        // failed instead of re-showing the cause chain.
        let source = if source
            .downcast_ref::<crate::postinstall::structured::NoticedPostinstallFailure>()
            .is_some()
        {
            None
        } else {
            Some(source)
        };
        InstallExecutionError {
            what,
            failure,
            source,
        }
        .into()
    }

    fn ready(
        &self,
        ready_ids: &BTreeSet<String>,
        active_by_pool: &BTreeMap<ExecPool, usize>,
        pool_limits: &BTreeMap<ExecPool, usize>,
    ) -> Vec<ExecNode> {
        let mut downloads = Vec::new();
        let mut non_downloads = Vec::new();
        for node in ready_ids
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .cloned()
        {
            if node.kind == NodeKind::GhcrBottleDownload {
                downloads.push(node);
            } else {
                non_downloads.push(node);
            }
        }
        non_downloads.sort_by(|a, b| self.priority_order(a, b));

        let download_slots = pool_limits
            .get(&ExecPool::Download)
            .copied()
            .unwrap_or(1)
            .saturating_sub(
                active_by_pool
                    .get(&ExecPool::Download)
                    .copied()
                    .unwrap_or(0),
            );

        non_downloads.extend(self.ready_downloads(downloads, download_slots));
        non_downloads
    }

    fn ready_downloads(&self, mut ready_downloads: Vec<ExecNode>, slots: usize) -> Vec<ExecNode> {
        ready_downloads.sort_by_key(|node| {
            (
                std::cmp::Reverse(self.download_tail_cost.get(&node.id).copied().unwrap_or(0)),
                self.node_index(node),
            )
        });
        ready_downloads.truncate(slots);
        for node in &mut ready_downloads {
            let tail = self.download_tail_cost.get(&node.id).copied().unwrap_or(0);
            node.priority = self.max_download_tail_cost.saturating_sub(tail) as f64;
        }
        ready_downloads
    }

    fn pool_limits(&self) -> BTreeMap<ExecPool, usize> {
        self.plan
            .nodes
            .iter()
            .map(|node| {
                (
                    node.pool,
                    self.plan.pools.get(&node.pool).copied().unwrap_or(1).max(1),
                )
            })
            .collect()
    }

    fn priority_order(&self, a: &ExecNode, b: &ExecNode) -> Ordering {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.node_index(a).cmp(&self.node_index(b)))
    }

    fn node_index(&self, node: &ExecNode) -> usize {
        self.node_index.get(&node.id).copied().unwrap_or(usize::MAX)
    }
}

fn classify_install_node_error(kind: NodeKind, error: &anyhow::Error) -> RuntimeErrorCode {
    if error.chain().any(|cause| cause.is::<Sha256Mismatch>()) {
        return RuntimeErrorCode::ChecksumMismatch;
    }
    match kind {
        NodeKind::GhcrAuth | NodeKind::GhcrBottleDownload => RuntimeErrorCode::DownloadFailed,
        NodeKind::BottlePrepare => RuntimeErrorCode::PrepareFailed,
        NodeKind::FormulaPostinstall | NodeKind::CachePostinstall => {
            RuntimeErrorCode::PostinstallFailed
        }
        NodeKind::KegLink
        | NodeKind::KegLinkExisting
        | NodeKind::KegRenameExisting
        | NodeKind::RegistryWrite => RuntimeErrorCode::LinkFailed,
    }
}

fn known_downstream_tail(
    node_id: &str,
    nodes: &BTreeMap<String, ExecNode>,
    dependents: &BTreeMap<String, BTreeSet<String>>,
    memo: &mut BTreeMap<String, u64>,
) -> u64 {
    if let Some(cost) = memo.get(node_id) {
        return *cost;
    }
    let downstream = dependents
        .get(node_id)
        .into_iter()
        .flatten()
        .map(|dependent| known_downstream_tail(dependent, nodes, dependents, memo))
        .max()
        .unwrap_or(0);
    let cost = nodes.get(node_id).map(known_node_cost).unwrap_or(0) + downstream;
    memo.insert(node_id.to_string(), cost);
    cost
}

#[derive(Clone, Copy)]
enum KnownCostSubject {
    Formula(&'static str),
    CacheKind(&'static str),
}

#[derive(Clone, Copy)]
struct KnownNodeCost {
    node_kind: NodeKind,
    subject: KnownCostSubject,
    milliseconds: u64,
}

// Maintainer-derived one-shot scheduling hints. Unlisted work has zero known cost.
// Add or revise entries here after validating representative install traces.
const KNOWN_NODE_COSTS: &[KnownNodeCost] = &[
    KnownNodeCost {
        node_kind: NodeKind::BottlePrepare,
        subject: KnownCostSubject::Formula("gcc"),
        milliseconds: 2_000,
    },
    KnownNodeCost {
        node_kind: NodeKind::BottlePrepare,
        subject: KnownCostSubject::Formula("llvm"),
        milliseconds: 4_000,
    },
    KnownNodeCost {
        node_kind: NodeKind::FormulaPostinstall,
        subject: KnownCostSubject::Formula("ca-certificates"),
        milliseconds: 5_000,
    },
    KnownNodeCost {
        node_kind: NodeKind::CachePostinstall,
        subject: KnownCostSubject::CacheKind("fontconfig_fc_cache"),
        milliseconds: 2_800,
    },
    KnownNodeCost {
        node_kind: NodeKind::CachePostinstall,
        subject: KnownCostSubject::CacheKind("gdk_pixbuf_query_loaders"),
        milliseconds: 15_000,
    },
];

fn known_node_cost(node: &ExecNode) -> u64 {
    KNOWN_NODE_COSTS
        .iter()
        .find(|known| {
            known.node_kind == node.kind
                && match known.subject {
                    KnownCostSubject::Formula(formula) => node
                        .formula
                        .as_ref()
                        .is_some_and(|candidate| candidate.0 == formula),
                    KnownCostSubject::CacheKind(kind) => node.inputs["kind"].as_str() == Some(kind),
                }
        })
        .map_or(0, |known| known.milliseconds)
}

async fn run_node(
    node: ExecNode,
    operations: Arc<dyn InstallOperations>,
    observer: Option<Arc<dyn ExecutionObserver>>,
    ctx: ExecutionContext,
) -> (String, Result<()>) {
    if let Some(observer) = &observer {
        observer.node_started(&node);
    }
    let start = ctx.now();
    let result = operations.execute(&node, ctx.clone()).await;
    let status = if result.is_err() {
        NodeCompletionStatus::Error
    } else {
        NodeCompletionStatus::Ok
    };
    if let Some(observer) = &observer {
        observer.node_completed(&node, status);
    }
    let end = ctx.now();
    if let Some(message) = describe_event(&node, status) {
        ctx.log(&message);
    }
    ctx.record_event(RuntimeEvent {
        node_id: node.id.clone(),
        phase: None,
        start,
        end,
        status: status.as_str().to_string(),
        pool: node.pool.as_str().to_string(),
        slot: node.slot,
    });
    (node.id, result)
}

/// One-line description of a completed node, printed when `--verbose` is set.
fn describe_event(node: &ExecNode, status: NodeCompletionStatus) -> Option<String> {
    let formula = node.formula.as_ref().map(|name| name.0.as_str());
    let input = |key: &str| node.inputs.get(key).and_then(Value::as_str).unwrap_or("");
    let message = match node.kind {
        NodeKind::GhcrAuth => "authenticating with registry".to_string(),
        NodeKind::GhcrBottleDownload => format!(
            "downloading bottle {} {} ({})",
            formula?,
            input("version"),
            input("tag")
        ),
        NodeKind::BottlePrepare => format!("extracting {} {}", formula?, input("version")),
        NodeKind::KegLink => format!("linked {}", formula?),
        NodeKind::KegLinkExisting => format!("already installed {}", formula?),
        NodeKind::KegRenameExisting => format!(
            "renamed installed {} to {}",
            input("old_name"),
            input("new_name")
        ),
        NodeKind::FormulaPostinstall => format!("installed {} -> {}", formula?, input("keg")),
        NodeKind::RegistryWrite => format!("registered {}", formula?),
        NodeKind::CachePostinstall => format!("cache postinstall {}", input("kind")),
    };
    Some(if status == NodeCompletionStatus::Error {
        format!("{message} (failed)")
    } else {
        message
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        events::{RecordedExecutionEvent, RecordingExecutionEvents},
        install::dag::{ExecEdge, ExecPool, NodeKind},
    };
    use futures_util::FutureExt;
    use glu_core::PackageName;
    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;

    #[test]
    fn verbose_execution_logs_use_the_event_sink() {
        let events = Arc::new(RecordingExecutionEvents::default());
        let context = ExecutionContext::with_events(true, events.clone());
        context.log("scheduled fixture");

        assert!(matches!(
            events.events().as_slice(),
            [RecordedExecutionEvent::Notice { stream: OutputStream::Stderr, message }]
                if message == "glu: scheduled fixture"
        ));
    }

    fn test_pool(name: &str) -> ExecPool {
        match name {
            "setup" => ExecPool::Setup,
            "download" => ExecPool::Download,
            "prepare" => ExecPool::Prepare,
            "commit" => ExecPool::Commit,
            "postinstall" => ExecPool::Postinstall,
            "registry" => ExecPool::Registry,
            other => panic!("unknown test pool {other}"),
        }
    }

    fn node(
        id: &str,
        kind: NodeKind,
        pool: &str,
        formula: Option<&str>,
        size: u64,
        priority: f64,
    ) -> ExecNode {
        ExecNode {
            id: id.to_string(),
            kind,
            pool: test_pool(pool),
            slot: None,
            package_id: None,
            formula: formula.map(|name| PackageName(name.to_string())),
            label: None,
            inputs: json!({ "size": size }),
            outputs: json!({}),
            subphases: vec![],
            priority,
        }
    }

    fn plan(nodes: Vec<ExecNode>, edges: Vec<ExecEdge>, pools: &[(&str, usize)]) -> ExecutionPlan {
        ExecutionPlan {
            nodes,
            edges,
            pools: pools.iter().map(|(k, v)| (test_pool(k), *v)).collect(),
        }
    }

    struct RecordingOps {
        seen: Arc<AsyncMutex<Vec<String>>>,
    }

    impl InstallOperations for RecordingOps {
        fn execute<'a>(
            &'a self,
            node: &'a ExecNode,
            _ctx: ExecutionContext,
        ) -> BoxFuture<'a, Result<()>> {
            async move {
                self.seen.lock().await.push(node.id.clone());
                Ok(())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn executor_respects_dependencies() {
        let seen = Arc::new(AsyncMutex::new(Vec::new()));
        let plan = plan(
            vec![
                node("a", NodeKind::GhcrAuth, "setup", None, 0, 0.0),
                node(
                    "b",
                    NodeKind::FormulaPostinstall,
                    "commit",
                    Some("b"),
                    0,
                    0.0,
                ),
            ],
            vec![ExecEdge {
                source: "a".to_string(),
                target: "b".to_string(),
                reason: "test".to_string(),
            }],
            &[("setup", 1), ("commit", 1)],
        );

        let result = execute_plan(
            plan,
            Arc::new(RecordingOps { seen: seen.clone() }),
            None,
            ExecutionContext::new(false),
        )
        .await
        .unwrap();

        assert!(result.error.is_none());
        assert_eq!(*seen.lock().await, vec!["a", "b"]);
    }

    #[test]
    fn ready_downloads_are_stable_without_known_tail_costs() {
        let auth = node(
            "ghcr_auth:install",
            NodeKind::GhcrAuth,
            "setup",
            None,
            0,
            0.0,
        );
        let big = node(
            "ghcr_bottle_download:big",
            NodeKind::GhcrBottleDownload,
            "download",
            Some("big"),
            1_000,
            0.0,
        );
        let dep = node(
            "ghcr_bottle_download:dep",
            NodeKind::GhcrBottleDownload,
            "download",
            Some("dep"),
            10,
            0.0,
        );
        let root = node(
            "ghcr_bottle_download:root",
            NodeKind::GhcrBottleDownload,
            "download",
            Some("root"),
            20,
            0.0,
        );
        let root_commit = node(
            "formula_postinstall:root",
            NodeKind::FormulaPostinstall,
            "commit",
            Some("root"),
            0,
            0.0,
        );
        let dep_commit = node(
            "formula_postinstall:dep",
            NodeKind::FormulaPostinstall,
            "commit",
            Some("dep"),
            0,
            0.0,
        );
        let plan = plan(
            vec![
                auth,
                big.clone(),
                dep.clone(),
                root.clone(),
                dep_commit,
                root_commit,
            ],
            vec![
                ExecEdge {
                    source: "ghcr_auth:install".into(),
                    target: big.id.clone(),
                    reason: "auth".into(),
                },
                ExecEdge {
                    source: "ghcr_auth:install".into(),
                    target: dep.id.clone(),
                    reason: "auth".into(),
                },
                ExecEdge {
                    source: "ghcr_auth:install".into(),
                    target: root.id.clone(),
                    reason: "auth".into(),
                },
                ExecEdge {
                    source: "formula_postinstall:dep".into(),
                    target: "formula_postinstall:root".into(),
                    reason: "formula_dependency_committed".into(),
                },
            ],
            &[("setup", 1), ("download", 2), ("commit", 1)],
        );
        let runner = PlanRunner::new(
            plan,
            Arc::new(RecordingOps {
                seen: Arc::new(AsyncMutex::new(Vec::new())),
            }),
            ExecutionContext::new(false),
            None,
        );
        let ready_ids = BTreeSet::from([
            "ghcr_bottle_download:big".to_string(),
            "ghcr_bottle_download:dep".to_string(),
            "ghcr_bottle_download:root".to_string(),
        ]);
        let active_by_pool = BTreeMap::from([
            (ExecPool::Download, 0),
            (ExecPool::Setup, 0),
            (ExecPool::Commit, 0),
        ]);
        let pool_limits = BTreeMap::from([
            (ExecPool::Download, 2),
            (ExecPool::Setup, 1),
            (ExecPool::Commit, 1),
        ]);
        let ready = runner.ready(&ready_ids, &active_by_pool, &pool_limits);

        assert_eq!(ready.len(), 2);
        assert_eq!(ready[0].id, "ghcr_bottle_download:big");
        assert_eq!(ready[1].id, "ghcr_bottle_download:dep");
    }

    #[test]
    fn known_cost_catalog_is_explicit() {
        let gcc = node(
            "bottle_prepare:gcc",
            NodeKind::BottlePrepare,
            "prepare",
            Some("gcc"),
            0,
            0.0,
        );
        let llvm = node(
            "bottle_prepare:llvm",
            NodeKind::BottlePrepare,
            "prepare",
            Some("llvm"),
            0,
            0.0,
        );
        let unknown = node(
            "formula_postinstall:unknown",
            NodeKind::FormulaPostinstall,
            "commit",
            Some("unknown"),
            0,
            0.0,
        );
        let mut cache = node(
            "cache_postinstall:gdk_pixbuf_query_loaders",
            NodeKind::CachePostinstall,
            "postinstall",
            None,
            0,
            0.0,
        );
        cache.inputs = json!({ "kind": "gdk_pixbuf_query_loaders" });

        assert_eq!(known_node_cost(&gcc), 2_000);
        assert_eq!(known_node_cost(&llvm), 4_000);
        assert_eq!(known_node_cost(&cache), 15_000);
        assert_eq!(known_node_cost(&unknown), 0);
    }

    /// Ops that sleep so nodes overlap in time, letting us observe concurrency
    /// token (slot) assignment and reuse across the download pool.
    struct SleepingOps;

    impl InstallOperations for SleepingOps {
        fn execute<'a>(
            &'a self,
            _node: &'a ExecNode,
            _ctx: ExecutionContext,
        ) -> BoxFuture<'a, Result<()>> {
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Ok(())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn assigns_and_reuses_concurrency_slots_per_pool() {
        // 4 downloads against a 2-token pool: the first two take slots 0 and 1;
        // the next two must reuse a freed token (0 or 1) — never slots 2+.
        let plan = plan(
            vec![
                node(
                    "d0",
                    NodeKind::GhcrBottleDownload,
                    "download",
                    Some("d0"),
                    0,
                    1.0,
                ),
                node(
                    "d1",
                    NodeKind::GhcrBottleDownload,
                    "download",
                    Some("d1"),
                    0,
                    1.0,
                ),
                node(
                    "d2",
                    NodeKind::GhcrBottleDownload,
                    "download",
                    Some("d2"),
                    0,
                    1.0,
                ),
                node(
                    "d3",
                    NodeKind::GhcrBottleDownload,
                    "download",
                    Some("d3"),
                    0,
                    1.0,
                ),
            ],
            vec![],
            &[("download", 2)],
        );
        let result = execute_plan(
            plan,
            Arc::new(SleepingOps),
            None,
            ExecutionContext::new(false),
        )
        .await
        .unwrap();

        let events = result.events;
        let node_events: Vec<&RuntimeEvent> = events.iter().filter(|e| e.phase.is_none()).collect();
        assert_eq!(node_events.len(), 4, "one node-level event per node");
        for e in &node_events {
            assert_eq!(e.pool, "download");
            assert!(
                matches!(e.slot, Some(s) if s <= 1),
                "download slot must be within the 2-token pool, got {:?}",
                e.slot
            );
        }

        // Peak concurrency never exceeds the two-token pool: no three
        // node-level download events may overlap in time.
        let mut slots = node_events
            .iter()
            .filter_map(|e| e.slot.map(|s| (e.start, e.end, s)))
            .collect::<Vec<_>>();
        slots.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut max_active = 0usize;
        let mut ends: Vec<f64> = Vec::new();
        for (start, end, _) in slots {
            ends.retain(|&e| e > start);
            let active = ends.len() + 1;
            max_active = max_active.max(active);
            ends.push(end);
        }
        assert!(
            max_active <= 2,
            "peak download concurrency {max_active} > pool 2"
        );

        // Both distinct tokens were actually used at peak (the pool fills).
        let used: std::collections::BTreeSet<Option<usize>> =
            node_events.iter().map(|e| e.slot).collect();
        assert!(used.len() >= 2, "expected both tokens to be exercised");
    }
}
