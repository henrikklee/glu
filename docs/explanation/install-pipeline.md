# Install pipeline

The install path has a simple outer contract:

```txt
compute one plan → render or approve it in the CLI → execute that plan → present one typed result
```

That boundary is deliberate. Planning owns decisions, the CLI owns confirmation and final output, the orchestrator owns execution, state code owns durable records, and tracing owns observability. Live progress and notices travel through an invocation-scoped event sink instead of selecting a renderer or printing final output inside the client.

## 1. Resolve

The client asks the registry for a complete install manifest for the requested names and target.

The manifest contains the package closure, dependency edges, artifact facts, install metadata, and structured postinstall facts. Resolve errors happen before any local mutation.

## 2. Validate support

The client validates the manifest before prefix mutation. Validation covers:

- prefix compatibility, including fixed-cellar equal-length requirements;
- complete root and dependency bindings, with every concrete package ID agreeing with its stable package key;
- package names, aliases, old names, versions, and keg versions as safe path components;
- artifact URL, SHA-256, and cellar fields;
- supported structured postinstall steps;
- absence of unsupported unstructured postinstall behavior.

Unsupported behavior fails early.

## 3. Plan local work

The planner compares the manifest with installed state.

Plain `glu install` resolves the requested selector to a package key and treats any installed version of that identity as satisfied. It records or promotes canonical intent in `glu.json` without upgrading that package.

Named and bare updates reconcile selected roots while retaining dependencies that still satisfy the active requirement. They still repour a root when its persisted topology or package facts changed, including added, rebound, or dropped providers. The simulated post-update receipt graph identifies dependencies made dangling by those changes before execution. `update --all` selects the complete declared-root closure. Reinstall and force modes repour the requested roots or closure according to their command flags.

Dependency satisfaction is evaluated in installer-root context. The planner carries one selected bottle's complete flattened requirement map through that package's topology closure. Satisfied intermediate packages are not selected for installation, but traversal continues through their children. A dependency selected for installation is expanded again with its own bottle's requirement map. If multiple roots reach the same package, any context that requires replacement wins.

Old-name matches become explicit rename work.

The client returns this resolved plan to the CLI. `--plan` renders it and stops without taking the mutation lock or running recovery. Execution mode asks for confirmation when the plan requires it, then passes the same plan back to the client. JSON mode never prompts; it returns a typed `confirmation_required` failure unless `--yes` approved the plan.

## 4. Build the DAG

Packages that need work become execution nodes. A typical package path is:

```txt
download → prepare → link/postinstall → receipt write
```

Other nodes cover setup/authentication, old-name transitions, and coalesced global-cache postinstall work.

Already satisfied packages do not get download or prepare nodes. They still participate as dependency-ready anchors for ordering. When an installer-root context selects a transitive package below a satisfied intermediate, the selected transitive package directly gates the dependent's commit.

## 5. Download and verify

Each selected package artifact reference remains one DAG download node. The DAG computes static downstream-tail priority; the install-wide downloader uses that priority whenever ordinary request capacity becomes available. Known-size large artifacts are generated as lazy fixed-size ranges, while small artifacts remain whole requests. This keeps request concurrency independent of both active DAG nodes and artifact size. If references share a digest, their independent random staging files safely converge on the same verified cache entry.

Four independently constructed HTTP clients use normal ALPN negotiation rather than forcing a protocol. They retain separate connection pools beneath one global logical request budget. Initial ranges explore the pools; subsequent ranges select the pool with the lowest projected work-to-observed-capacity ratio. Useful body bytes increase a pool's demonstrated capacity, assigned-but-unreceived bytes represent its outstanding work, and cancellation or failure removes the abandoned remainder. This lets proven fast pools accept more streams without imposing per-pool fairness or introducing a separate health classifier.

An individual request can fail without failing the requested artifact. Request and response-body failures, interrupted bodies, early EOF, and unsuccessful HTTP statuses use capped exponential backoff and retry for as long as the installation remains active. Known-size ranges continue at the first unwritten byte after validating the exact `206` response. Unknown-size responses restart as whole-object attempts because a suffix cannot be validated safely without a trusted size.

Emergency recovery is restricted to the highest-priority artifact at its final one or two ranges. A range with no useful progress or a genuinely pathological remaining-time estimate can race one suffix request through a separate HTTP client, temporarily exceeding ordinary admission through a small global allowance. The first exact complete suffix wins, the loser is cancelled, and that range cannot launch another emergency. Healthy bulk traffic and elapsed duration alone never trigger speculation.

