use super::*;
use glu_core::{
    ArtifactId, Exposure, InstallManifest, KegVersion, PackageInstallMetadata, PackageKey,
    PackageSelection, ResolveRequestEcho, ResolvedPackage, Target,
};

fn plan(exposure: Exposure, declared_before: &[&str]) -> InstallPlan {
    let id = PackageId("pkg:test/current@1.0".to_string());
    let name = PackageName("current".to_string());
    let package = ResolvedPackage {
        package_key: PackageKey("package:current".to_string()),
        name: name.clone(),
        aliases: Vec::new(),
        oldnames: vec![PackageSelector("old".to_string())],
        version: "1.0".to_string(),
        revision: 0,
        keg_version: KegVersion("1.0".to_string()),
        deps: Vec::new(),
        dependency_requirements: BTreeMap::new(),
        exposure,
        artifact: ArtifactId("art:test".to_string()),
        install: PackageInstallMetadata {
            opt_names: Vec::new(),
            link_overwrite: Vec::new(),
            post_install_defined: false,
            post_install_steps: Vec::new(),
            postinstall_network_access_allowed: true,
        },
    };
    let mut declaration_after = Declaration::default();
    declaration_after
        .dependencies
        .insert(name.clone(), "1.0".to_string());
    InstallPlan {
        requested: vec![PackageSelector("old".to_string())],
        would_install: Vec::new(),
        satisfied: Vec::new(),
        promoted: Vec::new(),
        renamed: Vec::new(),
        would_remove: Vec::new(),
        requires_confirmation: false,
        would_download_bytes: None,
        manifest: InstallManifest {
            schema: "glu.resolve.v1".to_string(),
            request: ResolveRequestEcho {
                name: vec![PackageSelector("old".to_string())],
                target: Target("arm64_test".to_string()),
                slim: false,
            },
            roots: vec![PackageSelection {
                requested_as: PackageSelector("old".to_string()),
                package_key: package.package_key.clone(),
                package: id.clone(),
            }],
            packages: BTreeMap::from([(id, package)]),
            artifacts: BTreeMap::new(),
        },
        workset: planner::InstallWorkSet {
            satisfied: Vec::new(),
            rename: Vec::new(),
            install: Vec::new(),
        },
        declaration_after,
        deactivated_after: BTreeSet::new(),
        declared_before: declared_before
            .iter()
            .map(|name| PackageName((*name).to_string()))
            .collect(),
        command_start: std::time::Instant::now(),
        startup_diagnostics: InstallStartupDiagnostics::default(),
    }
}

#[test]
fn migration_deactivates_only_new_global_roots() {
    let candidates = BTreeSet::from([PackageSelector("old".to_string())]);
    let mut global = plan(Exposure::Global, &[]);
    assert_eq!(
        global.preserve_deactivation_for_new_global_roots(&candidates),
        vec![PackageName("current".to_string())]
    );
    assert!(global
        .deactivated_after
        .contains(&PackageName("current".to_string())));

    let mut isolated = plan(
        Exposure::Isolated {
            reason: Some("provided by macOS".to_string()),
        },
        &[],
    );
    assert!(isolated
        .preserve_deactivation_for_new_global_roots(&candidates)
        .is_empty());

    let mut already_declared = plan(Exposure::Global, &["current"]);
    assert!(already_declared
        .preserve_deactivation_for_new_global_roots(&candidates)
        .is_empty());
}
