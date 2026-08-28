//! `glu outdated` support: compare installed kegs against the registry's
//! staleness envelope (`update` = newest installable on this target, what
//! `glu up` installs; `latest` = newest visible overall).

use crate::{homebrew_version::PackageVersion, state::installed::InstalledState, style};
use glu_core::{OutdatedPackage as OutdatedEnvelope, PackageName, VersionRevision};
use semver::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedPackage {
    pub package_key: glu_core::PackageKey,
    pub name: PackageName,
    pub installed: String,
    /// Newest version installable on this target — what `glu up` installs.
    pub update: Option<String>,
    /// Newest visible version overall.
    pub latest: String,
}

/// The full result of an outdated check: the packages with newer versions
/// plus the registry's optional latest-version header (for the "a newer glu
/// is available" notice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedResult {
    pub packages: Vec<OutdatedPackage>,
    pub latest_glu_version: Option<String>,
}

/// Semver-compare the registry's latest glu version against the running
/// client and return a notice only when the client is lower. Absent or
/// unparseable versions mean "unknown" — never nag (the key is optional
/// per the outdated-endpoint spec). The message is yellow with the command
/// bold; styling falls back to plain text when stdout is not a terminal.
pub fn glu_update_hint(latest: Option<&str>, own: &str) -> Option<String> {
    let (Ok(latest), Ok(own)) = (Version::parse(latest?), Version::parse(own)) else {
        return None;
    };
    (own < latest).then(|| {
        format!(
            "{} {}",
            style::yellow(&format!("glu {latest} is available (you have {own}); run")),
            style::bold_yellow("glu upgrade"),
        )
    })
}

/// Compares the newest installed keg of each name against the registry's
/// staleness envelope and returns the packages with a newer installable
/// version available, sorted by name. The newest-keg-per-name reduction is
/// `InstalledState`'s job (kegs are indexed newest-first), so this is a
/// pure per-name compare; names missing from the registry response are
/// skipped.
///
/// A package counts as outdated when the envelope's `update` version (the
/// newest version installable on this target — what `glu up` would
/// install) is newer than the installed keg. When `update` is `null` (the
/// newest visible version has no compatible bottle for this target) the
/// package is *not* listed: there is nothing to install, so it is not
/// outdated in a way the user can act on.
pub fn outdated_entries(
    state: &InstalledState,
    latest: &[OutdatedEnvelope],
) -> Vec<OutdatedPackage> {
    let mut out = Vec::new();
    for name in state.names() {
        let Some(installed) = state.find(&name) else {
            continue;
        };
        let Some(envelope) = latest
            .iter()
            .find(|candidate| candidate.package_key == installed.package_key)
        else {
            continue;
        };
        let Some(update) = &envelope.update else {
            continue;
        };
        let ordering = PackageVersion::new(&update.version, update.revision)
            .compare(PackageVersion::new(&installed.version, installed.revision));
        if ordering.is_gt() {
            out.push(OutdatedPackage {
                package_key: installed.package_key.clone(),
                name,
                installed: installed.keg_version.0.clone(),
                update: Some(format_version(update)),
                latest: format_version(&envelope.latest),
            });
        }
    }
    out
}

