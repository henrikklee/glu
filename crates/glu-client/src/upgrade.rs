//! `glu upgrade` — self-update of the glu client.
//!
//! Version discovery comes from the registry (the same latest-version header
//! the outdated notification uses), so "already up to date" never downloads
//! the binary. When newer, the exact release is fetched from the
//! distribution, checksum-verified, sanity-checked, and swapped in atomically.

use crate::config::ClientConfig;
use crate::events::{ExecutionEvents, ProgressEvent};
use crate::hash::sha256_hex;
use crate::registry::resolve_client::HttpResolveClient;
use anyhow::{bail, Context, Result};
use glu_core::DISTRIBUTION_TARGET;
use semver::Version;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeStatus {
    AlreadyCurrent,
    Updated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeResult {
    pub previous_version: String,
    pub version: String,
    pub status: UpgradeStatus,
}

pub async fn upgrade(config: &ClientConfig, events: &dyn ExecutionEvents) -> Result<UpgradeResult> {
    let own = env!("CARGO_PKG_VERSION");
    let registry = HttpResolveClient::new(&config.registry_base_url)?;
    let (_, latest_glu_version) = registry
        .outdated(&[], &config.target)
        .await
        .context("failed to check for glu updates against the registry")?;
    let Some(latest) = latest_glu_version else {
        bail!(
            "the registry does not report a latest glu version (GLU_LATEST_GLU_VERSION is not set); \
             cannot determine whether an update exists"
        );
    };
    let latest = latest.trim().to_string();
    if !newer_than(&latest, own)? {
        return Ok(UpgradeResult {
            previous_version: own.to_string(),
            version: own.to_string(),
            status: UpgradeStatus::AlreadyCurrent,
        });
    }

    // The swap target is the running binary's own location, never
    // config.prefix: an env/CLI prefix override must not clobber a
    // different install, and dev trees must not be "upgraded".
    let bin = running_bin()?;

    let release_tag = release_tag(&latest)?;
    let asset = format!("glu-{DISTRIBUTION_TARGET}.tar.gz");
    let asset_url = format!(
        "{}/download/{release_tag}/{asset}",
        config.distribution_base_url
    );
    let checksum_url = format!("{asset_url}.sha256");
    let http = http_client()?;

    events.progress(ProgressEvent::UpgradeDownloadStarted {
        version: latest.clone(),
    });
    let asset_bytes = fetch_bytes(&http, &asset_url).await?;
    let checksum = fetch_text(&http, &checksum_url).await?;
    let expected = checksum
        .split_whitespace()
        .next()
        .context("empty checksum sidecar")?;
    validate_hex(expected)?;
    let actual = sha256_hex(&asset_bytes);
    if actual != expected {
        bail!("checksum mismatch for {asset} (expected {expected}, got {actual})");
    }

    let tmp = tempfile::tempdir().context("failed to create a temp dir")?;
    let asset_path = tmp.path().join(&asset);
    fs::write(&asset_path, &asset_bytes).context("failed to write the downloaded asset")?;
    let file = fs::File::open(&asset_path).context("failed to open the downloaded asset")?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive
        .unpack(tmp.path())
        .context("failed to extract the asset")?;
    let new_bin = tmp.path().join("glu");
    if !new_bin.is_file() {
        bail!("{asset} did not contain a glu binary at its root");
    }

    // Pre-swap sanity: the downloaded binary must run and report the
    // registry-confirmed version. Verification, not discovery — the
    // "already up to date" path above never downloads.
    let out = Command::new(&new_bin)
        .arg("--version")
        .output()
        .context("downloaded glu does not run")?;
    if !out.status.success() {
        bail!(
            "downloaded glu failed to run: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let reported = String::from_utf8_lossy(&out.stdout);
    if !reported.contains(&latest) {
        bail!(
            "downloaded glu reports {reported:?} but the registry says {latest} is latest; \
             distribution and registry are out of sync"
        );
    }

    swap_binary(&bin, &new_bin)?;
    Ok(UpgradeResult {
        previous_version: own.to_string(),
        version: latest,
        status: UpgradeStatus::Updated,
    })
}

/// True when `latest` is a newer semver than `own`. A malformed registry
/// version fails closed (an explicit `upgrade` should not silently no-op on
/// a broken registry).
fn newer_than(latest: &str, own: &str) -> Result<bool> {
    let latest = Version::parse(latest)
        .with_context(|| format!("registry reported an invalid glu version: {latest:?}"))?;
    let own = Version::parse(own).context("own version is not valid semver")?;
    Ok(own < latest)
}

/// Registry versions are plain semver; GitHub release tags are v-prefixed.
fn release_tag(version: &str) -> Result<String> {
    Version::parse(version)
        .with_context(|| format!("registry reported an invalid glu version: {version:?}"))?;
    Ok(format!("v{version}"))
}

/// The binary `glu upgrade` manages: the running binary itself, in the
/// standard `<prefix>/bin/glu` layout. Dev trees (e.g.
/// target/dev-registry/release/glu) don't match and are refused.
fn running_bin() -> Result<PathBuf> {
    let exe = env::current_exe().context("cannot locate the running glu binary")?;
    let prefix = crate::config::self_located_prefix_from(&exe).context(
        "glu upgrade manages installed binaries; this looks like a dev build \
         (build with `cargo build --release` instead)",
    )?;
    Ok(prefix.join("bin/glu"))
}

fn swap_binary(bin: &Path, new_bin: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = bin
        .parent()
        .context("installed binary has no parent directory")?;
    let bytes = fs::read(new_bin).context("reading staged binary")?;
    // Create the staged file with O_EXCL so a pre-created symlink at the
    // pid-based name can't redirect the write;
    // retry a few names on collision.
    let staged = stage_with_create_new(dir, &bytes)?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
        .context("failed to set permissions on the staged binary")?;
    fs::rename(&staged, bin).with_context(|| format!("failed to replace {}", bin.display()))?;
    Ok(())
}

/// Writes `bytes` to a fresh file in `dir` via `create_new` (O_EXCL), so a
/// symlink or file an attacker pre-placed at the predictable name fails the
/// open instead of being written through.
fn stage_with_create_new(dir: &Path, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let candidate = dir.join(format!(".glu.upgrade.{}-{}.tmp", std::process::id(), nanos));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(bytes)
                    .with_context(|| format!("writing staged binary {}", candidate.display()))?;
                file.sync_all()
                    .with_context(|| format!("syncing staged binary {}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(_) => continue, // collision — vanishingly rare; try another name
        }
    }
    bail!("could not stage the new glu binary (too many name collisions)")
}

fn validate_hex(hash: &str) -> Result<()> {
    if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("malformed checksum: {hash:?}");
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("failed to build HTTP client"))
}

async fn fetch_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    if !response.status().is_success() {
        bail!("failed to fetch {url}: {}", response.status());
    }
    response
        .bytes()
        .await
        .with_context(|| format!("failed to read response from {url}"))
        .map(|bytes| bytes.to_vec())
}

async fn fetch_text(http: &reqwest::Client, url: &str) -> Result<String> {
    let bytes = fetch_bytes(http, url).await?;
    String::from_utf8(bytes).with_context(|| format!("response from {url} is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_than_compares_semver() {
        assert!(newer_than("0.2.0", "0.1.0").unwrap());
        assert!(!newer_than("0.1.0", "0.1.0").unwrap());
        assert!(!newer_than("0.1.0", "0.2.0").unwrap());
        assert!(newer_than("0.2.0-rc.1", "0.1.0").unwrap());
    }

    #[test]
    fn newer_than_fails_closed_on_malformed_registry_version() {
        let err = newer_than("not-a-version", "0.1.0").unwrap_err();
        assert!(err.to_string().contains("invalid glu version"));
    }

    #[test]
    fn release_tag_prefixes_registry_semver() {
        assert_eq!(release_tag("0.1.0").unwrap(), "v0.1.0");
        assert_eq!(release_tag("0.2.0-rc.1").unwrap(), "v0.2.0-rc.1");
        assert!(release_tag("v0.1.0").is_err());
    }

    #[test]
    fn running_bin_refuses_dev_tree() {
        // In tests, current_exe is the test harness binary, which is not in
        // a `<prefix>/bin/glu` layout.
        let err = running_bin().unwrap_err();
        assert!(err.to_string().contains("dev build"));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn validate_hex_accepts_only_64_hex_digits() {
        assert!(validate_hex(&"a".repeat(64)).is_ok());
        assert!(validate_hex(&"g".repeat(64)).is_err());
        assert!(validate_hex(&"a".repeat(63)).is_err());
    }

    #[test]
    fn swap_binary_replaces_and_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("glu");
        let new = dir.path().join("new-glu");
        fs::write(&bin, "old").unwrap();
        fs::write(&new, "new").unwrap();
        swap_binary(&bin, &new).unwrap();
        assert_eq!(fs::read_to_string(&bin).unwrap(), "new");
        assert_eq!(
            fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // The staged temp file is gone after the rename.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".glu.upgrade."))
            .collect();
        assert!(leftovers.is_empty());
    }
}

#[cfg(test)]
mod swap_safety_tests {
    use super::*;

    #[test]
    fn stage_with_create_new_does_not_follow_a_preplaced_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"original").unwrap();
        // An attacker pre-places a symlink at one of the predictable names;
        // staging must fail that open (not write through) and retry a fresh
        // name, leaving the victim untouched.
        let candidate = dir
            .path()
            .join(format!(".glu.upgrade.{}-0.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &candidate).unwrap();

        let staged = stage_with_create_new(dir.path(), b"new-binary").unwrap();
        assert_ne!(staged, candidate);
        assert_eq!(fs::read(&victim).unwrap(), b"original"); // untouched
        assert_eq!(fs::read(&staged).unwrap(), b"new-binary");
        assert!(!staged.is_symlink());
    }
}
