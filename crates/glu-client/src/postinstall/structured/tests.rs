use super::*;
use flate2::{write::GzEncoder, Compression};
use glu_core::{
    ArtifactId, KegVersion, PackageDependency, PackageInstallMetadata, PackageName, ResolvedPackage,
};
use serde_json::json;
use tempfile::TempDir;

fn run_structured_postinstalls(
    prefix: &Prefix,
    package: &ResolvedPackage,
    keg: &Path,
    deferred: Option<&mut DeferredPostinstallQueue>,
    verbose: bool,
) -> Result<()> {
    let analysis = analyze_structured_postinstalls(prefix, package, keg)?;
    run_structured_postinstalls_with_plan(prefix, package, keg, &analysis.plan, deferred, verbose)
}

fn package_with_steps(steps: Vec<Value>) -> ResolvedPackage {
    ResolvedPackage {
        package_key: glu_core::PackageKey("package:fixture".to_string()),
        name: PackageName("fixture".to_string()),
        aliases: vec![],
        oldnames: vec![],
        version: "1.2.3".to_string(),
        revision: 0,
        keg_version: KegVersion("1.2.3".to_string()),
        deps: Vec::<PackageDependency>::new(),
        dependency_requirements: Default::default(),
        exposure: glu_core::Exposure::Global,
        artifact: ArtifactId("art:test:fixture".to_string()),
        install: PackageInstallMetadata {
            opt_names: Vec::new(),
            link_overwrite: vec![],
            post_install_defined: !steps.is_empty(),
            post_install_steps: steps,
            postinstall_network_access_allowed: true,
        },
    }
}

#[test]
fn gtk3_legacy_cache_runs_are_canonicalized_as_deferred_globals() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let mut package = package_with_steps(vec![
        json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        }),
        json!({
            "type": "run",
            "command": {"base": "bin", "path": "gtk3-update-icon-cache"},
            "args": ["-f", "-t", "{{HOMEBREW_PREFIX}}/share/icons/hicolor"]
        }),
        json!({
            "type": "run",
            "command": {"base": "bin", "path": "gtk-query-immodules-3.0"},
            "stdout_path": {"path": "{{HOMEBREW_PREFIX}}/lib/gtk-3.0/3.0.0/immodules.cache"}
        }),
    ]);
    package.name = PackageName("gtk+3".to_string());
    package.keg_version = KegVersion("3.24.52".to_string());
    let keg = prefix.0.join("Cellar/gtk+3/3.24.52");

    let analysis = analyze_structured_postinstalls(&prefix, &package, &keg).unwrap();

    assert_eq!(
        analysis.plan.per_step,
        vec![
            PostinstallStepPlan::DeferredGlobal {
                kind: "compile_gsettings_schemas".to_string(),
                key: vec![prefix
                    .0
                    .join("share/glib-2.0/schemas")
                    .to_string_lossy()
                    .to_string()],
            },
            PostinstallStepPlan::DeferredGlobal {
                kind: "gtk_update_icon_cache".to_string(),
                key: vec![prefix
                    .0
                    .join("share/icons/hicolor")
                    .to_string_lossy()
                    .to_string()],
            },
            PostinstallStepPlan::DeferredGlobal {
                kind: "gtk_query_immodules_3".to_string(),
                key: vec![prefix
                    .0
                    .join("lib/gtk-3.0/3.0.0/immodules.cache")
                    .to_string_lossy()
                    .to_string()],
            },
        ]
    );
}

#[test]
fn temp_based_global_keys_stay_inline() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let mut package = package_with_steps(vec![
        json!({
            "type": "update_mime_database",
            "path": {"base": "temp", "path": "share/mime"}
        }),
        json!({
            "type": "run",
            "command": {
                "base": "opt_prefix",
                "formula": "gtk+3",
                "path": "bin/gtk3-update-icon-cache"
            },
            "args": ["-f", "-t", "{{temp}}/icons/hicolor"]
        }),
    ]);
    package.name = PackageName("gtk+3".to_string());
    package.keg_version = KegVersion("3.24.52".to_string());
    let keg = prefix.0.join("Cellar/gtk+3/3.24.52");

    let analysis = analyze_structured_postinstalls(&prefix, &package, &keg).unwrap();

    assert_eq!(
        analysis.plan.per_step,
        vec![PostinstallStepPlan::Inline, PostinstallStepPlan::Inline]
    );
    assert!(analysis.deferred_contributions().is_empty());
}

