use anyhow::{Context, Result};
use glu_core::Prefix;
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

const SYSTEM_POSTINSTALL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Clone, Debug)]
pub(super) struct PostinstallEnvSnapshot {
    pub(super) home: PathBuf,
    pub(super) temp: PathBuf,
    pub(super) path: OsString,
    pub(super) user: String,
    pub(super) vars: Vec<(OsString, OsString)>,
}

pub(super) struct PostinstallEnv {
    pub(super) snapshot: PostinstallEnvSnapshot,
    _home: tempfile::TempDir,
    _temp: tempfile::TempDir,
}

pub(super) fn system_prefixed_postinstall_path() -> OsString {
    let mut paths = Vec::new();
    for path in env::split_paths(OsStr::new(SYSTEM_POSTINSTALL_PATH)) {
        push_unique_path(&mut paths, path);
    }
    if let Some(user_path) = env::var_os("PATH") {
        for path in env::split_paths(&user_path) {
            push_unique_path(&mut paths, path);
        }
    }
    env::join_paths(paths).unwrap_or_else(|_| OsString::from(SYSTEM_POSTINSTALL_PATH))
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

impl PostinstallEnv {
    pub(super) fn new(prefix: &Prefix) -> Result<Self> {
        let home = tempfile::Builder::new()
            .prefix("glu-postinstall-home-")
            .tempdir()
            .context("creating postinstall HOME")?;
        let temp = tempfile::Builder::new()
            .prefix("glu-postinstall-temp-")
            .tempdir()
            .context("creating postinstall TMPDIR")?;
        let path = system_prefixed_postinstall_path();
        let user = env::var("USER").unwrap_or_default();
        let home_path = home.path().to_path_buf();
        let temp_path = temp.path().to_path_buf();
        let mut vars = env::vars_os()
            .filter(|(key, _)| !is_sensitive_env_key(key))
            .collect::<Vec<_>>();
        remove_env(&mut vars, "HOMEBREW_PATH");
        upsert_env(&mut vars, "HOME", home_path.as_os_str().to_os_string());
        upsert_env(&mut vars, "TMPDIR", temp_path.as_os_str().to_os_string());
        upsert_env(&mut vars, "TEMP", temp_path.as_os_str().to_os_string());
        upsert_env(&mut vars, "TMP", temp_path.as_os_str().to_os_string());
        upsert_env(&mut vars, "PATH", path.clone());
        upsert_env(&mut vars, "USER", OsString::from(&user));
        upsert_common_sandbox_env(prefix, &home_path, &temp_path, &mut vars);
        fs::write(
            home_path.join(".bazelrc"),
            format!(
                "startup --output_user_root={}/_bazel\n",
                home_path.display()
            ),
        )
        .context("creating postinstall HOME bazel config")?;
        Ok(Self {
            snapshot: PostinstallEnvSnapshot {
                home: home_path,
                temp: temp_path,
                path,
                user,
                vars,
            },
            _home: home,
            _temp: temp,
        })
    }
}

fn is_sensitive_env_key(key: &OsString) -> bool {
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

// Homebrew 7d2a02d2: extend/ENV/sensitive.rb — clear any environment key
// matching /(cookie|key|token|password|passphrase|auth)/i before formula
// evaluation/postinstall. This is broader than just dynamic-loader hygiene and
// preserves Homebrew compatibility better than an invented allowlist.
fn homebrew_sensitive_env_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["cookie", "key", "token", "password", "passphrase", "auth"]
        .iter()
        .any(|needle| lower.contains(needle))
}

// Homebrew 7d2a02d2: Formula#common_sandbox_env + PackageManagerCache.env.
// Cache paths are glu-owned; Java's explicit temporary directory is retained.
fn upsert_common_sandbox_env(
    prefix: &Prefix,
    home: &Path,
    temp: &Path,
    vars: &mut Vec<(OsString, OsString)>,
) {
    let cache = prefix.0.join("var/glu/cache");
    let curl_home = env::var_os("CURL_HOME").unwrap_or_else(|| home.as_os_str().to_os_string());
    for (key, value) in [
        (
            "_JAVA_OPTIONS",
            format!(
                "-Duser.home={}/java_cache -Djava.io.tmpdir={}",
                cache.display(),
                temp.display()
            ),
        ),
        ("BUNDLE_GLOBAL_GEM_CACHE", "true".to_string()),
        (
            "BUNDLE_USER_CACHE",
            format!("{}/bundler_cache", cache.display()),
        ),
        ("CABAL_DIR", format!("{}/cabal_cache", cache.display())),
        (
            "COMPOSER_CACHE_DIR",
            format!("{}/composer_cache", cache.display()),
        ),
        ("GEM_SPEC_CACHE", format!("{}/gem_cache", cache.display())),
        ("GOCACHE", format!("{}/go_cache", cache.display())),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("GOENV", "off".to_string()),
        ("GOPATH", format!("{}/go_mod_cache", cache.display())),
        ("CARGO_HOME", format!("{}/cargo_cache", cache.display())),
        ("HEX_HOME", format!("{}/hex_cache", cache.display())),
        ("NPM_CONFIG_CACHE", format!("{}/npm_cache", cache.display())),
        (
            "NPM_CONFIG_STORE_DIR",
            format!("{}/pnpm_cache", cache.display()),
        ),
        ("NUGET_PACKAGES", format!("{}/nuget_cache", cache.display())),
        ("UV_CACHE_DIR", format!("{}/uv_cache", cache.display())),
        (
            "YARN_CACHE_FOLDER",
            format!("{}/yarn_cache", cache.display()),
        ),
        (
            "ZIG_GLOBAL_CACHE_DIR",
            format!("{}/zig_cache", cache.display()),
        ),
        ("BUNDLE_COOLDOWN", "1".to_string()),
        ("PIP_CACHE_DIR", format!("{}/pip_cache", cache.display())),
        ("PIP_CONFIG_FILE", "/dev/null".to_string()),
        ("NPM_CONFIG_USERCONFIG", "/dev/null".to_string()),
        ("PYTHONDONTWRITEBYTECODE", "1".to_string()),
        ("XDG_CONFIG_HOME", format!("{}/.config", home.display())),
    ] {
        upsert_env(vars, key, OsString::from(value));
    }
    upsert_env(vars, "CURL_HOME", curl_home);
}

fn upsert_env(vars: &mut Vec<(OsString, OsString)>, key: &str, value: OsString) {
    let key_os = OsString::from(key);
    if let Some((_, existing)) = vars.iter_mut().find(|(candidate, _)| candidate == &key_os) {
        *existing = value;
    } else {
        vars.push((key_os, value));
    }
}

fn remove_env(vars: &mut Vec<(OsString, OsString)>, key: &str) {
    let key = OsString::from(key);
    vars.retain(|(candidate, _)| candidate != &key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_manager_caches_override_inherited_locations() {
        let prefix = Prefix(PathBuf::from("/opt/glustore"));
        let caches = [
            ("BUNDLE_USER_CACHE", "bundler_cache"),
            ("CABAL_DIR", "cabal_cache"),
            ("CARGO_HOME", "cargo_cache"),
            ("COMPOSER_CACHE_DIR", "composer_cache"),
            ("GEM_SPEC_CACHE", "gem_cache"),
            ("GOCACHE", "go_cache"),
            ("GOPATH", "go_mod_cache"),
            ("HEX_HOME", "hex_cache"),
            ("NPM_CONFIG_CACHE", "npm_cache"),
            ("NPM_CONFIG_STORE_DIR", "pnpm_cache"),
            ("NUGET_PACKAGES", "nuget_cache"),
            ("PIP_CACHE_DIR", "pip_cache"),
            ("UV_CACHE_DIR", "uv_cache"),
            ("YARN_CACHE_FOLDER", "yarn_cache"),
            ("ZIG_GLOBAL_CACHE_DIR", "zig_cache"),
        ];
        let mut vars = caches
            .iter()
            .map(|(key, _)| (OsString::from(key), OsString::from("/caller/cache")))
            .collect::<Vec<_>>();
        vars.push(("BUNDLE_GLOBAL_GEM_CACHE".into(), "false".into()));
        upsert_common_sandbox_env(
            &prefix,
            Path::new("/tmp/home"),
            Path::new("/tmp/temp"),
            &mut vars,
        );
        for (key, dir) in caches {
            let matches = vars.iter().filter(|(k, _)| k == key).collect::<Vec<_>>();
            assert_eq!(matches.len(), 1, "duplicate {key}");
            assert_eq!(
                matches[0].1,
                prefix.0.join("var/glu/cache").join(dir).as_os_str(),
                "{key}"
            );
        }
        assert!(vars.contains(&("BUNDLE_GLOBAL_GEM_CACHE".into(), "true".into())));
        assert!(vars.contains(&(
            "_JAVA_OPTIONS".into(),
            "-Duser.home=/opt/glustore/var/glu/cache/java_cache -Djava.io.tmpdir=/tmp/temp".into()
        )));
    }

    #[test]
    fn postinstall_path_prefers_system_tools_before_user_path() {
        let path = system_prefixed_postinstall_path();
        let parts = env::split_paths(&path).take(4).collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/usr/sbin"),
                PathBuf::from("/sbin"),
            ]
        );
    }
}
