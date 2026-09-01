use crate::state::{
    declaration::Declaration, installed::InstalledState, recovery, store::InstalledStateStore,
};
use anyhow::Result;
use glu_core::Prefix;

#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub declaration: Declaration,
    pub installed: InstalledState,
    pub warnings: Vec<String>,
}

impl StateSnapshot {
    pub fn load(prefix: &Prefix) -> Result<Self> {
        record_test_load();
        let store = InstalledStateStore::new(prefix.clone());
        let declaration = store.load_declaration()?;
        let mut warnings = Vec::new();
        let installed = store.load_installed_state_with_warnings(&declaration, &mut warnings)?;
        Ok(Self {
            declaration,
            installed,
            warnings,
        })
    }

    pub fn load_for_mutation(prefix: &Prefix) -> Result<Self> {
        recovery::cleanup_interrupted(prefix)?;
        record_test_load();
        let store = InstalledStateStore::new(prefix.clone());
        let declaration = store.load_declaration()?;
        let installed = store.load_installed_state_with_declaration_strict(&declaration)?;
        Ok(Self {
            declaration,
            installed,
            warnings: Vec::new(),
        })
    }
}

#[cfg(feature = "dev-registry")]
fn record_test_load() {
    use std::io::Write;

    let Some(path) = std::env::var_os("GLU_TEST_STATE_LOAD_LOG") else {
        return;
    };
    let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(log, "load");
}

#[cfg(not(feature = "dev-registry"))]
fn record_test_load() {}
