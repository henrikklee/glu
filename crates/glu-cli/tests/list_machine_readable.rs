#![cfg(feature = "dev-registry")]

use std::{fs, path::Path, process::Command};

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

#[test]
fn list_all_machine_readable_outputs_preserve_installed_scope() {
    let prefix = tempfile::tempdir().unwrap();
    write_declaration(prefix.path(), &[("root", "1.0")]);
    write_receipt(prefix.path(), "root", "1.0", &["dep"]);
    write_receipt(prefix.path(), "dep", "1.0", &[]);

    let default_json = json_names(prefix.path(), &["ls", "--json"]);
    assert_eq!(
        default_json,
        vec!["root"],
        "fixture must distinguish declared from installed scope"
    );

    let plain = plain_names(prefix.path(), &["ls", "--all"]);
    let json = json_names(prefix.path(), &["ls", "--all", "--json"]);
    let json_short = json_names(prefix.path(), &["ls", "-aj"]);
    let nul = nul_names(prefix.path(), &["ls", "--all", "-0"]);
    let nul_short = nul_names(prefix.path(), &["ls", "-a0"]);

    let expected = vec!["dep", "root"];
    assert_eq!(plain, expected);
    assert_eq!(json, plain);
    assert_eq!(json_short, plain);
    assert_eq!(nul, plain);
    assert_eq!(nul_short, plain);

    let tree = run_glu(prefix.path(), &["--json", "--tree", "ls", "--all"]);
    let tree: serde_json::Value = serde_json::from_slice(&tree).unwrap();
    assert_eq!(tree["result"]["view"], "tree");
    assert!(tree["result"]["nodes"]
        .as_object()
        .unwrap()
        .values()
        .all(|node| node["installed"] == true));
}

fn plain_names(prefix: &Path, args: &[&str]) -> Vec<String> {
    let output = run_glu(prefix, args);
    let mut names: Vec<String> = String::from_utf8(output)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

fn json_names(prefix: &Path, args: &[&str]) -> Vec<String> {
    let output = run_glu(prefix, args);
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["result"]["view"], "flat");
    let mut names: Vec<String> = value["result"]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| package["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

fn nul_names(prefix: &Path, args: &[&str]) -> Vec<String> {
    let output = run_glu(prefix, args);
    let mut names: Vec<String> = output
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8(part.to_vec()).unwrap())
        .collect();
    names.sort();
    names
}

fn run_glu(prefix: &Path, args: &[&str]) -> Vec<u8> {
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
    output.stdout
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
        .map(|dep| {
            format!(
                r#"{{"package_key":"package:{dep}","package":"{dep}@1.0","requested_as":"{dep}","requires":{{"version":"1.0","revision":0}}}}"#
            )
        })
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
    "deps":[{deps_json}]
  }}
}}"#,
        "0".repeat(64),
        keg.display(),
        opt.display()
    );
    fs::create_dir_all(keg.join(".glu")).unwrap();
    fs::write(keg.join(".glu/receipt.json"), receipt).unwrap();
}
