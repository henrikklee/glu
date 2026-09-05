//! `glu upgrade` — self-update of the glu client.
//!
//! Version discovery comes from the registry (the same latest-version header
//! the outdated notification uses), so "already up to date" never downloads
//! the binary. When newer, the exact release is fetched from the
//! distribution, checksum-verified, sanity-checked, and swapped in atomically.

use crate::config::ClientConfig;
use crate::events::{ExecutionEvents, ProgressEvent};
use crate::registry::resolve_client::HttpResolveClient;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use glu_core::DISTRIBUTION_TARGET;
use ring::digest;
use semver::Version;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::AsyncWriteExt;

const RELEASE_ARCHIVE_ENTRIES: [&str; 5] = [
    "LICENSE-BSD-2-Clause",
    "LICENSE-MIT",
    "THIRD_PARTY_LICENSES.html",
    "THIRD_PARTY_NOTICES.md",
    "glu",
];

const MACHO_64_LE_MAGIC: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_EXECUTE: u32 = 0x2;

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

pub async fn upgrade(
    config: &ClientConfig,
    cancellation: tokio_util::sync::CancellationToken,
    events: &dyn ExecutionEvents,
) -> Result<UpgradeResult> {
    let own = env!("CARGO_PKG_VERSION");
    let registry = HttpResolveClient::with_cancellation(&config.registry_base_url, cancellation)?;
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
    let expected = fetch_checksum(&http, &checksum_url).await?;

    let tmp = tempfile::tempdir().context("failed to create a temp dir")?;
    let asset_path = tmp.path().join(&asset);
    let actual = download_hashed(&http, &asset_url, &asset_path).await?;
    if actual != expected {
        bail!("checksum mismatch for {asset} (expected {expected}, got {actual})");
    }

    let new_bin = extract_release_binary(&asset_path, tmp.path())?;
    validate_arm64_macho(&new_bin)?;

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
    validate_version_stdout(&out.stdout, &latest)?;

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
    let dir = bin
        .parent()
        .context("installed binary has no parent directory")?;
    let dir_handle = fs::File::open(dir)
        .with_context(|| format!("failed to open binary directory {}", dir.display()))?;
    // Create the staged file with O_EXCL so a pre-created symlink at the
    // pid-based name can't redirect the write;
    // retry a few names on collision.
    let staged = stage_with_create_new(dir, new_bin)?;
    fs::rename(&staged, bin).with_context(|| format!("failed to replace {}", bin.display()))?;
    dir_handle
        .sync_all()
        .with_context(|| format!("failed to sync binary directory {}", dir.display()))
}

/// Copies `source_path` to a fresh file in `dir` via `create_new` (O_EXCL), so a
/// symlink or file an attacker pre-placed at the predictable name fails the
/// open instead of being written through.
fn stage_with_create_new(dir: &Path, source_path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let mut source = fs::File::open(source_path).context("reading staged binary")?;
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
                let copied = (|| -> Result<()> {
                    std::io::copy(&mut source, &mut file).with_context(|| {
                        format!("writing staged binary {}", candidate.display())
                    })?;
                    file.set_permissions(fs::Permissions::from_mode(0o755))
                        .context("failed to set permissions on the staged binary")?;
                    file.sync_all().with_context(|| {
                        format!("syncing staged binary {}", candidate.display())
                    })?;
                    Ok(())
                })();
                if let Err(error) = copied {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(_) => continue, // collision — vanishingly rare; try another name
        }
    }
    bail!("could not stage the new glu binary (too many name collisions)")
}

