use super::{
    macho,
    writer::{FilePatch, WritableFileGuard, WriterPool},
};
use anyhow::{bail, Context, Result};
use apple_codesign::{MachOSigner, SettingsScope, SigningSettings};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// One ad-hoc signing pool, shared across the whole install run.
///
/// Workers perform Mach-O parsing, hashing, and signature generation. They submit only the byte
/// ranges changed by the signature to the shared [`WriterPool`], so signing cannot bypass the
/// install-wide APFS concurrency limit or memory budget. Sparse writes also avoid rewriting
/// hundreds of MiB of unchanged GCC and LLVM code.
pub struct CodeSignPool {
    tx: mpsc::SyncSender<CodeSignJob>,
    sign_busy_us: Arc<AtomicU64>,
    workers: usize,
}

struct CodeSignJob {
    path: PathBuf,
    reply: mpsc::Sender<Result<()>>,
}

impl CodeSignPool {
    const MAX_WORKERS: usize = 8;

    pub fn new(writer_pool: Arc<WriterPool>) -> Self {
        let workers = thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1)
            .clamp(1, Self::MAX_WORKERS);
        let (tx, rx) = mpsc::sync_channel::<CodeSignJob>(workers * 4);
        let rx = Arc::new(Mutex::new(rx));
        let sign_busy_us = Arc::new(AtomicU64::new(0));
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let writer_pool = Arc::clone(&writer_pool);
            let sign_busy_us = Arc::clone(&sign_busy_us);
            thread::spawn(move || loop {
                let job = { rx.lock().unwrap().recv() };
                let Ok(job) = job else { break };
                let start = Instant::now();
                let reply = job.reply;
                let result = enqueue_adhoc_signed_path(&job.path, &writer_pool, reply.clone());
                sign_busy_us.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                if let Err(error) = result {
                    let _ = reply.send(Err(error));
                }
            });
        }
        Self {
            tx,
            sign_busy_us,
            workers,
        }
    }

    pub fn total_sign_time(&self) -> Duration {
        Duration::from_micros(self.sign_busy_us.load(Ordering::Relaxed))
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    pub(crate) fn sign_all(&self, paths: &BTreeSet<PathBuf>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        for path in paths {
            self.tx
                .send(CodeSignJob {
                    path: path.clone(),
                    reply: reply_tx.clone(),
                })
                .context("codesign pool disconnected")?;
        }
        drop(reply_tx);

        let mut errors = Vec::<String>::new();
        for _ in 0..paths.len() {
            if let Err(error) = reply_rx
                .recv()
                .context("codesign pool disconnected before all jobs completed")?
            {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            bail!(errors.join("\n"));
        }
        Ok(())
    }
}

/// ruby-macho `CodeSigning.identifier` parity: embedded Info.plist identifier first, then a
/// dotted stem, then `"UUID" + LC_UUID` or a SHA-1 of the header and load-command region.
fn signing_identifier_for_path(path: &Path) -> Result<Option<String>> {
    let Some(stem) = path.file_stem() else {
        return Ok(None);
    };
    let stem = stem.to_string_lossy().to_string();
    let mut header = [0u8; 32];
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    if file.read_exact(&mut header).is_err() {
        return Ok(Some(stem));
    }
    let Some(layout) = macho::header_layout(&header) else {
        return Ok(Some(stem));
    };
    let Some(region_len) = layout.header_size.checked_add(layout.sizeofcmds) else {
        return Ok(Some(stem));
    };
    if u64::try_from(region_len)
        .ok()
        .is_none_or(|region_len| region_len > file_len)
    {
        return Ok(Some(stem));
    }
    let mut region = zeroed_buffer(region_len)
        .with_context(|| format!("reserving Mach-O header for {}", path.display()))?;
    if file.seek(SeekFrom::Start(0)).is_err() || file.read_exact(&mut region).is_err() {
        return Ok(Some(stem));
    }

    let info_plist = match macho::section_range(&region, layout, b"__TEXT", b"__info_plist") {
        Some((offset, size)) => {
            let Some(end) = offset.checked_add(size) else {
                return Ok(Some(
                    macho::signing_identifier(&region, &stem, None).unwrap_or(stem),
                ));
            };
            if let Some(in_region) = region.get(offset..end) {
                Some(Cow::Borrowed(in_region))
            } else {
                read_file_range(&mut file, file_len, offset, size, path)?.map(Cow::Owned)
            }
        }
        None => None,
    };
    Ok(Some(
        macho::signing_identifier(&region, &stem, info_plist.as_deref()).unwrap_or(stem),
    ))
}

fn zeroed_buffer(len: usize) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .context("requested byte range does not fit in memory")?;
    buffer.resize(len, 0);
    Ok(buffer)
}

