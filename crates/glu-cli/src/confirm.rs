use crate::{
    output::print_update_tree,
    package_list::{self, PackageListItem},
};
use anyhow::{bail, Result};
use console::{Key, Term};
use glu_client::download::cache::CacheCleanupPlan;
use glu_client::install::UpdatePlan;
use glu_client::remove::RemovalPlan;
use glu_core::InstalledPackage;
use std::{
    collections::BTreeMap,
    io::{BufRead, IsTerminal, Write},
};

pub(super) fn confirm_install(
    plan: &glu_client::install::InstallPlan,
    command: &str,
) -> Result<bool> {
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

pub(super) fn confirm_removal(plan: &RemovalPlan) -> Result<bool> {
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
pub(super) fn confirm_update(plan: &UpdatePlan, tree: bool) -> Result<bool> {
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
pub(super) fn confirm_autoremove(packages: &[InstalledPackage]) -> Result<bool> {
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

pub(super) fn confirm_cleanup(plan: &CacheCleanupPlan) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu cleanup -y` to confirm",
            glu_client::format::plural(plan.bottles().len(), "cached download")
        );
    }
    let mut packages = BTreeMap::<(String, String), (usize, u64)>::new();
    let mut unassociated_bottles = 0;
    let mut unassociated_bytes = 0_u64;
    for bottle in plan.bottles() {
        if let Some(package) = bottle.package() {
            let summary = packages
                .entry((package.name.0.clone(), package.keg_version.0.clone()))
                .or_default();
            summary.0 += 1;
            summary.1 += bottle.bytes();
        } else {
            unassociated_bottles += 1;
            unassociated_bytes += bottle.bytes();
        }
    }
    let mut items: Vec<_> = packages
        .into_iter()
        .map(|((name, version), (bottles, bytes))| {
            let annotation = if bottles == 1 {
                glu_client::format::human_bytes(bytes)
            } else {
                format!(
                    "{}, {}",
                    glu_client::format::plural(bottles, "download"),
                    glu_client::format::human_bytes(bytes)
                )
            };
            PackageListItem::package(name, version).annotated(annotation)
        })
        .collect();
    if unassociated_bottles > 0 {
        items.push(PackageListItem::name("Unassociated").annotated(format!(
            "{}, {}",
            glu_client::format::plural(unassociated_bottles, "download"),
            glu_client::format::human_bytes(unassociated_bytes)
        )));
    }
    package_list::print_counted_section_with_total(
        "Will remove",
        "cached download",
        plan.bottles().len(),
        &items,
    );
    println!(
        "Will reclaim: {}",
        glu_client::format::human_bytes(plan.reclaimable_bytes())
    );
    ask_yes_no()
}

/// The shared interactive confirmation. Enter accepts the default; Y accepts
/// and N or Escape abort immediately without waiting for Enter.
fn ask_yes_no() -> Result<bool> {
    print!("Continue? [Y/n] ");
    std::io::stdout().flush()?;

    let term = Term::stdout();
    if term.is_term() {
        loop {
            let key = term.read_key()?;
            let Some(confirmed) = confirmation_for_key(&key) else {
                continue;
            };
            match key {
                Key::Char('y' | 'Y') => println!("y"),
                Key::Char('n' | 'N') => println!("n"),
                _ => println!(),
            }
            return Ok(confirmed);
        }
    }

    // Preserve line-oriented behavior in the unusual case where stdin is a
    // terminal but stdout is redirected and cannot support key events.
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    let confirmed = matches!(answer.as_str(), "" | "y" | "yes");
    println!();
    Ok(confirmed)
}

fn confirmation_for_key(key: &Key) -> Option<bool> {
    match key {
        Key::Enter | Key::Char('y' | 'Y') => Some(true),
        Key::Escape | Key::Char('n' | 'N') => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::confirmation_for_key;
    use console::Key;

    #[test]
    fn confirmation_keys_accept_or_cancel_without_enter() {
        assert_eq!(confirmation_for_key(&Key::Char('y')), Some(true));
        assert_eq!(confirmation_for_key(&Key::Char('Y')), Some(true));
        assert_eq!(confirmation_for_key(&Key::Enter), Some(true));
        assert_eq!(confirmation_for_key(&Key::Char('n')), Some(false));
        assert_eq!(confirmation_for_key(&Key::Char('N')), Some(false));
        assert_eq!(confirmation_for_key(&Key::Escape), Some(false));
        assert_eq!(confirmation_for_key(&Key::ArrowDown), None);
    }
}
