#![cfg(feature = "dev-registry")]

use std::{fs, path::Path, process::Command};

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

#[test]
fn deps_human_hides_versions_until_verbose() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    assert_eq!(human_command(prefix.path(), &["deps", "root"]), "dep\n");
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-v"]),
        "dep (1.0 installed)\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-t"]),
        "└── root\n    └── dep\n"
    );
    assert_eq!(
        human_command(prefix.path(), &["deps", "root", "-tv"]),
        concat!(
            "└── root (1.0 installed)\n",
            "    └── dep (requires >= 1.0; 1.0 installed)\n",
        )
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
    "keg_only":false,
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
