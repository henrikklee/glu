use crate::state::installed::DependencyTreeNode;
use glu_core::PackageName;
use std::collections::BTreeMap;

/// Where a `glu deps` answer came from — offline receipts (the installed
/// reality) or an online resolve (prospective). The CLI states it so the two
/// are never silently confused: they answer different questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsSource {
    /// The package is installed; the tree came from its receipts.
    Installed,
    /// The package is not installed; the tree came from a registry resolve.
    Resolved,
}

/// `glu deps <name>`: the forward dependency tree of one package.
#[derive(Debug)]
pub struct DepsView {
    pub source: DepsSource,
    /// Whether the package has an installed keg (so the CLI can word the
    /// source marker correctly when `--online` forced the registry answer
    /// for an installed package).
    pub installed: bool,
    pub root: DependencyTreeNode,
    pub statuses: BTreeMap<PackageName, PackageStatus>,
}

/// `glu why <name>` result. The status map travels with the graph so
/// presentation never has to guess local installation/declaration facts.
#[derive(Debug)]
pub struct ReverseDepsView {
    pub root: Option<DependencyTreeNode>,
    pub statuses: BTreeMap<PackageName, PackageStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct PackageStatus {
    pub installed: bool,
    /// Concrete local version, when installed. This is deliberately separate
    /// from a resolved graph node's candidate version.
    pub installed_version: Option<String>,
    pub linked: bool,
    pub declared: bool,
    pub deactivated: bool,
    pub download_bytes: Option<u64>,
    pub installed_bytes: Option<u64>,
}
