#![cfg(feature = "dev-registry")]

use flate2::{write::GzEncoder, Compression};
use ring::digest;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    thread,
};

fn glu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_glu"))
}

fn run(prefix: &Path, args: &[&str]) -> std::process::Output {
    glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("GLU_REGISTRY", "not-used-for-empty-migration")
        .env("NO_COLOR", "1")
        .output()
        .expect("run glu")
}

fn source_with_receipt(requested: bool) -> tempfile::TempDir {
    let source = tempfile::tempdir().unwrap();
    let keg = source.path().join("Cellar/demo/1.0");
    fs::create_dir_all(&keg).unwrap();
    fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({"installed_on_request": requested}).to_string(),
    )
    .unwrap();
    source
}

fn write_requested_receipt(source: &Path, name: &str) {
    let keg = source.join("Cellar").join(name).join("1.0");
    fs::create_dir_all(&keg).unwrap();
    fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        r#"{"installed_on_request":true}"#,
    )
    .unwrap();
}

fn registry_for_migration() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 8192];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|path| path.split("target=").nth(1))
            .and_then(|tail| tail.split('&').next())
            .unwrap_or("arm64_sequoia");
        let sha = "0".repeat(64);
        let global_id = "pkg:test/global-root@1.0";
        let isolated_id = "pkg:test/isolated-root@1.0";
        let artifact_id = format!("art:sha256:{sha}");
        let package = |name: &str, exposure: serde_json::Value| {
            serde_json::json!({
                "package_key": format!("package:{name}"),
                "name": name,
                "aliases": [],
                "oldnames": [],
                "version": "1.0",
                "revision": 0,
                "keg_version": "1.0",
                "deps": [],
                "dependency_requirements": {},
                "exposure": exposure,
                "artifact": artifact_id,
                "install": {
                    "opt_names": [],
                    "link_overwrite": [],
                    "post_install_defined": false,
                    "post_install_steps": []
                }
            })
        };
        let body = serde_json::json!({
            "schema": "glu.resolve.v1",
            "request": {
                "name": ["global-root", "isolated-root"],
                "target": target
            },
            "roots": [
                {"requested_as":"global-root", "package_key":"package:global-root", "package":global_id},
                {"requested_as":"isolated-root", "package_key":"package:isolated-root", "package":isolated_id}
            ],
            "packages": {
                (global_id): package("global-root", serde_json::json!({"mode":"global"})),
                (isolated_id): package("isolated-root", serde_json::json!({"mode":"isolated", "reason":"provided by macOS"}))
            },
            "artifacts": {
                (artifact_id): {
                    "url":"https://example.invalid/bottle.tar.gz",
                    "sha256":sha,
                    "bytes":1,
                    "bottle_tag":target,
                    "cellar":":any"
                }
            }
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    (url, handle)
}

fn registry_for_one_package(
    name: &'static str,
    sha256: String,
    bytes: u64,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 8192];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|path| path.split("target=").nth(1))
            .and_then(|tail| tail.split('&').next())
            .unwrap_or("arm64_sequoia");
        let package_id = format!("pkg:test/{name}@1.0");
        let artifact_id = format!("art:sha256:{sha256}");
        let body = serde_json::json!({
            "schema":"glu.resolve.v1",
            "request":{"name":[name], "target":target},
            "roots":[{"requested_as":name, "package_key":format!("package:{name}"), "package":package_id}],
            "packages":{
                (package_id):{
                    "package_key":format!("package:{name}"),
                    "name":name,
                    "aliases":[],
                    "oldnames":[],
                    "version":"1.0",
                    "revision":0,
                    "keg_version":"1.0",
                    "deps":[],
                    "dependency_requirements":{},
                    "exposure":{"mode":"global"},
                    "artifact":artifact_id,
                    "install":{"opt_names":[], "link_overwrite":[], "post_install_defined":false, "post_install_steps":[]}
                }
            },
            "artifacts":{
                (artifact_id):{
                    "url":"https://ghcr.io/v2/homebrew/core/test/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
                    "sha256":sha256,
                    "bytes":bytes,
                    "bottle_tag":target,
                    "cellar":":any"
                }
            }
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    (url, handle)
}

fn cached_bottle(prefix: &Path, name: &str) -> (String, u64) {
    let mut encoded = Vec::new();
    {
        let encoder = GzEncoder::new(&mut encoded, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let data = b"#!/bin/sh\necho migrated\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("{name}/1.0/bin/{name}"), &data[..])
            .unwrap();
        archive.finish().unwrap();
    }
    let sha256 = digest::digest(&digest::SHA256, &encoded)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let cache = prefix.join("var/glu/cache/artifacts/sha256");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join(&sha256), &encoded).unwrap();
    (sha256, encoded.len() as u64)
}

fn run_with_registry(prefix: &Path, registry: &str, args: &[&str]) -> std::process::Output {
    glu()
        .args(args)
        .env("GLU_PREFIX", prefix)
        .env("GLU_REGISTRY", registry)
        .env("NO_COLOR", "1")
        .output()
        .expect("run glu")
}

#[test]
fn migrate_plan_preserves_only_clear_global_unlink_intent() {
    let prefix = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    write_requested_receipt(source.path(), "global-root");
    write_requested_receipt(source.path(), "isolated-root");
    let (registry, server) = registry_for_migration();

    let output = run_with_registry(
        prefix.path(),
        &registry,
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--plan",
            "--json",
        ],
    );
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["result"]["requires_confirmation"], true);
    assert_eq!(
        value["result"]["roots"],
        serde_json::json!(["global-root", "isolated-root"])
    );
    assert_eq!(
        value["result"]["inferred_deactivated"],
        serde_json::json!(["global-root"])
    );
    assert!(!prefix.path().join("glu.json").exists());
}

