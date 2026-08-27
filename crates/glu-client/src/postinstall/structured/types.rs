use super::analysis::analyze_structured_postinstalls;
use anyhow::{Context, Result};
use glu_core::{InstallManifest, PackageId, Prefix};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum PostinstallStepPlan {
    Inline,
    DeferredGlobal { kind: String, key: Vec<String> },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostinstallPlan {
    pub per_step: Vec<PostinstallStepPlan>,
}

impl PostinstallPlan {
    fn empty() -> Self {
        Self {
            per_step: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostinstallAnalysis {
    pub plan: PostinstallPlan,
    pub(super) deferred_contributions: BTreeMap<(String, Vec<String>), BTreeSet<String>>,
}

impl PostinstallAnalysis {
    pub(super) fn empty() -> Self {
        Self {
            plan: PostinstallPlan::empty(),
            deferred_contributions: BTreeMap::new(),
        }
    }

    pub fn deferred_contributions(&self) -> &BTreeMap<(String, Vec<String>), BTreeSet<String>> {
        &self.deferred_contributions
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostinstallPlans {
    by_package: BTreeMap<PackageId, PostinstallAnalysis>,
}

impl PostinstallPlans {
    pub fn analyze(
        manifest: &InstallManifest,
        package_ids: &[PackageId],
        prefix: &Prefix,
    ) -> Result<Self> {
        let mut by_package = BTreeMap::new();
        for package_id in package_ids {
            let package = manifest
                .packages
                .get(package_id)
                .with_context(|| format!("missing package {}", package_id.0))?;
            let keg = prefix
                .0
                .join("Cellar")
                .join(&package.name.0)
                .join(&package.keg_version.0);
            by_package.insert(
                package_id.clone(),
                analyze_structured_postinstalls(prefix, package, &keg)?,
            );
        }
        Ok(Self { by_package })
    }

    pub fn get(&self, package_id: &PackageId) -> Option<&PostinstallAnalysis> {
        self.by_package.get(package_id)
    }

    pub fn analysis_for(&self, package_id: &PackageId) -> Result<&PostinstallAnalysis> {
        self.get(package_id)
            .with_context(|| format!("missing postinstall analysis for {}", package_id.0))
    }

    pub fn plan_for(&self, package_id: &PackageId) -> Result<&PostinstallPlan> {
        Ok(&self.analysis_for(package_id)?.plan)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredPostinstallRequest {
    pub kind: String,
    pub key: Vec<String>,
    pub source_formula: String,
    /// Formula-level Homebrew postinstall sandbox network policy for this
    /// contributing formula. Coalesced global postinstalls use the restrictive
    /// merge: network is allowed only when every actual contributor allowed it.
    #[serde(default = "default_network_allowed")]
    pub network_access_allowed: bool,
}

#[derive(Clone, Debug)]
pub struct DeferredPostinstallItem {
    pub kind: String,
    pub key: Vec<String>,
    pub source_formulas: BTreeSet<String>,
    pub network_access_allowed: bool,
}

fn default_network_allowed() -> bool {
    true
}

// glu invention (no Homebrew equivalent); see DEFERRABLE_GLOBAL_TYPES comment.
// Readiness is modeled by the execution DAG: every possible contributor's
// formula_postinstall node must complete before its cache_postinstall node can
// run. The queue below is the runtime source of truth for whether a guarded
// step actually requested the cache rebuild.
pub struct DeferredPostinstallQueue {
    pub(super) items: BTreeMap<(String, Vec<String>), BTreeMap<String, bool>>,
}

impl DeferredPostinstallQueue {
    pub fn new() -> Self {
        Self {
            items: BTreeMap::new(),
        }
    }

    pub fn defer(
        &mut self,
        kind: String,
        key: Vec<String>,
        source_formula: String,
        network_access_allowed: bool,
    ) {
        self.items
            .entry((kind, key))
            .or_default()
            .insert(source_formula, network_access_allowed);
    }

    pub fn take(&mut self, kind: &str, key: &[String]) -> Option<DeferredPostinstallItem> {
        let map_key = (kind.to_string(), key.to_vec());
        self.items.remove(&map_key).map(|sources| {
            let network_access_allowed = sources.values().all(|allowed| *allowed);
            DeferredPostinstallItem {
                kind: map_key.0,
                key: map_key.1,
                source_formulas: sources.keys().cloned().collect(),
                network_access_allowed,
            }
        })
    }

    pub fn requests(&self) -> Vec<DeferredPostinstallRequest> {
        let mut requests = Vec::new();
        for ((kind, key), sources) in &self.items {
            for (source_formula, network_access_allowed) in sources {
                requests.push(DeferredPostinstallRequest {
                    kind: kind.clone(),
                    key: key.clone(),
                    source_formula: source_formula.clone(),
                    network_access_allowed: *network_access_allowed,
                });
            }
        }
        requests
    }
}

impl Default for DeferredPostinstallQueue {
    fn default() -> Self {
        Self::new()
    }
}
