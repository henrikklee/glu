# glu – fast and friendly package management for humans and agents

> [!NOTE]
> **Brand artwork placeholder**
>
> Wide product artwork or logo lockup for GitHub light and dark themes.

glu is an ultra-fast package manager for Apple Silicon Macs, built on Homebrew's trusted ecosystem of prebuilt packages.

[Read the docs →](https://glu.run/docs/)

## Why glu

Package installation is a graph problem. glu resolves the complete graph in one request, then overlaps downloads, preparation, linking, and final setup to finish the whole install sooner.

- **Fast end to end.** glu is designed to be up to 3x faster across large dependency graphs, optimizing the complete installation rather than one isolated phase.
- **More than 8,000 packages from day one.** glu uses Homebrew's mature package ecosystem instead of starting a new catalog.
- **No metadata-update ritual.** Relevant commands resolve against the current registry as part of the operation.
- **Declarative package state.** `glu.json` records the packages you chose; glu manages the dependencies they need.
- **Built for humans and agents.** Terminal output stays concise, while plans, JSON envelopes, schemas, and a machine-readable command manifest give tools a strict interface.
- **Inspectable by default.** Every install records a detailed trace of scheduling, downloads, preparation, linking, and shared work.

> [!NOTE]
> **Benchmark proof placeholder**
>
> Comparison chart with hardware, macOS version, package sets, dependency
> counts, cache conditions, and a link to the reproducible methodology.

## Install

glu supports macOS on Apple Silicon.

```sh
curl -fsSL https://glu.run/install | bash
```

### Upgrade

Upgrade glu to the latest version:

```sh
glu upgrade
```

## Quick start

> [!TIP]
> **Let your agent introduce glu**
>
> Ask your coding agent:
>
> ```text
> Run `glu help --json`. Explain what makes glu useful and show me how to
> manage packages safely. Do not make any changes.
> ```
>
> The command is read-only and publishes glu's complete command surface as structured JSON.

Install your first package:

```sh
glu install jq
```

Install a small development setup:

```sh
glu install ripgrep neovim tree node
```

List the packages you chose, then inspect the complete dependency graph:

```sh
glu list
glu list --tree
```

Remove a package:

```sh
glu remove jq
```

Continue with the [full Quick Start](https://glu.run/docs/quick-start/).

## How glu works

```text
resolve → download → prepare → link → postinstall → record state
```

The registry returns one complete install manifest containing package versions, dependency edges, artifact URLs, checksums, linking metadata, and structured postinstall work. With the graph available up front, glu can run independent work concurrently, prioritize packages that unlock expensive downstream operations, commit in dependency order, and coalesce shared setup.

glu installs packages under `/opt/glustore`, separate from Homebrew's prefix. It verifies artifacts, performs compatibility-sensitive relocation and signing, confines local filesystem work, and fails closed when it cannot complete a transformation safely.

### Every install explains itself

Open the latest install trace:

```sh
glu trace view
```

The built-in viewer renders the timeline and dependency graph together, making bottlenecks, critical work, failures, and idle gaps visible.

> [!NOTE]
> **Trace viewer placeholder**
>
> Real trace screenshot with parallel timeline lanes, the dependency graph,
> and recognizable packages from a large installation.

Read more about the [install pipeline](https://glu.run/docs/concepts/install-pipeline/), [performance](https://glu.run/docs/concepts/performance/), and [traces](https://glu.run/docs/reference/traces/).

## Design principles

### Keep complexity bounded

Fast orchestration requires real machinery: concurrent execution, fused work, critical-path scheduling, indexed registry data, caching, state transitions, and strict output contracts. glu keeps that complexity inside focused subsystems with narrow responsibilities.

### Optimize the time users wait

glu optimizes the time between entering a command and using the installed software. It overlaps independent work, prioritizes costly downstream paths, coalesces shared operations, and removes repeated reads, scans, and process launches from the critical path.

### Earn compatibility

glu implements package behavior in Rust with close reference to mature upstream sources. Compatibility-sensitive work is cited, tested, and reviewed so faster execution preserves expected package behavior and filesystem state.

## Common commands

| Task                                            | Command                     |
| ----------------------------------------------- | --------------------------- |
| Install packages                                | `glu install <name...>`     |
| Sync from `glu.json`                            | `glu install`               |
| Update declared packages                        | `glu update`                |
| Update the complete declared dependency closure | `glu update --all`          |
| Remove a package                                | `glu remove <name>`         |
| List declared packages                          | `glu list`                  |
| Show the installed dependency tree              | `glu list --tree`           |
| Inspect a package                               | `glu info <name>`           |
| Explain why a package is installed              | `glu why <name>`            |
| Preview a change                                | `glu install <name> --plan` |
| Inspect the latest trace                        | `glu trace view`            |
| Check local setup                               | `glu status`                |

Run `glu help <command>` for human-readable help. Run `glu help --json` for the machine-readable command manifest, or read the [CLI reference](https://glu.run/docs/reference/cli/).

## Development

```sh
cargo build-local
./target/local/glu --version
```

Contributor documentation:

- [Codebase documentation](docs/README.md)
- [Architecture](docs/explanation/architecture.md)
- [Install pipeline](docs/explanation/install-pipeline.md)
- [State model](docs/explanation/state-model.md)
- [Security and correctness](docs/explanation/security-and-correctness.md)
- [CLI behavior](docs/reference/cli-behavior.md)
- [Registry contract](docs/reference/registry-contract.md)
- [Release process](RELEASING.md)

User documentation is available at [glu.run/docs](https://glu.run/docs/).

## License

Original glu source is available under the MIT License. `glu-client` also contains portions adapted from Homebrew under the BSD 2-Clause License. See [`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-BSD-2-Clause`](LICENSE-BSD-2-Clause), and [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

The generated dependency license report is in [`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html).

glu is an independent project and is not affiliated with or endorsed by Homebrew or Apple Inc.