#[test]
fn validation_rejects_missing_required_fields() {
    let steps = vec![json!({"type": "run"})];
    let err = validate_structured_postinstall_steps("fixture", &steps).unwrap_err();
    assert!(err.to_string().contains("missing command"));

    let steps = vec![json!({"type": "bootstrap_pypy"})];
    let err = validate_structured_postinstall_steps("fixture", &steps).unwrap_err();
    assert!(err.to_string().contains("abi_version"));
}

#[test]
fn validation_rejects_unsafe_bootstrap_pypy_abi_version() {
    let steps = vec![json!({"type": "bootstrap_pypy", "abi_version": "3.10/../../.."})];
    let err = validate_structured_postinstall_steps("fixture", &steps).unwrap_err();
    assert!(err.to_string().contains("unsafe abi_version"));
}

#[test]
fn run_step_supports_env_stdin_chdir_and_stdout_path() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(keg.join("work")).unwrap();
    fs::write(keg.join("input.txt"), "payload").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/sh"},
        "args": ["-c", "printf '%s:%s:' \"$FOO\" \"$PWD\"; cat"],
        "env": {"FOO": "{{name}}"},
        "stdin_path": {"base": "prefix", "path": "input.txt"},
        "stdout_path": {"base": "prefix", "path": "out.txt"},
        "chdir": {"base": "prefix", "path": "work"},
        "suppress_stderr": true
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    let output = fs::read_to_string(keg.join("out.txt")).unwrap();
    assert!(output.starts_with("fixture:"));
    assert!(output.ends_with(":payload"));
    assert!(output.contains("/prefix/Cellar/fixture/1.2.3/work"));
}

#[test]
fn run_step_expands_all_base_tokens_in_args() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/echo"},
        "args": ["{{pkgshare}}/cacert.pem"],
        "stdout_path": {"base": "prefix", "path": "out.txt"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    let output = fs::read_to_string(keg.join("out.txt")).unwrap();
    assert_eq!(
        output.trim(),
        keg.join("share/fixture/cacert.pem").to_string_lossy()
    );
}

#[test]
fn install_gzipped_executable_decompresses_removes_source_and_sets_executable_bits() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(keg.join("libexec")).unwrap();
    let source = keg.join("libexec/tool.gz");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"#!/bin/sh\necho ok\n").unwrap();
    fs::write(&source, encoder.finish().unwrap()).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "install_gzipped_executable",
        "source": {"base": "prefix", "path": "libexec/tool.gz"},
        "target": {"base": "bin", "path": "tool"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert!(!source.exists());
    assert_eq!(
        fs::read_to_string(keg.join("bin/tool")).unwrap(),
        "#!/bin/sh\necho ok\n"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(keg.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn absolute_paths_expand_tilde_against_postinstall_home() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &env.snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };

    assert_eq!(
        path(&ctx, &json!({"base": "absolute", "path": "~/cache"})).unwrap(),
        env.snapshot.home.join("cache")
    );
}

#[test]
fn run_step_reads_stdin_path_as_bytes() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    fs::write(keg.join("input.bin"), [0xff, b'o', b'k']).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/cat"},
        "stdin_path": {"base": "prefix", "path": "input.bin"},
        "stdout_path": {"base": "prefix", "path": "out.bin"},
        "suppress_stderr": true
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert_eq!(
        fs::read(keg.join("out.bin")).unwrap(),
        vec![0xff, b'o', b'k']
    );
}

#[test]
fn remove_content_contains_matches_binary_files() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    fs::write(
        keg.join("hit.bin"),
        [0xff, b'n', b'e', b'e', b'd', b'l', b'e'],
    )
    .unwrap();
    fs::write(keg.join("miss.bin"), [0xff, b'm', b'i', b's', b's']).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "remove",
        "paths": [
            {"base": "prefix", "path": "hit.bin"},
            {"base": "prefix", "path": "miss.bin"}
        ],
        "content_contains": "needle"
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert!(!keg.join("hit.bin").exists());
    assert!(keg.join("miss.bin").exists());
}