#[test]
fn migrate_execution_preflight_uses_will_wording() {
    let prefix = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    write_requested_receipt(source.path(), "global-root");
    write_requested_receipt(source.path(), "isolated-root");
    let (registry, server) = registry_for_migration();

    let output = run_with_registry(
        prefix.path(),
        &registry,
        &["migrate", "--from", source.path().to_str().unwrap()],
    );
    server.join().unwrap();

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Will migrate 2 requested packages:"),
        "{stdout}"
    );
    assert!(stdout.contains("Will install 2 packages:"), "{stdout}");
    assert!(stdout.contains("Will download:"), "{stdout}");
    assert!(
        stdout.contains("Will keep 1 package deactivated."),
        "{stdout}"
    );
    assert!(
        stdout.contains("Homebrew configuration and runtime data will be left untouched."),
        "{stdout}"
    );
    assert!(!stdout.contains("Would"), "{stdout}");
}

#[test]
fn migrate_executes_through_normal_install_and_preserves_source() {
    let prefix = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    write_requested_receipt(source.path(), "demo");
    let receipt = source.path().join("Cellar/demo/1.0/INSTALL_RECEIPT.json");
    let source_before = fs::read(&receipt).unwrap();
    fs::write(
        prefix.path().join("glu.json"),
        serde_json::json!({
            "schema":"glu.declaration.v1",
            "dependencies":{"keep":"9.0"},
            "deactivated":{"keep":true}
        })
        .to_string(),
    )
    .unwrap();
    let (sha256, bytes) = cached_bottle(prefix.path(), "demo");
    let (registry, server) = registry_for_one_package("demo", sha256, bytes);

    let output = run_with_registry(
        prefix.path(),
        &registry,
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--yes",
            "--json",
        ],
    );
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "migrate");
    assert_eq!(value["result"]["mode"], "executed");
    assert_eq!(value["result"]["roots"], serde_json::json!(["demo"]));
    assert_eq!(
        value["result"]["inferred_deactivated"],
        serde_json::json!(["demo"])
    );
    assert_eq!(value["result"]["configuration_migrated"], false);

    let declaration: serde_json::Value =
        serde_json::from_slice(&fs::read(prefix.path().join("glu.json")).unwrap()).unwrap();
    assert_eq!(declaration["dependencies"]["demo"], "1.0");
    assert_eq!(declaration["dependencies"]["keep"], "9.0");
    assert_eq!(declaration["deactivated"]["demo"], true);
    assert_eq!(declaration["deactivated"]["keep"], true);
    assert!(prefix
        .path()
        .join("Cellar/demo/1.0/.glu/receipt.json")
        .is_file());
    assert!(prefix.path().join("opt/demo").is_symlink());
    assert!(!prefix.path().join("bin/demo").exists());
    assert_eq!(fs::read(receipt).unwrap(), source_before);
}

#[test]
fn migrate_json_execution_requires_explicit_approval() {
    let prefix = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    write_requested_receipt(source.path(), "global-root");
    write_requested_receipt(source.path(), "isolated-root");
    let (registry, server) = registry_for_migration();

    let output = run_with_registry(
        prefix.path(),
        &registry,
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--json",
        ],
    );
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["code"], "confirmation_required");
    assert_eq!(
        value["error"]["details"]["command"],
        serde_json::json!("migrate")
    );
    assert!(!prefix.path().join("glu.json").exists());
}

#[test]
fn migrate_empty_requested_set_is_a_read_only_noop() {
    let prefix = tempfile::tempdir().unwrap();
    let source = source_with_receipt(false);
    let receipt = source.path().join("Cellar/demo/1.0/INSTALL_RECEIPT.json");
    let before = fs::read(&receipt).unwrap();

    let output = run(
        prefix.path(),
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--plan",
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "No requested Homebrew packages found.\n"
    );
    assert_eq!(fs::read(receipt).unwrap(), before);
    assert!(!prefix.path().join("glu.json").exists());
}

#[test]
fn migrate_empty_json_has_a_complete_result_without_approval() {
    let prefix = tempfile::tempdir().unwrap();
    let source = source_with_receipt(false);

    let output = run(
        prefix.path(),
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--json",
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "migrate");
    assert_eq!(value["result"]["mode"], "executed");
    assert_eq!(value["result"]["roots"], serde_json::json!([]));
    assert_eq!(value["result"]["configuration_migrated"], false);
    assert!(value["result"].get("install").is_none());
}

#[test]
fn migrate_reports_a_missing_homebrew_source() {
    let prefix = tempfile::tempdir().unwrap();
    let source = prefix.path().join("missing-homebrew");

    let output = run(
        prefix.path(),
        &["migrate", "--from", source.to_str().unwrap(), "--plan"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Homebrew installation not found"));
    assert!(!prefix.path().join("glu.json").exists());
}

#[test]
fn migrate_warns_about_malformed_receipts_without_declaring_them() {
    let prefix = tempfile::tempdir().unwrap();
    let source = source_with_receipt(false);
    fs::write(
        source.path().join("Cellar/demo/1.0/INSTALL_RECEIPT.json"),
        "{",
    )
    .unwrap();

    let output = run(
        prefix.path(),
        &[
            "migrate",
            "--from",
            source.path().to_str().unwrap(),
            "--plan",
        ],
    );

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipped malformed Homebrew receipt"));
    assert!(!prefix.path().join("glu.json").exists());
}
