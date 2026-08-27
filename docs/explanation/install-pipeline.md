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
- package names, aliases, old names, versions, and keg versions as safe path components;
- artifact URL, SHA-256, and cellar fields;
- supported structured postinstall steps;
- absence of unsupported unstructured postinstall behavior.

Unsupported behavior fails early.

## 3. Plan local work

The planner compares the manifest with installed state.

Plain `glu install` resolves the requested selector to a package key and treats any installed version of that identity as satisfied. It records or promotes canonical intent in `glu.json` without upgrading that package.

Force-style modes (`update`, `reinstall`, `install --force`) repour roots. Dependency satisfaction is requirement-based: an installed dependency can be reused when its version/revision satisfies the edge's built-against floor.

Old-name matches become explicit rename work.

The client returns this resolved plan to the CLI. `--plan` renders it and stops without taking the mutation lock or running recovery. Execution mode asks for confirmation when the plan requires it, then passes the same plan back to the client. JSON mode never prompts; it returns a typed `confirmation_required` failure unless `--yes` approved the plan.

## 4. Build the DAG

Packages that need work become execution nodes. A typical package path is:

```txt
download → prepare → link/postinstall → receipt write
```

Other nodes cover setup/authentication, old-name transitions, and coalesced global-cache postinstall work.

Already satisfied packages do not get download or prepare nodes. They still participate as dependency-ready anchors for ordering.

## 5. Download and verify

Each artifact remains one DAG node. For a known-size artifact, the transfer runtime may divide the byte range into segments and schedule their HTTP attempts through one install-wide request coordinator. This keeps ordinary request concurrency bounded independently of the number of active artifact nodes.

Download priority comes from known downstream tail cost. The scheduler propagates a small catalog of measured prepare and postinstall costs backward through the DAG. Downloads that release a known expensive tail receive request capacity first; operations without an explicit hint contribute zero cost.

Retryable transport failures, interrupted bodies, early EOF, HTTP 408, HTTP 429, and server errors use bounded backoff. Known-size segments resume at the first unwritten byte after validating the range response. Unknown-size responses restart as whole-object attempts because a suffix cannot be validated safely without a trusted size.

Shared health telemetry distinguishes a broad interruption from one pathological connection. A localized straggler can receive a bounded emergency hedge for its remaining suffix; one ambiguous final stream can receive a diagnostic attempt. The original request stays available as a fallback, hedge bytes remain isolated, and speculative traffic never inflates logical progress.

Only the complete artifact SHA-256 supplied by the registry is a trust boundary. The client verifies the complete result before atomic cache admission. Cached artifacts are verified again during prepare; a corrupt cache hit is deleted, redownloaded, and retried once.

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

Active packages get stable opt links and public prefix links under roots such as `bin`, `sbin`, `lib`, `include`, `share`, `etc`, `var`, and `Frameworks`.

Deactivated packages keep stable opt links but skip public prefix projection.

Linking owns destination classification. It distinguishes missing paths, same-target links, links into this keg, links into another live keg, stale keg links, non-keg links, real files, and real directories. This keeps retries safe and prevents unrelated prefix state from being removed.

Mutable configuration defaults are copied as real files rather than linked back into kegs.

## 8. Run package-local postinstall

Package-local postinstall runs during commit, after package files are visible and before the package is complete.

Postinstall is structured. The client validates supported step types before execution and refuses unsupported manual Ruby postinstall behavior.

Execution happens through a sandboxed worker model on macOS. The worker receives a sanitized environment and a generated sandbox profile that grants expected package, prefix, temp, cache, and setup access while protecting sensitive locations.

## 9. Coalesce global caches

Some postinstall work updates shared global caches. Running those rebuilds once per package is slow and produces unnecessary repeated writes.

The postinstall subsystem can defer compatible global-cache requests, group contributors, and run a coalesced cache rebuild after all contributing packages are ready.

This localizes complexity: package formulas express setup needs, postinstall planning turns those needs into package-local work plus efficient shared work, and the rest of the installer sees only DAG nodes and edges.

## 10. Write installed state

A package counts as installed only after required commit and package-local postinstall work succeed. Complete receipts are written at that boundary.

Incomplete receipts and staging roots are interrupted-install state. Mutating commands clean them before planning.

## 11. Trace

The install writes a trace containing the planned graph and observed runtime events. Trace writing is separate from execution decisions: it explains the run after the fact without becoming the scheduler.

## Failure model

On failure, the scheduler stops dispatching new dependent work and lets safe in-flight work drain. Failed packages are not marked complete. Roots whose dependencies failed are not reported as successful. Shared postinstall work does not run for packages that did not commit.

The original error is preserved in command output and trace output.