fn validate_hex(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
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

async fn fetch_response(http: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    if !response.status().is_success() {
        bail!("failed to fetch {url}: {}", response.status());
    }
    Ok(response)
}

/// Reads only the first whitespace-delimited SHA-256 token. Its fixed digest
/// syntax bounds memory without imposing a size policy on the sidecar file.
async fn fetch_checksum(http: &reqwest::Client, url: &str) -> Result<String> {
    let mut stream = fetch_response(http, url).await?.bytes_stream();
    let mut token = Vec::with_capacity(64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to read response from {url}"))?;
        for byte in chunk {
            if byte.is_ascii_whitespace() {
                if token.is_empty() {
                    continue;
                }
                let token = String::from_utf8(token).context("checksum is not UTF-8")?;
                validate_hex(&token)?;
                return Ok(token);
            }
            if token.len() == 64 {
                bail!("malformed checksum in {url}");
            }
            token.push(byte);
        }
    }
    if token.is_empty() {
        bail!("empty checksum sidecar");
    }
    let token = String::from_utf8(token).context("checksum is not UTF-8")?;
    validate_hex(&token)?;
    Ok(token)
}

/// Streams an asset directly to disk and hashes each response chunk in the
/// same pass. Memory use is independent of artifact size.
async fn download_hashed(http: &reqwest::Client, url: &str, path: &Path) -> Result<String> {
    let mut stream = fetch_response(http, url).await?.bytes_stream();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut hash = digest::Context::new(&digest::SHA256);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to read response from {url}"))?;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        hash.update(&chunk);
    }
    file.flush()
        .await
        .with_context(|| format!("failed to flush {}", path.display()))?;
    drop(file);
    Ok(crate::hash::hex_lower(hash.finish().as_ref()))
}

/// Validates the release archive in one pass and writes only its executable.
/// The release package contains exactly these root-level regular files; no
/// archive-controlled path is unpacked directly.
fn extract_release_binary(asset_path: &Path, output_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let file = fs::File::open(asset_path).context("failed to open the downloaded asset")?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut seen = [false; RELEASE_ARCHIVE_ENTRIES.len()];
    let binary_path = output_dir.join("glu");

    for entry in archive
        .entries()
        .context("failed to read the release archive")?
    {
        let mut entry = entry.context("failed to read a release archive entry")?;
        let path = entry
            .path()
            .context("failed to read a release archive path")?
            .into_owned();
        let Some(index) = RELEASE_ARCHIVE_ENTRIES
            .iter()
            .position(|expected| path == Path::new(expected))
        else {
            bail!("release archive has unexpected entry {}", path.display());
        };
        if seen[index] {
            bail!("release archive has duplicate entry {}", path.display());
        }
        seen[index] = true;
        if !entry.header().entry_type().is_file() {
            bail!(
                "release archive entry {} is not a regular file",
                path.display()
            );
        }

        if RELEASE_ARCHIVE_ENTRIES[index] == "glu" {
            let mut binary = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&binary_path)
                .context("failed to create the extracted glu binary")?;
            std::io::copy(&mut entry, &mut binary).context("failed to extract the glu binary")?;
            binary.flush().context("failed to flush the glu binary")?;
        } else {
            std::io::copy(&mut entry, &mut std::io::sink())
                .with_context(|| format!("failed to read archive entry {}", path.display()))?;
        }
    }

    let missing: Vec<_> = RELEASE_ARCHIVE_ENTRIES
        .iter()
        .zip(seen)
        .filter_map(|(entry, present)| (!present).then_some(*entry))
        .collect();
    if !missing.is_empty() {
        bail!("release archive is missing entries: {}", missing.join(", "));
    }
    fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))
        .context("failed to set permissions on the extracted glu binary")?;
    Ok(binary_path)
}

fn validate_arm64_macho(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path).context("failed to inspect the downloaded glu binary")?;
    let mut header = [0_u8; 32];
    file.read_exact(&mut header)
        .context("downloaded glu is not a complete 64-bit Mach-O executable")?;
    let cpu_type = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let file_type = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if header[..4] != MACHO_64_LE_MAGIC || cpu_type != CPU_TYPE_ARM64 || file_type != MH_EXECUTE {
        bail!("downloaded glu is not an ARM64 Mach-O executable");
    }
    Ok(())
}

