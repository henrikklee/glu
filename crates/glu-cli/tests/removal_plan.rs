#![cfg(feature = "dev-registry")]

use std::{fs, path::Path, process::Command};

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

#[test]
fn deps_default_all_and_tree_follow_progressive_disclosure() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &["leaf"]);
    write_receipt(prefix.path(), "leaf", "1.0", &[]);

    assert_eq!(human_command(prefix.path(), &["deps", "root"]), "dep\n");
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-v"]),
        "dep (1.0 installed)\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "--all"]),
        "dep\nleaf\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-t"]),
        "└── dep\n    └── leaf\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-tv"]),
        concat!(
            "└── dep (requires >= 1.0; 1.0 installed)\n",
            "    └── leaf (requires >= 1.0; 1.0 installed)\n",
        )
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "--all", "--tree"]),
        human_command(prefix.path(), &["deps", "root", "--tree"])
    );

    let summary = json_command(prefix.path(), &["deps", "root", "--json"]);
    assert_eq!(names(&summary["result"]["dependencies"]), vec!["dep"]);
    let all = json_command(prefix.path(), &["deps", "root", "--all", "--json"]);
    assert_eq!(names(&all["result"]["dependencies"]), vec!["dep", "leaf"]);
    assert_eq!(
        json_command(
            prefix.path(),
            &["deps", "root", "--all", "--tree", "--json"]
        ),
        json_command(prefix.path(), &["deps", "root", "--tree", "--json"])
    );
}

#[test]
fn list_tree_covers_the_complete_graph_and_all_is_redundant() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);
    write_receipt(prefix.path(), "orphan", "1.0", &[]);

    assert_eq!(human_command(prefix.path(), &["list"]), "root 1.0\n");
    assert_eq!(
        human_command(prefix.path(), &["list", "--all"]),
        "dep 1.0\norphan 1.0\nroot 1.0\n"
    );

    let tree = human_command(prefix.path(), &["list", "--tree"]);
    assert_eq!(tree, "orphan 1.0\nroot 1.0\n└── dep 1.0\n");
    assert_eq!(
        human_command(prefix.path(), &["list", "--all", "--tree"]),
        tree
    );
    assert_eq!(
        json_command(prefix.path(), &["list", "--all", "--tree", "--json"]),
        json_command(prefix.path(), &["list", "--tree", "--json"])
    );

    assert_eq!(
        human_command(prefix.path(), &["list", "--declared", "--tree"]),
        "root 1.0\n└── dep 1.0\n"
    );
    let declared_tree = json_command(prefix.path(), &["list", "--declared", "--tree", "--json"]);
    assert_eq!(declared_tree["result"]["scope"], "declared");
    assert_eq!(
        declared_tree["result"]["roots"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        declared_tree["result"]["nodes"].as_object().unwrap().len(),
        2
    );
}

#[test]
fn why_default_all_and_tree_follow_progressive_disclosure() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["mid"]);
    write_receipt(prefix.path(), "mid", "1.0", &["target"]);
    write_receipt(prefix.path(), "target", "1.0", &[]);

    assert_eq!(
        human_command(prefix.path(), &["why", "target"]),
        "root 1.0\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["why", "target", "--all"]),
        "mid 1.0\nroot 1.0\n"
    );
    let tree = human_command(prefix.path(), &["why", "target", "--tree"]);
    assert_eq!(tree, "└── mid 1.0\n    └── root 1.0\n");
    assert_eq!(
        human_command(prefix.path(), &["why", "target", "--all", "--tree"]),
        tree
    );

    let summary = json_command(prefix.path(), &["why", "target", "--json"]);
    assert_eq!(names(&summary["result"]["dependents"]), vec!["root"]);
    let all = json_command(prefix.path(), &["why", "target", "--all", "--json"]);
    assert_eq!(names(&all["result"]["dependents"]), vec!["mid", "root"]);
    assert_eq!(
        json_command(
            prefix.path(),
            &["why", "target", "--all", "--tree", "--json"]
        ),
        json_command(prefix.path(), &["why", "target", "--tree", "--json"])
    );
}

#[test]
fn remove_plan_human_uses_the_shared_package_list() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let output = human_command(prefix.path(), &["rm", "-p", "root"]);
    assert_eq!(
        output,
        "Would remove 2 packages:\n  ▪ dep 1.0\n  ▪ root 1.0\n"
    );
}

#[test]
fn remove_execution_prints_the_package_list_once_before_mutation() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let output = human_command(prefix.path(), &["rm", "-y", "root"]);
    assert_eq!(
        output,
        "Will remove 2 packages:\n  ▪ dep 1.0\n  ▪ root 1.0\n"
    );
}

