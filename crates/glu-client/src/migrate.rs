mod homebrew;

use crate::{events::ExecutionEvents, install, GluClient};
use anyhow::{bail, Result};
use glu_core::{PackageName, PackageSelector};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

pub use homebrew::{HomebrewRoot, HomebrewSnapshot};

#[derive(Debug, Clone)]
pub struct MigrationPlan {
    pub source: PathBuf,
    pub roots: Vec<PackageName>,
    pub inferred_deactivated: Vec<PackageName>,
    pub warnings: Vec<String>,
    pub configuration_migrated: bool,
    pub install: Option<install::InstallPlan>,
    source_snapshot: HomebrewSnapshot,
}

#[derive(Debug, Clone, Default)]
pub struct MigrationSummary {
    pub source: PathBuf,
    pub roots: Vec<PackageName>,
    pub inferred_deactivated: Vec<PackageName>,
    pub warnings: Vec<String>,
    pub configuration_migrated: bool,
    pub install: Option<install::InstallSummary>,
}

pub async fn plan_migration(client: &GluClient, source: PathBuf) -> Result<MigrationPlan> {
    if source == client.config().prefix.0 {
        bail!("Homebrew source prefix and glu prefix must be different");
    }
    let source_snapshot = homebrew::discover(&source)?;
    let roots = source_snapshot
        .roots
        .iter()
        .map(|root| root.name.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(MigrationPlan {
            source,
            roots,
            inferred_deactivated: Vec::new(),
            warnings: source_snapshot.warnings.clone(),
            configuration_migrated: false,
            install: None,
            source_snapshot,
        });
    }

    let unlinked = source_snapshot
        .roots
        .iter()
        .filter(|root| !root.linked)
        .map(|root| PackageSelector(root.name.0.clone()))
        .collect::<BTreeSet<_>>();
    let mut install = client
        .plan_install(
            roots
                .iter()
                .map(|name| PackageSelector(name.0.clone()))
                .collect(),
            install::InstallOptions::default(),
        )
        .await?;
    let inferred_deactivated = install.preserve_deactivation_for_new_global_roots(&unlinked);

    Ok(MigrationPlan {
        source,
        roots,
        inferred_deactivated,
        warnings: source_snapshot.warnings.clone(),
        configuration_migrated: false,
        install: Some(install),
        source_snapshot,
    })
}

pub async fn execute_migration(
    client: &GluClient,
    plan: MigrationPlan,
    options: install::InstallOptions,
    events: Arc<dyn ExecutionEvents>,
) -> Result<MigrationSummary> {
    let current = homebrew::discover(&plan.source)?;
    if current != plan.source_snapshot {
        bail!("Homebrew installation changed after planning; rerun `glu migrate`");
    }

    let install = match plan.install {
        Some(install) => Some(client.execute_install(install, options, events).await?),
        None => None,
    };
    Ok(MigrationSummary {
        source: plan.source,
        roots: plan.roots,
        inferred_deactivated: plan.inferred_deactivated,
        warnings: plan.warnings,
        configuration_migrated: false,
        install,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_snapshot_equality_ignores_warnings_but_not_roots() {
        let source = PathBuf::from("/tmp/homebrew");
        let mut left = HomebrewSnapshot {
            source: source.clone(),
            roots: Vec::new(),
            warnings: vec!["one".to_string()],
        };
        let right = HomebrewSnapshot {
            source,
            roots: Vec::new(),
            warnings: vec!["two".to_string()],
        };
        assert_eq!(left, right);
        left.roots.push(HomebrewRoot {
            name: PackageName("curl".to_string()),
            linked: false,
            receipts: Vec::new(),
        });
        assert_ne!(left, right);
    }
}
