use glu_client::state::installed::DependencyTreeNode;
use glu_core::{InstalledPackage, PackageName};

pub(crate) fn generated_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    // The public contract describes emitted JSON, not accepted input. On fields
    // skipped when `Option::None`, `schemars(required)` keeps the present value
    // non-null while the serialization contract keeps the property optional.
    let settings = schemars::generate::SchemaSettings::default().for_serialize();
    serde_json::to_value(settings.into_generator().into_root_schema_for::<T>())
        .expect("serialize generated JSON Schema")
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CommandSchema {
    pub(crate) schema_version: u32,
    pub(crate) global_options: &'static [OptionSpec],
    pub(crate) commands: &'static [CommandSpec],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandGroup {
    PackageManagement,
    Query,
    Configuration,
    Observability,
    Maintenance,
    Other,
    Internal,
}

impl CommandGroup {
    pub(crate) const PUBLIC_ORDER: &'static [Self] = &[
        Self::PackageManagement,
        Self::Query,
        Self::Configuration,
        Self::Observability,
        Self::Maintenance,
    ];

    pub(crate) const fn heading(self) -> &'static str {
        match self {
            Self::PackageManagement => "Package management",
            Self::Query => "Query",
            Self::Configuration => "Configuration",
            Self::Observability => "Observability",
            Self::Maintenance => "Maintenance",
            Self::Other => "Other",
            Self::Internal => "Internal",
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ArgumentSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct CommandSpec {
    pub(crate) id: CommandId,
    pub(crate) name: &'static str,
    pub(crate) group: CommandGroup,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) summary: &'static str,

    pub(crate) default_behavior: &'static str,
    pub(crate) arguments: &'static [ArgumentSpec],
    pub(crate) mutates: bool,
    pub(crate) default_scope: Option<&'static str>,
    pub(crate) output_protocols: &'static [OutputProtocol],
    pub(crate) capabilities: CommandCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result_schema: Option<&'static str>,
    pub(crate) options: &'static [OptionSpec],
    pub(crate) examples: &'static [&'static str],
    pub(crate) subcommands: &'static [CommandSpec],
}

impl CommandSpec {
    pub(crate) fn supports_global_option(&self, long: &str) -> bool {
        match long {
            "--json" => self
                .output_protocols
                .contains(&OutputProtocol::JsonEnvelope),
            "--null" => self.output_protocols.contains(&OutputProtocol::NullNames),
            "--tree" => self.capabilities.tree,
            "--verbose" => self.capabilities.verbose,
            "--plan" => self.capabilities.plan,
            "--yes" => self.capabilities.yes,
            _ => false,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct OptionSpec {
    pub(crate) long: &'static str,
    pub(crate) short: Option<char>,
    pub(crate) kind: OptionKind,
    pub(crate) scope: OptionScope,
    pub(crate) description: &'static str,
    pub(crate) conflicts_with: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OptionKind {
    Bool,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OptionScope {
    GlobalOutput,
    GlobalPresentation,
    GlobalSafety,
    CommandSelection,
    CommandBehavior,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputProtocol {
    Human,
    JsonEnvelope,
    NullNames,
    RawShellText,
    InternalWorker,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct CommandCapabilities {
    pub(crate) tree: bool,
    pub(crate) verbose: bool,
    pub(crate) plan: bool,
    pub(crate) yes: bool,
}

const CAP_NONE: CommandCapabilities = CommandCapabilities {
    tree: false,
    verbose: false,
    plan: false,
    yes: false,
};
const CAP_TREE: CommandCapabilities = CommandCapabilities {
    tree: true,
    ..CAP_NONE
};
const CAP_VERBOSE: CommandCapabilities = CommandCapabilities {
    verbose: true,
    ..CAP_NONE
};
const CAP_TREE_VERBOSE: CommandCapabilities = CommandCapabilities {
    tree: true,
    verbose: true,
    ..CAP_NONE
};
const CAP_PLAN_YES: CommandCapabilities = CommandCapabilities {
    plan: true,
    yes: true,
    ..CAP_NONE
};
const CAP_MUTATION_GRAPH: CommandCapabilities = CommandCapabilities {
    tree: true,
    verbose: true,
    plan: true,
    yes: true,
};

const EMPTY_OPTIONS: &[OptionSpec] = &[];
const EMPTY_ARGUMENTS: &[ArgumentSpec] = &[];
const EMPTY_COMMANDS: &[CommandSpec] = &[];
const HUMAN: &[OutputProtocol] = &[OutputProtocol::Human];
const HUMAN_JSON: &[OutputProtocol] = &[OutputProtocol::Human, OutputProtocol::JsonEnvelope];
const HUMAN_JSON_NULL: &[OutputProtocol] = &[
    OutputProtocol::Human,
    OutputProtocol::JsonEnvelope,
    OutputProtocol::NullNames,
];
const RAW_SHELL_TEXT: &[OutputProtocol] = &[OutputProtocol::RawShellText];
const INTERNAL_WORKER: &[OutputProtocol] = &[OutputProtocol::InternalWorker];

const INSTALL_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAMES",
    description: "optional package names; without NAMES, sync declared packages",
}];
const REINSTALL_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAMES",
    description: "package names to reinstall",
}];
const UPDATE_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAMES",
    description: "optional package names; omitted means declared packages unless --all is used",
}];
const REMOVE_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "SELECTORS",
    description: "package selectors: name, name@version, or name@version_revision",
}];
const PACKAGE_NAMES_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAMES",
    description: "package names",
}];
const PACKAGE_NAME_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAME",
    description: "package name",
}];
const INFO_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "NAMES",
    description: "one or more package names",
}];
const TRACE_TARGET_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "TARGET",
    description: "optional trace target; omitted means latest trace",
}];
const HELP_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "COMMAND",
    description: "optional command path, aliases accepted",
}];

const HELP_OPTIONS: &[OptionSpec] = &[OptionSpec {
    long: "--schemas",
    short: None,
    kind: OptionKind::Bool,
    scope: OptionScope::CommandBehavior,
    description: "include generated result and error schemas (requires --json)",
    conflicts_with: &[],
}];
const SHELL_ARGUMENTS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "SHELL",
    description: "optional shell name; omitted means detect the calling shell",
}];