Every request lifecycle and control decision records structured diagnostics: queue admission, selected transport pool, pool load and capacity inputs, response origin, headers, negotiated protocol, first body byte, sampled progress, failure chain, retry backoff, emergency evidence, winner, and cancellation. The normal event timeline also retains `wait_first_byte`, `body_write`, `verify`, and `sync` download subphases. Diagnostics are passive observability and never select transfer behavior.

Only the complete artifact SHA-256 supplied by the registry is a trust boundary. The client creates a private random staging file exclusively, retains its descriptor through writes, verification, and synchronization, checks that the staging pathname still identifies that inode, and only then atomically admits it to the cache. Cached artifacts are verified again during prepare; a corrupt cache hit is deleted, redownloaded, and retried once.

## 6. Prepare

Prepare transforms a verified artifact into a staged keg.

It owns:

- archive extraction;
- extraction safety checks;
- text relocation;
- fixed-prefix relocation;
- Mach-O load-command relocation;
- ad-hoc signing of mutated Mach-O files;
- fail-closed checks for unresolved relocation bytes.

Prepare can run before runtime dependencies are committed because prepared files are not installed state and are not public prefix state.

Extraction and code-signing writes share one install-wide writer pool and memory budget. Signing workers compute the canonical signed Mach-O bytes, compare them with the relocated staged file, and enqueue only changed load-command and signature ranges. This preserves the signer's exact output without rewriting unchanged executable code pages or bypassing the filesystem concurrency limit.

## 7. Commit and link

Commit moves a prepared keg into the Cellar and projects it into the prefix.

The default prefix is:

```txt
/opt/glustore
```

Active global packages get stable opt links and public prefix links under roots such as `bin`, `sbin`, `lib`, `include`, `share`, `etc`, `var`, and `Frameworks`.

Isolated packages always keep stable opt links but skip public prefix projection according to their registry policy. Deactivated packages also skip public projection, but deactivation remains a separate user-controlled state.

Linking owns destination classification. It distinguishes missing paths, same-target links, links into this keg, links into another live keg, stale keg links, non-keg links, real files, and real directories. This keeps retries safe and prevents unrelated prefix state from being removed.

Mutable configuration defaults are copied as real files rather than linked back into kegs.

## 8. Run package-local postinstall

Package-local postinstall runs during commit, after package files are visible and before the package is complete.

Postinstall is structured. The client validates supported step types before execution and refuses unsupported manual Ruby postinstall behavior.

Execution happens through a sandboxed worker model on macOS. The generated profile denies writes by default, then permits the package, prefix, temporary, cache, and setup locations required for compatibility. The worker uses a temporary `HOME`, a system-first `PATH`, and Homebrew-compatible filtering of credential-like and dynamic-loader environment keys.

This is a write-confinement and correctness boundary, not a confidentiality boundary for malicious package code. Compatibility exceptions can leave non-enumerated home paths readable, environment filtering is not a complete secret allowlist, caller `PATH` entries remain available after system paths, and network access follows package metadata with Homebrew's allow-by-default fallback.

## 9. Coalesce global caches

Some postinstall work updates shared global caches. Running those rebuilds once per package is slow and produces unnecessary repeated writes.

The postinstall subsystem can defer compatible global-cache requests, group contributors, and run a coalesced cache rebuild after all contributing packages are ready.

This localizes complexity: package formulas express setup needs, postinstall planning turns those needs into package-local work plus efficient shared work, and the rest of the installer sees only DAG nodes and edges.

## 10. Write installed state

A package counts as installed only after required commit and package-local postinstall work succeed. Complete receipts are written at that boundary. They persist exact dependency selectors, the package's typed requirement map, and its complete exposure policy including the normalized reason. A later state load resolves selectors against the complete installed package set to rebuild provider bindings.

Incomplete receipts and staging roots are interrupted-install state. Mutating commands clean them before planning.

## 11. Trace

The install writes a trace containing the planned graph and observed runtime events. Trace writing is separate from execution decisions: it explains the run after the fact without becoming the scheduler.

## Failure model

On failure, the scheduler stops dispatching new dependent work and lets safe in-flight work drain. Failed packages are not marked complete. Roots whose dependencies failed are not reported as successful. Shared postinstall work does not run for packages that did not commit.

The original error is preserved in command output and trace output.