#[test]
fn list_human_remains_an_unmarked_primitive() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);

    assert_eq!(human_command(prefix.path(), &["ls"]), "root 1.0\n");
}

#[test]
fn remove_plan_json_reports_would_remove_without_mutating() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let value = json_command(prefix.path(), &["rm", "-pj", "root"]);
    assert_eq!(value["command"], "remove");
    let result = &value["result"];
    assert_eq!(result["mode"], "plan");
    assert_eq!(result["requires_confirmation"], true);
    assert_eq!(names(&result["named"]), vec!["root"]);
    assert_eq!(names(&result["would_remove"]), vec!["dep", "root"]);
    assert!(result["would_keep"].as_array().unwrap().is_empty());

    assert!(prefix.path().join("glu.json").exists());
    assert!(prefix
        .path()
        .join("Cellar/root/1.0/.glu/receipt.json")
        .exists());
    assert!(prefix
        .path()
        .join("Cellar/dep/1.0/.glu/receipt.json")
        .exists());
}

#[test]
fn remove_json_execution_reports_executed_mode() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let value = json_command(prefix.path(), &["rm", "-yj", "root"]);
    assert_eq!(value["command"], "remove");
    let result = &value["result"];
    assert_eq!(result["mode"], "executed");
    assert_eq!(names(&result["removed"]), vec!["dep", "root"]);
}

#[test]
fn autoremove_plan_json_reports_dangling_without_mutating() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let value = json_command(prefix.path(), &["autoremove", "-pj"]);
    assert_eq!(value["command"], "autoremove");
    let result = &value["result"];
    assert_eq!(result["mode"], "plan");
    assert_eq!(result["requires_confirmation"], true);
    assert_eq!(names(&result["would_remove"]), vec!["dep"]);

    assert!(prefix.path().join("glu.json").exists());
    assert!(prefix
        .path()
        .join("Cellar/root/1.0/.glu/receipt.json")
        .exists());
    assert!(prefix
        .path()
        .join("Cellar/dep/1.0/.glu/receipt.json")
        .exists());
}

#[test]
fn autoremove_execution_prints_the_package_list_once_before_mutation() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let output = human_command(prefix.path(), &["autoremove", "-y"]);
    assert_eq!(output, "Will remove 1 unused package:\n  ▪ dep 1.0\n");
}

#[test]
fn autoremove_json_execution_reports_executed_mode() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let value = json_command(prefix.path(), &["autoremove", "-yj"]);
    assert_eq!(value["command"], "autoremove");
    let result = &value["result"];
    assert_eq!(result["mode"], "executed");
    assert_eq!(names(&result["packages"]), vec!["dep"]);
}

#[test]
fn remove_plan_summarizes_configuration_and_json_keeps_exact_paths() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    let unchanged = write_mutable_file(
        prefix.path(),
        "root",
        "1.0",
        "etc/root/default.conf",
        b"default\n",
        b"default\n",
    );
    let modified = write_mutable_file(
        prefix.path(),
        "root",
        "1.0",
        "etc/root/user.conf",
        b"default\n",
        b"changed\n",
    );

    let human = human_command(prefix.path(), &["rm", "--plan", "root"]);
    assert!(human.contains(
        "Configuration: 1 unchanged file will be removed · 1 modified file kept by default\n"
    ));
    assert!(!human.contains(unchanged.to_string_lossy().as_ref()));
    assert!(!human.contains(modified.to_string_lossy().as_ref()));

    let verbose = human_command(prefix.path(), &["rm", "--plan", "--verbose", "root"]);
    assert!(verbose.contains(unchanged.to_string_lossy().as_ref()));
    assert!(verbose.contains(modified.to_string_lossy().as_ref()));

    let json = json_command(prefix.path(), &["rm", "--plan", "--json", "root"]);
    assert_eq!(
        json["result"]["configuration"]["would_remove"],
        serde_json::json!([unchanged])
    );
    assert_eq!(
        json["result"]["configuration"]["would_retain_modified"],
        serde_json::json!([modified.clone()])
    );

    let explicit = human_command(prefix.path(), &["rm", "--plan", "--remove-config", "root"]);
    assert!(explicit.contains(
        "Configuration: 1 unchanged file will be removed · 1 modified file will be removed\n"
    ));
    let explicit_json = json_command(
        prefix.path(),
        &["rm", "--plan", "--remove-config", "--json", "root"],
    );
    assert_eq!(explicit_json["result"]["requires_confirmation"], true);
    assert_eq!(
        explicit_json["result"]["configuration"]["would_remove_modified"],
        serde_json::json!([modified])
    );
    assert!(
        explicit_json["result"]["configuration"]["would_retain_modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn remove_cleans_unchanged_files_and_preserves_modified_files_by_default() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    let unchanged = write_mutable_file(
        prefix.path(),
        "root",
        "1.0",
        "etc/root/default.conf",
        b"default\n",
        b"default\n",
    );
    let modified = write_mutable_file(
        prefix.path(),
        "root",
        "1.0",
        "etc/root/user.conf",
        b"default\n",
        b"changed\n",
    );

    let result = json_command(prefix.path(), &["rm", "--yes", "--json", "root"]);

    assert!(!unchanged.exists());
    assert!(modified.exists());
    assert_eq!(
        result["result"]["configuration"]["removed"],
        serde_json::json!([unchanged])
    );
    assert_eq!(
        result["result"]["configuration"]["retained_modified"],
        serde_json::json!([modified])
    );
}

