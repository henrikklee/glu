use super::postinstall_env::PostinstallEnvSnapshot;
use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsString,
    io,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// A must_succeed postinstall subprocess exited non-zero. Carries the captured
/// stderr so the step dispatcher can surface it with a dimmed context line
/// (red — the failure is fatal). `Display` names only the command so the raw
/// stderr appears exactly once (in the notice), not duplicated in the error
/// chain.
#[derive(Debug)]
pub(super) struct PostinstallCommandFailed {
    pub(super) command: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) stderr: Vec<u8>,
}

impl std::fmt::Display for PostinstallCommandFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "postinstall command failed: {}", self.command.display())
    }
}

impl std::error::Error for PostinstallCommandFailed {}

/// `{basename} {args…}` for the dimmed notice label, truncated on a char
/// boundary so path-heavy commands (post-install scripts, initdb) don't sprawl.
pub(super) fn command_label(command: &Path, args: &[String]) -> String {
    let base = command
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| command.display().to_string());
    let joined = if args.is_empty() {
        base
    } else {
        format!("{base} {}", args.join(" "))
    };
    const MAX: usize = 40;
    let mut chars = joined.chars();
    let head: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

pub(super) fn postinstall_label(pkg: &str, command: &Path, args: &[String]) -> String {
    format!("postinstall: {pkg} ({})", command_label(command, args))
}

/// Emits a postinstall subprocess failure as a dimmed context line + the
/// captured detail, colored by whether the install will fail (`fatal` → red,
/// otherwise yellow). Returns whether a notice was actually emitted — an empty
/// detail is skipped (a bare label is noise, and the error line already names
/// the command).
pub(super) fn postinstall_failure_notice(label: &str, detail: &str, fatal: bool) -> bool {
    let detail = detail.trim();
    if detail.is_empty() {
        return false;
    }
    let colored = if fatal {
        crate::style::red(detail)
    } else {
        crate::style::yellow(detail)
    };
    crate::worker_output::notice(&format!("{}\n{}", crate::style::dim(label), colored));
    true
}

/// A fatal postinstall failure whose stderr was already surfaced as a labeled
/// notice. The scheduler's error headline must not re-describe the cause (the
/// notice did); the inner error keeps the detail alive for any fallback path
/// that misses the marker.
#[derive(Debug)]
pub(crate) struct NoticedPostinstallFailure(pub(crate) anyhow::Error);

impl std::fmt::Display for NoticedPostinstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NoticedPostinstallFailure {}

// Homebrew 7d2a02d2: system_command.rb (SystemCommand.run / run!) — the
// equivalent of glu's spawn. Sudo mirrors Homebrew's `/usr/bin/sudo ... -E --`
// shape, including SUDO_ASKPASS `-A` and the nested
// HOMEBREW_SUDO_THROUGH_SUDO_USER path. Non-sudo children reset real UID to the
// effective UID when those differ, matching `reset_uid: true` for serialized
// postinstall commands. `sudo_as_root` is intentionally absent here: Homebrew's
// formula install-step runner does not pass it for structured formula steps
// (plain sudo already runs as root by default); Homebrew uses that option on
// other surfaces such as cask/pkg handling, which glu v0.1 does not implement.
// Runs under the explicit per-context postinstall environment when called from
// a postinstall; outside that context it inherits the process env minus
// sensitive dynamic-loader variables.
fn sudo_args(
    post_env: Option<&PostinstallEnvSnapshot>,
    command: &Path,
    args: &[String],
) -> Result<Vec<OsString>> {
    let askpass = env::var_os("SUDO_ASKPASS").is_some();
    let mut out = Vec::new();
    if postinstall_env_var_present(post_env, "HOMEBREW_SUDO_THROUGH_SUDO_USER") {
        let user = postinstall_env_value(post_env, "HOMEBREW_SUDO_USER")
            .or_else(|| postinstall_env_value(post_env, "SUDO_USER"))
            .filter(|value| !value.is_empty())
            .context("HOMEBREW_SUDO_THROUGH_SUDO_USER is set but SUDO_USER is unset")?;
        out.extend([
            OsString::from("--prompt"),
            OsString::from("Password for %p:"),
            OsString::from("-u"),
            OsString::from(user),
        ]);
        if askpass {
            out.push(OsString::from("-A"));
        }
        out.push(OsString::from("-E"));
        out.push(OsString::from("--"));
        out.push(OsString::from("/usr/bin/sudo"));
    }
    if askpass {
        out.push(OsString::from("-A"));
    }
    out.push(OsString::from("-E"));
    out.push(OsString::from("--"));
    out.push(command.as_os_str().to_os_string());
    out.extend(args.iter().map(OsString::from));
    Ok(out)
}