#[test]
fn search_path_no_glob_returns_candidates_without_requiring_existence() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let mut snapshot = env.snapshot.clone();
    let first = tmp.path().join("path-a");
    let second = tmp.path().join("path-b");
    snapshot.path = env::join_paths([&first, &second]).unwrap();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };

    assert_eq!(
        expand_glob(
            &ctx,
            &json!({"base": "search_path", "path": "missing-tool"})
        )
        .unwrap(),
        vec![first.join("missing-tool"), second.join("missing-tool")]
    );
}

#[test]
#[cfg(unix)]
fn permissions_and_ownership_skip_dangling_symlinks_like_existing_step_paths() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    std::os::unix::fs::symlink(keg.join("missing"), keg.join("dangling")).unwrap();
    let package = package_with_steps(vec![
        json!({
            "type": "set_permissions",
            "paths": [{"base": "prefix", "path": "dangling"}],
            "permissions": "0644"
        }),
        json!({
            "type": "set_ownership",
            "paths": [{"base": "prefix", "path": "dangling"}]
        }),
    ]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert!(keg.join("dangling").is_symlink());
}

#[test]
#[cfg(unix)]
fn recursive_copy_preserves_symlinks_and_unlinks_overwrite_destinations() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let src = keg.join("src");
    let dst = keg.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(dst.join("src")).unwrap();
    fs::write(src.join("file"), "new").unwrap();
    fs::write(keg.join("old-target"), "old").unwrap();
    std::os::unix::fs::symlink("file", src.join("link")).unwrap();
    std::os::unix::fs::symlink("../../old-target", dst.join("src/link")).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "copy",
        "source": {"base": "prefix", "path": "src"},
        "target": {"base": "prefix", "path": "dst"},
        "recursive": true,
        "overwrite": true
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    let copied = dst.join("src");
    assert_eq!(fs::read_to_string(copied.join("file")).unwrap(), "new");
    let link = copied.join("link");
    assert!(link.is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("file"));
    assert_eq!(fs::read_to_string(&link).unwrap(), "new");
}

#[test]
#[cfg(unix)]
fn atomic_write_uses_random_temp_not_predictable_symlink() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    let outside = tmp.path().join("outside");
    let predictable = tmp.path().join(".target.glu-tmp");
    fs::write(&outside, "sentinel").unwrap();
    std::os::unix::fs::symlink(&outside, &predictable).unwrap();

    atomic_write(&target, b"new").unwrap();

    assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");
    assert!(predictable.is_symlink());
}

#[test]
fn deferred_global_network_policy_is_restrictive_across_contributors() {
    let mut queue = DeferredPostinstallQueue::new();
    let key = vec!["cache".to_string()];
    queue.defer("kind".to_string(), key.clone(), "a".to_string(), true);
    queue.defer("kind".to_string(), key.clone(), "b".to_string(), false);

    let item = queue.take("kind", &key).unwrap();

    assert!(!item.network_access_allowed);
    assert_eq!(
        item.source_formulas,
        BTreeSet::from(["a".to_string(), "b".to_string()])
    );
}

#[test]
fn postinstall_env_var_present_uses_explicit_snapshot() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let env = PostinstallEnv::new(&prefix).unwrap();
    let mut snapshot = env.snapshot.clone();
    snapshot
        .vars
        .retain(|(key, _)| key != "HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT");
    assert!(!postinstall_env_var_present(
        Some(&snapshot),
        "HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT",
    ));
    snapshot.vars.push((
        OsString::from("HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT"),
        OsString::from("1"),
    ));
    assert!(postinstall_env_var_present(
        Some(&snapshot),
        "HOMEBREW_NO_APP_MANAGEMENT_PERMISSIONS_PROMPT",
    ));
}