fn validate_version_stdout(stdout: &[u8], version: &str) -> Result<()> {
    let expected = format!("glu {version}\n");
    if stdout != expected.as_bytes() {
        bail!(
            "downloaded glu reports {:?} but the registry says {version} is latest; \
             distribution and registry are out of sync",
            String::from_utf8_lossy(stdout)
        );
    }
    Ok(())
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
    fn validate_hex_accepts_only_lowercase_sha256() {
        assert!(validate_hex(&"a".repeat(64)).is_ok());
        assert!(validate_hex(&"A".repeat(64)).is_err());
        assert!(validate_hex(&"g".repeat(64)).is_err());
        assert!(validate_hex(&"a".repeat(63)).is_err());
    }

    #[test]
    fn version_output_must_be_exact() {
        assert!(validate_version_stdout(b"glu 0.2.0\n", "0.2.0").is_ok());
        assert!(validate_version_stdout(b"not-glu 0.2.0\n", "0.2.0").is_err());
        assert!(validate_version_stdout(b"glu 0.2.0-extra\n", "0.2.0").is_err());
        assert!(validate_version_stdout(b"glu 0.2.0\nextra\n", "0.2.0").is_err());
    }

    #[test]
    fn binary_identity_requires_thin_arm64_macho_executable() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("glu");
        let mut header = [0_u8; 32];
        header[..4].copy_from_slice(&MACHO_64_LE_MAGIC);
        header[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        header[12..16].copy_from_slice(&MH_EXECUTE.to_le_bytes());
        fs::write(&binary, header).unwrap();
        validate_arm64_macho(&binary).unwrap();

        header[4..8].copy_from_slice(&0x0100_0007_u32.to_le_bytes());
        fs::write(&binary, header).unwrap();
        assert!(validate_arm64_macho(&binary).is_err());

        header[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        header[12..16].copy_from_slice(&0x6_u32.to_le_bytes());
        fs::write(&binary, header).unwrap();
        assert!(validate_arm64_macho(&binary).is_err());
    }

    fn write_release_archive(path: &Path, entries: &[&str]) {
        let file = fs::File::create(path).unwrap();
        let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut archive = tar::Builder::new(gzip);
        for name in entries {
            let data = if *name == "glu" {
                b"binary".as_slice()
            } else {
                b"license".as_slice()
            };
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(if *name == "glu" { 0o755 } else { 0o644 });
            header.set_cksum();
            archive.append_data(&mut header, name, data).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn release_archive_requires_exact_allowlist() {
        let valid = tempfile::tempdir().unwrap();
        let valid_archive = valid.path().join("valid.tar.gz");
        write_release_archive(&valid_archive, &RELEASE_ARCHIVE_ENTRIES);
        let binary = extract_release_binary(&valid_archive, valid.path()).unwrap();
        assert_eq!(fs::read(binary).unwrap(), b"binary");

        let missing = tempfile::tempdir().unwrap();
        let missing_archive = missing.path().join("missing.tar.gz");
        write_release_archive(&missing_archive, &RELEASE_ARCHIVE_ENTRIES[..4]);
        let error = extract_release_binary(&missing_archive, missing.path()).unwrap_err();
        assert!(error.to_string().contains("missing entries"));

        let unexpected = tempfile::tempdir().unwrap();
        let unexpected_archive = unexpected.path().join("unexpected.tar.gz");
        let mut entries = RELEASE_ARCHIVE_ENTRIES.to_vec();
        entries.push("surprise");
        write_release_archive(&unexpected_archive, &entries);
        let error = extract_release_binary(&unexpected_archive, unexpected.path()).unwrap_err();
        assert!(error.to_string().contains("unexpected entry"));
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

        let source = dir.path().join("source");
        fs::write(&source, b"new-binary").unwrap();
        let staged = stage_with_create_new(dir.path(), &source).unwrap();
        assert_ne!(staged, candidate);
        assert_eq!(fs::read(&victim).unwrap(), b"original"); // untouched
        assert_eq!(fs::read(&staged).unwrap(), b"new-binary");
        assert!(!staged.is_symlink());
    }
}