fn read_file_range(
    file: &mut File,
    file_len: u64,
    offset: usize,
    size: usize,
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    let Some(end) = offset.checked_add(size) else {
        return Ok(None);
    };
    let (Ok(offset), Ok(end)) = (u64::try_from(offset), u64::try_from(end)) else {
        return Ok(None);
    };
    if end > file_len {
        return Ok(None);
    }
    let mut buffer = zeroed_buffer(size)
        .with_context(|| format!("reserving Mach-O section from {}", path.display()))?;
    Ok(
        (file.seek(SeekFrom::Start(offset)).is_ok() && file.read_exact(&mut buffer).is_ok())
            .then_some(buffer),
    )
}

fn adhoc_signed_macho_data_from(path: &Path, macho_data: &[u8]) -> Result<Vec<u8>> {
    let mut settings = SigningSettings::default();
    if let Some(id) = signing_identifier_for_path(path)? {
        settings.set_binary_identifier(SettingsScope::Main, id);
    }

    settings
        .import_settings_from_macho(macho_data)
        .with_context(|| format!("importing signing settings from {}", path.display()))?;
    if settings.binary_identifier(SettingsScope::Main).is_none() {
        let id = apple_codesign::path_identifier(path)
            .with_context(|| format!("deriving signing identifier for {}", path.display()))?;
        settings.set_binary_identifier(SettingsScope::Main, id);
    }

    let signer = MachOSigner::new(macho_data)
        .with_context(|| format!("parsing Mach-O {}", path.display()))?;
    let mut signed = Vec::with_capacity(macho_data.len());
    signer
        .write_signed_binary(&settings, &mut signed)
        .with_context(|| format!("ad-hoc signing {}", path.display()))?;
    Ok(signed)
}

fn adhoc_signed_macho_data(path: &Path) -> Result<Vec<u8>> {
    let macho_data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    adhoc_signed_macho_data_from(path, &macho_data)
}

fn enqueue_adhoc_signed_path(
    path: &Path,
    writer_pool: &WriterPool,
    reply: mpsc::Sender<Result<()>>,
) -> Result<()> {
    let file_size = fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    // apple-codesign holds source, intermediate Mach-O, signature, and final output buffers.
    // Reserve conservatively before reading so several large binaries share extraction's
    // memory backpressure instead of multiplying peak memory independently.
    let memory = writer_pool.reserve_bytes(file_size.saturating_mul(4).saturating_add(1024 * 1024));
    let original = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let signed = adhoc_signed_macho_data_from(path, &original)?;
    let patch = FilePatch::between(&original, &signed);
    writer_pool.submit_patch(path.to_path_buf(), patch, reply, memory)
}

fn write_existing_file_assume_writable(path: &Path, data: &[u8]) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("open {} for replacement", path.display()))?;
    file.write_all(data)
        .with_context(|| format!("write replacement {}", path.display()))
}

fn sign_path_adhoc_inner(path: &Path) -> Result<()> {
    let signed = adhoc_signed_macho_data(path)?;
    write_existing_file_assume_writable(path, &signed)
}

/// Ad-hoc sign a Mach-O in place while restoring a read-only bottle mode.
pub fn sign_path_adhoc(path: &Path) -> Result<()> {
    let _writable = WritableFileGuard::new(path)?;
    sign_path_adhoc_inner(path)
}