#[test]
fn mysql_init_data_dir_requires_user_like_env_fetch() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let mut snapshot = env.snapshot.clone();
    snapshot.user.clear();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };

    let err = init_data_dir(
        &ctx,
        &json!({
            "type": "init_data_dir",
            "using": "mysql_initialize",
            "path": {"base": "prefix", "path": "var/mysql"}
        }),
    )
    .unwrap_err();

    assert!(err.to_string().contains("requires USER"));
}

#[test]
fn guarded_deferred_global_skips_runtime_queue_request() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "compile_gsettings_schemas",
        "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"},
        "guards": [{"condition": "if_exists", "base": "prefix", "path": "missing"}]
    })]);
    let analysis = analyze_structured_postinstalls(&prefix, &package, &keg).unwrap();
    assert_eq!(analysis.deferred_contributions().len(), 1);
    let mut queue = DeferredPostinstallQueue::new();

    run_structured_postinstalls(&prefix, &package, &keg, Some(&mut queue), false).unwrap();

    assert!(queue.items.is_empty());
}

#[test]
fn global_before_local_step_stays_inline() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![
        json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        }),
        json!({"type": "write", "path": {"base": "prefix", "path": "marker"}, "content": "x"}),
    ]);

    let analysis = analyze_structured_postinstalls(&prefix, &package, &keg).unwrap();

    assert!(analysis.deferred_contributions().is_empty());
}

#[test]
fn postinstall_env_is_sanitized_for_paths_and_children() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let out = keg.join("env.txt");
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/sh"},
        "args": ["-c", "printf '%s\\n%s\\n%s\\n' \"$HOME\" \"$TMPDIR\" \"{{temp}}\""],
        "stdout_path": {"base": "prefix", "path": "env.txt"},
        "suppress_stderr": true
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    let data = fs::read_to_string(out).unwrap();
    let lines = data.lines().collect::<Vec<_>>();
    assert!(lines[0].contains("glu-postinstall-home-"));
    assert!(lines[1].contains("glu-postinstall-temp-"));
    assert_eq!(lines[1], lines[2]);
}

#[test]
fn deferred_global_postinstalls_are_coalesced() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let steps = vec![
        json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        }),
        json!({
            "type": "compile_gsettings_schemas",
            "path": {"base": "homebrew_prefix", "path": "share/glib-2.0/schemas"}
        }),
    ];
    let package = package_with_steps(steps);
    let mut queue = DeferredPostinstallQueue::new();

    run_structured_postinstalls(&prefix, &package, &keg, Some(&mut queue), false).unwrap();

    assert_eq!(queue.items.len(), 1);
    let ((kind, key), sources) = queue.items.iter().next().unwrap();
    assert_eq!(kind, "compile_gsettings_schemas");
    assert_eq!(
        key,
        &vec![prefix
            .0
            .join("share/glib-2.0/schemas")
            .to_string_lossy()
            .to_string()]
    );
    assert_eq!(sources.get("fixture"), Some(&true));
}

#[test]
fn move_into_existing_dir_target_preserves_the_directory() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(keg.join("src")).unwrap();
    fs::create_dir_all(prefix.0.join("share/dst")).unwrap();
    fs::write(keg.join("src/tool"), "x").unwrap();
    fs::write(prefix.0.join("share/dst/keep"), "y").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "move",
        "source": {"base": "prefix", "path": "src/tool"},
        "target": {"base": "homebrew_prefix", "path": "share/dst"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // FileUtils.mv: destination is a dir → move INTO it, never delete it.
    assert!(prefix.0.join("share/dst").is_dir());
    assert_eq!(
        fs::read_to_string(prefix.0.join("share/dst/tool")).unwrap(),
        "x"
    );
    assert_eq!(
        fs::read_to_string(prefix.0.join("share/dst/keep")).unwrap(),
        "y"
    );
    assert!(!keg.join("src/tool").exists());
}

