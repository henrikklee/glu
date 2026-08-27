use anyhow::{bail, Result};
use glu_core::{Prefix, Target};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub prefix: Prefix,
    pub target: Target,
    pub registry_base_url: String,
    /// Distribution root for the glu client itself (`glu upgrade`), from
    /// `GLU_BASE_URL` or the default. Same env the installer uses, so a
    /// mirror override applies to both.
    pub distribution_base_url: String,
}

impl ClientConfig {
    pub fn default_for_host() -> Self {
        Self {
            prefix: Prefix(resolve_prefix(
                env_prefix_override().map(PathBuf::from),
                self_located_prefix(),
            )),
            target: Target(default_target()),
            registry_base_url: registry_base_url(),
            distribution_base_url: distribution_base_url(),
        }
    }
}

/// `GLU_PREFIX`, read only in dev builds. Published binaries support only the
/// default prefix (`/opt/glustore`) and must not even observe the override —
/// the env read is compiled out under `default` features (see
/// `registry_base_url` below for the policy and `resolve_prefix` for what the
/// override is for).
fn env_prefix_override() -> Option<std::ffi::OsString> {
    #[cfg(feature = "dev-registry")]
    {
        env::var_os("GLU_PREFIX").filter(|value| !value.is_empty())
    }

    #[cfg(not(feature = "dev-registry"))]
    {
        None
    }
}

/// The registry the client talks to — build-time policy, not runtime config.
///
/// - Published build (`cargo build --release`, `default` features): the fixed
///   official HTTPS origin (`DEFAULT_REGISTRY_BASE_URL`). `GLU_REGISTRY` does
///   not exist in this binary; no HTTP, no mirrors.
/// - Dev build (`cargo build-dev`): `GLU_REGISTRY` overrides it, defaulting to
///   the local dev registry. The alias writes to `target/dev-registry`.
///
/// `--release` is an optimization profile; `dev-registry` is a capability
/// switch. Keeping them orthogonal means local optimized/benchmark builds stay
/// available without accidentally enabling development policy, while the
/// published build structurally lacks the override paths.
fn registry_base_url() -> String {
    #[cfg(feature = "dev-registry")]
    {
        env::var("GLU_REGISTRY")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "http://localhost:3000".to_string())
    }

    #[cfg(not(feature = "dev-registry"))]
    {
        glu_core::DEFAULT_REGISTRY_BASE_URL.to_string()
    }
}

/// Distribution root for `glu upgrade` / the installer: fixed in published
/// builds, `GLU_BASE_URL`-overridable in dev builds (same env the installer
/// uses, so a mirror override applies to both).
fn distribution_base_url() -> String {
    #[cfg(feature = "dev-registry")]
    {
        env::var("GLU_BASE_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| glu_core::DEFAULT_DISTRIBUTION_BASE_URL.to_string())
    }

    #[cfg(not(feature = "dev-registry"))]
    {
        glu_core::DEFAULT_DISTRIBUTION_BASE_URL.to_string()
    }
}

/// Central transport policy for registry URLs — the single place that decides
/// which origins the client will talk to. HTTPS always. Dev builds
/// (`dev-registry`) additionally allow plain HTTP to loopback addresses so the
/// local dev registry (`http://localhost:3000`) stays usable; everything else
/// (a dev build pointing at `http://evil.example`, any `file://` origin) is
/// rejected. Enforced at every `HttpResolveClient` construction; `new` is the
/// only trusted entry point for registry base URLs.
pub fn validate_registry_url(url: &url::Url) -> Result<()> {
    if url.scheme() == "https" {
        return Ok(());
    }

    #[cfg(feature = "dev-registry")]
    if url.scheme() == "http" && host_is_loopback(url) {
        return Ok(());
    }

    bail!("registry URL must use HTTPS (got {url})")
}

/// Loopback host check for the dev-HTTP allowance (`localhost`, `127.0.0.1`,
/// any loopback IPv4/IPv6).
#[cfg(feature = "dev-registry")]
fn host_is_loopback(url: &url::Url) -> bool {
    use url::Host;
    match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost" || domain == "127.0.0.1",
        Some(Host::Ipv4(addr)) => addr.is_loopback(),
        Some(Host::Ipv6(addr)) => addr.is_loopback(),
        _ => false,
    }
}

/// Where the resolved prefix came from. `Flag` is layered on top by the CLI
/// Resolution source for a prefix, reported by `glu status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixSource {
    Env,
    SelfLocated,
    Default,
}

/// Resolution source for the current host.
/// (main.rs layers that on and reports `Flag` itself).
pub fn host_prefix_source() -> PrefixSource {
    prefix_source(env_prefix_override().as_deref())
}

fn prefix_source(env_prefix: Option<&std::ffi::OsStr>) -> PrefixSource {
    match env_prefix {
        Some(value) if !value.is_empty() => PrefixSource::Env,
        _ if self_located_prefix().is_some() => PrefixSource::SelfLocated,
        _ => PrefixSource::Default,
    }
}

