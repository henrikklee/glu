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
    ask_yes_no()
}

pub(super) fn confirm_removal(plan: &RemovalPlan) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu rm -y` to confirm",
            glu_client::format::plural(plan.to_remove.len(), "package")
        );
    }
    ask_yes_no()
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
    ask_yes_no()
}

pub(super) fn confirm_cleanup(plan: &CacheCleanupPlan) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "would remove {}; re-run with `glu cleanup -y` to confirm",
            glu_client::format::plural(plan.bottles().len(), "cached download")
        );
    }
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
