use crate::{command_model::CommandId, trace_cmd::TraceCommand};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Invocation-wide presentation and safety policy. These options are truly
/// global: clap accepts them before or after a command or nested subcommand.
#[derive(Clone, Copy, Debug, Default, Args)]
pub(crate) struct GlobalArgs {
    /// Emit the final command result or diagnostic as JSON.
    #[arg(short = 'j', long, global = true, conflicts_with = "null")]
    pub(crate) json: bool,

    /// Emit package names separated by NUL bytes where supported.
    #[arg(short = '0', long, global = true, conflicts_with = "json")]
    pub(crate) null: bool,

    /// Select the nested graph view where supported.
    #[arg(short = 't', long, global = true)]
    pub(crate) tree: bool,

    /// Preview a mutation without changing package or declaration state.
    #[arg(short = 'p', long, global = true)]
    pub(crate) plan: bool,

    /// Approve a computed mutation without prompting.
    #[arg(short = 'y', long, global = true)]
    pub(crate) yes: bool,

    /// Request detailed progress or edge metadata where supported.
    #[arg(short = 'v', long, global = true)]
    pub(crate) verbose: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "glu",
    version,
    about = "Fast Homebrew-bottle-compatible package installer",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) globals: GlobalArgs,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Install packages and their dependencies.
    #[command(
        visible_alias = "i",
        visible_alias = "add",
        long_about = "\
Installs a package and its dependency closure.\n\n\
Bare `glu install` syncs the declaration — installs every declared package \
that isn't installed, errors when nothing is declared, and adds new roots to \
it (a package installed only as a dependency gets promoted to declared)."
    )]
    Install {
        /// Reinstall even if already installed.
        #[arg(short = 'f', long)]
        force: bool,

        /// Also reinstall the full dependency closure (requires --force).
        #[arg(short = 'd', long)]
        deps: bool,

        /// Package names to install; omit to sync the declaration.
        names: Vec<String>,
    },

    /// Reinstall installed packages.
    #[command(long_about = "\
Resolves and reinstalls named packages even when already installed. Errors \
when a named package isn't installed (use `glu install`). Membership is unchanged: \
reinstalling an automatic package does not declare it, reinstalling a \
declared one keeps it declared.")]
    Reinstall {
        /// Also reinstall the full dependency closure.
        #[arg(short = 'd', long)]
        deps: bool,

        /// Package names to reinstall (must be installed).
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },

    /// Update installed packages to the latest version and sync the declaration.
    #[command(
        visible_alias = "up",
        long_about = "\
Updates packages to the latest version and syncs. Bare `glu up` and \
`glu up --all` are broad mutations and always present the plan and ask; a \
named `glu up <name>` asks only when the update would remove something (a \
dependency the new version dropped). `-a` also bumps automatic packages; \
`-c`/`--dependents` makes a named update also update outdated dependents. `update` never \
changes what is declared."
    )]
    Update {
        /// Also update installed, outdated packages that depend (directly
        /// or transitively) on the named packages. No effect with --all,
        /// which already covers every outdated package.
        #[arg(short = 'c', long)]
        dependents: bool,

        /// Update every outdated package instead of naming specific ones.
        #[arg(short = 'a', long, conflicts_with = "names")]
        all: bool,

        /// Package names to update; omit to update every declared package.
        names: Vec<String>,
    },

    /// Remove packages from the declaration and sync.
    #[command(
        visible_alias = "rm",
        visible_alias = "uninstall",
        long_about = "\
Removes packages and syncs. Selectors: `name` (every installed version), \
`name@version` (every revision of that version), or `name@version_revision` \
(exactly that version); a name containing `@` (a versioned package like \
`postgresql@14`) matches literally first. Removing a declared package also \
removes automatic packages that become dangling — if removal exceeds what \
you named, glu lists the full set and asks first. A declared package another \
declared package still needs is demoted instead (`Removed X from your \
packages — it stays installed because Y needs it`); an automatic package a \
declared package needs can't be removed. There is no force flag."
    )]
    Remove {
        /// Package selectors: `name` (every installed version), `name@version`
        /// (every revision of that version), or `name@version_revision`
        /// (exactly that version).
        #[arg(required = true, num_args = 1.., value_name = "SELECTORS")]
        names: Vec<String>,
    },

    /// Remove installed packages that are no longer required by any declared package.
    #[command(long_about = "\