#[test]
fn move_children_excludes_target_child_inside_source() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let share = keg.join("share");
    fs::create_dir_all(share.join("icons")).unwrap();
    fs::write(share.join("a"), "a").unwrap();
    fs::write(share.join("b"), "b").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "move_children",
        "source": {"base": "prefix", "path": "share"},
        "target": {"base": "prefix", "path": "share/icons"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // The target child (inside the source) must not be moved into itself;
    // siblings land inside it (install_steps.rb:986).
    assert!(share.join("icons").is_dir());
    assert_eq!(fs::read_to_string(share.join("icons/a")).unwrap(), "a");
    assert_eq!(fs::read_to_string(share.join("icons/b")).unwrap(), "b");
    assert!(!share.join("a").exists());
    assert!(!share.join("b").exists());
}

#[test]
fn link_dir_skips_ds_store_preserves_real_dirs_and_links_relatively() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let src = keg.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("file"), "f").unwrap();
    fs::write(src.join(".DS_Store"), "x").unwrap();
    fs::write(src.join("sub/inner"), "i").unwrap();
    let tgt = prefix.0.join("share/linked");
    fs::create_dir_all(tgt.join("sub")).unwrap();
    fs::write(tgt.join("sub/keep"), "k").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "link_dir",
        "source": {"base": "prefix", "path": "src"},
        "target": {"base": "homebrew_prefix", "path": "share/linked"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // .DS_Store never linked (install_steps.rb:1042).
    assert!(!tgt.join(".DS_Store").exists());
    // Files become RELATIVE symlinks.
    let link = tgt.join("file");
    assert!(link.is_symlink());
    assert!(!fs::read_link(&link).unwrap().is_absolute());
    assert_eq!(fs::read_to_string(&link).unwrap(), "f");
    // An existing real dir at the link target is PRESERVED, not replaced
    // (install_steps.rb:1043), and its subtree still gets linked.
    assert!(tgt.join("sub").is_dir());
    assert!(!tgt.join("sub").is_symlink());
    assert_eq!(fs::read_to_string(tgt.join("sub/keep")).unwrap(), "k");
    let inner = tgt.join("sub/inner");
    assert!(inner.is_symlink());
    assert!(!fs::read_link(&inner).unwrap().is_absolute());
    assert_eq!(fs::read_to_string(&inner).unwrap(), "i");
}

#[test]
fn link_children_creates_relative_symlinks_with_prefix_suffix() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let src = keg.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a"), "a").unwrap();
    fs::write(src.join("b"), "b").unwrap();
    let tgt = prefix.0.join("share/children");
    let package = package_with_steps(vec![json!({
        "type": "link_children",
        "source": {"base": "prefix", "path": "src"},
        "target": {"base": "homebrew_prefix", "path": "share/children"},
        "prefix": "p-",
        "suffix": "-s"
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    for (name, content) in [("p-a-s", "a"), ("p-b-s", "b")] {
        let link = tgt.join(name);
        assert!(link.is_symlink(), "{name} missing");
        assert!(
            !fs::read_link(&link).unwrap().is_absolute(),
            "{name} must be a relative symlink"
        );
        assert_eq!(fs::read_to_string(&link).unwrap(), content);
    }
}

#[test]
fn run_step_allow_failure_does_not_fail_install_and_skips_stdout_path() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    // Homebrew 4dacfe77: install_steps.rb:1199-1220 — `allow_failure` is the
    // JSON inverse of the DSL's `must_succeed` (install_steps.rb:697); when
    // true a non-zero exit does not fail the install and stdout_path is NOT
    // written (only written on success, :1216-1219).
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/sh"},
        "args": ["-c", "echo out; echo err >&2; exit 3"],
        "allow_failure": true,
        "stdout_path": {"base": "prefix", "path": "out.txt"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert!(!keg.join("out.txt").exists());

    // Without allow_failure the same step must fail the install; the
    // stderr was surfaced as a labeled notice, so the propagated error is
    // the NoticedPostinstallFailure marker (the scheduler's headline must
    // not re-show the cause).
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/sh"},
        "args": ["-c", "echo boom >&2; exit 3"],
        "allow_failure": false,
    })]);
    let err = run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap_err();
    assert!(err.downcast_ref::<NoticedPostinstallFailure>().is_some());
}