fn reset_real_uid_to_effective(cmd: &mut Command, sudo: bool) {
    if sudo {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `pre_exec` runs in the child after fork and before exec. The
        // closure only calls async-signal-safe libc UID getters/setuid and
        // constructs an io::Error from errno on failure.
        unsafe {
            cmd.pre_exec(|| {
                let uid = libc::getuid();
                let euid = libc::geteuid();
                if uid != euid && libc::setuid(euid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(super) struct RunCommand<'a> {
    post_env: Option<&'a PostinstallEnvSnapshot>,
    command: &'a Path,
    args: &'a [String],
    cwd: Option<&'a Path>,
    stdin: Option<Vec<u8>>,
    envs: Option<Vec<(String, String)>>,
    must_succeed: bool,
    env_remove: Option<&'a str>,
    sudo: bool,
}

impl<'a> RunCommand<'a> {
    pub(super) fn new(
        post_env: Option<&'a PostinstallEnvSnapshot>,
        command: &'a Path,
        args: &'a [String],
    ) -> Self {
        Self {
            post_env,
            command,
            args,
            cwd: None,
            stdin: None,
            envs: None,
            must_succeed: true,
            env_remove: None,
            sudo: false,
        }
    }

    pub(super) fn cwd(mut self, cwd: Option<&'a Path>) -> Self {
        self.cwd = cwd;
        self
    }

    pub(super) fn stdin(mut self, stdin: Option<Vec<u8>>) -> Self {
        self.stdin = stdin;
        self
    }

    pub(super) fn envs(mut self, envs: Option<Vec<(String, String)>>) -> Self {
        self.envs = envs;
        self
    }

    pub(super) fn must_succeed(mut self, must_succeed: bool) -> Self {
        self.must_succeed = must_succeed;
        self
    }

    pub(super) fn env_remove(mut self, key: &'a str) -> Self {
        self.env_remove = Some(key);
        self
    }

    pub(super) fn sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }
}

pub(super) fn run_command(spec: RunCommand<'_>) -> Result<std::process::Output> {
    let RunCommand {
        post_env,
        command,
        args,
        cwd,
        stdin,
        envs,
        must_succeed,
        env_remove,
        sudo,
    } = spec;

    let mut cmd = if sudo {
        let mut c = Command::new("/usr/bin/sudo");
        c.args(sudo_args(post_env, command, args)?);
        c
    } else {
        let mut c = Command::new(command);
        c.args(args);
        c
    };
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(post_env) = post_env {
        cmd.env_clear();
        cmd.envs(post_env.vars.iter().cloned());
    }
    if let Some(envs) = envs {
        cmd.envs(envs);
    }
    if let Some(key) = env_remove {
        cmd.env_remove(key);
    }
    // Never let the caller's dynamic-loader environment reach a postinstall
    // child — especially under `sudo -E` — so a formula's
    // `run` step can't be hijacked by injected libraries.
    for key in [
        "DYLD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_FRAMEWORK_PATH",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
    ] {
        cmd.env_remove(key);
    }
    reset_real_uid_to_effective(&mut cmd, sudo);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", command.display()))?;
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .context("opening postinstall command stdin")?
            .write_all(&stdin)?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("running {}", command.display()))?;
    if !output.status.success() && must_succeed {
        // Fatal failure: return the captured stderr to the step dispatcher
        // instead of noticing it here — this layer has no package context for
        // the dimmed label, and the dispatcher knows whether the install
        // fails (red) or continues (yellow).
        return Err(PostinstallCommandFailed {
            command: command.to_path_buf(),
            args: args.to_vec(),
            stderr: output.stderr,
        }
        .into());
    }
    Ok(output)
}

pub(super) fn postinstall_env_var_present(
    post_env: Option<&PostinstallEnvSnapshot>,
    key: &str,
) -> bool {
    postinstall_env_value(post_env, key).is_some_and(|value| !value.is_empty())
}

fn postinstall_env_value(post_env: Option<&PostinstallEnvSnapshot>, key: &str) -> Option<String> {
    if let Some(env) = post_env {
        let key = OsString::from(key);
        env.vars
            .iter()
            .find(|(candidate, _)| candidate == &key)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    } else {
        env::var_os(key).map(|value| value.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_label_truncates_on_char_boundary() {
        assert_eq!(
            command_label(Path::new("/usr/bin/killall"), &["gpg-agent".into()]),
            "killall gpg-agent"
        );
        assert_eq!(command_label(Path::new("/usr/bin/true"), &[]), "true");
        // Long path-heavy args truncate at 40 chars, still on the char
        // boundary, with a trailing ellipsis.
        let long = "a".repeat(50);
        let label = command_label(Path::new("/opt/glustore/bin/post-install"), &[long]);
        assert!(label.starts_with("post-install "));
        assert!(label.ends_with('…'));
        // head is capped at 40 chars including the basename prefix, plus the
        // trailing ellipsis.
        assert_eq!(label.chars().count(), 41);
        assert_eq!(
            postinstall_label(
                "gnupg",
                Path::new("/usr/bin/killall"),
                &["gpg-agent".into()]
            ),
            "postinstall: gnupg (killall gpg-agent)"
        );
    }
}
