# Security and correctness

`glu` is fast without making correctness implicit. The source makes compatibility assumptions visible, verifies local work, and fails closed when it cannot prove a safe result.

This document explains the client's security and correctness posture. It does not claim that package contents are untrusted-safe.

## Trust model

`glu` trusts Homebrew as the upstream package source and trusts the official `glu` registry to supply package metadata, dependency edges, artifact URLs, checksums, install metadata, and structured postinstall facts.

This is not a malicious-package containment boundary. Package contents are executable code, supported postinstall steps may invoke package tools, and installed tools later run with the user's privileges. A compromise of the trusted package source is therefore equivalent to arbitrary code execution. The client does not impose artifact-size, expansion-ratio, output, or execution-time policies that diverge from Homebrew merely to limit what trusted package code could already do. It still uses checked ranges and fallible allocations so accidental malformed metadata fails normally instead of aborting the process.

The client verifies the local work it performs:

- registry transport policy;
- manifest support and path safety;
- artifact SHA-256;
- archive extraction boundaries;
- relocation completion;
- link destination ownership;
- postinstall structure and sandboxing;
- receipt boundaries.

The registry is a metadata trust root. The client is the local correctness boundary.

## Registry and transport policy

Published clients use a fixed compile-time HTTPS registry origin. Development registry and prefix overrides require the explicit development build feature.

Registry URL validation is centralized. HTTPS is required for normal builds; loopback HTTP is allowed only for development builds.

## Artifact verification

Artifacts are verified before cache admission and again when cached bytes are reused during prepare.

A corrupt reused cache hit is evicted, redownloaded, and retried once. Verification is fused into paths that already read bytes where practical, matching the performance principle without weakening the boundary.

## Path safety

Manifest-derived path components are validated before use. Package names, aliases, old names, versions, and keg versions must be safe single path components.

Receipts are also confined. A tampered receipt must not be able to drive removal outside the prefix Cellar.

This is a core correctness pattern: the registry is trusted for metadata, but metadata is still checked before it controls local paths.

## Archive extraction

Extraction rejects unsafe hardlink targets and avoids writing through symlinks created earlier in the archive. Archive setuid/setgid bits are stripped.

Large tar header size claims must not force unbounded allocation.

## Relocation correctness

Prepare adapts upstream artifacts to the active prefix. It owns text relocation, fixed-prefix relocation, Mach-O load-command relocation, and ad-hoc signing of mutated Mach-O files.

Relocation is fail-closed. If required placeholders or build-prefix bytes remain where they indicate a missed transformation, prepare fails instead of recording a broken package as installed.

The prepare path is in-process Rust. Its normal relocation/signing work does not rely on `otool`, `install_name_tool`, or `codesign` subprocesses.

For ad-hoc signing, the signing library still produces the complete canonical output. The client derives sparse byte ranges from that output and applies them through the shared writer pool. Tests compare the result byte-for-byte with the full-rewrite signer output and validate the resulting macOS signature. A source-size change before the queued write fails closed. Any interrupted sparse update remains inside an uncommitted staging keg, which interrupted-install cleanup removes.

## Homebrew-derived behavior and Ruby provenance

Much of the compatibility-sensitive behavior mirrors Homebrew Ruby code. That provenance must stay strict.

When Rust ports or mirrors Homebrew behavior, the code records enough upstream source and reasoning for future reviewers to answer:

- Which Homebrew behavior is being matched?
- Which upstream file or method motivated it?
- Is this a faithful port or an intentional divergence?
- What changed upstream since this was written?

This matters most for postinstall and relocation, where small compatibility differences can silently break packages.

Homebrew-derived implementations need ongoing review against current upstream behavior. A correct port can become stale when Homebrew changes its Ruby implementation, formula metadata shape, sandbox assumptions, or postinstall conventions.

## Linking correctness

Linking projects package files into the shared prefix. It must not remove unrelated prefix state or clobber another live package's projection.

Important invariants:

- stable opt links are created for installed packages;
- public prefix links are written only after destination classification;
- linked markers are written last;
- failed projection rolls back this package's partial links;
- unlink removes only links that resolve into the target package;
- shared directories are pruned only when empty;
- mutable config defaults are real files and survive package removal.

Destination classification is the local complexity boundary. The rest of the installer does not need to know every symlink/materialization case.

## Postinstall sandboxing

Postinstall is structured, validated, and run through a sandboxed worker model on macOS.

The worker environment strips sensitive keys and dynamic-loader variables, clears `HOMEBREW_PATH`, and runs under a generated sandbox profile. The profile grants expected write access for package, prefix, temp, cache, and setup paths while protecting sensitive locations.

Unsupported manual Ruby postinstall behavior is refused. The client does not execute arbitrary upstream Ruby as an escape hatch.

Deferred global-cache work may require broader prefix write allowances because stable cache paths can resolve through prefix links into package directories. That complexity belongs in the postinstall subsystem and remains visible in review.

## Shell setup and upgrade safety

Shell setup rewrites managed blocks atomically: create-new temporary file, preserve mode when possible, fsync, rename, and best-effort parent fsync. If an rc file is a symlink, the write follows the symlink target.

`glu upgrade` verifies release asset SHA-256 sidecars, checks that the downloaded binary reports the registry-confirmed version, stages the replacement with create-new semantics, and atomically swaps the installed binary.

Checksum sidecars provide integrity against corruption from the same distribution channel. Release authenticity depends on the final distribution and attestation mechanism.

## External differential validation

glu is validated with a separate macOS lab that compares Homebrew and glu without shipping the privileged runner in this repository.

The lab runs real package install sequences in clean Homebrew and glu states, snapshots both final states, and reviews the normalized differences. Its harness parks the user's current installations, rebuilds the standard prefixes, then restores the parked installations. For that reason it is a supervised privileged workflow and must not be automated by a coding agent. The default suite is cumulative: each package is installed into the state left by previous packages, so shared caches, postinstall side effects, link state, and dependency interactions are exercised together.

The lab records artifacts for review, including installed package lists, prefix trees, file manifests, Mach-O summaries, command probes, logs, timings, and public-prefix archives. Review compares the behaviorally relevant surface:

- installed package/version lists;
- normalized public prefix tree shape;
- entry type, mode, size, and symlink target;
- normalized content hashes for safe files;
- Mach-O load commands, rpaths, and codesign verification status;
- package-specific command probe output.

Known expected differences are normalized explicitly, such as Homebrew runtime files, Homebrew receipts versus glu receipts, tool-owned state directories, generated cache bytes, SBOM metadata, and Mach-O code/signature bytes after prefix relocation.

A clean run over a broad real-bottle suite is a strong release signal because it exercises package closures, shared caches, native code, postinstall behavior, linking, and probes together. The external lab produces evidence and artifacts for review; it is not a substitute for source review or a public CI gate.

## Review posture

Security and correctness work prefers narrow boundaries over broad trust:

- validate before mutation;
- keep local state ownership centralized;
- preserve upstream provenance comments;
- make intentional divergences explicit;
- fail closed on unsupported behavior;
- add trace and external differential coverage for complex behavior.
