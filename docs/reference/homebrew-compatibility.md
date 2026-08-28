# Homebrew compatibility boundary

`glu` 0.1.0 implements Homebrew-compatible dependency behavior for stable, default-option, Apple Silicon macOS bottle installs. It does not claim compatibility with unrestricted Homebrew installation.

The source oracle for package-version ordering and dependency semantics is Homebrew commit:

```txt
da12368691a124f3e22a00a02e6587e4f148f8d0
```

## Within the boundary

For the supported bottle path, the registry and client preserve these behaviors:

- target-specific formula variations and `uses_from_macos` filtering;
- exact, `all`, then older-compatible macOS bottle selection from the current formula release;
- direct dependency topology kept separate from each selected bottle's flattened runtime requirement map;
- installer-root requirement contexts applied through the complete topology closure;
- traversal through satisfied intermediate dependencies;
- a selected dependency's own installer-root context;
- exact stable package identity for requirements, without alias guessing;
- Homebrew `Version` behavior followed by formula revision comparison;
- exact-name precedence over aliases and old names;
- explicit old-name transitions and receipt-backed installed graph reconstruction.

The registry does not substitute a historical formula release when the current release is unavailable. A full resolve must produce a complete installable closure before the client mutates local state.

## Intentional product differences

Plain `glu install <name>` is root-idempotent. If that stable package identity is already installed, the command records or promotes intent without upgrading its bytes. Use `update`, `reinstall`, or force mode when replacement is intended.

Update scope is explicit:

- `update <name>` updates the named declared root while retaining dependencies that satisfy its active requirements;
- bare `update` applies that behavior to every declared root;
- `update --all` converges every declared root and its complete dependency closure to the current registry selection.

Installed and prospective graphs remain different authorities. `deps` normally reads receipts for an installed package; `deps --online` and `uses` read registry graphs.

## Outside the 0.1.0 boundary

The following are not compatibility claims:

- source builds and build-only dependency behavior;
- formula options;
- general Homebrew requirements evaluation;
- Intel installation;
- migration of obsolete prerelease receipt schemas;
- pinning and arbitrary Homebrew local-tab states;
- equivalence with Homebrew commands whose product policy differs from Glu's declared-root model.

Glu's default prefix is `/opt/glustore`. Fixed-cellar bottles are supported only when relocation can preserve the required prefix length.

## Verification boundary

The version-ordering corpus contains 8,480 directional comparisons against the pinned Homebrew source and is executable with:

```sh
./scripts/verify-homebrew-version-corpus.sh PATH_TO_HOMEBREW_CHECKOUT
```

Client tests cover installer-root floors, satisfied intermediates, shared contexts, aliases, old names, changed and dropped providers, update scopes, receipt reconstruction, removal, and autoremove. Registry tests cover target variation, `uses_from_macos`, compatible bottle selection, identity precedence, full/slim topology parity, and literal bottle requirement keys.

There is not yet an exhaustive pinned Homebrew oracle for every published formula and supported target. Consequently, the compatibility claim is limited to the boundary and tested cases above rather than all-formula behavioral equivalence. Requirements, source builds, and source/payload provenance remain explicit gaps rather than inferred compatibility.
