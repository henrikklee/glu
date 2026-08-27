# glu

<!--
BRAND HERO PLACEHOLDER
Wide product artwork or logo lockup, prepared for GitHub light and dark themes.
-->

**Fast, friendly package management for humans and agents.**

glu is an ultra-fast package manager for Apple Silicon Macs. It builds on
Homebrew's trusted, mature ecosystem of prebuilt packages, making more than
8,000 packages available from day one while installing them up to 3x faster,
especially across large dependency graphs.

A single request resolves the complete package graph. glu then overlaps
downloads with installation and prioritizes the work that gets the entire
install finished sooner. It optimizes the time that matters: from running the
command to using the installed software.

<!--
BENCHMARK PROOF PLACEHOLDER
Add the comparison chart, hardware and macOS version, package sets, dependency
counts, cache conditions, and a link to the reproducible methodology.
-->

## A package manager that gets out of the way

glu does not stop to update a local package database before it can install
something. The registry keeps package data indexed and ready, so the client can
resolve the whole job in one request and start useful work immediately.

The default output is clean and compact: the plan, the work in progress, and the
result. It does not bury routine installs in endless logs. Power-user flags add
dependency trees, plans, scopes, and detailed traces when you need them.

For automation, the package-management command set exposes consistent JSON
success and error envelopes, stable result identities, and versioned JSON
Schemas. `glu help --json --schemas` publishes the contract directly from the
same command model used by the CLI. It is closer to an OpenAPI contract than a
collection of output formats that scripts have to reverse-engineer.

## Why glu

- **Fast end to end.** glu optimizes the complete installation, not one isolated
  download or extraction benchmark.
- **More than 8,000 packages from day one.** It uses Homebrew's established
  package and bottle ecosystem instead of starting a new catalog.
- **Modern package semantics.** Familiar commands, explicit plans, clear
  dependency scopes, declarative state, and useful power-user controls.
- **Built for humans and agents.** Readable terminal output and strict machine
  protocols come from the same command model.

## Built on three principles

### Keep complexity bounded

Fast orchestration requires real machinery: concurrent execution, fused work,
critical-path scheduling, indexed registry data, caching, state transitions,
and a command model with strict output contracts.

glu keeps that complexity inside typed subsystems with narrow
responsibilities. Each part owns a bounded problem and exposes a small contract
to the rest of the client. The implementation can be sophisticated without
making the product or codebase impossible to reason about.

### Optimize the time users wait

Performance work starts with the complete time between entering a command and
using the installed software.

glu resolves the graph in one request, overlaps independent work, prioritizes
packages that unlock costly downstream work, coalesces shared operations, and
removes repeated reads, scans, and process launches from the critical path.
Detailed install traces make the next bottleneck visible and keep performance
work grounded in real end-to-end installs.

### Earn compatibility

glu implements package behavior in Rust with close reference to mature upstream
sources. Compatibility-sensitive work is cited, tested, and reviewed against
hundreds of real package artifacts and behaviors.

Every optimized path is reviewed against the same compatibility evidence, so
faster execution preserves expected package behavior and filesystem state.
Intentional differences are documented and tested explicitly.

## Declare what you want

`glu.json` plays a role similar to `package.json`: it records the packages you
chose separately from packages installed only as dependencies, without acting
as a rigid lockfile.

## Every install explains itself

glu records a detailed trace of scheduling, downloads, installation, and shared
work. The built-in trace viewer renders the timeline and dependency graph
together, making bottlenecks, critical work, and idle gaps visible.

<!--
TRACE VIEWER PLACEHOLDER
Add a real trace screenshot with parallel timeline lanes, the dependency DAG,
and recognizable packages from a large installation.
-->

The default prefix is `/opt/glustore`.

## Command line

```text
glu install <name...>              aliases: i, add
glu reinstall [--deps] <name...>
glu update [name...]               alias: up
glu remove <selector...>           aliases: rm, uninstall
glu autoremove
glu deactivate <name...>           alias: unlink
glu activate [--force] <name...>   alias: link
glu list                           alias: ls
glu outdated
glu deps <name>                    alias: d
glu why <name>
glu uses <name>
glu info <name>                    alias: view
glu setup
glu status                         alias: doctor
glu trace [view|list]
glu upgrade
glu help [command]
```

Run `glu help <command>` for command options. Run `glu help --json` for the
machine-readable command manifest.

Mutation commands support plans and non-interactive confirmation. Commands
with structured output expose JSON or null-delimited modes instead of requiring
scripts to parse decorated terminal output. Human progress output stays on the
terminal and does not contaminate piped output.

## Correctness boundaries

glu verifies bottle hashes before committing an installation. It performs
Mach-O relocation and ad-hoc signing in process, validates postinstall steps,
and records install traces for inspection. Development registry and prefix
overrides are compiled out of release builds.

Homebrew remains the upstream source for package artifacts and relevant install
semantics. glu intentionally owns resolution, scheduling, local state, output,
and automation contracts.

## Build from source

The repository pins Rust `1.98.0`.

```sh
cargo build-local
./target/local/glu --version
```

The local profile keeps optimized production behavior while favoring fast
incremental rebuilds. The distribution profile adds fat LTO, one codegen unit,
and symbol stripping. Both use panic abort. Development-only registry overrides
must use `cargo build-dev`; the build configuration prevents them from replacing
the production artifact.

## Documentation

- [Codebase documentation](docs/README.md)
- [Architecture](docs/explanation/architecture.md)
- [Install pipeline](docs/explanation/install-pipeline.md)
- [State model](docs/explanation/state-model.md)
- [Security and correctness](docs/explanation/security-and-correctness.md)
- [CLI behavior](docs/reference/cli-behavior.md)
- [Registry contract](docs/reference/registry-contract.md)
- [Release process](RELEASING.md)

User documentation is available under [glu.run/docs](https://glu.run/docs).

## License

Original glu source is available under the MIT License. `glu-client` also
contains portions adapted from Homebrew under the BSD 2-Clause License. See
[`LICENSE-MIT`](LICENSE-MIT),
[`LICENSE-BSD-2-Clause`](LICENSE-BSD-2-Clause), and
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

The generated dependency license report is in
[`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html).

glu is an independent project and is not affiliated with or endorsed by
Homebrew or Apple Inc.
