# Tracing

Install traces are local JSON records of planned and observed install execution.

They support debugging, performance analysis, and trace viewing. Trace commands do not upload traces. The schema matters for source work, but it is not a stable external API.

## Location

Trace files live under the active prefix:

```txt
<prefix>/var/glu/traces/
```

Install trace filenames have this shape:

```txt
trace-YYYYMMDD-HHMMSS-<plan>-<id>.json
```

`last.json` points to the most recent trace. On Unix it is a symlink to the latest trace filename.

## Top-level schema

Current trace schema version: `3`.

A trace is a JSON object:

```json
{
  "schema_version": 3,
  "status": "ok",
  "error": null,
  "plan": "vips+jq",
  "nodes": [],
  "edges": [],
  "events": []
}
```

Fields:

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | number | Trace schema version. |
| `status` | `"ok" | "failed"` | Overall execution status. |
| `error` | string or null | Failure or interruption text when present. |
| `plan` | string | Human-readable plan name, usually requested package names joined with `+`. |
| `nodes` | array | Planned execution nodes. |
| `edges` | array | Planned dependencies between nodes. |
| `events` | array | Observed runtime events. |

Planned nodes without runtime events can appear in failed or interrupted runs.

## Node schema

A node represents one planned unit of execution.

```json
{
  "id": "bottle_prepare:vips",
  "kind": "bottle_prepare",
  "pool": "prepare",
  "formula": "vips",
  "label": null,
  "package_id": "pkg:homebrew/core/vips@8.19.0",
  "inputs": {},
  "outputs": {},
  "subphases": ["extract", "writer_wait", "text_relocate", "fixed_prefix_relocate", "macho_patch", "codesign"],
  "priority": 12.3
}
```

Fields:

| Field | Type | Meaning |
|---|---|---|
| `id` | string | Opaque node identifier unique within the trace. Join by exact ID. |
| `kind` | string | Node kind. |
| `pool` | string | Scheduler pool. |
| `formula` | string or null | Formula/package name when applicable. |
| `label` | string or null | Display label for nodes with opaque IDs. |
| `package_id` | string or null | Registry package ID when applicable. |
| `inputs` | object | Planned input facts for the node. Shape depends on kind. |
| `outputs` | object | Planned output facts for the node. Shape depends on kind. |
| `subphases` | string[] | Expected subphase names. |
| `priority` | number | Opaque scheduler/debug value. Its ordering semantics can differ by node kind and implementation version. |

## Node kinds

Current node kinds include:

| Kind | Meaning |
|---|---|
| `ghcr_auth` | Acquire/configure install token for needed GHCR repositories. |
| `ghcr_bottle_download` | Fetch or reuse a package artifact. |
| `bottle_prepare` | Extract, relocate, patch, and sign a bottle into a staged keg. |
| `keg_link` | Commit and link a prepared keg. |
| `keg_link_existing` | Use an already existing keg as the commit anchor. |
| `keg_rename_existing` | Transition an installed old-name keg to the current name. |
| `formula_postinstall` | Run package-local structured postinstall. |
| `cache_postinstall` | Run coalesced shared/global cache postinstall work. |
| `registry_write` | Write package receipt state. |

A `ghcr_bottle_download` node represents the complete artifact operation. Runtime segments, resumed attempts, and emergency hedges remain beneath that node and do not appear as additional DAG nodes. Schema version 3 does not persist per-attempt transfer history.

## Pools

Pools model execution resources and correctness boundaries:

| Pool | Meaning |
|---|---|
| `setup` | One-time setup/authentication work. |
| `download` | Network/cache artifact work. |
| `prepare` | Extraction, relocation, Mach-O mutation, signing. |
| `commit` | Prefix mutation and package-local postinstall boundary. |
| `postinstall` | Deferred shared postinstall/cache work. |
| `registry` | Serialized local receipt writes. |

The pool name in traces is historical and user-facing. Source uses typed pool/kind enums to avoid stringly-typed scheduler decisions.

## Prepare subphases

`bottle_prepare` can record subphase events:

| Subphase | Meaning |
|---|---|
| `extract` | Archive decoding, tar processing, extraction safety checks, and inline filesystem metadata work. |
| `writer_wait` | Backpressure while reserving writer memory, enqueueing extraction writes, or waiting for this package's queued extraction writes to finish. |
| `text_relocate` | Text placeholder/prefix relocation. |
| `fixed_prefix_relocate` | Fixed build-prefix byte replacement. |
| `macho_patch` | Mach-O load-command relocation. |
| `codesign` | Ad-hoc signing of mutated Mach-O files. |

Subphase events explain time inside a node. Whole-node events remain the main timeline bars.

## Edge schema

An edge describes planned ordering:

```json
{
  "from": "bottle_prepare:vips",
  "to": "keg_link:vips",
  "reason": "phase_order"
}
```

Fields:

| Field | Type | Meaning |
|---|---|---|
| `from` | string | Source node ID. |
| `to` | string | Target node ID. |
| `reason` | string | Human-readable reason for the dependency. |

Common reasons include phase order, package dependency order, shared postinstall contributors, cache rebuild ordering, and serialized state writes.

Edges are planned constraints. Events show what actually ran.

## Event schema

An event records observed runtime timing for a whole node or subphase.

```json
{
  "node_id": "bottle_prepare:vips",
  "phase": "macho_patch",
  "start": 0.341,
  "end": 0.512,
  "status": "ok",
  "pool": "prepare",
  "slot": 1
}
```

Fields:

| Field | Type | Meaning |
|---|---|---|
| `node_id` | string | Node this event belongs to. |
| `phase` | string or null | Subphase name, or null for whole-node event. |
| `start` | number | Seconds from execution start. |
| `end` | number | Seconds from execution start. |
| `status` | string | Event status, usually `ok` or `error`. |
| `pool` | string | Pool that ran the work. |
| `slot` | number or null | Concurrency token within the pool. |

Whole-node events use `phase: null`. Subphase events use a string phase.

## Interpreting traces

Use nodes and edges for the planned graph. Use events for observed execution.

A node in `nodes` with no matching whole-node event was planned but did not run. This can happen when a failure or interruption stopped dependent work.

A failed trace can still be useful because it preserves the original plan, partial event stream, and error text.

## Critical path

A viewer can derive an observed wall-clock critical path from events and edges:

1. Find the whole-node event that ended last.
2. Walk backward through predecessor nodes whose whole-node events ended before the current node started.
3. Include same-pool predecessor events when worker serialization explains the start time.
4. Repeat until no predecessor explains the current start.

This is the observed critical path for one run, not scheduler priority or theoretical graph depth.

## Trace commands

Use:

```sh
glu trace list
glu trace view <target>
```

The CLI can resolve targets as recent trace IDs, filenames, paths, or plan names depending on command behavior. Exact CLI behavior is discoverable through `glu help --json`.

## Bug reports

A useful trace-backed report includes:

- command run;
- terminal output;
- trace file or trace ID;
- `glu status` output;
- whether the run was interrupted, retried, or affected by manual prefix changes.