/// Ad-hoc sign a Mach-O already covered by a [`WritableFileGuard`].
pub(crate) fn sign_path_adhoc_assume_writable(path: &Path) -> Result<()> {
    sign_path_adhoc_inner(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_declared_load_commands_fall_back_before_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool");
        let mut header = [0u8; 32];
        header[..4].copy_from_slice(&0xcffa_edfe_u32.to_be_bytes());
        header[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, header).unwrap();

        assert_eq!(
            signing_identifier_for_path(&path).unwrap(),
            Some("tool".to_string())
        );
    }

    #[test]
    fn oversized_declared_info_plist_falls_back_before_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool");
        let mut macho = vec![0u8; 32 + 72 + 80];
        macho[..4].copy_from_slice(&0xcffa_edfe_u32.to_be_bytes());
        macho[16..20].copy_from_slice(&1u32.to_le_bytes());
        macho[20..24].copy_from_slice(&(152u32).to_le_bytes());
        let command = 32;
        macho[command..command + 4].copy_from_slice(&0x19u32.to_le_bytes());
        macho[command + 4..command + 8].copy_from_slice(&152u32.to_le_bytes());
        macho[command + 8..command + 24].copy_from_slice(b"__TEXT\0\0\0\0\0\0\0\0\0\0");
        macho[command + 64..command + 68].copy_from_slice(&1u32.to_le_bytes());
        let section = command + 72;
        macho[section..section + 16].copy_from_slice(b"__info_plist\0\0\0\0");
        macho[section + 32..section + 36].copy_from_slice(&u32::MAX.to_le_bytes());
        macho[section + 40..section + 44].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, macho).unwrap();

        let identifier = signing_identifier_for_path(&path).unwrap().unwrap();
        assert!(identifier.starts_with("tool-"), "identifier: {identifier}");
    }

    #[test]
    fn out_of_file_and_overflowing_ranges_are_ignored_before_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool");
        fs::write(&path, b"small file").unwrap();
        let mut file = File::open(&path).unwrap();
        let file_len = file.metadata().unwrap().len();

        assert!(read_file_range(&mut file, file_len, 0, usize::MAX, &path)
            .unwrap()
            .is_none());
        assert!(read_file_range(&mut file, file_len, usize::MAX, 1, &path)
            .unwrap()
            .is_none());
    }

    #[test]
    fn valid_file_range_is_read_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool");
        fs::write(&path, b"prefix-plist-suffix").unwrap();
        let mut file = File::open(&path).unwrap();
        let file_len = file.metadata().unwrap().len();

        assert_eq!(
            read_file_range(&mut file, file_len, 7, 5, &path).unwrap(),
            Some(b"plist".to_vec())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pool_routes_bit_identical_sparse_update_through_writer_pool() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let expected_dir = tmp.path().join("expected");
        let actual_dir = tmp.path().join("actual");
        fs::create_dir_all(&expected_dir).unwrap();
        fs::create_dir_all(&actual_dir).unwrap();
        let expected = expected_dir.join("echo");
        let actual = actual_dir.join("echo");
        fs::copy("/bin/echo", &expected).unwrap();
        fs::copy("/bin/echo", &actual).unwrap();
        fs::set_permissions(&expected, fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&actual, fs::Permissions::from_mode(0o555)).unwrap();

        // Reproduce the previous direct UnifiedSigner path as the parity oracle.
        {
            let _writable = WritableFileGuard::new(&expected).unwrap();
            let mut settings = SigningSettings::default();
            if let Some(id) = signing_identifier_for_path(&expected).unwrap() {
                settings.set_binary_identifier(SettingsScope::Main, id);
            }
            apple_codesign::UnifiedSigner::new(settings)
                .sign_path_in_place(&expected)
                .unwrap();
        }

        let original = fs::read(&actual).unwrap();
        let expected_bytes = fs::read(&expected).unwrap();
        let expected_patch = FilePatch::between(&original, &expected_bytes);
        assert!(
            expected_patch.changed_bytes() < expected_bytes.len(),
            "codesign should not rewrite unchanged Mach-O code pages"
        );

        let writer_pool = Arc::new(WriterPool::with_limits(1, 32 * 1024 * 1024));
        let code_sign_pool = CodeSignPool::new(Arc::clone(&writer_pool));
        code_sign_pool
            .sign_all(&BTreeSet::from([actual.clone()]))
            .unwrap();

        assert_eq!(fs::read(&actual).unwrap(), expected_bytes);
        assert_eq!(
            fs::metadata(&actual).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert!(
            writer_pool.total_write_time() > Duration::ZERO,
            "the final signed update must be performed by WriterPool"
        );
    }
}
