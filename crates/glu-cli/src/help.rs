use crate::args::Cli;
use crate::command_model::{
    command_spec_by_id, generated_schema, CommandGroup, CommandId, CommandSpec, JsonErrorEnvelope,
    OptionSpec, OutputProtocol, COMMAND_SCHEMA, COMMAND_SPECS, GLOBAL_OPTION_SPECS,
};
use anyhow::Result;
use clap::CommandFactory;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, io::IsTerminal};

/// Workflow examples on the overview page (label + invocation).
const HELP_EXAMPLES: &[(&str, &str)] = &[
    ("Install", "glu install curl"),
    ("Update", "glu up"),
    ("Remove", "glu rm curl"),
    ("Deactivate", "glu deactivate imagemagick"),
    ("Activate", "glu activate imagemagick"),
    ("Inspect", "glu ls · glu deps curl · glu why libssl"),
    ("Install help page", "glu help install"),
];

/// The descriptor-backed shared-conventions footer under top-level help.
/// `docs/reference/cli-behavior.md` mirrors this as a narrative reference.
struct HelpFooterItem {
    label: &'static str,
    description: HelpFooterDescription,
}

enum HelpFooterDescription {
    Static(&'static str),
}

const HELP_FOOTER: &[HelpFooterItem] = &[
    HelpFooterItem {
        label: "-t, --tree",
        description: HelpFooterDescription::Static(
            "Preserve dependency structure instead of the flat deduplicated set.",
        ),
    },
    HelpFooterItem {
        label: "-p, --plan",
        description: HelpFooterDescription::Static(
            "Preview the computed mutation without changing glu state.",
        ),
    },
    HelpFooterItem {
        label: "-y, --yes",
        description: HelpFooterDescription::Static(
            "Approve the computed plan; does not imply --force, --all, or --dependents.",
        ),
    },
    HelpFooterItem {
        label: "-v, --verbose",
        description: HelpFooterDescription::Static(
            "Show additional progress or dependency detail where supported.",
        ),
    },
    HelpFooterItem {
        label: "-j, --json / -0, --null",
        description: HelpFooterDescription::Static(
            "JSON emits structured output and never prompts; NUL emits separated names.",
        ),
    },
    HelpFooterItem {
        label: "Piped",
        description: HelpFooterDescription::Static(
            "Decoration (colors, boxes, hints) is terminal-only; piped flat \
output is plain `name version` lines so `glu ls | xargs` works.",
        ),
    },
    HelpFooterItem {
        label: "Selectors",
        description: HelpFooterDescription::Static(
            "rm takes `name`, `name@version` (every revision), or \
`name@version_revision` (exactly that version).",
        ),
    },
];

#[cfg(test)]
fn machine_output_help() -> String {
    fn collect(
        specs: &[CommandSpec],
        parent: Option<&str>,
        machine: &mut Vec<String>,
        null: &mut Vec<String>,
    ) {
        for spec in specs {
            let path = parent
                .map(|parent| format!("{parent} {}", spec.name))
                .unwrap_or_else(|| spec.name.to_string());
            let supports_json = spec.supports_global_option("--json");
            let supports_null = spec.supports_global_option("--null");
            if supports_json || supports_null {
                machine.push(format!("`{path}`"));
            }
            if supports_null {
                null.push(format!("`{path}`"));
            }
            collect(spec.subcommands, Some(&path), machine, null);
        }
    }

    let mut machine = Vec::new();
    let mut null = Vec::new();
    collect(COMMAND_SPECS, None, &mut machine, &mut null);
    format!(
        "Machine-readable output ({}); `-0` emits NUL-separated names for {}.",
        machine.join(", "),
        null.join(", ")
    )
}

/// Wrap `text` into lines of at most `width` columns, splitting on
/// whitespace (long words pass through unwrapped).
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width.max(1) {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Help column budget: the terminal width when interactive (clamped), else a
/// fixed 88 so piped output and the snapshot stay deterministic.
fn help_width() -> usize {
    if !std::io::stdout().is_terminal() {
        return 88;
    }
    crate::tables::terminal_width()
        .map(|width| width.clamp(60, 120))
        .unwrap_or(88)
}

fn overview_line(out: &mut String, label: &str, text: &str, width: usize) {
    let prefix = format!("    {label}: ");
    let continuation = " ".repeat(prefix.len());
    for (index, line) in wrap_lines(text, width.saturating_sub(prefix.len()))
        .iter()
        .enumerate()
    {
        let indent = if index == 0 { &prefix } else { &continuation };
        out.push_str(&glu_client::style::dim(&format!("{indent}{line}")));
        out.push('\n');
    }
}

fn green_overview_line(out: &mut String, label: &str, text: &str, width: usize) {
    let prefix = format!("    {label}: ");
    let continuation = " ".repeat(prefix.len());
    for (index, line) in wrap_lines(text, width.saturating_sub(prefix.len()))
        .iter()
        .enumerate()
    {
        let indent = if index == 0 { &prefix } else { &continuation };
        out.push_str(&glu_client::style::green(&format!("{indent}{line}")));
        out.push('\n');
    }
}

fn option_label(option: &OptionSpec) -> String {
    option
        .short
        .map(|short| format!("-{short}/{}", option.long))
        .unwrap_or_else(|| option.long.to_string())
}

fn render_overview_entry(out: &mut String, spec: &CommandSpec, width: usize) {
    out.push_str("  ");
    out.push_str(&glu_client::style::bold_magenta(spec.id.as_str()));
    if !spec.aliases.is_empty() {
        out.push_str(&glu_client::style::magenta(&format!(
            ", {}",
            spec.aliases.join(", ")
        )));
    }
    out.push('\n');
    for line in wrap_lines(spec.summary, width.saturating_sub(4)) {
        out.push_str("    ");
        out.push_str(&line);
        out.push('\n');
    }
    for line in wrap_lines(&compact_usage(spec), width.saturating_sub(4)) {
        out.push_str("    ");
        out.push_str(&glu_client::style::yellow(&line));
        out.push('\n');
    }
    if let Some(scope) = spec.default_scope {
        green_overview_line(out, "Default scope", scope, width);
    }
    if !spec.arguments.is_empty() {
        let arguments = spec
            .arguments
            .iter()
            .map(|argument| format!("{} — {}", argument.name, argument.description))
            .collect::<Vec<_>>()
            .join("; ");
        overview_line(out, "Arguments", &arguments, width);
    }
    let options = spec
        .options
        .iter()
        .filter(|option| !is_shared_flag(option.long))
        .map(|option| format!("{} — {}", option_label(option), option.description))
        .collect::<Vec<_>>();
    if !options.is_empty() {
        overview_line(out, "Options", &options.join("; "), width);
    }
    if spec.mutates {
        let mutation = if spec.capabilities.plan && spec.capabilities.yes {
            "may prompt; --plan previews; --yes approves"
        } else {
            "changes state; no plan mode"
        };
        overview_line(out, "Mutation", mutation, width);
    }
    let modes = supported_global_flags(spec)
        .into_iter()
        .filter(|flag| !matches!(*flag, "--plan" | "--yes"))
        .collect::<Vec<_>>();
    if !modes.is_empty() {
        overview_line(out, "Modes", &modes.join(", "), width);
    }
    out.push('\n');
}

/// Rendered top-level help (`glu --help` / `glu help`), grouped by topic with
/// compact headers and mini usages from the command specs. The same specs
/// back `glu help --json`, so agents and humans see the same command model.
pub(super) fn top_help_text() -> String {
    let mut out = String::new();
    out.push_str(&glu_client::style::bold(
        "Fast Homebrew-bottle-compatible package installer",
    ));
    out.push_str("\n\n");
    out.push_str(&glu_client::style::bold("Usage: glu [OPTIONS] <COMMAND>"));
    out.push('\n');
    out.push_str(&glu_client::style::dim(
        "Machine-readable: `glu help --json`; add `--schemas` for validation schemas.",
    ));
    out.push_str("\n\n");

    let total = help_width();
    for group in CommandGroup::PUBLIC_ORDER {
        out.push_str(&glu_client::style::bold_blue(group.heading()));
        out.push('\n');
        for spec in COMMAND_SPECS.iter().filter(|spec| spec.group == *group) {
            render_overview_entry(&mut out, spec, total);
            for subcommand in spec.subcommands {
                render_overview_entry(&mut out, subcommand, total);
            }
        }
    }

    // The `help` command itself, outside the topic groups.
    let help = command_spec_by_id(CommandId::Help).expect("help descriptor");
    render_overview_entry(&mut out, help, total);
    out.push_str(&glu_client::style::dim(
        "Run `glu help <command>` for expanded arguments, options, and examples.",
    ));
    out.push_str("\n\n");

    // Options: `-V`/`--version` from clap metadata, plus our own `-h`/`--help`
    // row (the flag is handled by the pre-parse interception, not clap).
    let mut cmd = Cli::command();
    cmd.build(); // materialize the auto -V/--version flag
    let mut entries: Vec<(String, String)> = Vec::new();
    if let Some(arg) = cmd
        .get_arguments()
        .find(|a| a.get_long() == Some("version"))
    {
        entries.push((
            format!("-V, --{}", arg.get_long().unwrap_or_default()),
            arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
        ));
    }
    entries.push((
        "-h, --help".to_string(),
        "Print this help; `glu help <command>` for one command".to_string(),
    ));
    if !entries.is_empty() {
        out.push_str(&glu_client::style::bold_blue("Options"));
        out.push('\n');
        let opt_width = entries
            .iter()
            .map(|(spec, _)| spec.len())
            .max()
            .unwrap_or(0);
        for (spec, help) in &entries {
            let help_lines = wrap_lines(help, total.saturating_sub(2 + opt_width + 2));
            for (i, line) in help_lines.iter().enumerate() {
                out.push_str("  ");
                if i == 0 {
                    out.push_str(spec);
                    out.push_str(&" ".repeat(opt_width - spec.len() + 2));
                } else {
                    out.push_str(&" ".repeat(2 + opt_width));
                }
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
    }

    // Workflow examples.
    out.push_str(&glu_client::style::bold_blue("Examples"));
    out.push('\n');
    let ex_width = HELP_EXAMPLES
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    for (label, invocation) in HELP_EXAMPLES {
        out.push_str(&format!("  {label:<ex_width$}  "));
        out.push_str(&glu_client::style::bold(invocation));
        out.push('\n');
    }
    out.push('\n');

    // Shared-conventions footer (mirror of client-cli.md's summary).
    out.push_str(&glu_client::style::bold_blue("Shared conventions"));
    out.push('\n');
    let label_width = HELP_FOOTER
        .iter()
        .map(|item| item.label.len())
        .max()
        .unwrap_or(0);
    let foot_desc_indent = 2 + label_width + 2;
    let foot_desc_width = total.saturating_sub(foot_desc_indent);
    for item in HELP_FOOTER {
        let text = match item.description {
            HelpFooterDescription::Static(text) => text.to_string(),
        };
        let lines = wrap_lines(&text, foot_desc_width);
        for (i, line) in lines.iter().enumerate() {
            out.push_str("  ");
            if i == 0 {
                out.push_str(&format!("{:<width$}  ", item.label, width = label_width));
            } else {
                out.push_str(&format!("{}  ", " ".repeat(label_width)));
            }
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

pub(crate) enum HelpOutput {
    Human(String),
    Manifest(Box<HelpManifest>),
}

pub(crate) fn help_output(
    path: &[String],
    json: bool,
    include_schemas: bool,
) -> Result<HelpOutput> {
    if json {
        Ok(HelpOutput::Manifest(Box::new(help_manifest_for_path(
            path,
            include_schemas,
        )?)))
    } else if path.is_empty() {
        Ok(HelpOutput::Human(top_help_text()))
    } else {
        Ok(HelpOutput::Human(command_help_text(path)?))
    }
}

#[cfg(test)]
pub(super) fn compact_help_json_value(path: &[String]) -> Result<Value> {
    Ok(serde_json::to_value(help_manifest_for_path(path, true)?)?)
}

#[derive(Debug)]
pub(crate) struct UnknownHelpCommand {
    path: String,
}

impl std::fmt::Display for UnknownHelpCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown command '{}' — run `glu help --json` for the command schema",
            self.path
        )
    }
}

impl std::error::Error for UnknownHelpCommand {}

fn help_manifest_for_path(path: &[String], include_schemas: bool) -> Result<HelpManifest> {
    if path.is_empty() {
        return Ok(help_manifest(None, include_schemas));
    }
    let Some(spec) = resolve_command_spec(path) else {
        return Err(UnknownHelpCommand {
            path: path.join(" "),
        }
        .into());
    };
    Ok(help_manifest(Some(spec), include_schemas))
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "HelpManifest")]
pub(crate) struct HelpManifest {
    cli: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    command: Option<CommandManifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    commands: Option<BTreeMap<String, CommandManifest>>,
    global_flags: Value,
    json: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    registry_openapi: Option<Value>,
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    schemas: Option<BTreeMap<String, Value>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct CommandManifest {
    aliases: Vec<&'static str>,
    args: BTreeMap<String, String>,
    default_behavior: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    default_scope: Option<&'static str>,
    examples: Vec<&'static str>,
    flags: Value,
    group: CommandGroup,
    id: &'static str,
    json_result: Option<&'static str>,
    mutates: bool,
    may_prompt: bool,
    name: &'static str,
    output_protocols: Vec<OutputProtocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    subcommands: Option<BTreeMap<String, CommandManifest>>,
    summary: &'static str,
    supports_flags: Vec<&'static str>,
    usage: String,
}

fn help_manifest(command: Option<&CommandSpec>, include_schemas: bool) -> HelpManifest {
    let (command_manifest, command_manifests) = match command {
        Some(spec) => (Some(command_help_manifest(spec)), None),
        None => (None, Some(command_help_manifests(COMMAND_SPECS))),
    };
    HelpManifest {
        cli: "glu",
        command: command_manifest,
        commands: command_manifests,
        global_flags: global_flags(),
        json: json_help_contract(),
        registry_openapi: include_schemas.then(registry_openapi),
        schema_version: COMMAND_SCHEMA.schema_version,
        schemas: include_schemas.then(|| generated_schemas(command)),
    }
}

fn json_help_contract() -> Value {
    serde_json::json!({
        "success": {"ok": true, "command": "string", "result": "json_result type for the command"},
        "error": {"ok": false, "command": "string|null", "invocation": "Invocation", "error": "CliError"},
        "never_prompts": true,
        "confirmation_error": "confirmation_required",
        "error_codes": [
            "parse_error", "command_failed", "confirmation_required",
            "invalid_flag_combination", "empty_declaration", "interrupted",
            "package_info_failed", "package_info_task_failed", "package_not_found",
            "package_unavailable", "not_installed", "registry_unavailable",
            "registry_error", "download_failed", "checksum_mismatch", "prepare_failed",
            "postinstall_failed", "link_failed", "partial_install_failure"
        ],
        "conventions": [
            "--json emits one envelope-wrapped JSON document with no prompts or progress output",
            "--yes only approves the computed plan; it does not imply --force, --all, or --dependents",
            "--tree preserves graph/tree semantics and emits normalized graph JSON where supported",
            "add --schemas to help --json for generated Draft 2020-12 result and error schemas"
        ],
        "schemas": "add --schemas to this invocation"
    })
}

fn global_flags() -> Value {
    serde_json::json!({
        "--json": {"short": "-j", "effect": "emit one JSON envelope"},
        "--null": {"short": "-0", "effect": "emit NUL-separated names where supported"},
        "--tree": {"short": "-t", "effect": "preserve dependency tree/graph shape where supported"},
        "--verbose": {"short": "-v", "effect": "show more detail where supported"},
        "--plan": {"short": "-p", "effect": "preview the computed mutation plan without executing it"},
        "--yes": {"short": "-y", "effect": "approve the computed plan only"}
    })
}

fn registry_openapi() -> Value {
    serde_json::json!({
        "url": registry_openapi_url(),
        "description": "Registry HTTP API schema used by online package-resolution commands.",
    })
}

fn command_help_manifests(specs: &[CommandSpec]) -> BTreeMap<String, CommandManifest> {
    specs
        .iter()
        .map(|spec| (spec.name.to_string(), command_help_manifest(spec)))
        .collect()
}

fn command_help_manifest(spec: &CommandSpec) -> CommandManifest {
    CommandManifest {
        aliases: spec.aliases.to_vec(),
        args: argument_manifest(spec),
        default_behavior: spec.default_behavior,
        default_scope: spec.default_scope,
        examples: spec.examples.to_vec(),
        flags: command_specific_flags(spec.options),
        group: spec.group,
        id: spec.id.as_str(),
        json_result: spec.result_schema,
        mutates: spec.mutates,
        may_prompt: spec.supports_global_option("--yes"),
        name: spec.name,
        output_protocols: spec.output_protocols.to_vec(),
        subcommands: (!spec.subcommands.is_empty())
            .then(|| command_help_manifests(spec.subcommands)),
        summary: spec.summary,
        supports_flags: supported_global_flags(spec),
        usage: compact_usage(spec),
    }
}

fn supported_global_flags(spec: &CommandSpec) -> Vec<&'static str> {
    GLOBAL_OPTION_SPECS
        .iter()
        .filter(|option| spec.supports_global_option(option.long))
        .map(|option| option.long)
        .collect()
}

fn command_specific_flags(options: &[OptionSpec]) -> Value {
    let flags: Map<String, Value> = options
        .iter()
        .filter(|option| !is_shared_flag(option.long))
        .map(|option| {
            let mut value = Map::new();
            if let Some(short) = option.short {
                value.insert("short".to_string(), Value::String(format!("-{short}")));
            }
            value.insert(
                "effect".to_string(),
                Value::String(option.description.to_string()),
            );
            if !option.conflicts_with.is_empty() {
                value.insert(
                    "conflicts_with".to_string(),
                    serde_json::to_value(option.conflicts_with).expect("serialize conflicts"),
                );
            }
            (option.long.to_string(), Value::Object(value))
        })
        .collect();
    Value::Object(flags)
}

fn is_shared_flag(long: &str) -> bool {
    GLOBAL_OPTION_SPECS.iter().any(|option| option.long == long)
}

fn argument_manifest(spec: &CommandSpec) -> BTreeMap<String, String> {
    spec.arguments
        .iter()
        .map(|argument| (argument.name.to_string(), argument.description.to_string()))
        .collect()
}

fn generated_schemas(command: Option<&CommandSpec>) -> BTreeMap<String, Value> {
    fn add_result_schemas(specs: &[CommandSpec], schemas: &mut BTreeMap<String, Value>) {
        for spec in specs {
            if let Some(name) = spec.result_schema {
                if !schemas.contains_key(name) {
                    let generated = crate::output::result_schema(name)
                        .or_else(|| crate::trace_cmd::result_schema(name))
                        .unwrap_or_else(|| panic!("missing generated schema for {name}"));
                    schemas.insert(name.to_string(), generated);
                }
            }
            add_result_schemas(spec.subcommands, schemas);
        }
    }

    let mut schemas = BTreeMap::from([
        (
            "ErrorEnvelope".to_string(),
            generated_schema::<JsonErrorEnvelope>(),
        ),
        (
            "HelpManifest".to_string(),
            generated_schema::<HelpManifest>(),
        ),
    ]);
    match command {
        Some(spec) => add_result_schemas(std::slice::from_ref(spec), &mut schemas),
        None => add_result_schemas(COMMAND_SPECS, &mut schemas),
    }
    schemas
}

fn registry_openapi_url() -> String {
    let base = glu_client::config::ClientConfig::default_for_host().registry_base_url;
    format!("{}/openapi.json", base.trim_end_matches('/'))
}

fn resolve_command_spec(path: &[String]) -> Option<&'static CommandSpec> {
    let mut specs = COMMAND_SPECS;
    let mut found = None;
    for segment in path {
        let spec = specs.iter().find(|spec| {
            spec.name == segment || spec.aliases.iter().any(|alias| alias == segment)
        })?;
        found = Some(spec);
        specs = spec.subcommands;
    }
    found
}

/// Resolve a command path, accepting visible aliases, after clap has built
/// the command tree and propagated its global arguments.
fn resolve_command(path: &[String]) -> Option<(clap::Command, Vec<String>)> {
    let mut root = Cli::command();
    root.build();

    let mut command = &root;
    let mut canonical_path = Vec::with_capacity(path.len());
    for segment in path {
        command = command.find_subcommand(segment)?;
        canonical_path.push(command.get_name().to_string());
    }
    Some((command.clone(), canonical_path))
}

fn command_path_for_id(id: CommandId) -> Vec<String> {
    fn find(specs: &[CommandSpec], id: CommandId, path: &mut Vec<String>) -> bool {
        for spec in specs {
            path.push(spec.name.to_string());
            if spec.id == id || find(spec.subcommands, id, path) {
                return true;
            }
            path.pop();
        }
        false
    }

    let mut path = Vec::new();
    assert!(
        find(COMMAND_SPECS, id, &mut path),
        "missing command path for {id:?}"
    );
    path
}

fn compact_usage(spec: &CommandSpec) -> String {
    let path = command_path_for_id(spec.id);
    let mut command = scoped_help_command(&path).expect("descriptor must resolve through clap");
    command
        .render_usage()
        .to_string()
        .trim()
        .strip_prefix("Usage: ")
        .unwrap_or_else(|| panic!("clap usage lacked prefix for {}", spec.id.as_str()))
        .to_string()
}

fn scoped_help_footer(spec: &CommandSpec) -> String {
    let mut sections = Vec::new();
    if !spec.aliases.is_empty() {
        sections.push(format!("Aliases: {}", spec.aliases.join(", ")));
    }
    if !spec.examples.is_empty() {
        sections.push(format!("Examples:\n  {}", spec.examples.join("\n  ")));
    }
    sections.join("\n\n")
}

/// Build a scoped help command from clap's parser metadata. The command
/// descriptor decides which invocation-wide flags are meaningful here; clap
/// remains the source for syntax, argument cardinality, and option help.
fn scoped_help_command(path: &[String]) -> Result<clap::Command> {
    let Some(spec) = resolve_command_spec(path) else {
        return Err(UnknownHelpCommand {
            path: path.join(" "),
        }
        .into());
    };
    let Some((mut command, canonical_path)) = resolve_command(path) else {
        return Err(UnknownHelpCommand {
            path: path.join(" "),
        }
        .into());
    };

    for option in GLOBAL_OPTION_SPECS {
        if !spec.supports_global_option(option.long) {
            let id = option.long.trim_start_matches('-');
            if command
                .get_arguments()
                .any(|argument| argument.get_id() == id)
            {
                command = command.mut_arg(id, |argument| argument.hide(true));
            }
        }
    }

    let footer = scoped_help_footer(spec);
    command = command
        .bin_name(format!("glu {}", canonical_path.join(" ")))
        .term_width(help_width());
    if !footer.is_empty() {
        command = command.after_long_help(footer);
    }
    Ok(command)
}

fn command_help_text(path: &[String]) -> Result<String> {
    let mut command = scoped_help_command(path)?;
    let mut rendered = command.render_long_help().to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_help_global_options_match_command_capabilities() {
        fn check(specs: &[CommandSpec], path: &mut Vec<String>) {
            for spec in specs {
                path.push(spec.name.to_string());
                let command = scoped_help_command(path).unwrap();
                let rendered = command_help_text(path).unwrap();

                for option in GLOBAL_OPTION_SPECS {
                    let argument = command
                        .get_arguments()
                        .find(|argument| argument.get_long() == Some(&option.long[2..]))
                        .unwrap_or_else(|| {
                            panic!(
                                "clap did not propagate {} to scoped command {}",
                                option.long,
                                path.join(" ")
                            )
                        });
                    assert_eq!(
                        !argument.is_hide_set(),
                        spec.supports_global_option(option.long),
                        "scoped help capability drift for {} {}",
                        path.join(" "),
                        option.long
                    );
                }

                assert!(
                    rendered.contains("--help"),
                    "scoped help omitted --help for {}",
                    path.join(" ")
                );
                for example in spec.examples {
                    assert!(
                        rendered.contains(example),
                        "scoped help omitted descriptor example {example:?} for {}",
                        path.join(" ")
                    );
                }

                check(spec.subcommands, path);
                path.pop();
            }
        }

        check(COMMAND_SPECS, &mut Vec::new());
    }

    #[test]
    fn scoped_help_aliases_render_the_canonical_command() {
        let text = command_help_text(&["i".to_string()]).unwrap();
        assert!(text.contains("Usage: glu install"));
        assert!(text.contains("Aliases: i, add"));
    }

    #[test]
    fn machine_output_footer_matches_command_capabilities() {
        fn check(specs: &[CommandSpec], parent: Option<&str>, machine: &str, null: &str) {
            for spec in specs {
                let path = parent
                    .map(|parent| format!("{parent} {}", spec.name))
                    .unwrap_or_else(|| spec.name.to_string());
                let marker = format!("`{path}`");
                assert_eq!(
                    machine.contains(&marker),
                    spec.supports_global_option("--json") || spec.supports_global_option("--null"),
                    "machine-output footer capability drift for {path}"
                );
                assert_eq!(
                    null.contains(&marker),
                    spec.supports_global_option("--null"),
                    "NUL-output footer capability drift for {path}"
                );
                check(spec.subcommands, Some(&path), machine, null);
            }
        }

        let text = machine_output_help();
        let (machine, null) = text.split_once(';').unwrap();
        check(COMMAND_SPECS, None, machine, null);
        assert!(machine.contains("`trace summary`"));
    }
}