#[test]
fn version_major_minor_matches_homebrew() {
    // Homebrew 4dacfe77: version.rb:686-692 — tokens[0..1], single-component
    // versions yield that component.
    assert_eq!(version_major_minor("3.11.9").as_deref(), Some("3.11"));
    assert_eq!(version_major_minor("8.4.3").as_deref(), Some("8.4"));
    assert_eq!(version_major_minor("3.9").as_deref(), Some("3.9"));
    assert_eq!(version_major_minor("8").as_deref(), Some("8"));
    assert_eq!(version_major_minor(""), None);
}

#[test]
fn brace_expand_matches_ruby_dir_glob_shape() {
    assert_eq!(brace_expand("{a,b}"), vec!["a", "b"]);
    assert_eq!(brace_expand("{a}"), vec!["a"]);
    assert_eq!(brace_expand("{{a,b},c}"), vec!["a", "b", "c"]);
    assert_eq!(brace_expand("{a,{b,c}}"), vec!["a", "b", "c"]);
    assert_eq!(brace_expand("{a,b}{c,d}"), vec!["ac", "ad", "bc", "bd"]);
    assert!(brace_expand("a{b").is_empty());
    assert_eq!(brace_expand("a}b"), vec!["a}b"]);
    assert_eq!(brace_expand(r"a\{b,c\}"), vec![r"a\{b,c\}"]);
}

#[test]
fn expand_glob_unmatched_open_brace_does_not_match_literal_file() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    fs::write(keg.join("a{b"), "literal").unwrap();
    fs::write(keg.join("a}b"), "literal").unwrap();
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &env.snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };

    assert!(expand_glob(&ctx, &json!({"base": "prefix", "path": "a{b"}))
        .unwrap()
        .is_empty());
    assert_eq!(
        expand_glob(&ctx, &json!({"base": "prefix", "path": "a}b"})).unwrap(),
        vec![keg.join("a}b")]
    );
}

#[test]
fn inreplace_regexp_extended_handles_classes_and_comments() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    fs::write(keg.join("f.txt"), "a bc\n]bc\n").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "inreplace",
        "path": {"base": "prefix", "path": "f.txt"},
        "before": "a [ ] b # comment\n c",
        "after": "hit",
        "regexp": true,
        "regexp_options": 2
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert_eq!(fs::read_to_string(keg.join("f.txt")).unwrap(), "hit\n]bc\n");
}

#[test]
fn inreplace_regexp_file_replaces_with_audit() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.ini");
    fs::write(&path, "[opcache]\nzend_extension=\"/old\"\n").unwrap();

    inreplace_regexp_file(
        &path,
        r"^\s*zend_extension\s*=.*$",
        "zend_extension=\"/new\"",
    )
    .unwrap();

    assert!(fs::read_to_string(&path)
        .unwrap()
        .contains("zend_extension=\"/new\""));

    let backref = tmp.path().join("framework.py");
    fs::write(&backref, "  homebrew_prefix = None\n").unwrap();
    inreplace_regexp_file(
        &backref,
        r"^(\s+homebrew_prefix\s+=\s+).*",
        r"\1'/opt/glustore'",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&backref).unwrap(),
        "  homebrew_prefix = '/opt/glustore'\n"
    );

    // audit: no match must fail (Utils::Inreplace.inreplace audit_result: true)
    let other = tmp.path().join("other.ini");
    fs::write(&other, "nothing here\n").unwrap();
    assert!(inreplace_regexp_file(&other, r"^\s*zend_extension\s*=.*$", "x").is_err());
}

