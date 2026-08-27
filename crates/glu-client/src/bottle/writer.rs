use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// One writer-thread pool, shared across the whole install run. Every bottle prepare submits
/// extraction and codesign writes here instead of creating private writers per package or phase.
/// The production worker count is deliberately conservative because scaling past the APFS
/// metadata sweet spot reduces throughput. Tests and benchmarks can construct the pool with
/// another count without exposing a user-facing tuning setting.
pub struct WriterPool {
    tx: mpsc::SyncSender<WriteJob>,
    byte_budget: Arc<WriterByteBudget>,
    /// Sum of actual filesystem-write time across every worker for the pool's lifetime.
    write_busy_us: Arc<AtomicU64>,
    workers: usize,
}

struct WriteJob {
    path: PathBuf,
    operation: WriteOperation,
    reply: mpsc::Sender<Result<()>>,
    _memory: WriterMemoryPermit,
}

enum WriteOperation {
    Extracted { data: Vec<u8>, mode: Option<u32> },
    PatchExisting(FilePatch),
}

pub(crate) struct FilePatch {
    original_len: u64,
    final_len: u64,
    chunks: Vec<FilePatchChunk>,
}

struct FilePatchChunk {
    offset: u64,
    data: Vec<u8>,
}

impl FilePatch {
    /// Build a sparse transform from canonical source bytes to canonical final bytes.
    pub(crate) fn between(original: &[u8], final_bytes: &[u8]) -> Self {
        // Codesigning usually changes one load command and the embedded signature near EOF.
        // Coalesce ranges separated by small equal spans so code-directory fields do not become
        // thousands of tiny writes while executable code pages remain untouched.
        const COALESCE_GAP: usize = 4096;

        let common_len = original.len().min(final_bytes.len());
        let mut chunks = Vec::<FilePatchChunk>::new();
        let mut cursor = 0usize;
        while cursor < common_len {
            while cursor < common_len && original[cursor] == final_bytes[cursor] {
                cursor += 1;
            }
            if cursor == common_len {
                break;
            }

            let start = cursor;
            let mut last_difference = cursor;
            cursor += 1;
            while cursor < common_len {
                if original[cursor] != final_bytes[cursor] {
                    last_difference = cursor;
                } else if cursor - last_difference > COALESCE_GAP {
                    break;
                }
                cursor += 1;
            }
            let end = last_difference + 1;
            chunks.push(FilePatchChunk {
                offset: start as u64,
                data: final_bytes[start..end].to_vec(),
            });
            cursor = end;
        }

        if final_bytes.len() > common_len {
            let appended = &final_bytes[common_len..];
            if let Some(last) = chunks.last_mut() {
                let last_end = last.offset as usize + last.data.len();
                if common_len.saturating_sub(last_end) <= COALESCE_GAP {
                    last.data.extend_from_slice(&final_bytes[last_end..]);
                } else {
                    chunks.push(FilePatchChunk {
                        offset: common_len as u64,
                        data: appended.to_vec(),
                    });
                }
            } else {
                chunks.push(FilePatchChunk {
                    offset: common_len as u64,
                    data: appended.to_vec(),
                });
            }
        }

        Self {
            original_len: original.len() as u64,
            final_len: final_bytes.len() as u64,
            chunks,
        }
    }

    #[cfg(test)]
    pub(crate) fn changed_bytes(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.data.len()).sum()
    }
}

struct WriterByteBudget {
    limit: u64,
    state: Mutex<WriterByteBudgetState>,
    available: Condvar,
}

#[derive(Default)]
struct WriterByteBudgetState {
    used: u64,
}

pub(crate) struct WriterMemoryPermit {
    budget: Arc<WriterByteBudget>,
    bytes: u64,
}

impl WriterByteBudget {
    fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            state: Mutex::new(WriterByteBudgetState::default()),
            available: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>, bytes: u64) -> WriterMemoryPermit {
        let mut state = self.state.lock().expect("writer byte budget lock poisoned");
        if bytes > self.limit {
            while state.used != 0 {
                state = self
                    .available
                    .wait(state)
                    .expect("writer byte budget lock poisoned");
            }
        } else {
            while state.used.saturating_add(bytes) > self.limit {
                state = self
                    .available
                    .wait(state)
                    .expect("writer byte budget lock poisoned");
            }
        }
        state.used = state.used.saturating_add(bytes);
        WriterMemoryPermit {
            budget: Arc::clone(self),
            bytes,
        }
    }

    fn release(&self, bytes: u64) {
        let mut state = self.state.lock().expect("writer byte budget lock poisoned");
        state.used = state.used.saturating_sub(bytes);
        self.available.notify_all();
    }

    #[cfg(test)]
    fn used(&self) -> u64 {
        self.state
            .lock()
            .expect("writer byte budget lock poisoned")
            .used
    }
}

