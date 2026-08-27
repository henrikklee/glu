use crate::{
    output::print_update_tree,
    package_list::{self, PackageListItem},
};
use anyhow::{bail, Result};
use glu_client::install::UpdatePlan;
use glu_client::remove::RemovalPlan;
use glu_core::InstalledPackage;
use std::io::{BufRead, IsTerminal, Write};

pub(super) fn confirm_install(
    plan: &glu_client::install::InstallPlan,
    command: &str,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu {command} -y` to confirm",
            glu_client::format::plural(plan.would_remove.len(), "unused package")
        );
    }
    let items: Vec<_> = plan
        .would_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_counted_section("Will also remove", "unused package", &items);
    ask_yes_no()
}

pub(super) fn confirm_removal(plan: &RemovalPlan) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu rm -y` to confirm",
            glu_client::format::plural(plan.to_remove.len(), "package")
        );
    }
    let items: Vec<_> = plan
        .to_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_section("Will remove", &items);
    ask_yes_no()
}

/// Confirmation gate for `glu up`: the command always presents its plan
/// (version bumps plus any removals from dropped dependencies) before doing
/// anything. Non-interactive use refuses and points at `-y`.
pub(super) fn confirm_update(plan: &UpdatePlan, tree: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        let removals = if plan.to_remove.is_empty() {
            String::new()
        } else {
            format!(
                " and remove {}",
                glu_client::format::plural(plan.to_remove.len(), "package")
            )
        };
        bail!(
            "would update {}{removals}; re-run with `glu up -y` to confirm",
            glu_client::format::plural(plan.to_update.len(), "package")
        );
    }
    let update_tree = plan.dependency_tree();
    let changes: Vec<_> = plan
        .to_update
        .iter()
        .map(|update| {
            (
                update.name.0.as_str(),
                update.current.as_str(),
                update.latest.as_str(),
            )
        })
        .collect();
    if !(tree && print_update_tree("Will update", &update_tree, &changes)) {
        let items: Vec<_> = plan
            .to_update
            .iter()
            .map(|update| {
                PackageListItem::update(&update.name.0, &update.current, &update.latest)
                    .emphasized(update.direct == Some(true))
            })
            .collect();
        package_list::print_section("Will update", &items);
    }
    let removals: Vec<_> = plan
        .to_remove
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_section("Will also remove", &removals);
    ask_yes_no()
}

/// Confirmation gate for `glu autoremove`: always lists what will go and
/// asks — removal is its whole job. Non-interactive use refuses and points
/// at `-y`.
pub(super) fn confirm_autoremove(packages: &[InstalledPackage]) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu autoremove -y` to confirm",
            glu_client::format::plural(packages.len(), "package")
        );
    }
    let items: Vec<_> = packages
        .iter()
        .map(|package| PackageListItem::package(&package.name.0, &package.keg_version.0))
        .collect();
    package_list::print_counted_section("Will remove", "unused package", &items);
    ask_yes_no()
}

/// The shared interactive confirmation: `Continue? [Y/n]`, default yes,
/// anything but an empty answer or y/yes aborts.
fn ask_yes_no() -> Result<()> {
    print!("Continue? [Y/n] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if !matches!(answer.as_str(), "" | "y" | "yes") {
        bail!("aborted");
    }
    println!();
    Ok(())
}
