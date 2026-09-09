//! Homebrew-parity postinstall sandbox worker.
//!
//! Homebrew runs formula postinstall in a child process under the platform
//! sandbox (`FormulaInstaller#post_install` -> `Sandbox.run_or_fork`). glu's
//! structured postinstall steps include native Rust filesystem mutations, so the
//! sandbox boundary must wrap the whole postinstall worker process rather than
//! only subprocess `run` steps.

use crate::events::{ExecutionEvents, OutputStream};
use crate::postinstall::structured::{
    run_deferred_global_postinstall, run_structured_postinstalls_with_plan,
    DeferredPostinstallQueue, DeferredPostinstallRequest, PostinstallPlan,
};
use anyhow::{bail, Context, Result};
use glu_core::{Prefix, ResolvedPackage};
use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum PostinstallWorkerJob {
    Formula {
        prefix: Prefix,
        package: Box<ResolvedPackage>,
        keg: PathBuf,
        plan: PostinstallPlan,
        verbose: bool,
    },
    DeferredGlobal {
        prefix: Prefix,
        kind: String,
        key: Vec<String>,
        network_access_allowed: bool,
        verbose: bool,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PostinstallWorkerResult {
    #[serde(default)]
    pub deferred_requests: Vec<DeferredPostinstallRequest>,
}

pub fn run_formula_postinstall_sandboxed(
    prefix: &Prefix,
    package: &ResolvedPackage,
    keg: &Path,
    plan: &PostinstallPlan,
    deferred: &mut DeferredPostinstallQueue,
    verbose: bool,
    events: &dyn ExecutionEvents,
) -> Result<()> {
    if package.install.post_install_steps.is_empty() {
        return Ok(());
    }
    let result = run_job_sandboxed(
        PostinstallWorkerJob::Formula {
            prefix: prefix.clone(),
            package: Box::new(package.clone()),
            keg: keg.to_path_buf(),
            plan: plan.clone(),
            verbose,
        },
        events,
    )?;
    for request in result.deferred_requests {
        deferred.defer(
            request.kind,
            request.key,
            request.source_formula,
            request.network_access_allowed,
        );
    }
    Ok(())
}

pub fn run_deferred_global_postinstall_sandboxed(
    prefix: &Prefix,
    kind: &str,
    key: &[String],
    network_access_allowed: bool,
    verbose: bool,
    events: &dyn ExecutionEvents,
) -> Result<()> {
    run_job_sandboxed(
        PostinstallWorkerJob::DeferredGlobal {
            prefix: prefix.clone(),
            kind: kind.to_string(),
            key: key.to_vec(),
            network_access_allowed,
            verbose,
        },
        events,
    )?;
    Ok(())
}

pub fn run_worker(job_path: &Path, result_path: &Path) -> Result<()> {
    let job: PostinstallWorkerJob = serde_json::from_slice(
        &fs::read(job_path).with_context(|| format!("reading {}", job_path.display()))?,
    )
    .with_context(|| format!("decoding {}", job_path.display()))?;
    let result = match job {
        PostinstallWorkerJob::Formula {
            prefix,
            package,
            keg,
            plan,
            verbose,
        } => {
            let mut queue = DeferredPostinstallQueue::new();
            run_structured_postinstalls_with_plan(
                &prefix,
                &package,
                &keg,
                &plan,
                Some(&mut queue),
                verbose,
            )?;
            PostinstallWorkerResult {
                deferred_requests: queue.requests(),
            }
        }
        PostinstallWorkerJob::DeferredGlobal {
            prefix,
            kind,
            key,
            verbose,
            ..
        } => {
            run_deferred_global_postinstall(&prefix, &kind, &key, verbose)?;
            PostinstallWorkerResult::default()
        }
    };
    if let Some(parent) = result_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(result_path, serde_json::to_vec_pretty(&result)?)
        .with_context(|| format!("writing {}", result_path.display()))?;
    Ok(())
}

fn run_job_sandboxed(
    job: PostinstallWorkerJob,
    events: &dyn ExecutionEvents,
) -> Result<PostinstallWorkerResult> {
    let work = tempfile::Builder::new()
        .prefix("glu-postinstall-worker-")
        .tempdir()
        .context("creating postinstall worker directory")?;
    let job_path = work.path().join("job.json");
    let result_path = work.path().join("result.json");
    let profile_path = work.path().join("postinstall.sb");
    fs::write(&job_path, serde_json::to_vec_pretty(&job)?)
        .with_context(|| format!("writing {}", job_path.display()))?;
    let profile = SandboxProfile::for_job(&job, work.path())?;
    fs::write(&profile_path, profile.render())
        .with_context(|| format!("writing {}", profile_path.display()))?;

    let exe = env::current_exe().context("locating current glu executable")?;
    let mut command = sandbox_command(&profile_path, &exe, &job_path, &result_path)?;
    apply_worker_env(&mut command);
    let output = command.output().with_context(|| {
        format!(
            "running sandboxed postinstall worker with {}",
            profile_path.display()
        )
    })?;
    relay_worker_output(events, &output.stdout, &output.stderr);
    if !output.status.success() {
        bail!("sandboxed postinstall worker failed");
    }
    let result: PostinstallWorkerResult = serde_json::from_slice(
        &fs::read(&result_path).with_context(|| format!("reading {}", result_path.display()))?,
    )
    .with_context(|| format!("decoding {}", result_path.display()))?;
    Ok(result)
}

#[cfg(target_os = "macos")]
fn sandbox_command(
    profile_path: &Path,
    exe: &Path,
    job_path: &Path,
    result_path: &Path,
) -> Result<Command> {
    let sandbox_exec = Path::new("/usr/bin/sandbox-exec");
    if !sandbox_exec.is_file() {
        bail!(
            "macOS postinstall sandbox is unavailable: {}",
            sandbox_exec.display()
        );
    }
    let mut command = Command::new(sandbox_exec);
    command
        .arg("-f")
        .arg(profile_path)
        .arg(exe)
        .arg("__postinstall-worker")
        .arg("--job")
        .arg(job_path)
        .arg("--result")
        .arg(result_path);
    Ok(command)
}

#[cfg(not(target_os = "macos"))]
fn sandbox_command(
    _profile_path: &Path,
    _exe: &Path,
    _job_path: &Path,
    _result_path: &Path,
) -> Result<Command> {
    bail!("postinstall sandbox is only implemented for macOS")
}

fn apply_worker_env(command: &mut Command) {
    let vars = env::vars_os()
        .filter(|(key, _)| !worker_sensitive_env_key(key) && key != "HOMEBREW_PATH")
        .collect::<Vec<_>>();
    command.env_clear();
    command.envs(vars);
}

fn worker_sensitive_env_key(key: &OsString) -> bool {
    let key = key.to_string_lossy();
    matches!(
        key.as_ref(),
        "DYLD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_FRAMEWORK_PATH"
            | "LD_LIBRARY_PATH"
            | "LD_PRELOAD"
    ) || homebrew_sensitive_env_key(&key)
}

fn homebrew_sensitive_env_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["cookie", "key", "token", "password", "passphrase", "auth"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn relay_worker_output(events: &dyn ExecutionEvents, stdout: &[u8], stderr: &[u8]) {
    let stdout = String::from_utf8_lossy(stdout);
    events.notice(OutputStream::Stdout, &stdout);
    let stderr = String::from_utf8_lossy(stderr);
    events.notice(OutputStream::Stderr, &stderr);
}

struct SandboxProfile {
    rules: Vec<String>,
}

impl SandboxProfile {
    fn for_job(job: &PostinstallWorkerJob, worker_dir: &Path) -> Result<Self> {
        let mut profile = Self { rules: Vec::new() };
        let prefix = job.prefix();
        profile.add_install_hook_rules(prefix, job.network_access_allowed(), worker_dir)?;
        match job {
            PostinstallWorkerJob::Formula { package, .. } => {
                profile.allow_formula_postinstall_paths(prefix, package)?;
            }
            PostinstallWorkerJob::DeferredGlobal { .. } => {
                // Deferred globals are prefix-resource jobs, not formula-keg
                // jobs. Their nominal keys may point at prefix link paths that
                // Seatbelt resolves through symlinks into Cellar (for example
                // GTK 3's immodules.cache under prefix/lib/gtk-3.0 ->
                // Cellar/gtk+3/...). Maintaining per-kind canonical write
                // targets is brittle and would make the global-cache
                // optimization formula-specific. Allow writes anywhere under
                // the glu prefix instead; the explicit denies below still
                // protect glu/Homebrew internals and the caller's home.
                profile.allow_write_path(prefix.0.clone());
            }
        }
        profile.deny_installer_write_paths(prefix);
        profile.deny_toolchain_write_paths(prefix)?;
        Ok(profile)
    }

    fn add_install_hook_rules(
        &mut self,
        prefix: &Prefix,
        network_access_allowed: bool,
        worker_dir: &Path,
    ) -> Result<()> {
        self.allow_write_temp_and_cache(prefix, worker_dir)?;
        self.deny_read_home(prefix)?;
        if !network_access_allowed {
            self.rules.push("(deny network*)".to_string());
        }
        Ok(())
    }

    fn allow_formula_postinstall_paths(
        &mut self,
        prefix: &Prefix,
        package: &ResolvedPackage,
    ) -> Result<()> {
        self.allow_write_path(prefix.0.join("var/glu/logs"));
        self.allow_write_xcode()?;
        self.allow_write_path(prefix.0.join("Cellar").join(&package.name.0));
        self.allow_write_path(prefix.0.join("etc"));
        self.allow_write_path(prefix.0.join("var"));
        self.allow_prefix_link_dirs(prefix)?;
        Ok(())
    }

    fn allow_prefix_link_dirs(&mut self, prefix: &Prefix) -> Result<()> {
        for dir in [
            "bin",
            "etc",
            "include",
            "lib",
            "sbin",
            "share",
            "var",
            "Frameworks",
        ] {
            self.allow_write_path(prefix.0.join(dir));
        }
        Ok(())
    }

    fn allow_write_temp_and_cache(&mut self, prefix: &Prefix, worker_dir: &Path) -> Result<()> {
        for path in [
            PathBuf::from("/private/tmp"),
            PathBuf::from("/tmp"),
            PathBuf::from("/private/var/tmp"),
            PathBuf::from("/var/tmp"),
            env::temp_dir(),
            worker_dir.to_path_buf(),
            prefix.0.join("var/glu/cache"),
        ] {
            self.allow_write_path(path);
        }
        self.allow_write_regex(r#"^/private/var/folders/[^/]+/[^/]+/[C,T]/"#);
        Ok(())
    }

    fn allow_write_xcode(&mut self) -> Result<()> {
        let Some(home) = real_home() else {
            return Ok(());
        };
        self.allow_write_path(home.join("Library/Developer"));
        self.allow_write_path(home.join("Library/Caches/org.swift.swiftpm"));
        Ok(())
    }

    fn deny_read_home(&mut self, prefix: &Prefix) -> Result<()> {
        let Some(home) = real_home() else {
            return Ok(());
        };
        let home = canonicalize_existing(&home).unwrap_or(home);
        let readable_inside_home = prefix.0.starts_with(&home)
            || env::temp_dir().starts_with(&home)
            || home.join("Library/Developer").exists()
            || home.join("Library/Caches/org.swift.swiftpm").exists();
        if readable_inside_home {
            for rel in HOMEBREW_SENSITIVE_HOME_PATHS {
                let path = home.join(rel);
                if path.exists() {
                    self.deny_read_path(canonicalize_existing(&path).unwrap_or(path));
                }
            }
        } else {
            self.deny_read_path(home);
        }
        Ok(())
    }

    // Homebrew 7d2a02d2: Sandbox#deny_write_temp_cellar. Keep broad formula
    // and deferred-cache grants, but never let hooks alter prepared packages
    // or installer control state. Language caches and logs remain writable.
    fn deny_installer_write_paths(&mut self, prefix: &Prefix) {
        self.deny_write_path(prefix.0.join("var/glu"));
        self.allow_write_path(prefix.0.join("var/glu/logs"));
        self.allow_write_path(prefix.0.join("var/glu/cache"));
        self.deny_write_path(prefix.0.join("var/glu/cache/artifacts"));
        self.deny_write_path(prefix.0.join("var/glu/staging"));
        self.deny_write_path(prefix.0.join("glu.json"));

        // Denying descendants alone does not prevent renaming their ancestors.
        // Include both the lexical names (symlinks) and their resolved targets.
        let control = prefix.0.join("var/glu/cache");
        for root in [&control, &sandbox_path(&control)] {
            for ancestor in root.ancestors() {
                self.rules.push(format!(
                    "(deny file-write-unlink (literal \"{}\"))",
                    seatbelt_quote(&sandbox_entry_path(ancestor).to_string_lossy())
                ));
            }
        }
        let cellar = sandbox_path(&prefix.0.join("Cellar"));
        let cellar = regex::escape(&cellar.to_string_lossy()).replace('"', "\\\"");
        // Match future receipts as well as existing ones without a Cellar walk.
        self.deny_write_filter(&format!("(regex #\"^{cellar}/[^/]+/[^/]+/\\.glu(/|$)\")"));
        self.rules.push(format!(
            "(deny file-write-unlink (regex #\"^{cellar}(/[^/]+(/[^/]+)?)?$\"))"
        ));
    }

    fn deny_toolchain_write_paths(&mut self, prefix: &Prefix) -> Result<()> {
        if let Ok(exe) = env::current_exe() {
            self.deny_write_literal(canonicalize_existing(&exe).unwrap_or(exe.clone()));
            if let Some(repo) = dev_repo_from_exe(&exe) {
                self.deny_write_path(repo);
            }
        }
        // Protect glu's installed binary even though prefix/bin is writable for
        // Homebrew parity (`Keg.keg_link_directories`). Deny wins over the broad
        // allow in macOS Seatbelt, matching Homebrew's explicit brew-file deny.
        self.deny_write_literal(prefix.0.join("bin/glu"));

        let mut homebrew_roots = vec![PathBuf::from("/opt/homebrew")];
        if let Some(value) = env::var_os("HOMEBREW_PREFIX") {
            homebrew_roots.push(PathBuf::from(value));
        }
        if let Some(value) = env::var_os("HOMEBREW_REPOSITORY") {
            homebrew_roots.push(PathBuf::from(value));
        }
        homebrew_roots.sort();
        homebrew_roots.dedup();
        for root in homebrew_roots {
            self.deny_write_literal(root.join("bin/brew"));
            self.deny_write_path(root.join("Library"));
            self.deny_write_path(root.join(".git"));
        }
        Ok(())
    }

    fn allow_write_path(&mut self, path: PathBuf) {
        let path = sandbox_path(&path);
        let filter = format!("(subpath \"{}\")", seatbelt_quote(&path.to_string_lossy()));
        self.rules.push(format!("(allow file-write* {filter})"));
        self.rules
            .push(format!("(allow file-write-setugid {filter})"));
        self.rules.push(format!("(allow file-write-mode {filter})"));
    }

    fn allow_write_regex(&mut self, regex: &str) {
        let filter = format!("(regex #\"{}\")", regex.replace('"', "\\\""));
        self.rules.push(format!("(allow file-write* {filter})"));
        self.rules
            .push(format!("(allow file-write-setugid {filter})"));
        self.rules.push(format!("(allow file-write-mode {filter})"));
    }

    fn deny_write_path(&mut self, path: PathBuf) {
        let path = sandbox_path(&path);
        self.deny_write_filter(&format!(
            "(subpath \"{}\")",
            seatbelt_quote(&path.to_string_lossy())
        ));
    }

    fn deny_write_literal(&mut self, path: PathBuf) {
        let path = sandbox_path(&path);
        self.deny_write_filter(&format!(
            "(literal \"{}\")",
            seatbelt_quote(&path.to_string_lossy())
        ));
    }

    fn deny_write_filter(&mut self, filter: &str) {
        // Match the explicit mode/setugid grants too: a file-write* denial
        // alone does not override these more specific Seatbelt operations.
        for operation in ["file-write*", "file-write-mode", "file-write-setugid"] {
            self.rules.push(format!("(deny {operation} {filter})"));
        }
    }

    fn deny_read_path(&mut self, path: PathBuf) {
        let path = sandbox_path(&path);
        self.rules.push(format!(
            "(deny file-read* (subpath \"{}\"))",
            seatbelt_quote(&path.to_string_lossy())
        ));
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("(version 1)\n");
        out.push_str("(debug deny)\n");
        for rule in &self.rules {
            out.push_str(rule);
            out.push('\n');
        }
        out.push_str(
            r#"(allow file-write*
    (literal "/dev/ptmx")
    (literal "/dev/dtracehelper")
    (literal "/dev/null")
    (literal "/dev/random")
    (literal "/dev/zero")
    (regex #"^/dev/fd/[0-9]+$")
    (regex #"^/dev/tty[a-z0-9]*$")
)
(deny file-write*)
(deny file-write-setugid)
(deny file-write-mode)
(allow process-exec (literal "/bin/ps") (with no-sandbox))
(allow default)
"#,
        );
        out
    }
}

impl PostinstallWorkerJob {
    fn prefix(&self) -> &Prefix {
        match self {
            Self::Formula { prefix, .. } | Self::DeferredGlobal { prefix, .. } => prefix,
        }
    }

    fn network_access_allowed(&self) -> bool {
        match self {
            Self::Formula { package, .. } => package.install.postinstall_network_access_allowed,
            // Deferred globals are a glu coalescing deviation. The parent
            // queue passes the restrictive merge of actual contributors:
            // network is allowed only when every contributor allowed it.
            Self::DeferredGlobal {
                network_access_allowed,
                ..
            } => *network_access_allowed,
        }
    }
}

fn real_home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

// Resolve the parent but not the entry itself, to protect a symlink's name
// as well as the target when denying unlink/rename.
fn sandbox_entry_path(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => sandbox_path(parent).join(name),
        _ => path.to_path_buf(),
    }
}

// New prefixes may not have staging or cache directories yet. Resolve the
// existing ancestor so Seatbelt sees the eventual real path, not a symlink alias.
fn sandbox_path(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = fs::canonicalize(ancestor) {
            let suffix = path.strip_prefix(ancestor).unwrap();
            return if suffix.as_os_str().is_empty() {
                real
            } else {
                real.join(suffix)
            };
        }
    }
    path.to_path_buf()
}

fn dev_repo_from_exe(exe: &Path) -> Option<PathBuf> {
    let exe = canonicalize_existing(exe).unwrap_or_else(|| exe.to_path_buf());
    let mut cur = exe.as_path();
    while let Some(parent) = cur.parent() {
        if cur.file_name().is_some_and(|name| name == "target") {
            return Some(parent.to_path_buf());
        }
        cur = parent;
    }
    None
}

fn seatbelt_quote(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

const HOMEBREW_SENSITIVE_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".boto",
    ".docker",
    ".config/fish",
    ".config/gh",
    ".config/gcloud",
    ".config/huggingface",
    ".config/pip",
    ".config/pypoetry",
    ".config/rclone",
    ".config/containers/auth.json",
    ".config/composer/auth.json",
    ".config/sops/age/keys.txt",
    ".gnupg",
    ".git-credentials",
    ".gitconfig",
    ".gsutil",
    ".kube",
    ".netrc",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnpmrc",
    ".bunfig.toml",
    ".pypirc",
    ".pip",
    ".poetry",
    ".local/share/pypoetry",
    ".gem/credentials",
    ".bundle/config",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".composer/auth.json",
    ".condarc",
    ".m2/settings.xml",
    ".gradle/gradle.properties",
    ".sbt/1.0/credentials.sbt",
    ".terraform.d/credentials.tfrc.json",
    ".pulumi/credentials.json",
    ".oci/config",
    ".huggingface/token",
    ".cache/huggingface/token",
    ".claude",
    ".claude.json",
    ".kiro",
    ".bash_login",
    ".bash_logout",
    ".bash_profile",
    ".bashrc",
    ".bash_history",
    ".profile",
    ".zlogin",
    ".zlogout",
    ".zprofile",
    ".zshenv",
    ".zshrc",
    ".zsh_history",
    ".python_history",
    ".mysql_history",
    ".psql_history",
    ".env",
    ".env.local",
    "Documents",
    "Movies",
    "Music",
    "Pictures",
    "Library/Keychains",
    "Library/Mobile Documents",
    "Library/CloudStorage",
    "Dropbox",
    "Google Drive",
    "OneDrive",
];

#[cfg(all(test, target_os = "macos"))]
#[path = "sandbox/integration_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RecordedExecutionEvent, RecordingExecutionEvents};
    use glu_core::{
        ArtifactId, KegVersion, PackageDependency, PackageInstallMetadata, PackageName,
    };

    pub(super) fn package() -> ResolvedPackage {
        ResolvedPackage {
            package_key: glu_core::PackageKey("package:fixture".to_string()),
            name: PackageName("fixture".to_string()),
            aliases: vec![],
            oldnames: vec![],
            version: "1.0".to_string(),
            revision: 0,
            keg_version: KegVersion("1.0".to_string()),
            deps: Vec::<PackageDependency>::new(),
            dependency_requirements: Default::default(),
            exposure: glu_core::Exposure::Global,
            artifact: ArtifactId("art:test:fixture".to_string()),
            install: PackageInstallMetadata {
                opt_names: Vec::new(),
                link_overwrite: vec![],
                post_install_defined: true,
                post_install_steps: vec![],
                postinstall_network_access_allowed: false,
            },
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_protects_installer_state_without_breaking_cache_writes() {
        use std::os::unix::fs::symlink;

        for deferred in [false, true] {
            let tmp = tempfile::TempDir::new().unwrap();
            let real_prefix = tmp.path().join("real prefix (test) [1]");
            fs::create_dir(&real_prefix).unwrap();
            // Exercise canonicalization of a missing protected path beneath a symlink.
            let alias = tmp.path().join("prefix-alias");
            symlink(&real_prefix, &alias).unwrap();
            let prefix = Prefix(alias);
            let job = if deferred {
                PostinstallWorkerJob::DeferredGlobal {
                    prefix: prefix.clone(),
                    kind: "fontconfig_fc_cache".into(),
                    key: vec![],
                    network_access_allowed: false,
                    verbose: false,
                }
            } else {
                PostinstallWorkerJob::Formula {
                    prefix: prefix.clone(),
                    package: Box::new(package()),
                    keg: prefix.0.join("Cellar/fixture/1.0"),
                    plan: PostinstallPlan::default(),
                    verbose: false,
                }
            };
            // Generate before staging/cache directories exist, as on a fresh prefix.
            let profile = SandboxProfile::for_job(&job, tmp.path()).unwrap().render();
            let profile_path = tmp.path().join("test.sb");
            fs::write(&profile_path, profile).unwrap();
            let run = |script: &str, args: &[&Path]| {
                let output = Command::new("/usr/bin/sandbox-exec")
                    .args(["-f"])
                    .arg(&profile_path)
                    .args(["/bin/sh", "-c", script, "sandbox-test"])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "deferred={deferred}: {script} {args:?}\n{}\n{}",
                    String::from_utf8_lossy(&output.stderr),
                    fs::read_to_string(&profile_path).unwrap()
                );
            };

            let protected = [
                "var/glu/staging/other/1.0/bin/tool",
                "var/glu/cache/artifacts/sha256/bottle",
                "var/glu/operations.lock",
                "var/glu/link-overwrite-backups/other/bin/tool",
                "glu.json",
                "Cellar/fixture/1.0/.glu/receipt.json",
                "Cellar/other/1.0/.glu/receipt.json",
            ];
            for rel in protected {
                let path = real_prefix.join(rel);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"trusted").unwrap();
                run("if printf tampered > \"$1\"; then exit 1; fi", &[&path]);
                run("if /bin/rm \"$1\"; then exit 1; fi", &[&path]);
                run("if /bin/chmod 777 \"$1\"; then exit 1; fi", &[&path]);
                let replacement = tmp.path().join("replacement");
                fs::write(&replacement, b"untrusted").unwrap();
                run(
                    "if /bin/mv -f \"$1\" \"$2\"; then exit 1; fi",
                    &[&replacement, &path],
                );
                let hardlink = tmp.path().join("hardlink");
                run(
                    "if /bin/ln \"$1\" \"$2\"; then exit 1; fi",
                    &[&path, &hardlink],
                );
                assert_eq!(fs::read(&path).unwrap(), b"trusted");
            }
            for rel in [
                "var/glu/staging",
                "var/glu/cache",
                "var/glu",
                "var",
                "Cellar/fixture/1.0",
                "Cellar/fixture",
                "Cellar",
                "",
            ] {
                let path = real_prefix.join(rel);
                let replacement = path.with_extension("moved");
                run(
                    "if /bin/mv \"$1\" \"$2\"; then exit 1; fi",
                    &[&path, &replacement],
                );
                assert!(path.is_dir(), "{rel} was moved");
            }
            run(
                "if /bin/mv \"$1\" \"$2\"; then exit 1; fi",
                &[&prefix.0, &tmp.path().join("moved-alias")],
            );
            assert!(prefix.0.is_symlink());
            let staging = real_prefix.join("var/glu/staging");
            let staging_link = real_prefix.join("share/staging-alias");
            fs::create_dir_all(staging_link.parent().unwrap()).unwrap();
            symlink(&staging, &staging_link).unwrap();
            let new_staged_file = staging_link.join("planted");
            run(
                "if /usr/bin/touch \"$1\"; then exit 1; fi",
                &[&new_staged_file],
            );
            assert!(!new_staged_file.exists());

            for rel in [
                "var/glu/logs",
                "var/glu/cache/npm_cache",
                "var/glu/cache/uv_cache",
                "var/glu/cache/cargo_cache",
                "etc/fixture",
                "var/fixture",
                "share/glib-2.0/schemas",
                "Cellar/fixture/1.0/lib",
            ] {
                let dir = real_prefix.join(rel);
                run(
                    "/bin/mkdir -p \"$1\" && printf cache > \"$1/value\"",
                    &[&dir],
                );
                assert_eq!(fs::read(dir.join("value")).unwrap(), b"cache");
            }
            // The reason deferred jobs retain broad prefix access: a public cache
            // path can resolve into a different installed package's keg.
            if deferred {
                let target = real_prefix.join("Cellar/other/1.0/lib/gtk-3.0");
                fs::create_dir_all(&target).unwrap();
                let link = real_prefix.join("lib/gtk-3.0");
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(&target, &link).unwrap();
                run("printf cache > \"$1/immodules.cache\"", &[&link]);
                assert_eq!(fs::read(target.join("immodules.cache")).unwrap(), b"cache");
            }
        }
    }

    #[test]
    fn worker_output_is_relayed_through_typed_notices() {
        let events = RecordingExecutionEvents::default();
        relay_worker_output(&events, b"worker output\n", b"worker warning\n");

        let recorded = events.events();
        assert!(matches!(
            &recorded[0],
            RecordedExecutionEvent::Notice { stream: OutputStream::Stdout, message }
                if message == "worker output\n"
        ));
        assert!(matches!(
            &recorded[1],
            RecordedExecutionEvent::Notice { stream: OutputStream::Stderr, message }
                if message == "worker warning\n"
        ));
    }

    #[test]
    fn profile_denies_network_when_formula_policy_denies_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let job = PostinstallWorkerJob::Formula {
            prefix,
            package: Box::new(package()),
            keg: tmp.path().join("prefix/Cellar/fixture/1.0"),
            plan: PostinstallPlan::default(),
            verbose: false,
        };
        let profile = SandboxProfile::for_job(&job, tmp.path()).unwrap().render();
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("bin/glu"));
        assert!(profile.contains("bin/brew"));
    }

    #[test]
    fn profile_uses_deferred_global_network_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prefix = Prefix(tmp.path().join("prefix"));
        let allowed = PostinstallWorkerJob::DeferredGlobal {
            prefix: prefix.clone(),
            kind: "fontconfig_fc_cache".to_string(),
            key: vec![],
            network_access_allowed: true,
            verbose: false,
        };
        let denied = PostinstallWorkerJob::DeferredGlobal {
            prefix: prefix.clone(),
            kind: "fontconfig_fc_cache".to_string(),
            key: vec![],
            network_access_allowed: false,
            verbose: false,
        };

        let allowed_profile = SandboxProfile::for_job(&allowed, tmp.path())
            .unwrap()
            .render();
        let denied_profile = SandboxProfile::for_job(&denied, tmp.path())
            .unwrap()
            .render();

        assert!(!allowed_profile.contains("(deny network*)"));
        assert!(denied_profile.contains("(deny network*)"));
        assert!(allowed_profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            sandbox_path(&prefix.0).display()
        )));
    }
}