pub(crate) const GLOBAL_OPTION_SPECS: &[OptionSpec] = &[
    OptionSpec {
        long: "--json",
        short: Some('j'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalOutput,
        description: "emit structured JSON output",
        conflicts_with: &["--null"],
    },
    OptionSpec {
        long: "--null",
        short: Some('0'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalOutput,
        description: "emit NUL-separated names where applicable",
        conflicts_with: &["--json"],
    },
    OptionSpec {
        long: "--tree",
        short: Some('t'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalPresentation,
        description: "tree layout when the result has a useful tree representation",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--verbose",
        short: Some('v'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalPresentation,
        description: "more detail when available",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--plan",
        short: Some('p'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalSafety,
        description: "do not mutate; show what would happen",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--yes",
        short: Some('y'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalSafety,
        description: "answer confirmation prompts for the already-computed plan",
        conflicts_with: &[],
    },
];

const LIST_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--declared",
        short: None,
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "list declared packages (the default)",
        conflicts_with: &["--installed", "--all"],
    },
    OptionSpec {
        long: "--installed",
        short: None,
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "list every installed package",
        conflicts_with: &["--declared"],
    },
    OptionSpec {
        long: "--all",
        short: Some('a'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "alias for --installed",
        conflicts_with: &["--declared"],
    },
];

const OUTDATED_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--declared",
        short: None,
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "only show outdated declared packages",
        conflicts_with: &["--installed", "--all"],
    },
    OptionSpec {
        long: "--installed",
        short: None,
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "show outdated packages from everything installed (the default)",
        conflicts_with: &["--declared"],
    },
    OptionSpec {
        long: "--all",
        short: Some('a'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "alias for --installed",
        conflicts_with: &["--declared"],
    },
];

const UPDATE_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--all",
        short: Some('a'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "update every outdated package instead of declared/named packages only",
        conflicts_with: &["NAMES"],
    },
    OptionSpec {
        long: "--dependents",
        short: Some('c'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandBehavior,
        description: "also update installed outdated packages that depend on named packages",
        conflicts_with: &[],
    },
];

const INSTALL_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--force",
        short: Some('f'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandBehavior,
        description: "reinstall even if already installed",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--deps",
        short: Some('d'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandBehavior,
        description: "also reinstall the full dependency closure (requires --force)",
        conflicts_with: &[],
    },
];

const REINSTALL_OPTIONS: &[OptionSpec] = &[OptionSpec {
    long: "--deps",
    short: Some('d'),
    kind: OptionKind::Bool,
    scope: OptionScope::CommandBehavior,
    description: "also reinstall the full dependency closure",
    conflicts_with: &[],
}];

const YES_OPTIONS: &[OptionSpec] = &[OptionSpec {
    long: "--yes",
    short: Some('y'),
    kind: OptionKind::Bool,
    scope: OptionScope::GlobalSafety,
    description: "answer confirmation prompts for the already-computed plan",
    conflicts_with: &[],
}];

const ACTIVATE_OPTIONS: &[OptionSpec] = &[OptionSpec {
    long: "--force",
    short: Some('f'),
    kind: OptionKind::Bool,
    scope: OptionScope::CommandBehavior,
    description: "rebuild links even if already active",
    conflicts_with: &[],
}];

const DEPS_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--direct",
        short: Some('d'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "only direct dependencies",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--online",
        short: Some('o'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandBehavior,
        description: "force the registry answer even when the package is installed",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--status",
        short: None,
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalPresentation,
        description: "annotate human dependency output with installed/declared/link status",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--verbose",
        short: Some('v'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalPresentation,
        description:
            "annotate packages with (VERSION installed) and, in tree view, dependency requirements",
        conflicts_with: &[],
    },
];

const USES_OPTIONS: &[OptionSpec] = &[OptionSpec {
    long: "--direct",
    short: Some('d'),
    kind: OptionKind::Bool,
    scope: OptionScope::CommandSelection,
    description: "only direct dependents",
    conflicts_with: &[],
}];

const TRACE_SUMMARY_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--all",
        short: Some('a'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "show every package row in human output",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--verbose",
        short: Some('v'),
        kind: OptionKind::Bool,
        scope: OptionScope::GlobalPresentation,
        description: "show detailed human output; currently equivalent to --all",
        conflicts_with: &[],
    },
];

const TRACE_LIST_OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        long: "--failures",
        short: Some('f'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "only list failed runs",
        conflicts_with: &[],
    },
    OptionSpec {
        long: "--all",
        short: Some('a'),
        kind: OptionKind::Bool,
        scope: OptionScope::CommandSelection,
        description: "show all traces instead of the recent cap",
        conflicts_with: &[],
    },
];

const TRACE_SUBCOMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::TraceView,
        name: "view",
        group: CommandGroup::Observability,
        aliases: &["open"],
        summary: "Render a trace as an HTML timeline and open it",
        default_behavior: "Renders a trace as an HTML timeline and opens it.",
        arguments: TRACE_TARGET_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &["glu trace view", "glu trace view last.json"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::TraceSummary,
        name: "summary",
        group: CommandGroup::Observability,
        aliases: &[],
        summary: "Summarize trace timings and phase totals",
        default_behavior: "Summarize trace timings and phase totals",
        arguments: TRACE_TARGET_ARGUMENTS,
        mutates: false,
        default_scope: Some("latest"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_VERBOSE,
        result_schema: Some("TraceSummaryResult"),
        options: TRACE_SUMMARY_OPTIONS,
        examples: &[
            "glu trace summary",
            "glu trace summary 9719bb",
            "glu trace summary -j",
            "glu trace summary --all vips",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::TraceList,
        name: "list",
        group: CommandGroup::Observability,
        aliases: &["ls"],
        summary: "List recent install traces",
        default_behavior: "Lists recent install traces; --failures limits to failed runs and --all removes the recent cap.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: Some("recent"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("TraceListResult"),
        options: TRACE_LIST_OPTIONS,
        examples: &[
            "glu trace list",
            "glu trace list -f",
            "glu trace list --json",
        ],
        subcommands: EMPTY_COMMANDS,
    },
];

pub(crate) const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::Install,
        name: "install",
        group: CommandGroup::PackageManagement,
        aliases: &["i", "add"],
        summary: "Install packages and their dependencies",
        default_behavior: "With NAMES, installs those packages and dependencies. With no NAMES, installs declared packages.",
        arguments: INSTALL_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_MUTATION_GRAPH,
        result_schema: Some("InstallResult"),
        options: INSTALL_OPTIONS,
        examples: &["glu install curl", "glu i -yft vips", "glu install curl -j"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Reinstall,
        name: "reinstall",
        group: CommandGroup::PackageManagement,
        aliases: &[],
        summary: "Reinstall installed packages",
        default_behavior: "Resolves and reinstalls named packages even when already installed without changing declaration membership.",
        arguments: REINSTALL_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_PLAN_YES,
        result_schema: Some("ReinstallResult"),
        options: REINSTALL_OPTIONS,
        examples: &["glu reinstall vips", "glu reinstall -pj vips", "glu reinstall -d vips"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Update,
        name: "update",
        group: CommandGroup::PackageManagement,
        aliases: &["up"],
        summary: "Update packages",
        default_behavior: "With NAMES, updates those packages. With no NAMES, updates declared packages. --all includes automatic dependencies.",
        arguments: UPDATE_ARGUMENTS,
        mutates: true,
        default_scope: Some("declared"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_MUTATION_GRAPH,
        result_schema: Some("UpdateResult"),
        options: UPDATE_OPTIONS,
        examples: &["glu up", "glu up vips", "glu up -a", "glu up -yj"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Remove,
        name: "remove",
        group: CommandGroup::PackageManagement,
        aliases: &["rm", "uninstall"],
        summary: "Remove packages",
        default_behavior: "Removes selected packages from the declaration and syncs installed state; extra removals require confirmation or --yes.",
        arguments: REMOVE_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_PLAN_YES,
        result_schema: Some("RemovalResult"),
        options: YES_OPTIONS,
        examples: &["glu rm vips", "glu rm -y vips", "glu rm -yj vips"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Autoremove,
        name: "autoremove",
        group: CommandGroup::PackageManagement,
        aliases: &[],
        summary: "Remove unneeded packages",
        default_behavior: "Removes installed packages no longer required by declared packages; requires confirmation or --yes.",
        arguments: EMPTY_ARGUMENTS,
        mutates: true,
        default_scope: Some("orphan"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_PLAN_YES,
        result_schema: Some("AutoremoveResult"),
        options: YES_OPTIONS,
        examples: &["glu autoremove", "glu autoremove -yj"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Cleanup,
        name: "cleanup",
        group: CommandGroup::Maintenance,
        aliases: &[],
        summary: "Remove cached package downloads",
        default_behavior: "Removes all completed and partial package downloads from the artifact cache; requires confirmation or --yes.",
        arguments: EMPTY_ARGUMENTS,
        mutates: true,
        default_scope: Some("artifact cache"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_PLAN_YES,
        result_schema: Some("CleanupResult"),
        options: YES_OPTIONS,
        examples: &["glu cleanup --plan", "glu cleanup", "glu cleanup -yj"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Deactivate,
        name: "deactivate",
        group: CommandGroup::PackageManagement,
        aliases: &["unlink"],
        summary: "Keep packages installed but remove their public prefix links",
        default_behavior: "Keeps named packages installed but removes their public prefix links.",
        arguments: PACKAGE_NAMES_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("DeactivationResult"),
        options: EMPTY_OPTIONS,
        examples: &[
            "glu deactivate imagemagick",
            "glu deactivate imagemagick -j",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Activate,
        name: "activate",
        group: CommandGroup::PackageManagement,
        aliases: &["link"],
        summary: "Restore deactivated packages' public prefix links",
        default_behavior: "Restores named packages' public prefix links; --force repairs/rebuilds links.",
        arguments: PACKAGE_NAMES_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("ActivationResult"),
        options: ACTIVATE_OPTIONS,
        examples: &[
            "glu activate imagemagick",
            "glu activate --force imagemagick",
            "glu activate imagemagick -j",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::List,
        name: "list",
        group: CommandGroup::Query,
        aliases: &["ls"],
        summary: "List installed packages",
        default_behavior: "Lists declared packages by default. --installed or --all lists everything installed.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: Some("declared"),
        output_protocols: HUMAN_JSON_NULL,
        capabilities: CAP_TREE,
        result_schema: Some("ListResult"),
        options: LIST_OPTIONS,
        examples: &["glu ls", "glu ls -a", "glu ls -ajt"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Outdated,
        name: "outdated",
        group: CommandGroup::Query,
        aliases: &[],
        summary: "List installed packages with newer versions available",
        default_behavior: "Shows outdated installed packages by default. --declared limits to declared packages.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: Some("installed"),
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("OutdatedResult"),
        options: OUTDATED_OPTIONS,
        examples: &["glu outdated"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Deps,
        name: "deps",
        group: CommandGroup::Query,
        aliases: &[],
        summary: "Show what a package depends on",
        default_behavior: "Shows dependency names only. Uses installed receipts when available unless --online is used; --verbose adds installed versions and package-level minimum requirements.",
        arguments: PACKAGE_NAME_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN_JSON_NULL,
        capabilities: CAP_TREE_VERBOSE,
        result_schema: Some("DepsResult"),
        options: DEPS_OPTIONS,
        examples: &[
            "glu deps vips",
            "glu deps vips -v",
            "glu deps vips -tv",
            "glu deps vips -jt",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Why,
        name: "why",
        group: CommandGroup::Query,
        aliases: &[],
        summary: "Show why an installed package is needed",
        default_behavior: "Shows installed packages that depend on NAME.",
        arguments: PACKAGE_NAME_ARGUMENTS,
        mutates: false,
        default_scope: Some("installed"),
        output_protocols: HUMAN_JSON_NULL,
        capabilities: CAP_TREE,
        result_schema: Some("ReverseDepsResult"),
        options: EMPTY_OPTIONS,
        examples: &["glu why glib", "glu why pcre2 -t", "glu why pcre2 -jt"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Uses,
        name: "uses",
        group: CommandGroup::Query,
        aliases: &[],
        summary: "Show which registry packages depend on a package",
        default_behavior: "Shows registry packages that depend on NAME.",
        arguments: PACKAGE_NAME_ARGUMENTS,
        mutates: false,
        default_scope: Some("registry"),
        output_protocols: HUMAN_JSON_NULL,
        capabilities: CAP_TREE_VERBOSE,
        result_schema: Some("ReverseDepsResult"),
        options: USES_OPTIONS,
        examples: &["glu uses pcre2", "glu uses pcre2 -d", "glu uses pcre2 -jt"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Info,
        name: "info",
        group: CommandGroup::Query,
        aliases: &["view"],
        summary: "Show registry metadata and installed state for one or more packages",
        default_behavior: "Shows registry metadata for NAME plus local installed state when present.",
        arguments: INFO_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("InfoResult"),
        options: EMPTY_OPTIONS,
        examples: &[
            "glu info vips",
            "glu view vips",
            "glu info vips jq ripgrep -j",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Status,
        name: "status",
        group: CommandGroup::Configuration,
        aliases: &["doctor"],
        summary: "Show glu state: prefix, target, shell integration, and package counts",
        default_behavior: "Shows local glu client, prefix, registry, shell integration, and installed-package state.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("StatusResult"),
        options: EMPTY_OPTIONS,
        examples: &["glu status"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Setup,
        name: "setup",
        group: CommandGroup::Configuration,
        aliases: &[],
        summary: "Configure shell integration for the prefix",
        default_behavior: "Installs shell integration for supported shells.",
        arguments: EMPTY_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &["glu setup"],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Trace,
        name: "trace",
        group: CommandGroup::Observability,
        aliases: &[],
        summary: "Inspect install traces",
        default_behavior: "Groups trace inspection commands.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &["glu trace view", "glu trace list"],
        subcommands: TRACE_SUBCOMMANDS,
    },
    CommandSpec {
        id: CommandId::Help,
        name: "help",
        group: CommandGroup::Other,
        aliases: &[],
        summary: "Print help for glu or one of its commands",
        default_behavior: "Prints human help by default; --json emits the command manifest and --schemas adds generated schemas.",
        arguments: HELP_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: HUMAN_JSON,
        capabilities: CAP_NONE,
        result_schema: Some("HelpManifest"),
        options: HELP_OPTIONS,
        examples: &[
            "glu help",
            "glu help install",
            "glu help -j",
            "glu help install -j --schemas",
        ],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::Upgrade,
        name: "upgrade",
        group: CommandGroup::Maintenance,
        aliases: &[],
        summary: "Update the glu tool itself",
        default_behavior: "Upgrades the glu client binary.",
        arguments: EMPTY_ARGUMENTS,
        mutates: true,
        default_scope: None,
        output_protocols: HUMAN,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &["glu upgrade"],
        subcommands: EMPTY_COMMANDS,
    },
];

const INTERNAL_COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::Shellenv,
        name: "shellenv",
        group: CommandGroup::Internal,
        aliases: &[],
        summary: "Print the shell environment used by setup",
        default_behavior:
            "Prints eval-able shell environment text for the detected or named shell.",
        arguments: SHELL_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: RAW_SHELL_TEXT,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &[],
        subcommands: EMPTY_COMMANDS,
    },
    CommandSpec {
        id: CommandId::PostinstallWorker,
        name: "__postinstall-worker",
        group: CommandGroup::Internal,
        aliases: &[],
        summary: "Run one sandboxed postinstall job",
        default_behavior:
            "Runs under parent-owned orchestration and writes the internal worker result.",
        arguments: EMPTY_ARGUMENTS,
        mutates: false,
        default_scope: None,
        output_protocols: INTERNAL_WORKER,
        capabilities: CAP_NONE,
        result_schema: None,
        options: EMPTY_OPTIONS,
        examples: &[],
        subcommands: EMPTY_COMMANDS,
    },
];

pub(crate) fn command_spec_by_id(id: CommandId) -> Option<&'static CommandSpec> {
    fn find(specs: &'static [CommandSpec], id: CommandId) -> Option<&'static CommandSpec> {
        for spec in specs {
            if spec.id == id {
                return Some(spec);
            }
            if let Some(found) = find(spec.subcommands, id) {
                return Some(found);
            }
        }
        None
    }

    find(COMMAND_SPECS, id).or_else(|| find(INTERNAL_COMMAND_SPECS, id))
}

pub(crate) const COMMAND_SCHEMA: CommandSchema = CommandSchema {
    schema_version: 1,
    global_options: GLOBAL_OPTION_SPECS,
    commands: COMMAND_SPECS,
};

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct JsonErrorEnvelope {
    pub(crate) ok: bool,
    pub(crate) command: Option<String>,
    pub(crate) invocation: InvocationInfo,
    pub(crate) error: CliError,
}

#[derive(Debug, Default, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InvocationInfo {
    pub(crate) argv: Vec<String>,
    pub(crate) command_path: Vec<String>,
    pub(crate) recognized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub(crate) enum CommandId {
    #[serde(rename = "install")]
    Install,
    #[serde(rename = "reinstall")]
    Reinstall,
    #[serde(rename = "update")]
    Update,
    #[serde(rename = "remove")]
    Remove,
    #[serde(rename = "autoremove")]
    Autoremove,
    #[serde(rename = "cleanup")]
    Cleanup,
    #[serde(rename = "activate")]
    Activate,
    #[serde(rename = "deactivate")]
    Deactivate,
    #[serde(rename = "list")]
    List,
    #[serde(rename = "outdated")]
    Outdated,
    #[serde(rename = "deps")]
    Deps,
    #[serde(rename = "why")]
    Why,
    #[serde(rename = "uses")]
    Uses,
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "setup")]
    Setup,
    #[serde(rename = "shellenv")]
    Shellenv,
    #[serde(rename = "trace")]
    Trace,
    #[serde(rename = "trace view")]
    TraceView,
    #[serde(rename = "trace list")]
    TraceList,
    #[serde(rename = "trace summary")]
    TraceSummary,
    #[serde(rename = "help")]
    Help,
    #[serde(rename = "upgrade")]
    Upgrade,
    #[serde(rename = "__postinstall-worker")]
    PostinstallWorker,
}

impl CommandId {
    #[cfg(test)]
    pub(crate) const ALL: &'static [Self] = &[
        Self::Install,
        Self::Reinstall,
        Self::Update,
        Self::Remove,
        Self::Autoremove,
        Self::Cleanup,
        Self::Activate,
        Self::Deactivate,
        Self::List,
        Self::Outdated,
        Self::Deps,
        Self::Why,
        Self::Uses,
        Self::Info,
        Self::Status,
        Self::Setup,
        Self::Shellenv,
        Self::Trace,
        Self::TraceView,
        Self::TraceList,
        Self::TraceSummary,
        Self::Help,
        Self::Upgrade,
        Self::PostinstallWorker,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Reinstall => "reinstall",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Autoremove => "autoremove",
            Self::Cleanup => "cleanup",
            Self::Activate => "activate",
            Self::Deactivate => "deactivate",
            Self::List => "list",
            Self::Outdated => "outdated",
            Self::Deps => "deps",
            Self::Why => "why",
            Self::Uses => "uses",
            Self::Info => "info",
            Self::Status => "status",
            Self::Setup => "setup",
            Self::Shellenv => "shellenv",
            Self::Trace => "trace",
            Self::TraceView => "trace view",
            Self::TraceList => "trace list",
            Self::TraceSummary => "trace summary",
            Self::Help => "help",
            Self::Upgrade => "upgrade",
            Self::PostinstallWorker => "__postinstall-worker",
        }
    }
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct JsonSuccessEnvelope<'a, T: serde::Serialize + ?Sized> {
    pub(crate) ok: bool,
    pub(crate) command: CommandId,
    pub(crate) result: &'a T,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExitClass {
    Interrupted,
    Runtime,
    Usage,
}

impl ExitClass {
    pub(crate) const fn code(self) -> i32 {
        match self {
            Self::Interrupted => 130,
            Self::Runtime => 1,
            Self::Usage => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    ChecksumMismatch,
    CommandFailed,
    ConfirmationRequired,
    DownloadFailed,
    EmptyDeclaration,
    Interrupted,
    InvalidFlagCombination,
    LinkFailed,
    NotInstalled,
    PackageInfoFailed,
    PackageInfoTaskFailed,
    PackageNotFound,
    PackageUnavailable,
    ParseError,
    PartialInstallFailure,
    PostinstallFailed,
    PrepareFailed,
    RegistryError,
    RegistryUnavailable,
}

impl From<glu_client::error::RuntimeErrorCode> for ErrorCode {
    fn from(code: glu_client::error::RuntimeErrorCode) -> Self {
        use glu_client::error::RuntimeErrorCode;
        match code {
            RuntimeErrorCode::PackageNotFound => Self::PackageNotFound,
            RuntimeErrorCode::PackageUnavailable => Self::PackageUnavailable,
            RuntimeErrorCode::NotInstalled => Self::NotInstalled,
            RuntimeErrorCode::RegistryUnavailable => Self::RegistryUnavailable,
            RuntimeErrorCode::RegistryError => Self::RegistryError,
            RuntimeErrorCode::DownloadFailed => Self::DownloadFailed,
            RuntimeErrorCode::ChecksumMismatch => Self::ChecksumMismatch,
            RuntimeErrorCode::PrepareFailed => Self::PrepareFailed,
            RuntimeErrorCode::PostinstallFailed => Self::PostinstallFailed,
            RuntimeErrorCode::LinkFailed => Self::LinkFailed,
            RuntimeErrorCode::PartialInstallFailure => Self::PartialInstallFailure,
            RuntimeErrorCode::Interrupted => Self::Interrupted,
            RuntimeErrorCode::CommandFailed => Self::CommandFailed,
        }
    }
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CliError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) offending_arg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) usage: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) suggestions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) details: Option<CliErrorDetails>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum CliErrorDetails {
    Requirement(RequirementErrorDetails),
    UnsupportedOption(UnsupportedOptionDetails),
    Declaration(DeclarationErrorDetails),
    Parse(ParseErrorDetails),
    PlannedRemovals(PlannedRemovalsDetails),
    CleanupConfirmation(CleanupConfirmationDetails),
    RemovalConfirmation(RemovalConfirmationDetails),
    UpdateConfirmation(UpdateConfirmationDetails),
    InstallConfirmation(InstallConfirmationDetails),
    Registry(RegistryErrorDetails),
    Operation(OperationErrorDetails),
    Packages(PackagesErrorDetails),
    PartialInstall(Box<PartialInstallDetails>),
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RequirementErrorDetails {
    pub(crate) requires: &'static str,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct UnsupportedOptionDetails {
    pub(crate) unsupported_option: &'static str,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct DeclarationErrorDetails {
    pub(crate) declaration_file: &'static str,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ParseErrorDetails {
    pub(crate) clap_error_kind: String,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ErrorPackageRecord {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ErrorUpdateRecord {
    pub(crate) current: String,
    pub(crate) latest: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct PlannedRemovalsDetails {
    pub(crate) planned_removals: Vec<ErrorPackageRecord>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CleanupConfirmationRecord {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) downloads: usize,
    pub(crate) bytes: u64,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CleanupConfirmationDetails {
    pub(crate) planned_removals: Vec<CleanupConfirmationRecord>,
    pub(crate) planned_downloads: usize,
    pub(crate) unassociated_downloads: usize,
    pub(crate) unassociated_bytes: u64,
    pub(crate) reclaimable_bytes: u64,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RemovalConfirmationDetails {
    pub(crate) named: Vec<ErrorPackageRecord>,
    pub(crate) planned_removals: Vec<ErrorPackageRecord>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct UpdateConfirmationDetails {
    pub(crate) broad: bool,
    pub(crate) planned_removals: Vec<ErrorPackageRecord>,
    pub(crate) planned_updates: Vec<ErrorUpdateRecord>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InstallConfirmationDetails {
    pub(crate) command: String,
    pub(crate) planned_removals: Vec<ErrorPackageRecord>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RegistryErrorDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) name: Option<String>,
    pub(crate) operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) requested_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) target: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct OperationErrorDetails {
    pub(crate) operation: String,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct PackagesErrorDetails {
    pub(crate) packages: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct PartialInstallDetails {
    pub(crate) failed: Vec<PartialInstallPackageDetails>,
    pub(crate) failed_error: Option<String>,
    pub(crate) failed_kind: Option<String>,
    pub(crate) failed_node_id: Option<String>,
    pub(crate) failed_phase: Option<String>,
    pub(crate) failure_code: ErrorCode,
    pub(crate) installed: Vec<PartialInstallPackageDetails>,
    pub(crate) partial: Vec<PartialKegDetails>,
    pub(crate) skipped: Vec<PartialInstallPackageDetails>,
    pub(crate) suggested_commands: Vec<String>,
    pub(crate) trace_id: Option<String>,
    pub(crate) trace_path: std::path::PathBuf,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct PartialInstallPackageDetails {
    pub(crate) name: String,
    pub(crate) package_id: String,
    pub(crate) status: MutationStatus,
    pub(crate) version: String,
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct PartialKegDetails {
    pub(crate) name: String,
    pub(crate) package_id: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) version: String,
}

impl PartialInstallDetails {
    pub(crate) fn from_report(report: &glu_client::install::PartialInstallReport) -> Self {
        fn package(
            package: &glu_client::install::PartialInstallPackage,
        ) -> PartialInstallPackageDetails {
            PartialInstallPackageDetails {
                name: package.name.clone(),
                package_id: package.package_id.clone(),
                status: match package.status {
                    glu_client::install::PartialInstallStatus::Failed => MutationStatus::Failed,
                    glu_client::install::PartialInstallStatus::Installed => {
                        MutationStatus::Installed
                    }
                    glu_client::install::PartialInstallStatus::Skipped => MutationStatus::Skipped,
                },
                version: package.version.clone(),
            }
        }

        fn partial(keg: &glu_client::install::PartialKeg) -> PartialKegDetails {
            PartialKegDetails {
                name: keg.name.clone(),
                package_id: keg.package_id.clone(),
                path: keg.path.clone(),
                version: keg.version.clone(),
            }
        }

        Self {
            failed: report.failed.iter().map(package).collect(),
            failed_error: report.failed_error.clone(),
            failed_kind: report.failed_kind.clone(),
            failed_node_id: report.failed_node_id.clone(),
            failed_phase: report.failed_phase.clone(),
            failure_code: report.failure_code.into(),
            installed: report.installed.iter().map(package).collect(),
            partial: report.partial.iter().map(partial).collect(),
            skipped: report.skipped.iter().map(package).collect(),
            suggested_commands: report.suggested_commands.clone(),
            trace_id: report.trace_id.clone(),
            trace_path: report.trace_path.clone(),
        }
    }
}

impl CliError {
    pub(crate) fn command_failed(error: &anyhow::Error) -> Self {
        Self {
            code: ErrorCode::CommandFailed,
            message: error.to_string(),
            offending_arg: None,
            usage: None,
            suggestions: Vec::new(),
            details: None,
        }
    }

    pub(crate) fn runtime(
        code: ErrorCode,
        message: impl Into<String>,
        suggestions: Vec<String>,
        details: Option<CliErrorDetails>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            offending_arg: None,
            usage: None,
            suggestions,
            details,
        }
    }

    pub(crate) fn confirmation_required(
        message: impl Into<String>,
        suggestions: Vec<String>,
        details: Option<CliErrorDetails>,
    ) -> Self {
        Self {
            code: ErrorCode::ConfirmationRequired,
            message: message.into(),
            offending_arg: None,
            usage: None,
            suggestions,
            details,
        }
    }

    pub(crate) fn invalid_flag_combination(
        message: impl Into<String>,
        offending_arg: impl Into<String>,
        suggestions: Vec<String>,
        details: Option<CliErrorDetails>,
    ) -> Self {
        Self {
            code: ErrorCode::InvalidFlagCombination,
            message: message.into(),
            offending_arg: Some(offending_arg.into()),
            usage: None,
            suggestions,
            details,
        }
    }

    pub(crate) fn empty_declaration(message: impl Into<String>, suggestions: Vec<String>) -> Self {
        Self {
            code: ErrorCode::EmptyDeclaration,
            message: message.into(),
            offending_arg: None,
            usage: None,
            suggestions,
            details: Some(CliErrorDetails::Declaration(DeclarationErrorDetails {
                declaration_file: "glu.json",
            })),
        }
    }

    #[cfg(test)]
    pub(crate) fn parse_error(message: String) -> Self {
        Self {
            code: ErrorCode::ParseError,
            message,
            offending_arg: None,
            usage: None,
            suggestions: Vec::new(),
            details: None,
        }
    }

    pub(crate) fn clap_parse_error(error: &clap::Error) -> Self {
        use clap::error::{ContextKind, ErrorKind};

        let offending_arg = context_string(error, ContextKind::InvalidArg)
            .or_else(|| context_string(error, ContextKind::InvalidSubcommand));
        let usage = context_string(error, ContextKind::Usage).map(normalize_usage);
        let suggestions = parse_suggestions(error);
        let message = match (error.kind(), offending_arg.as_deref()) {
            (ErrorKind::UnknownArgument, Some(arg)) => format!("unexpected argument '{arg}'"),
            (ErrorKind::InvalidSubcommand, Some(arg)) => format!("unknown command '{arg}'"),
            (ErrorKind::InvalidValue, Some(arg)) => format!("invalid value for '{arg}'"),
            (ErrorKind::ValueValidation, Some(arg)) => format!("invalid value for '{arg}'"),
            (ErrorKind::MissingRequiredArgument, Some(arg)) => {
                format!("missing required argument '{arg}'")
            }
            _ => concise_clap_message(error),
        };
        Self {
            code: ErrorCode::ParseError,
            message,
            offending_arg,
            usage,
            suggestions,
            details: Some(CliErrorDetails::Parse(ParseErrorDetails {
                clap_error_kind: format!("{:?}", error.kind()),
            })),
        }
    }

    pub(crate) fn envelope(
        self,
        command: Option<String>,
        invocation: InvocationInfo,
    ) -> JsonErrorEnvelope {
        JsonErrorEnvelope {
            ok: false,
            command,
            invocation,
            error: self,
        }
    }
}

fn context_string(error: &clap::Error, kind: clap::error::ContextKind) -> Option<String> {
    use clap::error::ContextValue;
    match error.get(kind)? {
        ContextValue::None => None,
        ContextValue::Bool(value) => Some(value.to_string()),
        ContextValue::String(value) => Some(value.clone()),
        ContextValue::Strings(values) => Some(values.join(", ")),
        ContextValue::StyledStr(value) => Some(value.to_string()),
        ContextValue::StyledStrs(values) => Some(
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        ContextValue::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn normalize_usage(usage: String) -> String {
    usage
        .trim()
        .strip_prefix("Usage: ")
        .unwrap_or_else(|| usage.trim())
        .to_string()
}

fn parse_suggestions(error: &clap::Error) -> Vec<String> {
    use clap::error::ContextKind;
    [
        ContextKind::SuggestedCommand,
        ContextKind::SuggestedSubcommand,
        ContextKind::SuggestedArg,
        ContextKind::SuggestedValue,
        ContextKind::Suggested,
    ]
    .into_iter()
    .filter_map(|kind| context_string(error, kind))
    .filter(|value| !value.is_empty())
    .collect()
}

fn concise_clap_message(error: &clap::Error) -> String {
    let rendered = error.to_string();
    let first = rendered.lines().next().unwrap_or("parse error");
    first
        .strip_prefix("error: ")
        .unwrap_or(first)
        .strip_suffix(" found")
        .unwrap_or_else(|| first.strip_prefix("error: ").unwrap_or(first))
        .to_string()
}

/// Mutually exclusive process output protocol selected for one invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    #[default]
    Human,
    Json,
    Null,
}

/// Normalized invocation-wide presentation and safety policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlobalOptions {
    pub(crate) output: OutputFormat,
    pub(crate) tree: bool,
    pub(crate) verbose: bool,
    pub(crate) plan: bool,
    pub(crate) yes: bool,
}

impl GlobalOptions {
    pub(crate) fn from_args(args: crate::args::GlobalArgs) -> Self {
        let output = if args.json {
            OutputFormat::Json
        } else if args.null {
            OutputFormat::Null
        } else {
            OutputFormat::Human
        };
        Self {
            output,
            tree: args.tree,
            verbose: args.verbose,
            plan: args.plan,
            yes: args.yes,
        }
    }

    pub(crate) fn is_json(self) -> bool {
        self.output == OutputFormat::Json
    }

    pub(crate) fn is_null(self) -> bool {
        self.output == OutputFormat::Null
    }
}

/// Structured command outputs rendered by the CLI renderer layer.
pub(crate) enum CommandOutput {
    List(ListOutput),
    Deps(DepsOutput),
    ReverseDeps(ReverseDepsOutput),
    Info(Box<InfoOutput>),
    InfoMany(InfoManyOutput),
    Install(InstallOutput),
    InstallPlan(InstallPlanOutput),
    Reinstall(ReinstallOutput),
    ReinstallPlan(ReinstallPlanOutput),
    Update(UpdateOutput),
    UpdatePlan(UpdatePlanOutput),
    Activation(ActivationOutput),
    Deactivation(DeactivationOutput),
    Removal(RemovalOutput),
    RemovalPlan(RemovalPlanOutput),
    Autoremove(AutoremoveOutput),
    AutoremovePlan(AutoremovePlanOutput),
    Cleanup(CleanupOutput),
    CleanupPlan(CleanupPlanOutput),
    Status(StatusOutput),
    Outdated(OutdatedOutput),
    TraceView(crate::trace_cmd::TraceViewOutput),
    TraceList(crate::trace_cmd::TraceListOutput),
    TraceSummary(crate::trace_cmd::TraceSummary),
    Setup(glu_client::shell::SetupResult),
    Shellenv(String),
    Help(crate::help::HelpOutput),
    Upgrade(glu_client::upgrade::UpgradeResult),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ListScope {
    Declared,
    Installed,
}

/// `glu ls` computes either a flat selected package set or a selected package
/// graph. The presentation host then chooses human text, JSON, or NUL without recomputing
/// selection semantics.
pub(crate) struct ListOutput {
    pub(crate) scope: ListScope,
    pub(crate) view: ListView,
    pub(crate) statuses: std::collections::BTreeMap<PackageName, glu_client::deps::PackageStatus>,
    pub(crate) declared_names: Vec<PackageName>,
    pub(crate) deactivated_names: Vec<PackageName>,
    pub(crate) hidden_dependencies: usize,
    pub(crate) show_dependency_hint: bool,
}

pub(crate) enum ListView {
    Flat(Vec<InstalledPackage>),
    Tree(Vec<DependencyTreeNode>),
}

pub(crate) struct DepsOutput {
    pub(crate) source: DepsSource,
    pub(crate) installed: bool,
    pub(crate) direct: bool,
    pub(crate) status: bool,
    pub(crate) root: DependencyTreeNode,
    pub(crate) statuses:
        std::collections::BTreeMap<glu_core::PackageName, glu_client::deps::PackageStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepsSource {
    Installed,
    Resolved,
}

pub(crate) struct ReverseDepsOutput {
    pub(crate) command: CommandId,
    pub(crate) source: ReverseDepsSource,
    pub(crate) target: String,
    pub(crate) direct: bool,
    pub(crate) root: Option<DependencyTreeNode>,
    pub(crate) statuses:
        std::collections::BTreeMap<glu_core::PackageName, glu_client::deps::PackageStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReverseDepsSource {
    Installed,
    Registry,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InfoOutput {
    pub(crate) package: glu_core::InfoResponse,
    pub(crate) installed: Option<InstalledPackage>,
    pub(crate) declared: bool,
    pub(crate) deactivated: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InfoManyOutput {
    pub(crate) packages: Vec<InfoPackageResult>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InfoPackageResult {
    pub(crate) requested: String,
    pub(crate) found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) package: Option<glu_core::InfoResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) installed: Option<InstalledPackage>,
    pub(crate) declared: bool,
    pub(crate) deactivated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) error: Option<InfoPackageError>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InfoPackageError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutedMode {
    Executed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanMode {
    Plan,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InstallOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) requested: Vec<String>,
    pub(crate) installed: Vec<MutationPackageRecord>,
    pub(crate) satisfied: Vec<MutationPackageRecord>,
    pub(crate) promoted: Vec<MutationPackageRecord>,
    pub(crate) renamed: Vec<RenamePackageRecord>,
    pub(crate) removed: Vec<MutationPackageRecord>,
    pub(crate) execution: ExecutionSummaryRecord,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InstallPlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) requested: Vec<String>,
    pub(crate) would_install: Vec<MutationPackageRecord>,
    pub(crate) satisfied: Vec<MutationPackageRecord>,
    pub(crate) would_promote: Vec<MutationPackageRecord>,
    pub(crate) would_rename: Vec<RenamePackageRecord>,
    pub(crate) would_remove: Vec<MutationPackageRecord>,
    pub(crate) requires_confirmation: bool,
    pub(crate) would_download_bytes: Option<u64>,
    #[serde(skip)]
    pub(crate) dependency_tree: Vec<DependencyTreeNode>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ReinstallOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) requested: Vec<String>,
    pub(crate) reinstalled: Vec<MutationPackageRecord>,
    pub(crate) satisfied: Vec<MutationPackageRecord>,
    pub(crate) renamed: Vec<RenamePackageRecord>,
    pub(crate) removed: Vec<MutationPackageRecord>,
    pub(crate) execution: ExecutionSummaryRecord,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct ReinstallPlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) requested: Vec<String>,
    pub(crate) would_reinstall: Vec<MutationPackageRecord>,
    pub(crate) satisfied: Vec<MutationPackageRecord>,
    pub(crate) would_rename: Vec<RenamePackageRecord>,
    pub(crate) would_remove: Vec<MutationPackageRecord>,
    pub(crate) requires_confirmation: bool,
    pub(crate) would_download_bytes: Option<u64>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct UpdateOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) updates: Vec<UpdatePackageRecord>,
    pub(crate) removed: Vec<MutationPackageRecord>,
    pub(crate) execution: ExecutionSummaryRecord,
    pub(crate) latest_glu_version: Option<String>,
    #[serde(skip)]
    pub(crate) broad: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct UpdatePlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) would_update: Vec<UpdatePackageRecord>,
    pub(crate) would_remove: Vec<MutationPackageRecord>,
    pub(crate) requires_confirmation: bool,
    pub(crate) broad: bool,
    pub(crate) would_download_bytes: Option<u64>,
    pub(crate) latest_glu_version: Option<String>,
    #[serde(skip)]
    pub(crate) dependency_tree: Vec<DependencyTreeNode>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "ActivationResult")]
pub(crate) struct ActivationOutput {
    pub(crate) force: bool,
    pub(crate) packages: Vec<MutationPackageRecord>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "DeactivationResult")]
pub(crate) struct DeactivationOutput {
    pub(crate) packages: Vec<MutationPackageRecord>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RemovalOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) removed: Vec<MutationPackageRecord>,
    pub(crate) kept: Vec<KeptPackageRecord>,
    pub(crate) leftover_config_files: Vec<String>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RemovalPlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) named: Vec<MutationPackageRecord>,
    pub(crate) would_remove: Vec<MutationPackageRecord>,
    pub(crate) would_keep: Vec<KeptPackageRecord>,
    pub(crate) requires_confirmation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationStatus {
    Activated,
    AlreadyActive,
    AlreadyDeactivated,
    AlreadyInstalled,
    AlreadyInstalledPromotedToDeclared,
    Deactivated,
    Failed,
    Installed,
    InstalledOlderThanResolved,
    KeptNeededByDeclared,
    Reinstalled,
    Removed,
    Renamed,
    Selected,
    Skipped,
    Updated,
    WouldInstall,
    WouldKeepNeededByDeclared,
    WouldPromote,
    WouldReinstall,
    WouldRemove,
    WouldRename,
    WouldUpdate,
}

impl From<glu_client::install::PackageChangeStatus> for MutationStatus {
    fn from(status: glu_client::install::PackageChangeStatus) -> Self {
        use glu_client::install::PackageChangeStatus;
        match status {
            PackageChangeStatus::AlreadyInstalled => Self::AlreadyInstalled,
            PackageChangeStatus::AlreadyInstalledPromotedToDeclared => {
                Self::AlreadyInstalledPromotedToDeclared
            }
            PackageChangeStatus::Installed => Self::Installed,
            PackageChangeStatus::InstalledOlderThanResolved => Self::InstalledOlderThanResolved,
            PackageChangeStatus::WouldInstall => Self::WouldInstall,
            PackageChangeStatus::WouldUpdate => Self::WouldUpdate,
        }
    }
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct KeptPackageRecord {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) needed_by: Vec<String>,
    pub(crate) status: MutationStatus,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct AutoremoveOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) packages: Vec<MutationPackageRecord>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct AutoremovePlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) would_remove: Vec<MutationPackageRecord>,
    pub(crate) requires_confirmation: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CachedDownloadRecord {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) downloads: usize,
    pub(crate) bytes: u64,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CleanupOutput {
    pub(crate) mode: ExecutedMode,
    pub(crate) removed: Vec<CachedDownloadRecord>,
    pub(crate) removed_downloads: usize,
    pub(crate) unassociated_downloads: usize,
    pub(crate) unassociated_bytes: u64,
    pub(crate) reclaimed_bytes: u64,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CleanupPlanOutput {
    pub(crate) mode: PlanMode,
    pub(crate) would_remove: Vec<CachedDownloadRecord>,
    pub(crate) would_remove_downloads: usize,
    pub(crate) unassociated_downloads: usize,
    pub(crate) unassociated_bytes: u64,
    pub(crate) requires_confirmation: bool,
    pub(crate) would_reclaim_bytes: u64,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct MutationPackageRecord {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) status: MutationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) linked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) declared: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) deactivated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) direct: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) transitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) cached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) download_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) installed_bytes: Option<u64>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct RenamePackageRecord {
    pub(crate) old_name: String,
    pub(crate) new_name: String,
    pub(crate) version: String,
    pub(crate) status: MutationStatus,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct UpdatePackageRecord {
    pub(crate) name: String,
    pub(crate) current: String,
    pub(crate) latest: String,
    pub(crate) status: MutationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) linked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) declared: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) deactivated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) direct: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) transitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) cached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) download_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub(crate) installed_bytes: Option<u64>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema, Default)]
pub(crate) struct ExecutionSummaryRecord {
    pub(crate) trace_path: Option<String>,
    pub(crate) elapsed_seconds: f64,
    pub(crate) timing_breakdown: Option<TimingBreakdownRecord>,
    pub(crate) stats: Option<InstallStatsRecord>,
    #[serde(skip)]
    pub(crate) timing_description: Option<String>,
    #[serde(skip)]
    pub(crate) pool_stats: Option<InstallPoolStatsRecord>,
}

#[derive(Debug)]
pub(crate) struct InstallPoolStatsRecord {
    pub(crate) writer_seconds: f64,
    pub(crate) writer_workers: usize,
    pub(crate) codesign_seconds: f64,
    pub(crate) codesign_workers: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct TimingBreakdownRecord {
    pub(crate) download_seconds: f64,
    pub(crate) cache_rebuild_seconds: f64,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct InstallStatsRecord {
    pub(crate) downloaded: usize,
    pub(crate) reused: usize,
    pub(crate) prepared: usize,
    pub(crate) signed_machos: usize,
    pub(crate) linked_files: usize,
    pub(crate) global_postinstalls: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "StatusResult")]
pub(crate) struct StatusOutput {
    pub(crate) version: &'static str,
    pub(crate) prefix: String,
    pub(crate) prefix_source: String,
    pub(crate) prefix_length: usize,
    pub(crate) fixed_cellar_length: usize,
    pub(crate) target: String,
    pub(crate) registry: String,
    pub(crate) distribution: String,
    pub(crate) installed_count: usize,
    pub(crate) declared_count: usize,
    pub(crate) deactivated_count: usize,
    pub(crate) deactivated: Vec<String>,
    pub(crate) shells: Vec<StatusShell>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct StatusShell {
    pub(crate) name: String,
    pub(crate) config_path: String,
    pub(crate) configured: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "OutdatedResult")]
pub(crate) struct OutdatedOutput {
    pub(crate) scope: &'static str,
    pub(crate) packages: Vec<OutdatedRecord>,
    pub(crate) installed_client_version: &'static str,
    pub(crate) latest_glu_version: Option<String>,
    #[serde(skip)]
    pub(crate) human_packages: Vec<glu_client::outdated::OutdatedPackage>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct OutdatedRecord {
    pub(crate) name: String,
    pub(crate) current: String,
    pub(crate) update: Option<String>,
    pub(crate) latest: String,
}
