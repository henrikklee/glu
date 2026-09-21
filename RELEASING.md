# Releasing glu

This document describes how contributors validate and publish glu releases.

## Release model

`main` is the integration branch. A green `main` commit is eligible to become a
release candidate, but pushing to `main` does not publish a release.

Two GitHub Actions workflows enforce the process:

- **CI** runs for pull requests and pushes to `main`. It validates source,
  licenses, the production build, packaging, installation, upgrades, and the
  publication script.
- **Release** runs manually or for tags matching `v*.*.*`. A manual run retains
  inspection artifacts without publishing. A tag run publishes only after its
  release gate passes.

The publishing job is the only job with `contents: write`. It depends on the
complete release gate, so a failed check cannot publish a release.

## Version and artifact conventions

Cargo packages and the registry use plain semantic versions such as `0.1.2`.
Git tags use a `v` prefix such as `v0.1.2`.

A release publishes three assets:

```text
glu-aarch64-apple-darwin.tar.gz
glu-aarch64-apple-darwin.tar.gz.sha256
install.sh
```

The archive contains:

```text
LICENSE-BSD-2-Clause
LICENSE-MIT
THIRD_PARTY_LICENSES.html
THIRD_PARTY_NOTICES.md
glu
```

The archive is reproducible for identical input files. The workflow attests the
archive and the raw executable separately.

## Production build contract

The workspace owns the production profile:

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

Release builds use default features and write `target/release/glu`.
Development registry overrides use `cargo build-dev` and write
`target/dev-registry/release/glu`. Build guards prevent a development-feature
build from replacing the production binary.

Daily local testing uses `cargo build-local`, which writes `target/local/glu`.
The local profile keeps production behavior but uses faster build settings.
Packaging reads only the release-profile artifact.

## Local release validation

Install the pinned Rust toolchain from `rust-toolchain.toml`, ShellCheck,
Actionlint, and the pinned license tool:

```sh
brew install actionlint shellcheck
cargo install cargo-about --version 0.9.2 --locked --features cli
```

Run the same checks used by CI:

```sh
scripts/lint-release.sh
scripts/check-third-party-licenses.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/test-dev-build-isolation.sh
scripts/test-upgrade.sh
scripts/test-publish-release.sh
scripts/test-release.sh 0.1.2
```

`test-release.sh` builds the production executable, checks its architecture and
compiled policy, packages it, installs from those exact bytes, and packages it
a second time to prove reproducibility.

If `Cargo.lock` or dependency licensing changes, regenerate and review the
report before rerunning the checks:

```sh
scripts/generate-third-party-licenses.sh
scripts/check-third-party-licenses.sh
```

Do not hand-edit `THIRD_PARTY_LICENSES.html`.

## Publishing a release

Run the Release workflow manually against the intended commit and inspect its
retained artifacts. After the candidate passes validation, create and push its
version tag:

```sh
git tag v0.1.2 <validated-commit-sha>
git push origin v0.1.2
```

The tagged workflow reruns the complete release gate and publishes only after
every preceding step succeeds. Do not move a published tag. If validation
exposes a source defect, fix it and select a new release candidate.

## Independent verification

Download the release archive and extract a second copy of `glu`, then run:

```sh
gh attestation verify glu-aarch64-apple-darwin.tar.gz -R henrikklee/glu
gh attestation verify glu -R henrikklee/glu
gh release verify v0.1.2 -R henrikklee/glu
gh release verify-asset v0.1.2 glu-aarch64-apple-darwin.tar.gz -R henrikklee/glu
```

Also confirm:

```sh
shasum -a 256 -c glu-aarch64-apple-darwin.tar.gz.sha256
tar -tzf glu-aarch64-apple-darwin.tar.gz
./glu --version
file ./glu
```

The executable must report the expected version and be an ARM64 Mach-O
executable.

## Installation verification

After publication, verify both the stable installer endpoint and one real
package installation in an isolated prefix. Confirm that `glu status` reports
the production registry and distribution URLs and that `glu upgrade` recognizes
the installed release as current.