impl Drop for WriterMemoryPermit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl WriterPool {
    // Post-permission-fast-path aws-sdk-cpp benchmark: three workers improved median wall time
    // by only 2.4%, below the plan's 5% selection gate, so production remains at four.
    const DEFAULT_WORKERS: usize = 4;

    pub fn new() -> Self {
        Self::with_limits(Self::DEFAULT_WORKERS, default_writer_memory_budget())
    }

    #[cfg(test)]
    fn with_memory_budget(memory_budget: u64) -> Self {
        Self::with_limits(Self::DEFAULT_WORKERS, memory_budget)
    }

    pub(crate) fn with_limits(workers: usize, memory_budget: u64) -> Self {
        let workers = workers.max(1);
        let (tx, rx) = mpsc::sync_channel::<WriteJob>(workers * 4);
        let rx = Arc::new(Mutex::new(rx));
        let byte_budget = WriterByteBudget::new(memory_budget);
        let write_busy_us = Arc::new(AtomicU64::new(0));
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let write_busy_us = Arc::clone(&write_busy_us);
            thread::spawn(move || loop {
                let job = { rx.lock().unwrap().recv() };
                let Ok(job) = job else { break };
                let start = Instant::now();
                let result = match job.operation {
                    WriteOperation::Extracted { data, mode } => {
                        write_extracted_file(&job.path, &data, mode)
                    }
                    WriteOperation::PatchExisting(patch) => {
                        apply_existing_file_patch(&job.path, patch)
                    }
                };
                write_busy_us.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                let _ = job.reply.send(result);
            });
        }
        Self {
            tx,
            byte_budget,
            write_busy_us,
            workers,
        }
    }

    /// Cumulative worker busy-time so far, and the worker count to divide a wall-clock span by
    /// for a duty-cycle ratio (`total_write_time / (workers() * wall_clock)`).
    pub fn total_write_time(&self) -> Duration {
        Duration::from_micros(self.write_busy_us.load(Ordering::Relaxed))
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    pub(crate) fn reserve_bytes(&self, bytes: u64) -> WriterMemoryPermit {
        self.byte_budget.acquire(bytes)
    }

    #[cfg(test)]
    fn budget_used(&self) -> u64 {
        self.byte_budget.used()
    }

    pub(crate) fn submit_extracted(
        &self,
        path: PathBuf,
        data: Vec<u8>,
        mode: Option<u32>,
        reply: mpsc::Sender<Result<()>>,
        memory: WriterMemoryPermit,
    ) -> Result<()> {
        self.submit(
            path,
            WriteOperation::Extracted { data, mode },
            reply,
            memory,
        )
    }

    pub(crate) fn submit_patch(
        &self,
        path: PathBuf,
        patch: FilePatch,
        reply: mpsc::Sender<Result<()>>,
        memory: WriterMemoryPermit,
    ) -> Result<()> {
        self.submit(path, WriteOperation::PatchExisting(patch), reply, memory)
    }

    fn submit(
        &self,
        path: PathBuf,
        operation: WriteOperation,
        reply: mpsc::Sender<Result<()>>,
        memory: WriterMemoryPermit,
    ) -> Result<()> {
        self.tx
            .send(WriteJob {
                path,
                operation,
                reply,
                _memory: memory,
            })
            .map_err(|_| anyhow::anyhow!("writer pool disconnected"))
    }
}

impl Default for WriterPool {
    fn default() -> Self {
        Self::new()
    }
}

fn default_writer_memory_budget() -> u64 {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let fallback = 8 * GIB;
    let physical = physical_memory_bytes().unwrap_or(fallback);
    (physical / 8).clamp(512 * MIB, 4 * GIB)
}

