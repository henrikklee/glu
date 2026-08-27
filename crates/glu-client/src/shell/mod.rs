use crate::config::ClientConfig;
use anyhow::{Context, Result};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const MANAGED_START: &str = "# >>> glu >>>";
const MANAGED_END: &str = "# <<< glu <<<";

/// Shells glu can integrate with, in display order.
const KNOWN_SHELLS: [&str; 6] = ["zsh", "bash", "fish", "tcsh", "csh", "sh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    Fish,
    Csh,
}

#[derive(Debug, Clone)]
struct Shell {
    name: String,
    kind: ShellKind,
}

#[derive(Debug, Clone)]
pub struct ShellStatus {
    pub name: String,
    pub config_path: PathBuf,
    pub configured: bool,
}

#[derive(Debug, Clone)]
pub struct SetupResult {
    pub shells: Vec<ShellStatus>,
    pub current_rc: Option<PathBuf>,
}

/// Install or refresh shell integration for every present shell.
pub fn setup_shells(config: &ClientConfig) -> Result<SetupResult> {
    ensure_all_shell_hooks(config)
}

pub fn shellenv(config: &ClientConfig, shell_name: Option<&str>) -> Result<String> {
    let shell = shell_name
        .map(shell_from_name)
        .or_else(detected_shell)
        .unwrap_or_else(|| shell_from_name("sh"));
    let path = ordered_path(config);

    let output = match shell.kind {
        ShellKind::Fish => fish_shellenv(&path),
        ShellKind::Csh => csh_shellenv(&path),
        ShellKind::Posix => posix_shellenv(&path),
    };
    Ok(output)
}

fn ensure_all_shell_hooks(config: &ClientConfig) -> Result<SetupResult> {
    let mut applied = Vec::new();
    for shell in discovered_shells() {
        let path = shell_config_path(&shell)?;
        let block = managed_block(config, &shell);
        install_managed_block(&path, &block)?;
        applied.push(ShellStatus {
            name: shell.name.clone(),
            config_path: path,
            configured: true,
        });
    }
    let current_rc = detected_shell().and_then(|shell| shell_config_path(&shell).ok());
    Ok(SetupResult {
        shells: applied,
        current_rc,
    })
}

pub fn shell_statuses() -> Result<Vec<ShellStatus>> {
    let mut out = Vec::new();
    for shell in discovered_shells() {
        let path = shell_config_path(&shell)?;
        let configured = contains_managed_block(&path).unwrap_or(false);
        out.push(ShellStatus {
            name: shell.name.clone(),
            config_path: path,
            configured,
        });
    }
    Ok(out)
}

fn managed_block(config: &ClientConfig, shell: &Shell) -> String {
    let glu = config.prefix.0.join("bin/glu");
    let glu_q = sh_quote_path(&glu);
    match shell.kind {
        ShellKind::Fish => {
            format!("{MANAGED_START}\n{glu_q} shellenv fish | source\n{MANAGED_END}\n",)
        }
        ShellKind::Csh => format!(
            "{MANAGED_START}\neval `{glu_q} shellenv {name}`\n{MANAGED_END}\n",
            name = shell.name,
        ),
        ShellKind::Posix => format!(
            "{MANAGED_START}\neval \"$({glu_q} shellenv {name})\"\n{MANAGED_END}\n",
            name = shell.name,
        ),
    }
}

