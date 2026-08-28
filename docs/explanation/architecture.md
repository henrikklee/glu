# Architecture

`glu` is a macOS package manager client backed by a registry that resolves package graphs and supplies install manifests.

The architecture localizes complexity behind small contracts: the registry returns a complete manifest, the planner computes work, the orchestrator executes a DAG, state code owns durable records, and traces explain what happened.

## System boundary

The registry owns package metadata. The client owns local execution.

```txt
registry: resolve package topology, dependency requirements, artifacts, install facts
client:   plan, validate, download, prepare, link, postinstall, record, trace
```

This keeps the client model small: start with a complete manifest, compute local work, execute that work, and record what happened.

The registry is a trust root for metadata and checksums. The client still validates manifest shape, path safety, supported behavior, transport policy, artifact digests, and local filesystem boundaries.

## Main flow

A mutating package command has this shape:

```txt
parse with clap and validate command capabilities
acquire the prefix lock and recover interrupted state for execution
load local state and resolve the package graph
validate client support and compute one plan
render or confirm that plan in the CLI
execute the same plan through the DAG
write receipts and declaration changes
remove approved dangling packages
write the trace
return a typed result for final CLI presentation
```

Plan mode stops before the lock, recovery, or mutation steps. The CLI owns confirmation, final stdout and stderr, diagnostics, and exit status. Client operations return typed results and emit live progress or notices through an invocation-scoped event sink.

Each phase has a narrow boundary:

- Clap owns syntax and the command descriptor owns product capabilities;
- resolve returns a complete graph;
- planning returns a workset and future state;
- the DAG describes executable work;
- the orchestrator runs nodes and records typed lifecycle events;
- state code is the only durable-state authority;
- the CLI presentation host selects human, JSON, NUL, or raw output once.

## Complete manifests

The client asks the registry for the whole package closure before installing. It does not recursively discover packages while executing.

That design gives the planner and scheduler all facts up front:

- selected roots;
- direct dependency topology and separate package-level requirement maps;
- artifact URLs, sizes, and SHA-256 digests;
- install metadata;
- structured postinstall facts.

It also gives plan mode the same input as execution. `--plan` is the command's computed state transition without mutation, not a separate approximation.

## Planning boundary

Planning compares:

- `glu.json`, the user's intent and activation state;
- complete local receipts, the installed truth;
- the registry manifest, the desired graph.

The planner produces the meaningful transition: install, keep, promote, rename, update, remove, or report.

Plain `glu install` is idempotent by installed root name. It records or promotes intent without upgrading an already installed package. Update, reinstall, and force modes are the paths that repour roots.

## Execution boundary

Execution is a directed acyclic graph. Nodes represent concrete work such as:

- GHCR authentication;
- artifact download;
- bottle prepare;
- keg link or old-name transition;
- formula postinstall;
- coalesced cache postinstall;
- receipt write.

Edges encode correctness constraints: phase order, package dependencies, shared cache contributors, and serialized receipt writes.

The orchestrator does not rediscover work. It executes the graph, records runtime events, and preserves failure information for traces.

## Concurrency model

Work is divided into pools:

- setup;
- download;
- prepare;
- commit;
- postinstall;
- registry.

Downloads and prepare work can overlap because they do not make packages visible. Commit is the boundary where files become part of the prefix and dependencies become available to dependents.

Prepare has two shared resource services beneath its DAG pool. Signing workers own CPU-heavy Mach-O parsing and hashing. A separate writer pool owns physical extraction and sparse signature writes, with one concurrency limit and byte budget across all concurrently prepared packages. This prevents package-local signing work from creating an unbounded second set of APFS writers.

The scheduler operates at two levels. The DAG keeps one node per artifact, while the transfer runtime can divide a known-size artifact into segments and run retries or emergency hedges beneath that node. Segments and attempts are runtime details; they do not expand the execution plan or weaken the artifact-level digest boundary.

At the DAG level, download order is based on known downstream tail cost. A small, version-controlled catalog assigns costs only to operations that maintainers have measured and reviewed. Those costs propagate backward through the graph, so a download that unlocks expensive prepare or postinstall work can run before one whose remaining branch is cheap. Unlisted operations contribute zero rather than receiving speculative estimates.

At the transfer level, ordinary attempts from every artifact share one request coordinator. When requests queue, the coordinator preserves the DAG's tail-aware order. Emergency hedges are separately bounded and start after health telemetry demonstrates a localized pathological connection. One ambiguous final stream can receive a bounded diagnostic attempt; ordinary CDN rate variation does not justify duplicate traffic.

## State boundary

Installed state is not derived from one-off filesystem reads. The state subsystem owns durable state:

- `glu.json` for user intent and deactivation state;
- package-local receipts for installed truth.

Other subsystems ask the state store for snapshots or request explicit state writes. They do not independently interpret receipts or mutate declarations.

## Postinstall boundary

Postinstall is compatibility-sensitive by nature. `glu` localizes that complexity inside structured postinstall planning, validation, sandbox workers, and deferred global-cache handling.

The installer sees a bounded contract: package-local postinstall runs during commit; shared global cache work can be coalesced and run later; unsupported behavior fails before execution.

## Trace boundary

Every executed install produces a local trace. A trace records planned nodes, edges, events, pools, slots, subphases, and failures.

Traces make concurrent execution understandable after the fact without making execution code depend on presentation logic.