#[test]
fn modified_configuration_requires_explicit_approved_cleanup() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &[]);
    let modified = write_mutable_file(
        prefix.path(),
        "root",
        "1.0",
        "etc/root/user.conf",
        b"default\n",
        b"changed\n",
    );

    let refused = glu()
        .args(["rm", "--remove-config", "--json", "root"])
        .env("GLU_PREFIX", prefix.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["error"]["code"], "confirmation_required");
    assert_eq!(
        error["error"]["details"]["modified_config_files"],
        serde_json::json!([modified.clone()])
    );
    assert!(prefix.path().join("Cellar/root/1.0").exists());
    assert!(modified.exists());

    let result = json_command(
        prefix.path(),
        &["rm", "--remove-config", "--yes", "--json", "root"],
    );
    assert_eq!(
        result["result"]["configuration"]["removed"],
        serde_json::json!([modified.clone()])
    );
    assert!(!modified.exists());
}

fn human_command(prefix: &Path, args: &[&str]) -> String {
    let output = glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "glu {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

fn json_command(prefix: &Path, args: &[&str]) -> serde_json::Value {
    let output = glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "glu {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    value
}

fn names(value: &serde_json::Value) -> Vec<&str> {
    let mut names: Vec<&str> = value
        .as_array()
        .unwrap()
        .iter()
        .map(|package| package["name"].as_str().unwrap())
        .collect();
    names.sort();
    names
}

fn write_declaration(prefix: &Path, packages: &[(&str, &str)]) {
    let dependencies = packages
        .iter()
        .map(|(name, version)| format!(r#""{name}":"{version}""#))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(
        prefix.join("glu.json"),
        format!(r#"{{"schema":"glu.declaration.v1","dependencies":{{{dependencies}}},"deactivated":{{}}}}"#),
    )
    .unwrap();
}

fn write_receipt(prefix: &Path, name: &str, version: &str, deps: &[&str]) {
    let keg = prefix.join("Cellar").join(name).join(version);
    let opt = prefix.join("opt").join(name);
    let deps_json = deps
        .iter()
        .map(|dependency| format!(r#""{dependency}""#))
        .collect::<Vec<_>>()
        .join(",");
    let dependency_requirements_json = deps
        .iter()
        .map(|dependency| format!(r#""package:{dependency}":{{"version":"1.0","revision":0}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let receipt = format!(
        r#"{{
  "schema":"glu.install-receipt.v1",
  "status":"complete",
  "package":{{
    "id":"{name}@{version}",
    "package_key":"package:{name}",
    "name":"{name}",
    "aliases":[],
    "oldnames":[],
    "version":"{version}",
    "revision":0,
    "keg_version":"{version}"
  }},
  "artifact":{{
    "id":"artifact:{name}@{version}",
    "sha256":"{}",
    "bottle_tag":"test",
    "cellar":"/opt/homebrew"
  }},
  "paths":{{
    "keg":"{}",
    "opt":"{}"
  }},
  "links":{{"opt_names":[]}},
  "install":{{
    "exposure":{{"mode":"global"}},
    "linked":true,
    "link_overwrite":[],
    "deps":[{deps_json}],
    "dependency_requirements":{{{dependency_requirements_json}}}
  }}
}}"#,
        "0".repeat(64),
        keg.display(),
        opt.display()
    );
    fs::create_dir_all(keg.join(".glu")).unwrap();
    fs::write(keg.join(".glu/receipt.json"), receipt).unwrap();
}

fn write_mutable_file(
    prefix: &Path,
    name: &str,
    version: &str,
    relative: &str,
    default: &[u8],
    live: &[u8],
) -> std::path::PathBuf {
    let source = prefix
        .join("Cellar")
        .join(name)
        .join(version)
        .join(".bottle")
        .join(relative);
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(source, default).unwrap();
    let destination = prefix.join(relative);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(&destination, live).unwrap();
    destination
}