#[test]
fn inreplace_translates_ruby_backrefs() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    fs::write(keg.join("f.txt"), "  foo = old\n  bar = x\n").unwrap();
    // Ruby `\1` in the replacement → group 1 (verified: gsub(/(b)/, "\\1!"))
    let package = package_with_steps(vec![json!({
        "type": "inreplace",
        "path": {"base": "prefix", "path": "f.txt"},
        "before": r"^(\s+foo\s*=\s*).*$",
        "after": r"\1new",
        "regexp": true
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert_eq!(
        fs::read_to_string(keg.join("f.txt")).unwrap(),
        "  foo = new\n  bar = x\n"
    );
}

#[test]
fn inreplace_leaves_literal_dollar_and_empty_groups() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    // regexp mode: literal `$1` stays literal (Ruby never expands `$`)
    fs::write(keg.join("a.txt"), "a b c\n").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "inreplace",
        "path": {"base": "prefix", "path": "a.txt"},
        "before": r" b ",
        "after": "$1",
        "regexp": true
    })]);
    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();
    assert_eq!(fs::read_to_string(keg.join("a.txt")).unwrap(), "a$1c\n");

    // string pattern: `\1` expands to EMPTY (no groups) — Ruby-verified
    // "abc".gsub("b", "\\1") → "ac".
    fs::write(keg.join("b.txt"), "abc\n").unwrap();
    let package = package_with_steps(vec![json!({
        "type": "inreplace",
        "path": {"base": "prefix", "path": "b.txt"},
        "before": "b",
        "after": r"\1"
    })]);
    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();
    assert_eq!(fs::read_to_string(keg.join("b.txt")).unwrap(), "ac\n");
}

#[test]
fn inreplace_handles_binary_files_and_preserves_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    // binary content: 0xFF + "ab" — Ruby gsub works byte-wise (verified)
    fs::write(keg.join("bin.dat"), [0xffu8, b'a', b'b']).unwrap();
    fs::set_permissions(keg.join("bin.dat"), fs::Permissions::from_mode(0o600)).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "inreplace",
        "path": {"base": "prefix", "path": "bin.dat"},
        "before": "a",
        "after": "X"
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    assert_eq!(
        fs::read(keg.join("bin.dat")).unwrap(),
        vec![0xffu8, b'X', b'b']
    );
    assert_eq!(
        fs::metadata(keg.join("bin.dat"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn expand_substitutes_user_and_version_tokens() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "run",
        "command": {"path": "/bin/echo"},
        "args": [
            "{{user}}",
            "{{version.major}}",
            "{{version.major_minor}}",
            "{{HOMEBREW_BREW_FILE}}",
            "{{homebrew_prefix}}"
        ],
        "stdout_path": {"base": "prefix", "path": "out.txt"}
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // Homebrew 4dacfe77: install_steps.rb:1366-1402 — user / version.major /
    // version.major_minor / HOMEBREW_BREW_FILE substitute; homebrew_prefix is
    // NOT a content token upstream and passes through literally.
    let out = fs::read_to_string(keg.join("out.txt")).unwrap();
    let parts = out.split_whitespace().collect::<Vec<_>>();
    assert_eq!(parts[0], env::var("USER").unwrap());
    assert_eq!(parts[1], "1");
    assert_eq!(parts[2], "1.2");
    assert!(!parts[3].is_empty());
    assert_eq!(parts[4], "{{homebrew_prefix}}");
}

#[test]
fn completion_bases_are_keg_relative() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &env.snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };
    // Homebrew 4dacfe77: formula.rb:1401-1428 — keg-relative.
    assert_eq!(
        base_path(&ctx, "bash_completion", None),
        keg.join("etc/bash_completion.d")
    );
    assert_eq!(
        base_path(&ctx, "zsh_completion", None),
        keg.join("share/zsh/site-functions")
    );
    assert_eq!(
        base_path(&ctx, "fish_completion", None),
        keg.join("share/fish/vendor_completions.d")
    );
    assert_eq!(
        base_path(&ctx, "pwsh_completion", None),
        keg.join("share/pwsh/completions")
    );
}