Removes every dangling package (installed, not declared, unreachable from \
anything declared). Normally a no-op — install/rm/up already remove the \
dangling set — and the repair tool for bad state (interrupted commands, \
manual edits). Always lists what it will remove and asks (`-y` skips); \
prints `No unused packages to remove.` when there is nothing to do.")]
    Autoremove {},

    /// Keep packages installed but remove their public prefix links.
    #[command(
        visible_alias = "unlink",
        long_about = "\
Deactivates installed packages without removing their kegs. Public prefix links \
(bin, sbin, etc.) and the linked marker are removed, so commands no longer \
appear on PATH through glu. Stable opt links remain for dependents and full-path \
use."
    )]
    Deactivate {
        /// Installed package names to deactivate.
        #[arg(required = true)]
        names: Vec<String>,
    },

    /// Restore deactivated packages' public prefix links.
    #[command(
        visible_alias = "link",
        long_about = "\
Activates installed packages by restoring their public prefix links from local \
receipt metadata. No registry lookup is required. Use --force to rebuild links \
even when the package is already active."
    )]
    Activate {
        /// Rebuild links even if the package is already active.
        #[arg(short = 'f', long)]
        force: bool,
        /// Installed package names to activate.
        #[arg(required = true)]
        names: Vec<String>,
    },

    /// List declared packages by default.
    #[command(
        visible_alias = "ls",
        long_about = "\
Default: declared packages — \"what did I install?\" — with a hint \
to view all dependencies on terminals. `--installed`/`-a`/`--all` lists \
every installed package; with `-t`, it also roots dangling packages so the \
tree covers everything. `-t`/`--tree` renders the nested dependency tree; \
`-j`/`--json` and `-0`/`--null` emit machine-readable output; combined \
with `-t`, JSON preserves the nested tree and NUL emits tree node names. \
Flat output is plain `name version` lines — safe for `xargs`."
    )]
    List {
        /// Explicitly list only declared packages (the default).
        #[arg(long, conflicts_with_all = ["all", "installed"])]
        declared: bool,
        /// List every installed package. Alias: --all / -a.
        #[arg(long)]
        installed: bool,
        /// Alias for --installed.
        #[arg(short = 'a', long)]
        all: bool,
    },

    /// List installed packages with newer versions available.
    #[command(long_about = "\
Installed packages with a newer version available, as a table \
(`Package / Current / Update / Latest`, where `Update` is the newest \
installable version — what `glu up` installs — and `Latest` is the newest \
visible version overall). Prints `Nothing outdated.` when everything \
is current. Read-only. Default scope is every installed package; \
`--declared` limits the report to declared packages.")]
    Outdated {
        /// Explicitly check only declared packages.
        #[arg(long, conflicts_with_all = ["installed", "all"])]
        declared: bool,

        /// Check every installed package (the default). Alias: --all / -a.
        #[arg(long)]
        installed: bool,

        /// Alias for --installed.
        #[arg(short = 'a', long)]
        all: bool,
    },

    /// Forward dependency tree of one package: what it pulls in.
    #[command(long_about = "\
The forward dependency tree of one package. Installed packages answer from \
receipts (offline); not-installed ones resolve from the registry, marked \
`resolved from the registry — not installed`. `-o`/`--online` forces the \
registry answer for an installed package (what a fresh install would pull) — \
the two can differ. Human output lists names only by default. `-d`/`--direct` \
limits to one level, `-t`/`--tree` draws the nested tree, and `-v` annotates \
packages as `(VERSION installed)` plus each tree edge's declared requirement; resolver \
candidate versions are never presented as dependency requirements. `--status` \
annotates human output with installed/declared/link state. Unknown names get \
friendly `package 'X' not found` errors with `Did you mean …?` suggestions.")]
    Deps {
        /// Only the package's direct dependencies (one level).
        #[arg(short = 'd', long)]
        direct: bool,
        /// Annotate human dependency output with installed/declared/link status.
        #[arg(long)]
        status: bool,
        /// Force the registry answer (what a fresh install would pull)
        /// even when the package is installed.
        #[arg(short = 'o', long)]
        online: bool,

        /// Package name (installed by default; registry with -o).
        name: String,
    },

    /// Reverse dependency tree of one installed package: what transitively depends on it.
    #[command(long_about = "\
The reverse dependency tree of one installed package: who (transitively) \
depends on it. Offline only (receipts). Prints `Nothing depends on it.` when \
nobody does. Flat shows the dependents without the named package; `-t`/\
`--tree` shows it as the root. `-j`/`--json` emits structured output and \
`-0`/`--null` emits flat dependent names separated by NUL bytes.")]
    Why {
        /// Installed package name.
        name: String,
    },

    /// Reverse dependency tree from the registry: who could install this.
    #[command(long_about = "\