/// Prefix resolution, in order:
///
/// 1. `GLU_PREFIX` env — a *manual* per-invocation override, used verbatim
///    (no canonicalization). Dev builds only: the env read is compiled out of
///    published binaries, which support the default prefix alone. It is never
///    set by the product: `glu shellenv` emits PATH only, so the shell
///    integration carries the prefix by location.
/// 2. Self-location — the binary's own canonical path, when it lives in the
///    standard `<prefix>/bin/glu` layout. This makes the rc hook and every
///    later `glu` invocation resolve the same prefix without any state.
/// 3. `/opt/glustore` — the official default.
///
/// Officially supported prefixes must be the same byte length as
/// `/opt/homebrew` (13) because fixed-cellar bottle relocation is a
/// length-preserving rewrite; `validation::validate_fixed_cellar_prefix`
/// enforces this at install time. `GLU_PREFIX` is a dev/testing escape hatch
/// (e.g. `/tmp/glustore`, also 13 bytes), not an officially supported feature.
fn resolve_prefix(env_override: Option<PathBuf>, self_located: Option<PathBuf>) -> PathBuf {
    env_override
        .filter(|p| !p.as_os_str().is_empty())
        .or(self_located)
        .unwrap_or_else(|| PathBuf::from("/opt/glustore"))
}

fn self_located_prefix() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|exe| self_located_prefix_from(&exe))
}

/// Derive the prefix from a binary path in the standard `<prefix>/bin/glu`
/// layout. `pub(crate)` for `upgrade`, which swaps the running binary and
/// must refuse dev trees that don't match this shape.
pub(crate) fn self_located_prefix_from(exe: &Path) -> Option<PathBuf> {
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let exe = normalize_private_macos(exe);
    // Only the standard layout `<prefix>/bin/glu` self-locates. Dev trees
    // (e.g. target/dev-registry/release/glu) and symlinked copies elsewhere fall through
    // to the default.
    if exe.file_name()?.to_str()? != "glu" {
        return None;
    }
    if exe.parent()?.file_name()?.to_str()? != "bin" {
        return None;
    }
    Some(exe.parent()?.parent()?.to_path_buf())
}

/// macOS reports canonical paths under `/private/tmp` and `/private/var`
/// (`/tmp` and `/var` are symlinks). Stripping the `/private` prefix keeps
/// dev prefixes like `/tmp/glustore` at their intended 13-byte length so the
/// fixed-cellar relocation invariant holds.
pub(crate) fn normalize_private_macos(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("/private/tmp") {
        PathBuf::from(format!("/tmp{rest}"))
    } else if let Some(rest) = s.strip_prefix("/private/var") {
        PathBuf::from(format!("/var{rest}"))
    } else {
        path
    }
}