#[test]
fn fc_cache_interception_requires_the_opt_fontconfig_binary() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    let package = package_with_steps(vec![]);
    let env = PostinstallEnv::new(&prefix).unwrap();
    let ctx = PostinstallContext {
        prefix: &prefix,
        package: &package,
        keg: &keg,
        env: &env.snapshot,
        deferred: None,
        verbose: false,
        guards: RefCell::new(BTreeMap::new()),
    };
    // The exact opt/fontconfig command + flags → deferrable global step.
    let opt = json!({
        "type": "run",
        "command": {"base": "homebrew_prefix", "path": "opt/fontconfig/bin/fc-cache"},
        "args": ["--force", "--really-force", "--verbose"]
    });
    let (kind, _) = global_kind_key(&ctx, &opt).unwrap().unwrap();
    assert_eq!(kind, "fontconfig_fc_cache");

    // fontconfig's OWN keg binary (the common case: fontconfig's postinstall
    // `run "bin/fc-cache", ...`) is the same installed binary via the opt
    // link, so it IS deferrable — regression fix for the vips closure.
    let keg_fc = json!({
        "type": "run",
        "command": {"base": "homebrew_prefix", "path": "Cellar/fontconfig/2.16.0/bin/fc-cache"},
        "args": ["--force", "--really-force", "--verbose"]
    });
    assert!(global_kind_key(&ctx, &keg_fc).unwrap().is_some());

    // Any OTHER fc-cache (another formula's keg, PATH name, different path)
    // must NOT be intercepted — it runs inline unmodified, like Homebrew.
    for command in [
        json!({ "base": "prefix", "path": "bin/fc-cache" }),
        json!({ "path": "fc-cache" }),
    ] {
        let step = json!({
            "type": "run",
            "command": command,
            "args": ["--force", "--really-force", "--verbose"]
        });
        assert!(global_kind_key(&ctx, &step).unwrap().is_none());
    }
}

#[test]
fn if_exists_guard_ignores_dangling_symlinks() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    // Homebrew 4dacfe77: path_spec_exists? = .any?(&:exist?) — a DANGLING
    // symlink does NOT satisfy if_exists (Pathname#exist? follows the link).
    std::os::unix::fs::symlink(keg.join("nonexistent"), keg.join("dangling")).unwrap();
    let package = package_with_steps(vec![json!({
        "type": "write",
        "path": {"base": "prefix", "path": "ran.txt"},
        "content": "x",
        "guards": [{"condition": "if_exists", "base": "prefix", "path": "dangling"}]
    })]);

    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // Step must be SKIPPED: the dangling symlink does not exist.
    assert!(!keg.join("ran.txt").exists());
}

#[test]
fn terminate_process_swallows_no_match_failure_and_bails_on_must_succeed() {
    let tmp = TempDir::new().unwrap();
    let prefix = Prefix(tmp.path().join("prefix"));
    let keg = prefix.0.join("Cellar/fixture/1.2.3");
    fs::create_dir_all(&keg).unwrap();
    // A name that cannot possibly be running: killall exits 1 with "No
    // matching processes belonging to you were found" on stderr.
    let name = format!("glu-no-such-process-{}", std::process::id());

    // Homebrew 4dacfe77: install_steps.rb:1239-1251 — a nonzero exit is a
    // non-fatal failure when `must_succeed` is false: retry, continue
    // silently (only a step `failure_message` would show, opoo-style). The
    // raw killall stderr is routine — the agent usually isn't running —
    // and is never surfaced.
    let package = package_with_steps(vec![json!({
        "type": "terminate_process",
        "name": name,
    })]);
    run_structured_postinstalls(&prefix, &package, &keg, None, false).unwrap();

    // `must_succeed: true` (the step DSL flag, install_steps.rb:1250)
    // re-raises instead of swallowing — the install must fail.
    let package = package_with_steps(vec![json!({
        "type": "terminate_process",
        "name": name,
        "must_succeed": true,
    })]);
    assert!(run_structured_postinstalls(&prefix, &package, &keg, None, false).is_err());
}

#[test]
fn run_command_failure_carries_stderr_for_labeled_notice() {
    let err = run_command(RunCommand::new(
        None,
        Path::new("/bin/sh"),
        &["-c".into(), "echo boom >&2; exit 3".into()],
    ))
    .unwrap_err();
    let failed = err.downcast_ref::<PostinstallCommandFailed>().unwrap();
    assert_eq!(failed.command, PathBuf::from("/bin/sh"));
    assert_eq!(String::from_utf8_lossy(&failed.stderr).trim(), "boom");
    // Display names only the command, so the raw stderr isn't duplicated
    // in the error chain (it appears once, in the notice).
    assert_eq!(failed.to_string(), "postinstall command failed: /bin/sh");
}
