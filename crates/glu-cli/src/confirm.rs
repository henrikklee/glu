use anyhow::{bail, Result};
use console::{Key, Term};
use glu_client::download::cache::CacheCleanupPlan;
use glu_client::install::UpdatePlan;
use glu_client::remove::RemovalPlan;
use glu_core::InstalledPackage;
use std::io::{BufRead, IsTerminal, Write};

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
    ask_yes_no("Continue?", true)
}

pub(super) fn confirm_migration(packages: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would migrate {}; re-run with `glu migrate -y` to confirm",
            glu_client::format::plural(packages, "requested package")
        );
    }
    ask_yes_no("Continue?", true)
}

pub(super) fn confirm_removal(plan: &RemovalPlan, remove_config: bool) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        let command = if remove_config {
            "glu rm --remove-config -y"
        } else {
            "glu rm -y"
        };
        bail!(
            "would remove {}; re-run with `{command}` to confirm",
            glu_client::format::plural(plan.to_remove.len(), "package")
        );
    }
    ask_yes_no("Continue?", true)
}

/// Confirmation gate for `glu up`: the command always presents its plan
/// (version bumps plus any removals from dropped dependencies) before doing
/// anything. Non-interactive use refuses and points at `-y`.
pub(super) fn confirm_update(plan: &UpdatePlan) -> Result<bool> {
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
    ask_yes_no("Continue?", true)
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
    ask_yes_no("Continue?", true)
}

pub(super) fn confirm_cleanup(plan: &CacheCleanupPlan) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu cleanup -y` to confirm",
            glu_client::format::plural(plan.bottles().len(), "cached download")
        );
    }
    ask_yes_no("Continue?", true)
}

pub(super) fn confirm_purge(
    plan: &glu_client::purge::PurgePlan,
    remove_config: bool,
) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        let command = match (plan.keep_declaration, remove_config) {
            (true, true) => "glu purge --keep-declaration --remove-config -y",
            (true, false) => "glu purge --keep-declaration -y",
            (false, true) => "glu purge --remove-config -y",
            (false, false) => "glu purge -y",
        };
        if plan.packages.is_empty() {
            bail!("would remove glu.json; re-run with `{command}` to confirm");
        }
        bail!(
            "would purge {}; re-run with `{command}` to confirm",
            glu_client::format::plural(plan.packages.len(), "package")
        );
    }
    ask_yes_no("Continue?", true)
}

pub(super) fn offer_modified_config_cleanup(files: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    ask_yes_no(
        &format!(
            "Also remove {}?",
            glu_client::format::plural(files, "modified configuration file")
        ),
        false,
    )
}

pub(super) fn confirm_modified_config_cleanup(files: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `--remove-config --yes` to confirm",
            glu_client::format::plural(files, "modified configuration file")
        );
    }
    ask_yes_no(
        &format!(
            "Remove {}?",
            glu_client::format::plural(files, "modified configuration file")
        ),
        false,
    )
}

/// The shared interactive confirmation. Enter accepts the default; Y accepts
/// and N or Escape abort immediately without waiting for Enter.
fn ask_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let choice = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {choice} ");
    std::io::stdout().flush()?;

    let term = Term::stdout();
    if term.is_term() {
        loop {
            let key = term.read_key()?;
            let Some(confirmed) = confirmation_for_key(&key, default_yes) else {
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
    let confirmed = if answer.is_empty() {
        default_yes
    } else {
        matches!(answer.as_str(), "y" | "yes")
    };
    println!();
    Ok(confirmed)
}

fn confirmation_for_key(key: &Key, default_yes: bool) -> Option<bool> {
    match key {
        Key::Enter => Some(default_yes),
        Key::Char('y' | 'Y') => Some(true),
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
        assert_eq!(confirmation_for_key(&Key::Char('y'), true), Some(true));
        assert_eq!(confirmation_for_key(&Key::Char('Y'), false), Some(true));
        assert_eq!(confirmation_for_key(&Key::Enter, true), Some(true));
        assert_eq!(confirmation_for_key(&Key::Enter, false), Some(false));
        assert_eq!(confirmation_for_key(&Key::Char('n'), true), Some(false));
        assert_eq!(confirmation_for_key(&Key::Char('N'), false), Some(false));
        assert_eq!(confirmation_for_key(&Key::Escape, true), Some(false));
        assert_eq!(confirmation_for_key(&Key::ArrowDown, true), None);
    }
}
