# Documentation

Public user documentation lives on the website:

```txt
https://glu.run/docs
```

This repository keeps codebase documentation for readers of the client source. It explains what individual functions cannot show alone: how the parts fit together, which invariants matter, and where the source code remains the ground truth.

## Principles

The codebase is organized around three principles:

1. **Simple mental model / localized complexity** — each subsystem owns its complexity and exposes a small, predictable contract.
2. **Relentless performance and efficiency** — avoid redundant file reads, directory walks, graph scans, process work, and network work; fuse passes where it keeps the implementation clear.
3. **Correctness and security** — match upstream behavior where compatibility requires it, make intentional divergences explicit, verify local work, and fail closed when correctness is uncertain.

## Explanation

Read these to understand how the system works:

- [Architecture](./explanation/architecture.md)
- [Install pipeline](./explanation/install-pipeline.md)
- [State model](./explanation/state-model.md)
- [Security and correctness](./explanation/security-and-correctness.md)

## Reference

Use these for source-level lookup:

- [CLI behavior](./reference/cli-behavior.md)
- [Registry contract](./reference/registry-contract.md)
- [Homebrew compatibility boundary](./reference/homebrew-compatibility.md)
- [Version ordering](./reference/version-ordering.md)
- [Tracing](./reference/tracing.md)

For exact command shape, use:

```sh
glu help --json
```

For exact registry OpenAPI, use:

```txt
https://registry.glu.run/openapi.json
https://glu.run/docs/api
```

Historical plans, reviews, and raw implementation notes are intentionally not tracked in the public repository.
