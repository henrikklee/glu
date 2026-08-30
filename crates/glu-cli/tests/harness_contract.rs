#[cfg(feature = "dev-registry")]
use std::fs;
use std::process::Command;

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

fn rust_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn client_process_output_is_confined_to_the_internal_worker_host() {
    let client_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../glu-client/src");
    let mut violations = Vec::new();
    for path in rust_files(&client_src) {
        if path.file_name().and_then(|name| name.to_str()) == Some("worker_output.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for (index, line) in source.lines().enumerate() {
            if ["println!", "eprintln!", "print!", "eprint!"]
                .iter()
                .any(|output_macro| line.contains(output_macro))
            {
                violations.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "client output must use typed results/events:\n{}",
        violations.join("\n")
    );
}

fn run(prefix: &std::path::Path, args: &[&str]) -> std::process::Output {
    glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("NO_COLOR", "1")
        .output()
        .expect("run glu")
}

#[cfg(feature = "dev-registry")]
fn run_with_state_load_count(
    prefix: &std::path::Path,
    args: &[&str],
) -> (std::process::Output, usize) {
    let log = prefix.join("state-loads.log");
    let _ = fs::remove_file(&log);
    let output = glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("GLU_REGISTRY", "not a registry URL")
        .env("GLU_TEST_STATE_LOAD_LOG", &log)
        .env("NO_COLOR", "1")
        .output()
        .expect("run glu with state load counter");
    let loads = fs::read_to_string(log)
        .map(|contents| contents.lines().count())
        .unwrap_or(0);
    (output, loads)
}

fn parse_one_json(bytes: &[u8]) -> serde_json::Value {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<serde_json::Value>();
    let value = values.next().expect("one JSON value").expect("valid JSON");
    assert!(
        values.next().is_none(),
        "stream contained more than one JSON value"
    );
    value
}

#[test]
fn global_json_flag_works_before_and_after_command() {
    let prefix = tempfile::tempdir().unwrap();
    for args in [&["--json", "status"][..], &["status", "--json"][..]] {
        let output = run(prefix.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let value = parse_one_json(&output.stdout);
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "status");
        assert!(value["result"]["prefix_source"].is_string());
    }
}

#[test]
fn global_flags_work_at_each_supported_position_and_in_short_bundles() {
    let prefix = tempfile::tempdir().unwrap();
    for args in [
        &["--json", "status"][..],
        &["status", "--json"][..],
        &["--json", "trace", "list"][..],
        &["trace", "--json", "list"][..],
        &["trace", "list", "--json"][..],
        &["list", "-jt"][..],
    ] {
        let output = run(prefix.path(), args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "{args:?}");
        assert_eq!(parse_one_json(&output.stdout)["ok"], true, "{args:?}");
    }
}

#[test]
fn runtime_documents_validate_against_published_schemas() {
    let prefix = tempfile::tempdir().unwrap();
    let help = run(prefix.path(), &["help", "--json", "--schemas"]);
    assert!(help.status.success());
    let document = parse_one_json(&help.stdout);
    let manifest = &document["result"];

    let status = run(prefix.path(), &["status", "--json"]);
    assert!(status.status.success());
    let success = parse_one_json(&status.stdout);
    let status_validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&manifest["schemas"]["StatusResult"])
        .unwrap();
    assert!(status_validator.is_valid(&success["result"]));

    let failure = run(prefix.path(), &["status", "--json", "--all"]);
    assert_eq!(failure.status.code(), Some(2));
    let error = parse_one_json(&failure.stderr);
    let error_validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&manifest["schemas"]["ErrorEnvelope"])
        .unwrap();
    assert!(error_validator.is_valid(&error));
}

