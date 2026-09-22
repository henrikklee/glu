# State model

The state subsystem turns prefix reality into two small contracts:

```txt
managed closure = packages in glu.json + their dependency closure
installed truth = every complete package receipt in the prefix
```

A healthy prefix converges to the managed closure. Interrupted commands or manual edits can leave complete but dangling packages in installed truth until the next sync or `autoremove`.

## Intent and installed truth

`glu` separates two ideas:

| Concept | Stored in | Meaning |
|---|---|---|
| Intent | `glu.json` | Packages the user intentionally manages, plus activation state. |
| Installed truth | package receipts | Complete kegs physically installed on disk. |

`glu.json` is the state worth backing up. Receipts describe the prefix as installed by `glu`.

A package can be installed because it appears in `glu.json` or because something in `glu.json` depends on it.

## Ownership invariant

Only `InstalledStateStore` reads or mutates installed state.

Installed state includes:

- the declaration file (`glu.json`);
- package-local receipts;
- loaded snapshots derived from those records.

Other subsystems do not hand-parse receipts, scan the Cellar for their own state model, or write declaration changes directly. They ask the state store for snapshots or use state-store methods for receipt/declaration writes.

Read commands receive one immutable `LocalQuery` backed by one loaded snapshot. Counts, trees, selector lookups, status annotations, and local joins for that invocation derive from the same value. Declaration, deactivation, installation, and link statuses are indexed by stable `PackageKey`; aliases and old names resolve at the selector boundary and do not become status keys. Mutation planning owns separate snapshots, and reads after a write load fresh state.

This keeps the boundary simple:

```txt
state store owns durable state
planner consumes snapshots
installer writes receipts through state code
commands write intent through state code
```

## `glu.json`

A minimal declaration looks like:

```json
{
  "schema": "glu.declaration.v1",
  "dependencies": {
    "vips": "8.19.0",
    "jq": "1.8.2"
  }
}
```

`dependencies` records packages intentionally managed by the user. Internal code may call these declared packages.

The file may also contain `deactivated`, a map of packages that remain installed without public prefix links.

Versions in `glu.json` are advisory state and history, not lockfile constraints. Sync resolves the closure, installs what is needed, and writes resolved versions back for declared packages.

`glu migrate` imports declaration intent from an external Homebrew prefix, not installed truth. Its source scanner reads only foreign package-local receipts to identify names marked `installed_on_request`; it never treats those records as glu receipts or writes state directly. The resulting names go through the normal install planner and executor, so `InstalledStateStore` remains the only owner of glu declarations and receipts. Existing declarations and deactivation choices are preserved.

## Receipts

Each complete installed keg has a package-local `glu.install-receipt.v1` receipt. The receipt is the installed authority for:

- the concrete `PackageId` and stable `PackageKey`;
- the canonical display name, selector aliases, and old names;
- artifact provenance;
- exact direct dependency selectors and the package's separate, flattened `dependency_requirements` map;
- keg and opt paths;
- required filesystem `links.opt_names`;
- source-neutral exposure policy and its normalized reason;
- linked state and completion status.

Selector aliases and filesystem opt-link names are independent facts. An alias can resolve a command without creating a link, and a required link name does not participate in dependency topology.

Only complete receipts count as installed. Incomplete receipts are ignored by state reads and cleaned before mutating commands plan work. Unsupported receipt schemas fail closed; there is no compatibility or migration path for pre-release receipt shapes.

One-time state migrations run after recovery under the prefix operation lock, before mutating commands proceed. Plans and read-only queries leave pending migrations untouched. After self-upgrade, the old process releases its lock and invokes the replacement binary to complete its migrations. Completed migration IDs are recorded in `var/glu/migrations.json`; interrupted migrations can safely retry. On ARM64 macOS Golden Gate, the receipt-tag migration changes legacy `aarch64_macos` tags to `arm64_golden_gate` without changing other receipt fields.

Receipts do not decide why a package is installed. `glu.json` decides user intent. Installed state resolves receipt selectors against the complete installed package set, then traverses the resulting package-key graph for reachability. Exact installed names take precedence over another package's alias or old name; ambiguous non-exact selector claims fail closed.

## Declared, automatic, and dangling

A package in `glu.json` is declared. A package installed only because another package depends on it is automatic.

A dangling package is automatic and no longer reachable from any package in `glu.json`. Declaration names resolve once through installed receipt selectors; the reachability walk then uses `PackageKey` exclusively.

`autoremove` removes dangling packages. Normal sync-style mutations also account for dangling packages so the prefix converges back to the declaration closure. `deps`, `why`, removal planning, autoremove, and post-mutation dangling checks all traverse the same receipt-backed package-key graph.

`purge` is the explicit exception to normal convergence: it removes every installed package. By default it also removes `glu.json`; `--keep-declaration` preserves the declaration verbatim so installed truth is temporarily empty while intent remains available for a later bare `glu install`.

## Sync

Sync reconciles three inputs:

- packages listed in `glu.json`;
- complete local receipts;
- the registry-resolved closure.

Registry data describes prospective truth. Receipts describe installed truth. They remain separate authorities: registry dependencies bind selected package identities, while receipts retain selectors and rebuild provider bindings from the packages currently installed.

It installs missing packages, keeps packages still required by the closure, updates or repours when the command mode asks for that, and removes confirmed dangling packages.

Sync is idempotent. Re-running the same command after an interrupted operation cleans stale staging state and incomplete receipts before planning again.

## Install records intent

Plain `glu install <selector>` records intent. The selector resolves to a package key; if any version of that package identity is installed, the command treats the root as satisfied.

That gives `install` one meaning:

```txt
make this package part of my managed set
```

Update, reinstall, and force modes change installed bytes.

## Promotion

Installing an automatic package promotes it into `glu.json`.

The installed version stands during promotion. Promotion changes why the package is kept, not what bytes are installed.

## Activation state

Activation is separate from installation and from package exposure. Exposure is registry policy persisted in the receipt; deactivation is user intent persisted in `glu.json`.

An isolated package always keeps its stable opt link and skips automatic shared-prefix projection. Activating it does not override that package policy.

A deactivated package:

- still has a complete receipt;
- still has a stable opt link;
- can still satisfy dependents;
- does not project public prefix links such as `bin/*`.

Activation commands modify link state and `glu.json` deactivation state. They do not change package membership.

## Removal safety

There is no force-remove operation for a package still needed by something in `glu.json`.

To stop needing a dependency, remove or update the package that depends on it. This keeps the user model simple and prevents the prefix from entering an intentionally broken dependency state.

## Configuration preservation

Configuration defaults copied into prefix configuration areas are real files. They are not symlinks back into kegs.

Unlinking a package does not touch those files. Removal inventories the exact defaults under every installed package's `.bottle/etc` and `.bottle/var` trees before deleting anything. Installation and removal use Homebrew's lstat-based walk: a directory symlink can map to a real destination directory, but neither operation traverses its target. A file is removed automatically when it is unchanged and no retained package claims its destination. Modified, shared, ambiguous, and untracked files are preserved by default.

Interactive removal offers a separate, default-no confirmation for attributable modified files. `--remove-config` opts into that cleanup explicitly; non-interactive use also requires `--yes`. Unknown files beneath a mapped directory are never inferred to belong to the package, and directories are pruned only when empty.