#[cfg(target_os = "macos")]
fn physical_memory_bytes() -> Option<u64> {
    use std::{ffi::CString, mem, ptr};

    let name = CString::new("hw.memsize").ok()?;
    let mut value = 0_u64;
    let mut len = mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && len == mem::size_of::<u64>() {
        Some(value)
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn physical_memory_bytes() -> Option<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    Some((pages as u64).saturating_mul(page_size as u64))
}

fn write_extracted_file(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        if let Some(mode) = mode {
            options.mode(mode & 0o1777);
        }
        options
            .open(path)
            .with_context(|| format!("create {}", path.display()))?
    };
    #[cfg(not(unix))]
    let mut file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;

    file.write_all(data)
        .with_context(|| format!("write {}", path.display()))?;

    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;

        let sanitized_mode = mode & 0o1777;
        let actual_mode = file
            .metadata()
            .with_context(|| format!("reading permissions for {}", path.display()))?
            .permissions()
            .mode()
            & 0o7777;
        if actual_mode != sanitized_mode {
            file.set_permissions(fs::Permissions::from_mode(sanitized_mode))
                .with_context(|| format!("setting permissions for {}", path.display()))?;
        }
    }
    Ok(())
}

fn apply_existing_file_patch(path: &Path, patch: FilePatch) -> Result<()> {
    if patch.chunks.is_empty() && patch.original_len == patch.final_len {
        return Ok(());
    }

    let actual_len = fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    if actual_len != patch.original_len {
        bail!(
            "refusing to patch {} after its size changed from {} to {} bytes",
            path.display(),
            patch.original_len,
            actual_len
        );
    }

    let _writable = WritableFileGuard::new(path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open {} for patching", path.display()))?;
    for chunk in patch.chunks {
        file.seek(SeekFrom::Start(chunk.offset))
            .with_context(|| format!("seek {} to {}", path.display(), chunk.offset))?;
        file.write_all(&chunk.data)
            .with_context(|| format!("patch {} at {}", path.display(), chunk.offset))?;
    }
    if patch.final_len != patch.original_len {
        file.set_len(patch.final_len)
            .with_context(|| format!("resize {} to {}", path.display(), patch.final_len))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("making {} writable", path.display()))
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn current_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode())
}

#[cfg(unix)]
fn restore_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn current_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(not(unix))]
fn restore_mode(_path: &Path, _mode: Option<u32>) {}

/// Homebrew `Pathname#ensure_writable` parity for in-place Mach-O mutation.
#[cfg(unix)]
pub(crate) struct WritableFileGuard<'a> {
    path: &'a Path,
    original_mode: Option<u32>,
    changed: bool,
}

#[cfg(unix)]
impl<'a> WritableFileGuard<'a> {
    pub(crate) fn new(path: &'a Path) -> Result<Self> {
        let original_mode = current_mode(path);
        let changed = if let Some(mode) = original_mode {
            if mode & 0o200 == 0 {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))
                    .with_context(|| format!("making {} writable", path.display()))?;
                true
            } else {
                false
            }
        } else {
            make_writable(path)?;
            true
        };
        Ok(Self {
            path,
            original_mode,
            changed,
        })
    }
}

#[cfg(unix)]
impl Drop for WritableFileGuard<'_> {
    fn drop(&mut self) {
        if self.changed {
            restore_mode(self.path, self.original_mode);
        }
    }
}

#[cfg(not(unix))]
pub(crate) struct WritableFileGuard<'a> {
    _path: &'a Path,
}

#[cfg(not(unix))]
impl<'a> WritableFileGuard<'a> {
    pub(crate) fn new(path: &'a Path) -> Result<Self> {
        Ok(Self { _path: path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_budget_zero(pool: &WriterPool) {
        for _ in 0..50 {
            if pool.budget_used() == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pool.budget_used(), 0);
    }

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn writable_file_guard_restores_read_only_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("libfixture.dylib");
        fs::write(&path, b"before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        {
            let _writable = WritableFileGuard::new(&path).unwrap();
            assert_eq!(file_mode(&path) & 0o200, 0o200);
            fs::write(&path, b"after").unwrap();
        }

        assert_eq!(file_mode(&path), 0o444);
        assert_eq!(fs::read(&path).unwrap(), b"after");
    }

    #[cfg(unix)]
    #[test]
    fn extracted_files_keep_sanitized_archive_modes() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, archive_mode, expected_mode) in [
            ("regular", 0o644, 0o644),
            ("readonly", 0o444, 0o444),
            ("executable", 0o755, 0o755),
            ("sticky", 0o1755, 0o1755),
            ("privileged", 0o6755, 0o755),
        ] {
            let path = tmp.path().join(name);
            write_extracted_file(&path, b"payload", Some(archive_mode)).unwrap();
            assert_eq!(
                file_mode(&path),
                expected_mode,
                "archive mode {archive_mode:o}"
            );
            assert_eq!(fs::read(path).unwrap(), b"payload");
        }
    }

