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
- dependency queries project one authority into identity-bearing nodes and edges;
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

Dependency reuse is context-sensitive. For each package being expanded as an installer root, the planner applies that package's complete selected-artifact requirement map throughout its topology closure. It continues through satisfied intermediates, and packages selected for installation are expanded again with their own maps. Selection from multiple contexts is merged before dependency ordering and DAG construction.

Plain `glu install` is idempotent by stable installed root identity. It records or promotes intent without upgrading an already installed package. Update, reinstall, and force modes are the paths that repour roots. This product policy and the supported bottle-install scope are recorded in the [Homebrew compatibility boundary](../reference/homebrew-compatibility.md).

## Query projection boundary

Dependency queries never reduce graph nodes to display strings. The shared projection retains each stable `PackageKey`, selected `PackageId`, canonical name, concrete version, and exact edge `requested_as` selector until presentation. Flat and repeated-node deduplication uses `PackageKey`; JSON graph node IDs use `PackageId`.

Installed queries project only the receipt-backed installed graph. Registry queries project only full resolve, slim resolve, or uses responses. These sources share projection and rendering code but are never merged: they answer different questions and retain separate authority.

Forward human and NUL output uses the exact selector recorded on each dependency edge. Reverse output names the dependent package while structured output retains the reversed edge's original selector. Local installed, declared, deactivated, and link annotations join by `PackageKey`, not by a requested or displayed name.

Registry graphs fail closed when roots or dependency edges contradict their package IDs and stable keys. Slim resolves are closed graphs, so every dependency provider must be present. Uses responses are sparse reverse graphs and may omit unrelated forward providers, but an edge claiming an included stable key must reference that key's included package ID. Requirement floors remain typed on edges and are formatted only by presentation code.

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

The install executor and downloader have separate, narrow responsibilities. The DAG keeps one download node per selected package artifact reference and computes that node's static downstream-tail priority. After the install-scoped GHCR authentication node, each artifact download resolves its authenticated GHCR endpoint without following the redirect and immediately starts using the resulting signed CDN URL. Resolution is concurrent across active artifact nodes and is not an install-wide startup barrier. The downloader owns that ephemeral URL, request admission, runtime parts, retry backoff, and the exceptional critical-tail race beneath the node. Ranges and attempts are runtime details; they do not expand the execution plan or weaken the artifact-level digest boundary. Independent references to the same digest use separate random staging files and safely converge on the same verified cache entry.

Download order is based on known downstream tail cost. A small, version-controlled catalog assigns costs only to operations that maintainers have measured and reviewed. Those costs propagate backward through the graph, so a download that unlocks expensive prepare or postinstall work can run before one whose remaining branch is cheap. Unlisted operations contribute zero rather than receiving speculative estimates.

One install-wide downloader gives newly available ordinary request capacity to the highest-priority queued range. By default, artifacts larger than 1 MiB are divided into balanced parts targeting 1 MiB. Request concurrency is transport policy, not the DAG's historical artifact-node lane count.

Ordinary attempts use four independently constructed HTTP clients with four request slots each. Each client owns a separate connection pool and uses normal ALPN negotiation. Pending ranges have no pool identity: one central priority queue pairs its highest-priority waiter with any available pool-tagged slot. Returning a lease immediately gives that exact slot to the next waiter, so faster pools naturally complete more ranges while a slow pool can hold at most four ordinary assignments. No throughput estimate, health classifier, preassignment, or background allocator selects a pool.

An individual HTTP attempt may fail, but a retryable network outcome does not fail its artifact. Safely committed bytes remain in place and the exact suffix returns to the same priority queue after capped backoff. A small independent emergency allowance is reserved for pathological work on the highest-priority artifact near the artifact or install tail. One rescue per affected range can run at a time, with at most two rescues globally. A failed or pathological rescue can be replaced through a fresh HTTP client; the first completed attempt wins and cancels its competitor. Healthy bulk traffic and elapsed duration alone cannot create speculative requests.

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
