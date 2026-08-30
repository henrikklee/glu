use crate::remove::{self, RemovedPackage};
use crate::state::{store::InstalledStateStore, Declaration};
use anyhow::Result;
use glu_core::{InstalledPackage, Prefix};

/// The exact prefix-wide package and declaration transition approved by a
/// `purge` invocation. The download cache and glu executable are outside this
/// plan and are always preserved.
#[derive(Debug)]
pub struct PurgePlan {
    pub packages: Vec<InstalledPackage>,
    pub declaration: Option<Declaration>,
    pub keep_declaration: bool,
}

impl PurgePlan {
    pub fn declaration_present(&self) -> bool {
        self.declaration.is_some()
    }

    pub fn declared_packages(&self) -> usize {
        self.declaration
            .as_ref()
            .map_or(0, |declaration| declaration.dependencies.len())
    }

    pub fn requires_confirmation(&self) -> bool {
        !self.packages.is_empty() || (!self.keep_declaration && self.declaration_present())
    }

    /// Known installed bytes, or `None` when any installed receipt predates
    /// size recording. An empty prefix has a known total of zero.
    pub fn reclaimable_bytes(&self) -> Option<u64> {
        self.packages.iter().try_fold(0_u64, |total, package| {
            total.checked_add(package.installed_bytes?)
        })
    }
}

pub fn plan_purge(prefix: &Prefix, keep_declaration: bool) -> Result<PurgePlan> {
    let store = InstalledStateStore::new(prefix.clone());
    let declaration = store.read_declaration()?;
    let installed = store.load_installed_state_with_declaration(
        declaration.as_ref().unwrap_or(&Declaration::default()),
    )?;
    Ok(PurgePlan {
        packages: installed.list(),
        declaration,
        keep_declaration,
    })
}

/// Executes exactly the package set captured by `plan`. Declaration removal
/// happens first so interrupted default purges retain the user's requested
/// intent change; `--keep-declaration` never writes or prunes the declaration.
pub fn execute_purge(prefix: &Prefix, plan: &PurgePlan) -> Result<Vec<RemovedPackage>> {
    if !plan.keep_declaration && plan.declaration_present() {
        InstalledStateStore::new(prefix.clone()).remove_declaration()?;
    }
    remove::remove_installed_packages(prefix, &plan.packages)
}