fn install_managed_block(path: &Path, block: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let existing = fs::read_to_string(path).unwrap_or_default();
    let next = if let (Some(start), Some(end)) =
        (existing.find(MANAGED_START), existing.find(MANAGED_END))
    {
        let end = end + MANAGED_END.len();
        let mut next = String::new();
        next.push_str(&existing[..start]);
        if !next.ends_with('\n') && !next.is_empty() {
            next.push('\n');
        }
        next.push_str(block.trim_end());
        next.push_str(&existing[end..]);
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next
    } else {
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        if !next.is_empty() {
            next.push('\n');
        }
        next.push_str(block);
        next
    };

    atomic_write_shell_config(path, next.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

fn atomic_write_shell_config(path: &Path, contents: &[u8]) -> Result<()> {
    let write_path = symlink_write_target(path)?;
    let parent = write_path.parent().unwrap_or_else(|| Path::new("."));
    let filename = write_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shellrc".to_string());
    let original_permissions = fs::metadata(&write_path)
        .ok()
        .map(|meta| meta.permissions());

    let mut last_error = None;
    for attempt in 0..16u32 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp = parent.join(format!(
            ".{filename}.glu-tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(mut file) => {
                let result = (|| -> Result<()> {
                    file.write_all(contents)?;
                    if let Some(permissions) = original_permissions.clone() {
                        file.set_permissions(permissions)?;
                    } else {
                        #[cfg(unix)]
                        file.set_permissions(fs::Permissions::from_mode(0o644))?;
                    }
                    file.sync_all()?;
                    drop(file);
                    fs::rename(&temp, &write_path)?;
                    let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(last_error
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "temp exists"))
        .into())
}

fn symlink_write_target(path: &Path) -> Result<PathBuf> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    let target = fs::read_link(path)?;
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(path.parent().unwrap_or_else(|| Path::new(".")).join(target))
    }
}

fn contains_managed_block(path: &Path) -> Result<bool> {
    let content = fs::read_to_string(path)?;
    Ok(content.contains(MANAGED_START) && content.contains(MANAGED_END))
}

fn ordered_path(config: &ClientConfig) -> String {
    let glu_bin = config.prefix.0.join("bin");
    let glu_sbin = config.prefix.0.join("sbin");

    let current_path = env::var_os("PATH").unwrap_or_default();
    let mut ordered = vec![glu_bin.clone(), glu_sbin.clone()];
    ordered.extend(
        env::split_paths(&current_path).filter(|entry| entry != &glu_bin && entry != &glu_sbin),
    );

    env::join_paths(ordered)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn posix_shellenv(path: &str) -> String {
    format!(
        "export PATH={};\nhash -r 2>/dev/null || true;\n",
        sh_quote(path)
    )
}

fn fish_shellenv(path: &str) -> String {
    format!(
        "set --global --export PATH {};\n",
        env::split_paths(path)
            .map(|p| fish_quote(&p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn csh_shellenv(path: &str) -> String {
    format!("setenv PATH {};\n", csh_quote(path))
}

fn discovered_shells() -> Vec<Shell> {
    KNOWN_SHELLS
        .iter()
        .map(|name| shell_from_name(name))
        .filter(|shell| shell_present(&shell.name))
        .collect()
}

fn shell_present(name: &str) -> bool {
    if detected_shell()
        .map(|shell| shell.name == name)
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            if dir.join(name).is_file() {
                return true;
            }
        }
    }
    if let Ok(content) = fs::read_to_string("/etc/shells") {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if Path::new(line)
                .file_name()
                .map(|f| f == name)
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

fn detected_shell() -> Option<Shell> {
    parent_shell_name()
        .or_else(|| env::var("SHELL").ok().and_then(|shell| basename(&shell)))
        .map(|name| shell_from_name(&name))
}

fn parent_shell_name() -> Option<String> {
    let ppid = std::os::unix::process::parent_id().to_string();
    let output = Command::new("/bin/ps")
        .args(["-p", &ppid, "-c", "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name.trim_start_matches('-').to_string())
    }
}

fn shell_from_name(name: &str) -> Shell {
    let name = basename(name).unwrap_or_else(|| name.to_string());
    let name = name.trim_start_matches('-').to_string();
    let base = name.split('-').next().unwrap_or(&name).to_string();
    let kind = match base.as_str() {
        "fish" => ShellKind::Fish,
        "csh" | "tcsh" => ShellKind::Csh,
        _ => ShellKind::Posix,
    };
    Shell { name: base, kind }
}

fn shell_config_path(shell: &Shell) -> Result<PathBuf> {
    let home = home_dir()?;
    let path = match shell.name.as_str() {
        "zsh" => {
            let dir = env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or(home);
            dir.join(".zshrc")
        }
        "bash" => {
            let bash_profile = home.join(".bash_profile");
            if bash_profile.exists() {
                bash_profile
            } else {
                home.join(".bashrc")
            }
        }
        "fish" => home.join(".config/fish/config.fish"),
        "tcsh" => home.join(".tcshrc"),
        "csh" => home.join(".cshrc"),
        _ => home.join(".profile"),
    };
    Ok(path)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot find shell configuration file")
}

fn basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn sh_quote_path(path: &Path) -> String {
    sh_quote(&path.to_string_lossy())
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn csh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_core::{Prefix, Target};

    fn test_config() -> ClientConfig {
        ClientConfig {
            prefix: Prefix(PathBuf::from("/opt/glustore")),
            target: Target("arm64_tahoe".to_string()),
            registry_base_url: "http://localhost:3000".to_string(),
            distribution_base_url: glu_core::DEFAULT_DISTRIBUTION_BASE_URL.to_string(),
        }
    }

    #[test]
    fn managed_block_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rc");
        fs::write(&path, "before\n# >>> glu >>>\nold\n# <<< glu <<<\nafter\n").unwrap();
        install_managed_block(&path, "# >>> glu >>>\nnew\n# <<< glu <<<\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "before\n# >>> glu >>>\nnew\n# <<< glu <<<\nafter\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn managed_block_write_is_atomic_and_preserves_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rc");
        fs::write(&path, "before\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        install_managed_block(&path, "# >>> glu >>>\nnew\n# <<< glu <<<\n").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "before\n\n# >>> glu >>>\nnew\n# <<< glu <<<\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn managed_block_atomic_write_preserves_rc_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles/zshrc");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "before\n").unwrap();
        let link = dir.path().join(".zshrc");
        std::os::unix::fs::symlink("dotfiles/zshrc", &link).unwrap();

        install_managed_block(&link, "# >>> glu >>>\nnew\n# <<< glu <<<\n").unwrap();

        assert!(link.is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("dotfiles/zshrc")
        );
        assert!(fs::read_to_string(&target).unwrap().contains("new"));
    }

    #[test]
    fn shellenv_is_minimal_exports() {
        let output = posix_shellenv("/opt/glustore/bin:/usr/bin");
        assert!(output.contains("export PATH='"));
        assert!(!output.contains("glu()"));
        assert!(!output.contains("GLU_SHELL_WRAPPER"));
        // GLU_PREFIX is a manual override; shellenv must not set it.
        assert!(!output.contains("GLU_PREFIX"));
    }

    #[test]
    fn ordered_path_always_puts_glu_first_and_leaves_homebrew_in_the_tail() {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let old = env::var_os("PATH");
        env::set_var(
            "PATH",
            "/opt/homebrew/bin:/usr/bin:/opt/glustore/bin:/bin:/opt/glustore/sbin",
        );

        let path = ordered_path(&test_config());
        let entries: Vec<_> = env::split_paths(std::ffi::OsStr::new(&path)).collect();
        assert_eq!(entries[0], PathBuf::from("/opt/glustore/bin"));
        assert_eq!(entries[1], PathBuf::from("/opt/glustore/sbin"));
        assert_eq!(entries[2], PathBuf::from("/opt/homebrew/bin"));
        assert_eq!(entries[3], PathBuf::from("/usr/bin"));
        assert_eq!(entries[4], PathBuf::from("/bin"));
        assert_eq!(entries.len(), 5);

        match old {
            Some(value) => env::set_var("PATH", value),
            None => env::remove_var("PATH"),
        }
    }

    #[test]
    fn shell_from_name_maps_kinds() {
        assert_eq!(shell_from_name("zsh").kind, ShellKind::Posix);
        assert_eq!(shell_from_name("bash").kind, ShellKind::Posix);
        assert_eq!(shell_from_name("sh").kind, ShellKind::Posix);
        assert_eq!(shell_from_name("fish").kind, ShellKind::Fish);
        assert_eq!(shell_from_name("tcsh").kind, ShellKind::Csh);
        assert_eq!(shell_from_name("csh").kind, ShellKind::Csh);
        assert_eq!(shell_from_name("/bin/zsh").name, "zsh");
        assert_eq!(shell_from_name("-zsh").name, "zsh");
    }

    #[test]
    fn csh_shellenv_uses_setenv() {
        let output = csh_shellenv("/opt/glustore/bin:/usr/bin");
        assert!(output.contains("setenv PATH '"));
        assert!(!output.contains("GLU_PREFIX"));
    }

    #[test]
    fn managed_block_variants() {
        let config = test_config();
        let zsh = managed_block(&config, &shell_from_name("zsh"));
        assert!(zsh.contains("eval \"$('") && zsh.contains("shellenv zsh"));
        let fish = managed_block(&config, &shell_from_name("fish"));
        assert!(fish.contains("shellenv fish | source"));
        let tcsh = managed_block(&config, &shell_from_name("tcsh"));
        assert!(tcsh.contains("eval `") && tcsh.contains("shellenv tcsh"));
    }
}
