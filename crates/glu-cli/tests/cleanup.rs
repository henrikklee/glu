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

fn parse_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("valid JSON")
}

fn write_cache(prefix: &Path) {
    let root = prefix.join("var/glu/cache/artifacts");
    fs::create_dir_all(root.join("sha256")).unwrap();
    fs::create_dir_all(root.join("tmp")).unwrap();
    fs::write(root.join("sha256/aaa"), b"abc").unwrap();
    fs::write(root.join("sha256/bbb"), b"bottle").unwrap();
    fs::write(root.join("tmp/partial.tmp"), b"12").unwrap();
    write_receipt(prefix, "demo", "1.0", "aaa");
}

fn write_receipt(prefix: &Path, name: &str, version: &str, sha256: &str) {
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
    "sha256":"{sha256}",
    "bottle_tag":"test",
    "cellar":"/opt/homebrew"
  }},
  "paths":{{"keg":"{}","opt":"{}"}},
  "links":{{"opt_names":[]}},
  "install":{{"keg_only":false,"linked":true,"link_overwrite":[],"deps":[],"dependency_requirements":{{}}}}
}}"#,
        keg.display(),
        opt.display()
    );
    fs::create_dir_all(keg.join(".glu")).unwrap();
    fs::write(keg.join(".glu/receipt.json"), receipt).unwrap();
}

#[test]
fn cleanup_plan_lists_downloads_and_does_not_mutate() {
    let prefix = tempfile::tempdir().unwrap();
    write_cache(prefix.path());

    let output = run(prefix.path(), &["cleanup", "--plan", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout);
    assert_eq!(value["command"], "cleanup");
    assert_eq!(value["result"]["mode"], "plan");
    assert_eq!(value["result"]["requires_confirmation"], true);
    assert_eq!(value["result"]["would_reclaim_bytes"], 11);
    assert_eq!(value["result"]["would_remove_downloads"], 3);
    assert_eq!(value["result"]["unassociated_downloads"], 2);
    assert_eq!(value["result"]["unassociated_bytes"], 8);
    let entries = value["result"]["would_remove"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "demo");
    assert_eq!(entries[0]["version"], "1.0");
    assert_eq!(entries[0]["downloads"], 1);
    assert_eq!(entries[0]["bytes"], 3);
    assert!(entries[0].get("path").is_none());
    assert!(prefix
        .path()
        .join("var/glu/cache/artifacts/sha256/aaa")
        .exists());
    assert!(prefix
        .path()
        .join("var/glu/cache/artifacts/tmp/partial.tmp")
        .exists());
}

#[test]
fn cleanup_json_requires_yes_and_leaves_cache_unchanged() {
    let prefix = tempfile::tempdir().unwrap();
    write_cache(prefix.path());

    let output = run(prefix.path(), &["cleanup", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value = parse_json(&output.stderr);
    assert_eq!(value["command"], "cleanup");
    assert_eq!(value["error"]["code"], "confirmation_required");
    assert_eq!(value["error"]["details"]["reclaimable_bytes"], 11);
    assert_eq!(value["error"]["details"]["planned_downloads"], 3);
    assert_eq!(value["error"]["details"]["unassociated_downloads"], 2);
    assert_eq!(
        value["error"]["details"]["planned_removals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        value["error"]["details"]["planned_removals"][0]["name"],
        "demo"
    );
    assert!(prefix
        .path()
        .join("var/glu/cache/artifacts/sha256/aaa")
        .exists());
}

#[test]
fn cleanup_yes_removes_only_cached_downloads_and_reports_result() {
    let prefix = tempfile::tempdir().unwrap();
    write_cache(prefix.path());
    let unrelated_cache = prefix.path().join("var/glu/cache/postinstall/state");
    let receipt = prefix.path().join("Cellar/pkg/1.0/.glu/receipt.json");
    fs::create_dir_all(unrelated_cache.parent().unwrap()).unwrap();
    fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    fs::write(&unrelated_cache, b"state").unwrap();
    fs::write(&receipt, b"receipt").unwrap();

    let output = run(prefix.path(), &["cleanup", "--yes", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout);
    assert_eq!(value["result"]["mode"], "executed");
    assert_eq!(value["result"]["reclaimed_bytes"], 11);
    assert_eq!(value["result"]["removed_downloads"], 3);
    assert_eq!(value["result"]["unassociated_downloads"], 2);
    assert_eq!(value["result"]["removed"].as_array().unwrap().len(), 1);
    assert_eq!(value["result"]["removed"][0]["name"], "demo");
    assert!(!prefix
        .path()
        .join("var/glu/cache/artifacts/sha256/aaa")
        .exists());
    assert!(!prefix
        .path()
        .join("var/glu/cache/artifacts/tmp/partial.tmp")
        .exists());
    assert!(unrelated_cache.exists());
    assert!(receipt.exists());
}

#[test]
fn cleanup_empty_cache_is_a_successful_noop() {
    let prefix = tempfile::tempdir().unwrap();

    let output = run(prefix.path(), &["cleanup", "--json"]);

    assert!(output.status.success());
    let value = parse_json(&output.stdout);
    assert_eq!(value["result"]["mode"], "executed");
    assert_eq!(value["result"]["reclaimed_bytes"], 0);
    assert_eq!(value["result"]["removed_downloads"], 0);
    assert!(value["result"]["removed"].as_array().unwrap().is_empty());
}

#[test]
fn cleanup_human_execution_lists_downloads_once_before_mutation() {
    let prefix = tempfile::tempdir().unwrap();
    write_cache(prefix.path());

    let output = run(prefix.path(), &["cleanup", "--yes"]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Will remove 3 cached downloads:\n",
            "  ▪ demo 1.0 (3 B)\n",
            "  ▪ Unassociated (2 downloads, 8 B)\n",
            "Will reclaim: 11 B\n",
            "Reclaimed: 11 B\n",
        )
    );
}

#[test]
fn cleanup_human_plan_reports_package_names_and_total() {
    let prefix = tempfile::tempdir().unwrap();
    write_cache(prefix.path());

    let output = run(prefix.path(), &["cleanup", "--plan"]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "Would remove 3 cached downloads:\n",
            "  ▪ demo 1.0 (3 B)\n",
            "  ▪ Unassociated (2 downloads, 8 B)\n",
            "Would reclaim: 11 B\n",
        )
    );
}
