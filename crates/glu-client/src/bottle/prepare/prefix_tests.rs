//! Execution tests for relocation, independent of the production prefix gate.
use super::*;
use flate2::{write::GzEncoder, Compression};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn checked(command: &mut Command) -> std::process::Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn archive(path: &Path, files: &[(&str, Vec<u8>, u32)]) {
    let mut tar = tar::Builder::new(GzEncoder::new(
        File::create(path).unwrap(),
        Compression::fast(),
    ));
    for (name, bytes, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
#[ignore = "requires Xcode command-line tools; runs compiled arm64 executables"]
fn compiled_program_runs_at_short_equal_and_long_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let build = temp.path().join("buildroot");
    fs::create_dir(&build).unwrap();
    let library_source = build.join("library.c");
    fs::write(&library_source, "int answer(void) { return 42; }").unwrap();
    let source = build.join("main.c");
    // The snapshot marker is deliberately not a C string. Its field offsets
    // must survive shortening/growth, but equal-length rewriting may update
    // the stored path without shifting the following bytecode.
    fs::write(
        &source,
        format!(
            r#"
#include <stdio.h>
extern int answer(void);
volatile const unsigned char snapshot[] = "\001{}\002\003";
int main(void) {{
    if (snapshot[0] != 1 || snapshot[{}] != 2 || snapshot[{}] != 3) return 2;
    printf("%d\n", answer());
    return 0;
}}
"#,
            build.display(),
            build.as_os_str().len() + 1,
            build.as_os_str().len() + 2
        ),
    )
    .unwrap();
    let library = build.join("libfixture.dylib");
    checked(
        Command::new("/usr/bin/xcrun")
            .args(["clang", "-dynamiclib", "-O2"])
            .arg(&library_source)
            .arg("-o")
            .arg(&library)
            .args([
                "-Wl,-install_name,@@HOMEBREW_PREFIX@@/Cellar/fixture/1.0/lib/libfixture.dylib",
                "-Wl,-headerpad,0x4000",
            ]),
    );
    let executable = build.join("fixture");
    checked(
        Command::new("/usr/bin/xcrun")
            .args(["clang", "-O2"])
            .arg(&source)
            .arg(&library)
            .arg("-o")
            .arg(&executable)
            .arg("-Wl,-headerpad,0x4000"),
    );
    let bottle = temp.path().join("fixture.tar.gz");
    archive(
        &bottle,
        &[
            (
                "fixture/1.0/bin/fixture",
                fs::read(executable).unwrap(),
                0o755,
            ),
            (
                "fixture/1.0/lib/libfixture.dylib",
                fs::read(library).unwrap(),
                0o755,
            ),
            (
                "fixture/1.0/bin/run",
                format!(
                    "#!/bin/sh\nexec '{}/Cellar/fixture/1.0/bin/fixture'\n",
                    build.display()
                )
                .into_bytes(),
                0o755,
            ),
        ],
    );
    let sha = crate::hash::sha256_hex(&fs::read(&bottle).unwrap());
    // Prove the binaries use relocated dependencies, not anything left in the
    // build prefix. Every run goes through the relocated shell script too.
    fs::remove_dir_all(&build).unwrap();
    for name in ["s", "equalroot", "much-longer-install-prefix"] {
        let prefix = Prefix(temp.path().join(name));
        let profile = prefix_profile(&prefix, build.to_str().unwrap(), None, "/usr/bin/perl");
        let prepared = extract_patch_tar_gz_parallel(
            &bottle,
            &prefix.0.join("Cellar"),
            profile,
            &WriterPool::new(),
            false,
            &sha,
        )
        .unwrap();
        assert!(prepared.warnings.is_empty(), "{:?}", prepared.warnings);
        assert_eq!(prepared.to_sign.len(), 2);
        for path in prepared.to_sign {
            crate::bottle::codesign::sign_path_adhoc(&path).unwrap();
        }
        let output = checked(
            Command::new(prefix.0.join("Cellar/fixture/1.0/bin/run"))
                .env_remove("DYLD_LIBRARY_PATH")
                .env_remove("DYLD_FALLBACK_LIBRARY_PATH"),
        );
        assert_eq!(output.stdout, b"42\n");
    }
}

// This exercises a real Node executable's embedded build paths and its V8
// snapshot. Supply an executable and a build prefix actually present in it.
// GLU_TEST_NODE_BINARY=... GLU_TEST_NODE_BUILD_PREFIX=... cargo test
// -p glu-client real_node_executes_after_prefix_relocation -- --ignored --nocapture
#[test]
#[ignore = "requires a local Node executable containing GLU_TEST_NODE_BUILD_PREFIX"]
fn real_node_executes_after_prefix_relocation() {
    let binary = std::env::var_os("GLU_TEST_NODE_BINARY").expect("GLU_TEST_NODE_BINARY");
    let old = std::env::var("GLU_TEST_NODE_BUILD_PREFIX").expect("GLU_TEST_NODE_BUILD_PREFIX");
    let original = fs::read(binary).unwrap();
    assert!(
        memmem::find(&original, old.as_bytes()).is_some(),
        "no relocation exercised"
    );
    let temp = tempfile::tempdir().unwrap();
    let equal = format!("/{}", "x".repeat(old.len() - 1));
    for (index, new) in ["/x", equal.as_str()].iter().enumerate() {
        let mut data = original.clone();
        let profile = prefix_profile(&Prefix(PathBuf::from(new)), &old, None, "/usr/bin/perl");
        let mut warnings = vec![];
        assert!(patch_fixed_prefix_bytes(
            "node",
            &mut data,
            &profile,
            &mut warnings
        ));
        assert!(warnings.is_empty());
        assert!(find_required_build_prefix(&data, &profile).is_none());
        let path = temp.path().join(format!("node-{index}"));
        fs::write(&path, data).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        crate::bottle::codesign::sign_path_adhoc(&path).unwrap();
        for flags in [vec![], vec!["--no-node-snapshot"]] {
            let output = checked(Command::new(&path).args(flags).args([
                "-e",
                r#"
                const assert = require('node:assert/strict');
                const zlib = require('node:zlib');
                const fs = require('node:fs');
                const vm = require('node:vm');
                assert.equal(vm.runInNewContext('6 * 7'), 42);
                assert.equal(zlib.gunzipSync(zlib.gzipSync('relocation')).toString(), 'relocation');
                assert.ok(fs.statSync(process.execPath).isFile());
                console.log('ok');
            "#,
            ]));
            assert_eq!(output.stdout, b"ok\n");
        }
    }
}