/// `8.18.5` or `8.18.5_1` when the version has a nonzero revision.
fn format_version(version: &VersionRevision) -> String {
    if version.revision > 0 {
        format!("{}_{}", version.version, version.revision)
    } else {
        version.version.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{
        InstalledPackage, KegVersion, OutdatedPackage as Envelope, OutdatedResponse, PackageId,
    };

    fn installed(name: &str, version: &str, revision: u32) -> InstalledPackage {
        InstalledPackage {
            id: PackageId(format!("pkg:{name}@{version}_{revision}")),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            aliases: Vec::new(),
            oldnames: Vec::new(),
            version: version.to_string(),
            revision,
            keg_version: KegVersion(format!("{version}_{revision}")),
            keg_path: Default::default(),
            opt_path: Default::default(),
            keg_only: false,
            linked: true,
            deps: vec![],
            dependency_requirements: Default::default(),
            download_bytes: None,
            installed_bytes: None,
        }
    }

    fn vr(name: &str, version: &str, revision: u32) -> VersionRevision {
        VersionRevision {
            package: PackageId(format!("pkg:{name}@{version}_{revision}")),
            version: version.to_string(),
            revision,
        }
    }

    fn envelope(name: &str, update: Option<(&str, u32)>, latest: (&str, u32)) -> Envelope {
        Envelope {
            requested_as: glu_core::PackageSelector(name.to_string()),
            package_key: glu_core::PackageKey(format!("package:{name}")),
            name: PackageName(name.to_string()),
            update: update.map(|(v, r)| vr(name, v, r)),
            latest: vr(name, latest.0, latest.1),
        }
    }

    type EnvelopeFixture<'a> = (&'a str, Option<(&'a str, u32)>, (&'a str, u32));

    fn envelope_map(entries: Vec<EnvelopeFixture<'_>>) -> Vec<Envelope> {
        entries
            .into_iter()
            .map(|(name, update, latest)| envelope(name, update, latest))
            .collect()
    }

    #[test]
    fn decodes_sample_payload() {
        // New shape: each package carries an installable `update` version
        // and the newest visible `latest`.
        let json = r#"{"schema":"glu.outdated.v1","packages":[{"requested_as":"node","package_key":"package:node","name":"node","update":{"package":"pkg:node@26.7.0","version":"26.7.0","revision":0},"latest":{"package":"pkg:node@26.7.0","version":"26.7.0","revision":0}},{"requested_as":"vips","package_key":"package:vips","name":"vips","update":null,"latest":{"package":"pkg:vips@8.19.0_1","version":"8.19.0","revision":1}}]}"#;
        let response: OutdatedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.packages.len(), 2);
        let vips = response
            .packages
            .iter()
            .find(|package| package.name.0 == "vips")
            .unwrap();
        assert_eq!(vips.update, None);
        assert_eq!(vips.latest.revision, 1);
        let node = response
            .packages
            .iter()
            .find(|package| package.name.0 == "node")
            .unwrap();
        assert_eq!(node.update.as_ref().unwrap().version, "26.7.0");
    }

    #[test]
    fn glu_update_hint_only_when_own_is_lower() {
        assert_eq!(
            glu_update_hint(Some("0.2.0"), "0.1.0"),
            Some("glu 0.2.0 is available (you have 0.1.0); run glu upgrade".to_string())
        );
        // Equal and higher never notify.
        assert_eq!(glu_update_hint(Some("0.1.0"), "0.1.0"), None);
        assert_eq!(glu_update_hint(Some("0.1.0"), "0.2.0"), None);
        // Prereleases compare as semver: 0.2.0-rc.1 > 0.1.0, and
        // 0.1.0 < 0.1.1-rc.1.
        assert_eq!(
            glu_update_hint(Some("0.2.0-rc.1"), "0.1.0"),
            Some("glu 0.2.0-rc.1 is available (you have 0.1.0); run glu upgrade".to_string())
        );
        assert_eq!(
            glu_update_hint(Some("0.1.1-rc.1"), "0.1.0"),
            Some("glu 0.1.1-rc.1 is available (you have 0.1.0); run glu upgrade".to_string())
        );
        assert_eq!(glu_update_hint(Some("0.1.1-rc.1"), "0.1.1"), None);
    }

    #[test]
    fn glu_update_hint_never_nags_on_unknown() {
        // Missing key, unparseable, or unparseable own version -> no notice.
        assert_eq!(glu_update_hint(None, "0.1.0"), None);
        assert_eq!(glu_update_hint(Some("not-a-version"), "0.1.0"), None);
        assert_eq!(glu_update_hint(Some("0.2.0"), "not-a-version"), None);
    }

    #[test]
    fn lists_only_packages_with_newer_versions() {
        let installed = vec![
            installed("node", "26.0.0", 0),
            installed("vips", "8.18.5", 0),
            installed("git", "2.55.0", 0),
        ];
        let latest = envelope_map(vec![
            ("node", Some(("26.7.0", 0)), ("26.7.0", 0)),
            ("vips", Some(("8.18.5", 0)), ("8.18.5", 0)),
            ("git", Some(("2.55.0", 0)), ("2.55.0", 0)),
        ]);

        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].name.0, "node");
        assert_eq!(outdated[0].installed, "26.0.0_0");
        assert_eq!(outdated[0].update.as_deref(), Some("26.7.0"));
        assert_eq!(outdated[0].latest, "26.7.0");
    }

    #[test]
    fn revision_bump_is_outdated() {
        let installed = vec![installed("vips", "8.18.5", 0)];
        let latest = envelope_map(vec![("vips", Some(("8.18.5", 1)), ("8.18.5", 1))]);
        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].update.as_deref(), Some("8.18.5_1"));
        assert_eq!(outdated[0].latest, "8.18.5_1");
    }

    #[test]
    fn mixed_alphanumeric_homebrew_update_is_outdated() {
        let installed = vec![installed("jpeg", "9d", 0)];
        let latest = envelope_map(vec![("jpeg", Some(("10", 0)), ("10", 0))]);

        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);

        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].update.as_deref(), Some("10"));
    }

    #[test]
    fn update_null_means_no_installable_version_is_not_outdated() {
        // The newest visible version has no compatible bottle for this
        // target: update is null, so there is nothing to install and the
        // package is not listed as outdated.
        let installed = vec![installed("confuse", "3.3", 0)];
        let latest = envelope_map(vec![("confuse", None, ("3.4", 0))]);
        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);
        assert!(outdated.is_empty());
    }

    #[test]
    fn update_and_latest_differ_when_newest_has_no_bottle() {
        // The ticket's example: `glu up` installs 3.3 (the newest version
        // with a compatible bottle) while 3.4 is published but not yet
        // installable. The table renders both columns.
        let installed = vec![installed("confuse", "3.2", 0)];
        let latest = envelope_map(vec![("confuse", Some(("3.3", 0)), ("3.4", 0))]);
        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].name.0, "confuse");
        assert_eq!(outdated[0].installed, "3.2_0");
        assert_eq!(outdated[0].update.as_deref(), Some("3.3"));
        assert_eq!(outdated[0].latest, "3.4");
    }

    #[test]
    fn newest_keg_wins_for_duplicate_names() {
        let installed = vec![
            installed("node", "24.0.0", 0),
            installed("node", "26.7.0", 0),
        ];
        let latest = envelope_map(vec![("node", Some(("26.7.0", 0)), ("26.7.0", 0))]);
        // Newest keg equals update -> not outdated; the older keg is ignored.
        assert!(outdated_entries(&InstalledState::from_packages(installed), &latest).is_empty());
    }

    #[test]
    fn missing_names_are_skipped() {
        let installed = vec![installed("removed-pkg", "1.0.0", 0)];
        let outdated = outdated_entries(&InstalledState::from_packages(installed), &[]);
        assert!(outdated.is_empty());
    }

    #[test]
    fn sorted_by_name() {
        let installed = vec![
            installed("zlib", "1.2.0", 0),
            installed("node", "25.0.0", 0),
        ];
        let latest = envelope_map(vec![
            ("node", Some(("26.7.0", 0)), ("26.7.0", 0)),
            ("zlib", Some(("1.3.0", 0)), ("1.3.0", 0)),
        ]);
        let outdated = outdated_entries(&InstalledState::from_packages(installed), &latest);
        let names: Vec<&str> = outdated.iter().map(|p| p.name.0.as_str()).collect();
        assert_eq!(names, vec!["node", "zlib"]);
    }
}