#[test]
fn default_help_json_is_compact_and_schemas_are_explicit() {
    let prefix = tempfile::tempdir().unwrap();

    let overview = run(prefix.path(), &["help", "--json"]);
    assert!(overview.status.success());
    assert!(
        overview.stdout.len() <= 20_000,
        "{} bytes",
        overview.stdout.len()
    );
    let overview_document = parse_one_json(&overview.stdout);
    assert_eq!(overview_document["command"], "help");
    assert!(overview_document["result"].get("schemas").is_none());

    let scoped = run(prefix.path(), &["help", "install", "--json"]);
    assert!(scoped.status.success());
    assert!(
        scoped.stdout.len() <= 8_000,
        "{} bytes",
        scoped.stdout.len()
    );
    let scoped_document = parse_one_json(&scoped.stdout);
    assert!(scoped_document["result"].get("schemas").is_none());

    let schemas = run(prefix.path(), &["help", "install", "--json", "--schemas"]);
    assert!(schemas.status.success());
    let schemas_document = parse_one_json(&schemas.stdout);
    let keys = schemas_document["result"]["schemas"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        ["ErrorEnvelope", "HelpManifest", "InstallResult"]
            .into_iter()
            .collect()
    );

    let invalid = run(prefix.path(), &["help", "--schemas"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
}

#[test]
fn invalid_scoped_help_is_a_typed_usage_error() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["help", "wat", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let value = parse_one_json(&output.stderr);
    assert_eq!(value["error"]["code"], "parse_error");
    assert_eq!(value["command"], "help");
}

#[test]
fn shellenv_uses_raw_shell_text_protocol() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["shellenv"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("export PATH="), "{stdout}");
    assert!(!stdout.contains("\"ok\":"));
}

#[test]
fn renderer_flags_after_argument_boundary_are_positional_data() {
    let prefix = tempfile::tempdir().unwrap();

    std::fs::write(prefix.path().join("glu.json"), b"{bad").unwrap();
    let info = run(prefix.path(), &["info", "--", "--json"]);
    assert_eq!(info.status.code(), Some(1));
    assert!(info.stdout.is_empty());
    assert!(!info.stderr.is_empty());
    assert!(
        serde_json::from_slice::<serde_json::Value>(&info.stderr).is_err(),
        "positional --json selected JSON diagnostics: {}",
        String::from_utf8_lossy(&info.stderr)
    );

    for positional in ["--json", "-j", "--json=true"] {
        let status = run(prefix.path(), &["status", "--", positional]);
        assert_eq!(status.status.code(), Some(2), "argument {positional}");
        assert!(status.stdout.is_empty());
        assert!(
            serde_json::from_slice::<serde_json::Value>(&status.stderr).is_err(),
            "positional {positional} selected JSON diagnostics: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            String::from_utf8_lossy(&status.stderr).contains(positional),
            "human diagnostic omitted positional argument {positional}"
        );
    }
}

#[test]
fn bootstrap_json_forms_before_argument_boundary_are_preserved() {
    let prefix = tempfile::tempdir().unwrap();

    let malformed_long = run(prefix.path(), &["--json=true", "status"]);
    assert_eq!(malformed_long.status.code(), Some(2));
    assert!(malformed_long.stdout.is_empty());
    let value = parse_one_json(&malformed_long.stderr);
    assert_eq!(value["error"]["code"], "parse_error");
    assert_eq!(
        value["invocation"]["argv"],
        serde_json::json!(["--json=true", "status"])
    );

    let short_bundle = run(prefix.path(), &["-vj", "wat"]);
    assert_eq!(short_bundle.status.code(), Some(2));
    assert!(short_bundle.stdout.is_empty());
    let value = parse_one_json(&short_bundle.stderr);
    assert_eq!(value["error"]["code"], "parse_error");
    assert_eq!(
        value["invocation"]["argv"],
        serde_json::json!(["-vj", "wat"])
    );
}

#[cfg(feature = "dev-registry")]
#[test]
fn read_commands_load_one_local_snapshot_and_null_uses_loads_none() {
    let prefix = tempfile::tempdir().unwrap();

    for args in [
        &["list"][..],
        &["list", "--tree"][..],
        &["list", "--all"][..],
        &["list", "--json"][..],
        &["list", "--null"][..],
        &["list", "--tree", "--json"][..],
        &["list", "--tree", "--null"][..],
        &["list", "--all", "--tree"][..],
        &["status"][..],
        &["status", "--json"][..],
        &["why", "missing"][..],
        &["why", "--null", "missing"][..],
    ] {
        let (output, loads) = run_with_state_load_count(prefix.path(), args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(loads, 1, "local snapshot load count for {args:?}");
    }

    for args in [
        &["deps", "missing"][..],
        &["info", "missing"][..],
        &["info", "one", "two"][..],
        &["outdated"][..],
        &["uses", "missing"][..],
        &["uses", "--json", "missing"][..],
    ] {
        let (output, loads) = run_with_state_load_count(prefix.path(), args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(loads, 1, "local snapshot load count for {args:?}");
    }

    let (output, loads) = run_with_state_load_count(prefix.path(), &["uses", "--null", "missing"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(loads, 0, "uses --null loaded annotations it cannot emit");
}

#[test]
fn json_success_is_exactly_one_document_with_empty_stderr() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["--json", "status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "JSON success leaked stderr prose");
    let value = parse_one_json(&output.stdout);
    assert_eq!(value["ok"], true);
}

#[cfg(feature = "dev-registry")]
#[test]
fn json_output_is_pure_when_local_state_contains_a_bad_receipt() {
    let prefix = tempfile::tempdir().unwrap();
    let receipt = prefix.path().join("Cellar/bad/1.0/.glu/receipt.json");
    fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    fs::write(receipt, b"{bad").unwrap();

    let output = run(prefix.path(), &["--json", "status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "JSON success leaked stderr prose");
    let value = parse_one_json(&output.stdout);
    assert_eq!(value["ok"], true);
    assert_eq!(value["result"]["installed_count"], 0);

    let human = run(prefix.path(), &["status"]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stderr)
        .contains("warning: skipped unreadable install receipt"));
}

#[test]
fn parse_and_capability_errors_are_single_json_diagnostics() {
    let prefix = tempfile::tempdir().unwrap();

    let parse = run(prefix.path(), &["--json", "wat"]);
    assert_eq!(parse.status.code(), Some(2));
    assert!(parse.stdout.is_empty());
    let value = parse_one_json(&parse.stderr);
    assert_eq!(value["error"]["code"], "parse_error");
    assert!(value["invocation"].is_object());

    let unsupported = run(prefix.path(), &["--json", "setup"]);
    assert_eq!(unsupported.status.code(), Some(2));
    assert!(unsupported.stdout.is_empty());
    let value = parse_one_json(&unsupported.stderr);
    assert_eq!(value["error"]["code"], "invalid_flag_combination");
    assert_eq!(value["error"]["offending_arg"], "--json");
}

#[test]
fn top_level_machine_output_help_includes_nested_capabilities() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["--help"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(normalized.contains("trace summary"), "{text}");
    assert!(normalized.contains("Default scope: latest"), "{text}");
}

#[test]
fn scoped_human_help_is_generated_from_clap_and_command_capabilities() {
    fn visible_long_options(text: &str) -> std::collections::BTreeSet<&str> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim_start();
                if !line.starts_with('-') {
                    return None;
                }
                line.split_whitespace()
                    .find(|part| part.starts_with("--"))
                    .map(|part| part.trim_end_matches(','))
            })
            .collect()
    }

    let prefix = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (
            &["install", "--help"][..],
            &[
                "--deps",
                "--force",
                "--help",
                "--json",
                "--plan",
                "--tree",
                "--verbose",
                "--yes",
            ][..],
        ),
        (
            &["list", "--help"][..],
            &[
                "--all",
                "--declared",
                "--help",
                "--installed",
                "--json",
                "--null",
                "--tree",
            ][..],
        ),
        (
            &["trace", "summary", "--help"][..],
            &["--all", "--help", "--json", "--verbose"][..],
        ),
        (&["setup", "--help"][..], &["--help"][..]),
    ] {
        let output = run(prefix.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            visible_long_options(&text),
            expected.iter().copied().collect(),
            "scoped option drift for {args:?}\n{text}"
        );
    }

    let direct = run(prefix.path(), &["install", "--help"]);
    let help_command = run(prefix.path(), &["help", "install"]);
    assert_eq!(direct.status.code(), help_command.status.code());
    assert_eq!(direct.stdout, help_command.stdout);
    assert_eq!(direct.stderr, help_command.stderr);
}

#[test]
fn help_manifest_uses_explicit_result_identities_and_runtime_fields() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["help", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = parse_one_json(&output.stdout);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "help");
    let value = &document["result"];

    assert!(value.get("schemas").is_none());
    assert_eq!(
        value["commands"]["trace"]["subcommands"]["summary"]["id"],
        "trace summary"
    );
    assert_eq!(
        value["commands"]["trace"]["subcommands"]["summary"]["group"],
        "observability"
    );
    assert_eq!(
        value["commands"]["help"]["output_protocols"],
        serde_json::json!(["human", "json_envelope"])
    );
    assert_eq!(value["commands"]["help"]["json_result"], "HelpManifest");
    assert_eq!(
        value["commands"]["trace"]["subcommands"]["summary"]["json_result"],
        "TraceSummaryResult"
    );
    let schemas_output = run(prefix.path(), &["help", "--json", "--schemas"]);
    assert!(schemas_output.status.success());
    let schemas_document = parse_one_json(&schemas_output.stdout);
    let schemas = &schemas_document["result"]["schemas"];
    assert_eq!(schemas["ListResult"]["anyOf"].as_array().unwrap().len(), 2);
    for field in [
        "prefix_source",
        "prefix_length",
        "fixed_cellar_length",
        "registry",
        "distribution",
        "deactivated",
    ] {
        assert!(
            schemas["StatusResult"]["properties"].get(field).is_some(),
            "missing StatusResult.{field}"
        );
    }
    for field in ["requested_as", "package_key", "package", "exposure"] {
        assert!(
            schemas["InfoResult"]["$defs"]["InfoResponse"]["properties"]
                .get(field)
                .is_some(),
            "missing InfoResponse.{field}"
        );
    }
}

#[cfg(feature = "dev-registry")]
#[test]
fn plan_is_read_only_but_execution_runs_recovery() {
    let prefix = tempfile::tempdir().unwrap();
    let staging = prefix.path().join("var/glu/staging");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("marker"), b"keep during plan").unwrap();

    let planned = run(prefix.path(), &["autoremove", "--plan", "--json"]);
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    parse_one_json(&planned.stdout);
    assert!(staging.exists(), "--plan mutated recovery state");

    let executed = run(prefix.path(), &["autoremove", "--yes", "--json"]);
    assert!(
        executed.status.success(),
        "{}",
        String::from_utf8_lossy(&executed.stderr)
    );
    parse_one_json(&executed.stdout);
    assert!(
        !staging.exists(),
        "execution did not run interrupted-state recovery"
    );
}

#[test]
fn trace_list_json_has_a_stable_object_result() {
    let prefix = tempfile::tempdir().unwrap();
    let output = run(prefix.path(), &["--json", "trace", "list"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value = parse_one_json(&output.stdout);
    assert_eq!(value["command"], "trace list");
    assert!(value["result"]["traces"].is_array());
}

#[test]
fn required_mutation_selectors_fail_during_parsing() {
    let prefix = tempfile::tempdir().unwrap();
    for command in ["remove", "reinstall"] {
        let output = run(prefix.path(), &["--json", command]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let value = parse_one_json(&output.stderr);
        assert_eq!(value["error"]["code"], "parse_error");
    }
}
