use super::*;
use crate::postinstall::structured::PostinstallStepPlan;
use std::os::unix::fs::symlink;

// GLU_TEST_BINARY=<built glu> GLU_TEST_GLIB_PREFIX=<installed glib> cargo test
// -p glu-client real_worker_rebuilds_cache -- --ignored --nocapture
#[test]
#[ignore = "requires a built glu CLI and an installed glib (paths supplied via GLU_TEST_*)"]
fn real_worker_rebuilds_cache_and_denies_native_staging_write() {
    let binary = PathBuf::from(env::var_os("GLU_TEST_BINARY").expect("GLU_TEST_BINARY"));
    let glib = PathBuf::from(env::var_os("GLU_TEST_GLIB_PREFIX").expect("GLU_TEST_GLIB_PREFIX"));
    let temp = tempfile::tempdir().unwrap();
    let prefix = Prefix(temp.path().join("prefix"));
    let schemas = prefix
        .0
        .join("Cellar/schema-owner/1.0/share/glib-2.0/schemas");
    fs::create_dir_all(&schemas).unwrap();
    fs::write(
        schemas.join("org.glu.test.gschema.xml"),
        r#"
<schemalist><schema id="org.glu.test" path="/org/glu/test/">
<key name="enabled" type="b"><default>true</default></key>
</schema></schemalist>"#,
    )
    .unwrap();
    fs::create_dir_all(prefix.0.join("opt")).unwrap();
    symlink(glib, prefix.0.join("opt/glib")).unwrap();
    let public_schemas = prefix.0.join("share/glib-2.0/schemas");
    fs::create_dir_all(public_schemas.parent().unwrap()).unwrap();
    symlink(&schemas, &public_schemas).unwrap();

    let run = |job: PostinstallWorkerJob| {
        let job_path = temp.path().join("job.json");
        let result_path = temp.path().join("result.json");
        let profile_path = temp.path().join("worker.sb");
        fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        fs::write(
            &profile_path,
            SandboxProfile::for_job(&job, temp.path()).unwrap().render(),
        )
        .unwrap();
        let mut command = sandbox_command(&profile_path, &binary, &job_path, &result_path).unwrap();
        apply_worker_env(&mut command);
        command.output().unwrap()
    };
    let output = run(PostinstallWorkerJob::DeferredGlobal {
        prefix: prefix.clone(),
        kind: "compile_gsettings_schemas".into(),
        key: vec![public_schemas.to_string_lossy().into_owned()],
        network_access_allowed: false,
        verbose: true,
    });
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::metadata(schemas.join("gschemas.compiled"))
            .unwrap()
            .len()
            > 0
    );

    let staged = prefix.0.join("var/glu/staging/next/1.0/payload");
    fs::create_dir_all(staged.parent().unwrap()).unwrap();
    fs::write(&staged, b"trusted").unwrap();
    let keg = prefix.0.join("Cellar/fixture/1.0");
    fs::create_dir_all(&keg).unwrap();
    let mut package = tests::package();
    package.install.post_install_steps = vec![serde_json::json!({
        "type": "write",
        "path": {"base": "homebrew_prefix", "path": "var/glu/staging/next/1.0/payload"},
        "content": "tampered",
        "overwrite": true
    })];
    let output = run(PostinstallWorkerJob::Formula {
        prefix,
        package: Box::new(package),
        keg,
        plan: PostinstallPlan {
            per_step: vec![PostinstallStepPlan::Inline],
        },
        verbose: true,
    });
    assert!(!output.status.success());
    assert_eq!(fs::read(staged).unwrap(), b"trusted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "{stderr}"
    );
}