fn default_target() -> String {
    if std::env::consts::OS == "macos" {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        if let Some(codename) = macos_codename() {
            return format!("{arch}_{codename}");
        }
    }
    format!("{}_{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn macos_codename() -> Option<&'static str> {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let major = version.trim().split('.').next()?.parse::<u32>().ok()?;
    match major {
        26 => Some("tahoe"),
        15 => Some("sequoia"),
        14 => Some("sonoma"),
        13 => Some("ventura"),
        12 => Some("monterey"),
        11 => Some("big_sur"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefix_env_wins_verbatim() {
        let prefix = resolve_prefix(Some(PathBuf::from("/tmp/glustore")), None);
        assert_eq!(prefix, PathBuf::from("/tmp/glustore"));
        // Env is used verbatim, never canonicalized or normalized.
        let prefix = resolve_prefix(
            Some(PathBuf::from("~/.glu")),
            Some(PathBuf::from("/opt/glustore")),
        );
        assert_eq!(prefix, PathBuf::from("~/.glu"));
    }

    #[test]
    fn resolve_prefix_empty_env_is_ignored() {
        let prefix = resolve_prefix(
            Some(PathBuf::from("")),
            Some(PathBuf::from("/opt/glustore")),
        );
        assert_eq!(prefix, PathBuf::from("/opt/glustore"));
    }

    #[test]
    fn resolve_prefix_self_location_falls_back_to_default() {
        let prefix = resolve_prefix(None, Some(PathBuf::from("/opt/glustore")));
        assert_eq!(prefix, PathBuf::from("/opt/glustore"));
        assert_eq!(resolve_prefix(None, None), PathBuf::from("/opt/glustore"));
    }

    #[test]
    fn self_location_requires_bin_glu_shape() {
        assert_eq!(
            self_located_prefix_from(Path::new("/tmp/glustore/bin/glu")),
            Some(PathBuf::from("/tmp/glustore"))
        );
        // Not named glu.
        assert_eq!(
            self_located_prefix_from(Path::new("/opt/glustore/bin/glu2")),
            None
        );
        // Not under bin/.
        assert_eq!(
            self_located_prefix_from(Path::new("/opt/glustore/glu")),
            None
        );
        // Dev tree.
        assert_eq!(
            self_located_prefix_from(Path::new("/work/glu/target/release/glu")),
            None
        );
    }

    #[test]
    fn self_location_resolves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("glustore/bin/glu");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        // Symlink at the standard layout pointing elsewhere: canonicalize
        // resolves to the real location, so the prefix is the real prefix,
        // not the symlink's parent (a `~/bin/glu -> /opt/glustore/bin/glu`
        // alias must resolve to /opt/glustore, not ~/bin).
        let prefix = dir.path().join("prefix");
        let link = prefix.join("bin/glu");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            self_located_prefix_from(&link),
            Some(dir.path().join("glustore"))
        );
    }

    #[test]
    fn normalize_private_macos_keeps_dev_prefix_length() {
        assert_eq!(
            normalize_private_macos(PathBuf::from("/private/tmp/glustore/bin/glu")),
            PathBuf::from("/tmp/glustore/bin/glu")
        );
        assert_eq!(
            normalize_private_macos(PathBuf::from("/private/var/folders/x/bin/glu")),
            PathBuf::from("/var/folders/x/bin/glu")
        );
        // Unrelated paths are untouched.
        assert_eq!(
            normalize_private_macos(PathBuf::from("/opt/glustore/bin/glu")),
            PathBuf::from("/opt/glustore/bin/glu")
        );
    }
}

#[test]
fn prefix_source_detects_env_and_default() {
    assert_eq!(
        prefix_source(Some(std::ffi::OsStr::new("/tmp/glustore"))),
        PrefixSource::Env
    );
    // Empty env is ignored; in tests current_exe is the test harness
    // binary (not a <prefix>/bin/glu layout), so it falls to Default.
    assert_eq!(
        prefix_source(Some(std::ffi::OsStr::new(""))),
        PrefixSource::Default
    );
    assert_eq!(prefix_source(None), PrefixSource::Default);
}

/// Policy tests: published (default-features) behavior. Compiled and run only
/// in a default-features test build; the dev counterparts live in
/// `dev_registry_policy_tests` below.
#[cfg(all(test, not(feature = "dev-registry")))]
mod production_policy_tests {
    use super::*;

    #[test]
    fn published_defaults_to_fixed_https_registry() {
        let url = registry_base_url();
        assert_eq!(url, glu_core::DEFAULT_REGISTRY_BASE_URL);
        assert!(url.starts_with("https://"));
        // The dev default must not leak into a published build.
        assert!(!url.contains("localhost"));
    }

    #[test]
    fn published_has_no_dev_env_override_paths() {
        // The compile-time guarantee these env reads are gone: every value
        // that could have flowed through them is now fixed.
        assert_eq!(env_prefix_override(), None);
        assert_eq!(registry_base_url(), glu_core::DEFAULT_REGISTRY_BASE_URL);
        assert_eq!(
            distribution_base_url(),
            glu_core::DEFAULT_DISTRIBUTION_BASE_URL
        );
    }

    #[test]
    fn transport_policy_rejects_every_non_https_origin() {
        for bad in [
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "http://registry.example",
            "file:///tmp/registry",
            "ftp://registry.example",
        ] {
            let url = url::Url::parse(bad).unwrap();
            assert!(
                validate_registry_url(&url).is_err(),
                "{bad} must be rejected in a published build"
            );
        }
        for good in [
            "https://registry.glu.run",
            "https://registry.example:443/v1",
        ] {
            let url = url::Url::parse(good).unwrap();
            assert!(
                validate_registry_url(&url).is_ok(),
                "{good} must be accepted"
            );
        }
    }
}

/// Dev-build policy (`--features dev-registry`): env overrides and the
/// loopback-HTTP allowance. Compiled and run only with the feature enabled.
#[cfg(all(test, feature = "dev-registry"))]
mod dev_registry_policy_tests {
    use super::*;

    fn with_env<F: FnOnce()>(key: &str, value: &str, f: F) {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        f();
        match old {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn glu_registry_overrides_and_defaults_to_localhost() {
        with_env("GLU_REGISTRY", "http://localhost:4000", || {
            assert_eq!(registry_base_url(), "http://localhost:4000");
        });
        with_env("GLU_REGISTRY", "", || {
            assert_eq!(registry_base_url(), "http://localhost:3000");
        });
    }

    #[test]
    fn dev_transport_policy_allows_loopback_http_only() {
        for good in [
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            let url = url::Url::parse(good).unwrap();
            assert!(
                validate_registry_url(&url).is_ok(),
                "{good} must be allowed in dev builds"
            );
        }
        for bad in ["http://registry.example", "http://192.168.1.10:3000"] {
            let url = url::Url::parse(bad).unwrap();
            assert!(
                validate_registry_url(&url).is_err(),
                "{bad} must be rejected"
            );
        }
    }
}
