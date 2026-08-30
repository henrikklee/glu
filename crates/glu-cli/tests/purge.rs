#![cfg(feature = "dev-registry")]

use std::{fs, path::Path, process::Command};

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

fn run(prefix: &Path, args: &[&str]) -> std::process::Output {
    glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("NO_COLOR", "1")
        .output()
        .expect("run glu")
}

fn json_command(prefix: &Path, args: &[&str]) -> serde_json::Value {
    let output = run(prefix, args);
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

fn write_declaration(prefix: &Path) -> Vec<u8> {
    let declaration = br#"{
  "schema": "glu.declaration.v1",
  "dependencies": {
    "root": "1.0"
  },
  "deactivated": {
    "root": true
  }
}"#
    .to_vec();
    fs::write(prefix.join("glu.json"), &declaration).unwrap();
    declaration
}

fn write_receipt(prefix: &Path, name: &str, version: &str, installed_bytes: u64) {
    let keg = prefix.join("Cellar").join(name).join(version);
    let opt = prefix.join("opt").join(name);
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
    "sha256":"sha-{name}",
    "bottle_tag":"test",
    "cellar":"/opt/homebrew"
  }},
  "sizes":{{"installed_bytes":{installed_bytes}}},
  "paths":{{"keg":"{}","opt":"{}"}},
  "links":{{"opt_names":[]}},
  "install":{{"keg_only":false,"linked":false,"link_overwrite":[],"deps":[],"dependency_requirements":{{}}}}
}}"#,
        keg.display(),
        opt.display()
    );
    fs::create_dir_all(keg.join(".glu")).unwrap();
    fs::write(keg.join(".glu/receipt.json"), receipt).unwrap();
}

fn prepare_prefix(prefix: &Path) -> Vec<u8> {
    let declaration = write_declaration(prefix);
    write_receipt(prefix, "root", "1.0", 7);
    write_receipt(prefix, "dep", "2.0", 5);
    fs::create_dir_all(prefix.join("var/glu/cache/artifacts/sha256")).unwrap();
    fs::write(
        prefix.join("var/glu/cache/artifacts/sha256/cached"),
        b"cache",
    )
    .unwrap();
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::write(prefix.join("bin/glu"), b"preserve me").unwrap();
    declaration
}

#[test]
fn purge_plan_reports_everything_without_mutating() {
    let prefix = tempfile::tempdir().unwrap();
    prepare_prefix(prefix.path());

    let value = json_command(prefix.path(), &["purge", "--plan", "--json"]);

    assert_eq!(value["command"], "purge");
    let result = &value["result"];
    assert_eq!(result["mode"], "plan");
    assert_eq!(result["requires_confirmation"], true);
    assert_eq!(result["declaration"], "removed");
    assert_eq!(result["declared_packages"], 1);
    assert_eq!(result["would_reclaim_bytes"], 12);
    assert_eq!(result["would_remove"].as_array().unwrap().len(), 2);
    assert!(prefix.path().join("glu.json").exists());
    assert!(prefix.path().join("Cellar/root/1.0").exists());
    assert!(prefix.path().join("Cellar/dep/2.0").exists());
}

#[test]
fn purge_json_requires_yes_and_reports_the_approved_scope() {
    let prefix = tempfile::tempdir().unwrap();
    prepare_prefix(prefix.path());

    let output = run(prefix.path(), &["purge", "--keep-declaration", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["command"], "purge");
    assert_eq!(value["error"]["code"], "confirmation_required");
    assert_eq!(value["error"]["details"]["declaration"], "preserved");
    assert_eq!(value["error"]["details"]["declared_packages"], 1);
    assert_eq!(value["error"]["details"]["reclaimable_bytes"], 12);
    assert_eq!(
        value["error"]["details"]["planned_removals"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(prefix.path().join("glu.json").exists());
    assert!(prefix.path().join("Cellar/root/1.0").exists());
}

#[test]
fn purge_removes_all_packages_and_declaration_but_preserves_cache_and_glu() {
    let prefix = tempfile::tempdir().unwrap();
    prepare_prefix(prefix.path());

    let value = json_command(prefix.path(), &["purge", "--yes", "--json"]);

    let result = &value["result"];
    assert_eq!(result["mode"], "executed");
    assert_eq!(result["declaration"], "removed");
    assert_eq!(result["declared_packages"], 1);
    assert_eq!(result["reclaimed_bytes"], 12);
    assert_eq!(result["removed"].as_array().unwrap().len(), 2);
    assert!(!prefix.path().join("glu.json").exists());
    assert!(!prefix.path().join("Cellar/root/1.0").exists());
    assert!(!prefix.path().join("Cellar/dep/2.0").exists());
    assert!(prefix
        .path()
        .join("var/glu/cache/artifacts/sha256/cached")
        .exists());
    assert_eq!(
        fs::read(prefix.path().join("bin/glu")).unwrap(),
        b"preserve me"
    );
}

#[test]
fn keep_declaration_preserves_glu_json_verbatim() {
    let prefix = tempfile::tempdir().unwrap();
    let declaration = prepare_prefix(prefix.path());

    let value = json_command(
        prefix.path(),
        &["purge", "--keep-declaration", "--yes", "--json"],
    );

    let result = &value["result"];
    assert_eq!(result["declaration"], "preserved");
    assert_eq!(result["removed"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read(prefix.path().join("glu.json")).unwrap(),
        declaration
    );
    assert!(!prefix.path().join("Cellar/root/1.0").exists());
    assert!(!prefix.path().join("Cellar/dep/2.0").exists());
}

#[test]
fn purge_empty_prefix_is_a_successful_noop() {
    let prefix = tempfile::tempdir().unwrap();

    let value = json_command(prefix.path(), &["purge", "--json"]);

    let result = &value["result"];
    assert_eq!(result["mode"], "executed");
    assert_eq!(result["declaration"], "absent");
    assert_eq!(result["reclaimed_bytes"], 0);
    assert!(result["removed"].as_array().unwrap().is_empty());
}

#[test]
fn purge_can_remove_a_declaration_when_nothing_is_installed() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path());

    let value = json_command(prefix.path(), &["purge", "--yes", "--json"]);

    assert_eq!(value["result"]["declaration"], "removed");
    assert!(value["result"]["removed"].as_array().unwrap().is_empty());
    assert!(!prefix.path().join("glu.json").exists());
}

#[test]
fn purge_human_plan_explains_kept_declaration_and_restore_path() {
    let prefix = tempfile::tempdir().unwrap();
    prepare_prefix(prefix.path());

    let output = run(prefix.path(), &["purge", "--keep-declaration", "--plan"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Would remove 2 packages:\n",
            "  ▪ dep 2.0\n",
            "  ▪ root 1.0\n",
            "Would preserve glu.json (1 declared package).\n",
            "Would reclaim: 12 B\n",
        )
    );
}
