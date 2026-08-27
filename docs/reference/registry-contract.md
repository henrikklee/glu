# Registry contract

This repository contains the client. The registry is a separate service.

For exact OpenAPI details, use:

```txt
https://registry.glu.run/openapi.json
https://glu.run/docs/api
```

This document records the client-side assumptions that matter while reading or changing the client. It does not duplicate the OpenAPI specification.

## Role

The registry is the client's metadata trust root. It supplies:

- package identities and versions;
- dependency edges and requirement floors;
- artifact URLs and SHA-256 digests;
- artifact sizes when known;
- install metadata;
- structured postinstall facts;
- package staleness information;
- reverse dependency data;
- display metadata for info queries.

The client validates and executes the manifest. The registry does not decide which client capabilities are implemented.

The initial identity-preserving response schemas are `glu.resolve.v1`, `glu.uses.v1`, `glu.info.v1`, and `glu.outdated.v1`.

## Transport policy

Published clients use the fixed compiled registry origin. Development overrides are feature-gated.

The registry client validates base URLs centrally. HTTPS is required except loopback HTTP in development builds.

## Resolve

Resolve turns requested package selectors into a complete install manifest for the target.

Conceptually:

```txt
GET /v1/resolve?name=vips&name=ripgrep&target=arm64_sequoia
```

The full response is an install manifest. It is not a generic upstream metadata dump.

The response contains the complete closure needed for planning and execution. Installation does not recursively fetch package metadata after execution starts.

Each root binds three distinct facts:

- `requested_as`: the selector supplied by the caller;
- `package_key`: stable package identity across releases;
- `package`: the concrete package ID selected for this response.

The client resolves a selector at this boundary and uses package keys or concrete package IDs afterward.

## Slim resolve

Read-only commands may use slim resolve:

```txt
GET /v1/resolve?...&slim=true
```

Slim responses contain graph shape and minimal package records. They are for display and dependency queries. They must not be treated as install manifests.

## Dependency edges

Every dependency edge contains:

- `requested_as`: the dependency spelling declared by the parent;
- `package_key`: the stable identity of the target;
- `package`: the concrete target selected in this response;
- `requires`: the built-against version and revision floor.

Graph traversal uses `package_key`. Full and slim resolve use `package` to join an edge to the selected package record. The original spelling is retained for diagnostics and provenance; it does not define topology.

The client uses the requirement floor for dependency satisfaction. An installed dependency can be reused when its version and revision are at or above the floor.

## Package identity

The wire contract keeps four concepts separate:

| Field | Meaning |
|---|---|
| `package_key` | Stable package identity across releases. |
| package map key / `package` | One concrete resolved release. |
| `requested_as` | A caller or dependency selector, including aliases and old names. |
| `name` | Canonical display name. |

Package records advertise selector aliases and old names. The planner validates that each selector has one owner and that every root and dependency edge agrees with its target package record.

Old-name transitions are explicit install work. They are not reconstructed independently by individual commands.

Full resolve also supplies `install.opt_names`. These are required filesystem opt-link names. They are separate from selector aliases even when the current registry happens to publish the same spelling in both sets.

## Info

`/v1/info` returns display metadata for a package and target:

```txt
GET /v1/info?name=vips&target=arm64_sequoia
```

The client may combine registry info with local installed receipts for command output.

## Outdated

`/v1/outdated` returns staleness envelopes for requested names and target.

Each package has:

- `update`: newest version installable on the target, or null;
- `latest`: newest visible version overall.

`update` is what package update commands can install. `latest` can be ahead when the newest visible version is not installable for the target.

A zero-name outdated request may also carry latest-client-version metadata for `glu upgrade`.

## Uses

`/v1/uses` answers reverse dependency questions for the registry graph:

```txt
GET /v1/uses?name=pcre2&target=arm64_sequoia
GET /v1/uses?name=pcre2&target=arm64_sequoia&direct=true
```

It is the registry-wide counterpart to local `why`:

- `why` uses installed receipts;
- `uses` uses registry dependency data.

## Errors

Registry errors are structured so the client can produce useful messages.

Important cases include:

- unknown package name;
- known package unavailable for the target;
- no compatible artifact for the target;
- unsupported target;
- dependency-origin failures that identify the requested root that led to the failure.

Suggestions, when present, contain useful alternatives rather than tautological echoes.

## Caching expectation

Install resolve responses are live answers. The client does not install from stale cached graph data unless a future protocol adds explicit freshness validation.

Read-only responses can be cached only under a registry-provided freshness model.