    #[cfg(unix)]
    #[test]
    fn extracted_file_corrects_mode_on_existing_inode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("existing");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o6600)).unwrap();
        write_extracted_file(&path, b"new", Some(0o644)).unwrap();
        assert_eq!(file_mode(&path), 0o644);
        assert_eq!(fs::read(path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn extracted_file_preserves_mode_under_restrictive_umask() {
        const CHILD_ENV: &str = "GLU_TEST_RESTRICTIVE_UMASK_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            unsafe { libc::umask(0o077) };
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("restricted");
            write_extracted_file(&path, b"payload", Some(0o644)).unwrap();
            assert_eq!(file_mode(&path), 0o644);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("extracted_file_preserves_mode_under_restrictive_umask")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success(), "restrictive-umask child test failed");
    }

    #[test]
    fn extracted_file_errors_include_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing").join("out");
        let error = write_extracted_file(&path, b"payload", Some(0o644)).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn writer_pool_uses_configured_worker_count() {
        assert_eq!(WriterPool::with_limits(3, 4).workers(), 3);
        assert_eq!(WriterPool::with_limits(0, 4).workers(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_patch_reconstructs_growth_shrink_and_sparse_changes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut cases = Vec::<(Vec<u8>, Vec<u8>)>::new();
        let original = vec![b'a'; 20_000];
        let mut sparse = original.clone();
        sparse[17] = b'b';
        sparse[9_000] = b'c';
        sparse[19_999] = b'd';
        cases.push((original, sparse));
        cases.push((b"short source".to_vec(), b"a longer signed result".to_vec()));
        cases.push((b"a source that will shrink".to_vec(), b"small".to_vec()));

        for (index, (original, final_bytes)) in cases.into_iter().enumerate() {
            let path = tmp.path().join(format!("case-{index}"));
            fs::write(&path, &original).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
            apply_existing_file_patch(&path, FilePatch::between(&original, &final_bytes)).unwrap();
            assert_eq!(fs::read(&path).unwrap(), final_bytes);
            assert_eq!(file_mode(&path), 0o444);
        }
    }

    #[test]
    fn file_patch_refuses_a_changed_source_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("changed");
        let patch = FilePatch::between(b"before", b"after!");
        fs::write(&path, b"different size").unwrap();
        let error = apply_existing_file_patch(&path, patch).unwrap_err();
        assert!(error.to_string().contains("after its size changed"));
        assert_eq!(fs::read(path).unwrap(), b"different size");
    }

    #[test]
    fn writer_pool_releases_budget_after_successful_write() {
        let pool = WriterPool::with_memory_budget(4);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out");
        let (reply_tx, reply_rx) = mpsc::channel();
        let memory = pool.reserve_bytes(4);
        pool.submit_extracted(path.clone(), vec![1, 2, 3, 4], None, reply_tx, memory)
            .unwrap();
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        wait_for_budget_zero(&pool);
        assert_eq!(fs::read(path).unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn writer_pool_releases_budget_after_write_error() {
        let pool = WriterPool::with_memory_budget(4);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing-parent/out");
        let (reply_tx, reply_rx) = mpsc::channel();
        let memory = pool.reserve_bytes(4);
        pool.submit_extracted(path, vec![1, 2, 3, 4], None, reply_tx, memory)
            .unwrap();
        assert!(reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err());
        wait_for_budget_zero(&pool);
    }

    #[test]
    fn writer_budget_blocks_until_bytes_are_released() {
        let budget = WriterByteBudget::new(4);
        let first = budget.acquire(3);
        let (tx, rx) = mpsc::channel();
        let budget_for_thread = Arc::clone(&budget);
        thread::spawn(move || tx.send(budget_for_thread.acquire(2)).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        drop(rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn writer_budget_allows_oversized_entry_alone() {
        let budget = WriterByteBudget::new(4);
        let oversized = budget.acquire(8);
        assert_eq!(budget.used(), 8);
        let (tx, rx) = mpsc::channel();
        let budget_for_thread = Arc::clone(&budget);
        thread::spawn(move || tx.send(budget_for_thread.acquire(1)).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(oversized);
        drop(rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(budget.used(), 0);
    }
}
