use crate::output::print_tree_roots;
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
    println!(
        "Will also remove {}:",
        glu_client::format::plural(plan.would_remove.len(), "unused package")
    );
    for package in &plan.would_remove {
        println!("- {} {}", package.name.0, package.keg_version.0);
    }
    ask_yes_no()
}

pub(super) fn confirm_removal(plan: &RemovalPlan) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu rm -y` to confirm",
            glu_client::format::plural(plan.to_remove.len(), "package")
        );
    }
    println!(
        "Will remove {}:",
        glu_client::format::plural(plan.to_remove.len(), "package")
    );
    for package in &plan.to_remove {
        println!("- {} {}", package.name.0, package.keg_version.0);
    }
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
    if tree {
        if let Some(manifest) = &plan.manifest {
            println!(
                "Will update {}:",
                glu_client::format::plural(plan.to_update.len(), "package")
            );
            for root in &manifest.roots {
                if let Some(node) =
                    glu_client::install::dependency_tree_from_manifest(manifest, &root.package)
                {
                    print_tree_roots(std::slice::from_ref(&node), false, false);
                }
            }
        }
    } else {
        println!(
            "Will update {}:",
            glu_client::format::plural(plan.to_update.len(), "package")
        );
        for update in &plan.to_update {
            println!(
                "- {} {} -> {}",
                update.name.0, update.current, update.latest
            );
        }
    }
    if !plan.to_remove.is_empty() {
        println!(
            "Will also remove {}:",
            glu_client::format::plural(plan.to_remove.len(), "package")
        );
        for package in &plan.to_remove {
            println!("- {} {}", package.name.0, package.keg_version.0);
        }
    }
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
    println!(
        "Will remove {} unused:",
        glu_client::format::plural(packages.len(), "package")
    );
    for package in packages {
        println!("- {} {}", package.name.0, package.keg_version.0);
    }
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