The registry-wide reverse dependency tree — who could install this package. \
Always online (`GET /v1/uses`); the counterpart of `glu why` that works for \
not-installed packages and isn't limited to this system. `-d`/`--direct` \
limits to one-hop dependents, `-t`/`--tree` the nested view, `-v` each \
dependent's version floor on the package it pulls. Prints `Nothing depends \
on it.` when the registry has none. `-j`/`--json` emits structured output and \
`-0`/`--null` emits flat dependent names separated by NUL bytes.")]
    Uses {
        /// Only the packages that directly depend on it (one hop).
        #[arg(short = 'd', long)]
        direct: bool,
        /// Package name.
        name: String,
    },

    /// Show registry metadata and installed state for one or more packages.
    #[command(
        visible_alias = "view",
        long_about = "\
Registry metadata for one or more packages plus their installed copies, if any: \
version, license, homepage, sizes, dependency counts, bottle tag."
    )]
    Info {
        /// Package name(s).
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },

    /// Show glu state: prefix, target, shell integration, and package counts.
    #[command(
        visible_alias = "doctor",
        long_about = "\
Shows glu state: resolved prefix and its source (flag, env, self-located, or \
default), the fixed-cellar length gate, target, registry/distribution, \
shell integration, and installed/declared/deactivated counts."
    )]
    Status,

    /// Configure shell integration for the prefix.
    #[command(long_about = "\
Configures shell integration (PATH etc.) in the shells it finds, and prints \
how to apply it to the current terminal (`source …`). Idempotent.")]
    Setup,

    /// Internal: print the eval-able shell environment (used by `setup`).
    #[command(hide = true)]
    Shellenv {
        /// Shell name; omit to detect the calling shell.
        shell: Option<String>,
    },

    /// Inspect install traces (timeline + flow viewer).
    #[command(
        subcommand,
        long_about = "\
Inspect install traces. `view` renders the most recent trace (or a trace id, \
filename, or path) as an HTML timeline + flow viewer and opens it; `summary` \
prints phase totals; `list` shows recent traces, newest first, capped at 20."
    )]
    Trace(#[command(subcommand)] TraceCommand),

    /// Print help for glu or one of its commands (e.g. `glu help install`).
    #[command(long_about = "\
Prints the grouped overview (`glu help`) or the full page for one command \
(`glu help install`; nested commands take the full path, e.g. \
`glu help trace view`).")]
    Help {
        /// Include generated result and error schemas (requires --json).
        #[arg(long, requires = "json")]
        schemas: bool,

        /// Command path to show help for (e.g. `install`); omit for the overview.
        command: Vec<String>,
    },

    /// Update the glu tool itself.
    #[command(long_about = "\
Updates the glu client itself, then re-runs setup so the installed copy \
stays the current one.")]
    Upgrade,

    /// Internal: sandboxed postinstall worker.
    #[command(name = "__postinstall-worker", hide = true)]
    PostinstallWorker {
        #[arg(long)]
        job: PathBuf,
        #[arg(long)]
        result: PathBuf,
    },
}

impl Command {
    pub(crate) fn id(&self) -> CommandId {
        match self {
            Self::Install { .. } => CommandId::Install,
            Self::Reinstall { .. } => CommandId::Reinstall,
            Self::Update { .. } => CommandId::Update,
            Self::Remove { .. } => CommandId::Remove,
            Self::Autoremove { .. } => CommandId::Autoremove,
            Self::Deactivate { .. } => CommandId::Deactivate,
            Self::Activate { .. } => CommandId::Activate,
            Self::List { .. } => CommandId::List,
            Self::Outdated { .. } => CommandId::Outdated,
            Self::Deps { .. } => CommandId::Deps,
            Self::Why { .. } => CommandId::Why,
            Self::Uses { .. } => CommandId::Uses,
            Self::Info { .. } => CommandId::Info,
            Self::Status => CommandId::Status,
            Self::Setup => CommandId::Setup,
            Self::Shellenv { .. } => CommandId::Shellenv,
            Self::Trace(trace) => trace.id(),
            Self::Help { .. } => CommandId::Help,
            Self::Upgrade => CommandId::Upgrade,
            Self::PostinstallWorker { .. } => CommandId::PostinstallWorker,
        }
    }
}
