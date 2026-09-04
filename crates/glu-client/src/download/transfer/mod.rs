use crate::{download::http::GhcrTransport, hash::Sha256Mismatch};
mod session;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use serde::Serialize;
use session::{cleanup_emergency_files, run_segment, SegmentJob};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, ErrorKind},
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    sync::{
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{oneshot, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};

#[derive(Debug, Clone)]
struct StagingFile {
    file: Arc<File>,
    pending_writes: Arc<PendingWrites>,
}

#[derive(Debug, Default)]
struct PendingWrites {
    count: Mutex<usize>,
    idle: Condvar,
}

impl PendingWrites {
    fn begin(self: &Arc<Self>) -> PendingWriteGuard {
        *self.count.lock().expect("pending write lock poisoned") += 1;
        PendingWriteGuard {
            writes: Arc::clone(self),
        }
    }

    fn wait(&self) {
        let mut count = self.count.lock().expect("pending write lock poisoned");
        while *count != 0 {
            count = self.idle.wait(count).expect("pending write lock poisoned");
        }
    }
}

struct PendingWriteGuard {
    writes: Arc<PendingWrites>,
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        let mut count = self
            .writes
            .count
            .lock()
            .expect("pending write lock poisoned");
        *count -= 1;
        if *count == 0 {
            self.writes.idle.notify_all();
        }
    }
}

impl StagingFile {
    async fn create(path: &Path, len: Option<u64>) -> Result<Self> {
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true).mode(0o600);
        let file = options
            .open(path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;
        if let Some(len) = len {
            file.set_len(len)
                .await
                .with_context(|| format!("sizing {}", path.display()))?;
        }
        Ok(Self {
            file: Arc::new(file.into_std().await),
            pending_writes: Arc::new(PendingWrites::default()),
        })
    }

    async fn truncate(&self, path: &Path) -> Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.set_len(0))
            .await
            .context("staging truncate task failed")?
            .with_context(|| format!("truncating {}", path.display()))
    }

    fn start_write_all_at<B>(&self, offset: u64, bytes: B) -> tokio::task::JoinHandle<io::Result<B>>
    where
        B: AsRef<[u8]> + Send + 'static,
    {
        let file = Arc::clone(&self.file);
        let pending = self.pending_writes.begin();
        tokio::task::spawn_blocking(move || {
            let _pending = pending;
            positioned_write_all(&file, offset, bytes.as_ref())?;
            Ok(bytes)
        })
    }

    async fn finish_write_all_at<B>(
        path: &Path,
        task: tokio::task::JoinHandle<io::Result<B>>,
    ) -> Result<B>
    where
        B: Send + 'static,
    {
        task.await
            .context("positioned staging write task failed")?
            .with_context(|| format!("writing {}", path.display()))
    }

    async fn write_all_at<B>(&self, path: &Path, offset: u64, bytes: B) -> Result<B>
    where
        B: AsRef<[u8]> + Send + 'static,
    {
        Self::finish_write_all_at(path, self.start_write_all_at(offset, bytes)).await
    }

    async fn wait_for_pending_writes(&self) -> Result<()> {
        let pending = Arc::clone(&self.pending_writes);
        tokio::task::spawn_blocking(move || pending.wait())
            .await
            .context("pending staging write wait failed")
    }

    async fn copy_to_at(&self, target: &Self, target_path: &Path, offset: u64) -> Result<u64> {
        let source = Arc::clone(&self.file);
        let target = Arc::clone(&target.file);
        tokio::task::spawn_blocking(move || positioned_copy(&source, &target, offset))
            .await
            .context("positioned emergency commit task failed")?
            .with_context(|| format!("committing emergency suffix into {}", target_path.display()))
    }

    async fn sha256(&self, path: &Path) -> Result<String> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || sha256_file_descriptor(&file))
            .await
            .context("staging hash task failed")?
            .with_context(|| format!("reading {}", path.display()))
    }

    async fn sync_all(&self, path: &Path) -> Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .context("staging sync task failed")?
            .with_context(|| format!("syncing {}", path.display()))
    }

    async fn validate_path_identity(&self, path: &Path) -> Result<()> {
        let file = Arc::clone(&self.file);
        let descriptor = tokio::task::spawn_blocking(move || file.metadata())
            .await
            .context("staging metadata task failed")?
            .with_context(|| format!("reading staging descriptor for {}", path.display()))?;
        let named = tokio::fs::symlink_metadata(path)
            .await
            .with_context(|| format!("reading staging path {}", path.display()))?;
        if !named.file_type().is_file()
            || descriptor.dev() != named.dev()
            || descriptor.ino() != named.ino()
        {
            bail!(
                "staging path identity changed during download: {}",
                path.display()
            );
        }
        Ok(())
    }
}

fn positioned_write_all(file: &File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match file.write_at(bytes, offset) {
            Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero)),
            Ok(written) => {
                offset = offset.checked_add(written as u64).ok_or_else(|| {
                    io::Error::new(ErrorKind::InvalidInput, "write offset overflow")
                })?;
                bytes = &bytes[written..];
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn positioned_copy(source: &File, target: &File, target_offset: u64) -> io::Result<u64> {
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut source_offset = 0_u64;
    loop {
        let read = match source.read_at(&mut buffer, source_offset) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(source_offset);
        }
        positioned_write_all(target, target_offset + source_offset, &buffer[..read])?;
        source_offset += read as u64;
    }
}

fn sha256_file_descriptor(file: &File) -> io::Result<String> {
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut offset = 0_u64;
    loop {
        let read = match file.read_at(&mut buffer, offset) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(crate::hash::hex_lower(hasher.finish().as_ref()));
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
}

#[derive(Debug, Clone)]
struct OrdinaryTransports {
    pools: Arc<Vec<OrdinaryTransportPool>>,
    next_tie: Arc<Mutex<usize>>,
}

#[derive(Debug)]
struct OrdinaryTransportPool {
    http: reqwest::Client,
    active: AtomicUsize,
    outstanding_bytes: AtomicU64,
    observed_bytes: AtomicU64,
}

impl OrdinaryTransports {
    fn new(clients: Vec<reqwest::Client>) -> Self {
        assert!(
            !clients.is_empty(),
            "transfer runtime requires an ordinary HTTP client"
        );
        Self {
            pools: Arc::new(
                clients
                    .into_iter()
                    .map(|http| OrdinaryTransportPool {
                        http,
                        active: AtomicUsize::new(0),
                        outstanding_bytes: AtomicU64::new(0),
                        observed_bytes: AtomicU64::new(0),
                    })
                    .collect(),
            ),
            next_tie: Arc::new(Mutex::new(0)),
        }
    }

    fn len(&self) -> usize {
        self.pools.len()
    }

    fn first_client(&self) -> reqwest::Client {
        self.pools[0].http.clone()
    }

    fn acquire(&self, candidate_bytes: u64) -> OrdinaryTransportLease {
        let mut next_tie = self.next_tie.lock().expect("transport tie lock poisoned");
        let bootstrap_capacity = observed_capacity(&self.pools);
        let len = self.pools.len();
        let id = (0..len)
            .map(|offset| (next_tie.wrapping_add(offset)) % len)
            .min_by(|left, right| {
                transport_finish_score(&self.pools[*left], candidate_bytes, bootstrap_capacity)
                    .total_cmp(&transport_finish_score(
                        &self.pools[*right],
                        candidate_bytes,
                        bootstrap_capacity,
                    ))
            })
            .expect("ordinary transport exists");
        *next_tie = (id + 1) % len;
        let pool = &self.pools[id];
        let active_at_admission = pool.active.fetch_add(1, Ordering::Relaxed) + 1;
        let outstanding_at_admission = pool
            .outstanding_bytes
            .fetch_add(candidate_bytes, Ordering::Relaxed)
            .saturating_add(candidate_bytes);
        let observed_at_admission = pool.observed_bytes.load(Ordering::Relaxed);
        let effective_capacity = if observed_at_admission == 0 {
            bootstrap_capacity
        } else {
            observed_at_admission
        };
        OrdinaryTransportLease {
            id,
            active_at_admission,
            outstanding_at_admission,
            observed_at_admission,
            work_capacity_ratio: outstanding_at_admission as f64 / effective_capacity as f64,
            remaining_bytes: candidate_bytes,
            pools: Arc::clone(&self.pools),
        }
    }
}

fn observed_capacity(pools: &[OrdinaryTransportPool]) -> u64 {
    let mut observed = pools
        .iter()
        .map(|pool| pool.observed_bytes.load(Ordering::Relaxed))
        .filter(|bytes| *bytes > 0)
        .collect::<Vec<_>>();
    if observed.is_empty() {
        return 1;
    }
    observed.sort_unstable();
    observed[observed.len() / 2]
}

/// Project each pool's remaining work against the useful capacity it has demonstrated during
/// this install. All pools begin their initial work together, so the shared elapsed-time factor
/// in `work / (observed_bytes / elapsed)` cancels when comparing candidates. A pool without body
/// bytes yet borrows the median observation instead of winning merely because it is unknown.
fn transport_finish_score(
    pool: &OrdinaryTransportPool,
    candidate_bytes: u64,
    bootstrap_capacity: u64,
) -> f64 {
    let observed = pool.observed_bytes.load(Ordering::Relaxed);
    let capacity = if observed == 0 {
        bootstrap_capacity
    } else {
        observed
    };
    let work = pool
        .outstanding_bytes
        .load(Ordering::Relaxed)
        .saturating_add(candidate_bytes);
    work as f64 / capacity as f64
}

#[derive(Debug)]
struct OrdinaryTransportLease {
    id: usize,
    active_at_admission: usize,
    outstanding_at_admission: u64,
    observed_at_admission: u64,
    work_capacity_ratio: f64,
    remaining_bytes: u64,
    pools: Arc<Vec<OrdinaryTransportPool>>,
}

impl OrdinaryTransportLease {
    fn assignment(&self) -> TransportAssignment {
        TransportAssignment {
            id: self.id,
            active: self.active_at_admission,
            outstanding_bytes: self.outstanding_at_admission,
            observed_bytes: self.observed_at_admission,
            work_capacity_ratio: self.work_capacity_ratio,
        }
    }

    fn client(&self) -> &reqwest::Client {
        &self.pools[self.id].http
    }

    fn record_bytes(&mut self, bytes: u64) {
        let received = bytes.min(self.remaining_bytes);
        self.remaining_bytes -= received;
        let pool = &self.pools[self.id];
        pool.observed_bytes.fetch_add(bytes, Ordering::Relaxed);
        pool.outstanding_bytes
            .fetch_sub(received, Ordering::Relaxed);
    }
}

impl Drop for OrdinaryTransportLease {
    fn drop(&mut self) {
        let pool = &self.pools[self.id];
        pool.outstanding_bytes
            .fetch_sub(self.remaining_bytes, Ordering::Relaxed);
        pool.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The one install-wide authority for request admission, active artifact tails, retries,
/// emergency capacity, and transfer diagnostics. Artifact sessions own only their staging file
/// and range completion state.
#[derive(Debug, Clone)]
pub struct TransferRuntime {
    transport: GhcrTransport,
    ordinary_http: OrdinaryTransports,
    emergency_http: reqwest::Client,
    ordinary: RequestBudget,
    emergency: Arc<Semaphore>,
    activity: Arc<Mutex<RuntimeActivity>>,
    next_attempt_id: Arc<AtomicU64>,
    transport_profile: String,
    policy: TransferPolicy,
}

impl TransferRuntime {
    pub fn new(
        ordinary_http: Vec<reqwest::Client>,
        emergency_http: reqwest::Client,
        transport_profile: impl Into<String>,
    ) -> Self {
        Self::with_policy_and_profile(
            ordinary_http,
            emergency_http,
            TransferPolicy::default(),
            transport_profile.into(),
        )
    }

    #[cfg(test)]
    fn with_policy(
        ordinary_http: Vec<reqwest::Client>,
        emergency_http: reqwest::Client,
        policy: TransferPolicy,
    ) -> Self {
        Self::with_policy_and_profile(ordinary_http, emergency_http, policy, "test".to_string())
    }

    fn with_policy_and_profile(
        ordinary_http: Vec<reqwest::Client>,
        emergency_http: reqwest::Client,
        policy: TransferPolicy,
        transport_profile: String,
    ) -> Self {
        let ordinary_http = OrdinaryTransports::new(ordinary_http);
        Self {
            transport: GhcrTransport::new(ordinary_http.first_client()),
            ordinary_http,
            emergency_http,
            ordinary: RequestBudget::new(policy.ordinary_attempts),
            emergency: Arc::new(Semaphore::new(policy.emergency_attempts)),
            activity: Arc::new(Mutex::new(RuntimeActivity::default())),
            next_attempt_id: Arc::new(AtomicU64::new(1)),
            transport_profile,
            policy,
        }
    }

    pub async fn configure_install_token<'a>(
        &self,
        urls: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        self.transport.configure_install_token(urls).await
    }

    fn next_ordinary_transport(&self, candidate_bytes: u64) -> OrdinaryTransportLease {
        self.ordinary_http.acquire(candidate_bytes)
    }

    pub async fn download_blob_to_path<F>(
        &self,
        request: TransferRequest<'_>,
        on_progress: Arc<F>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let (header_name, header_value) =
            self.transport.auth_header_for_blob_url(request.url).await?;
        self.download_to_path(request, Some((header_name, header_value)), on_progress)
            .await
    }

    #[cfg(test)]
    async fn download_with_header_to_path<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_sha256: &str,
        expected_size: Option<u64>,
        auth_header: Option<(String, String)>,
        on_progress: Arc<F>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        self.download_to_path(
            TransferRequest {
                artifact_id: "test-artifact",
                url,
                dest,
                expected_sha256,
                expected_size,
                priority: 0,
            },
            auth_header,
            on_progress,
        )
        .await
    }

    async fn download_to_path<F>(
        &self,
        request: TransferRequest<'_>,
        auth_header: Option<(String, String)>,
        on_progress: Arc<F>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let TransferRequest {
            artifact_id,
            url,
            dest,
            expected_sha256,
            expected_size,
            priority,
        } = request;
        let trace = TransferTrace::new(
            artifact_id,
            self.ordinary_http.len(),
            self.transport_profile.clone(),
        );
        let Some(expected_size) = expected_size else {
            return self
                .download_unknown_size_to_path(
                    url,
                    dest,
                    expected_sha256,
                    auth_header,
                    priority,
                    on_progress,
                    trace,
                )
                .await;
        };
        if expected_size == 0 {
            bail!("artifact {url} has an invalid expected size of zero");
        }

        let staging = StagingFile::create(dest, Some(expected_size)).await?;
        let ranges = segment_ranges(expected_size, &self.policy);
        let range_count = ranges.len();
        let activity = self.register_artifact(priority, range_count);
        trace.record(TransferEvent::artifact(
            "artifact_started",
            priority,
            range_count,
        ));
        let is_whole_request = range_count == 1;
        let streaming_hasher = is_whole_request.then(|| {
            Arc::new(Mutex::new(Some(ring::digest::Context::new(
                &ring::digest::SHA256,
            ))))
        });
        let downloaded = Arc::new(AtomicU64::new(0));
        let last_reported = Arc::new(AtomicU64::new(0));
        let last_reported_at = Arc::new(Mutex::new(Instant::now()));
        let mut tasks = JoinSet::new();
        let mut next_range = 0_usize;
        let window = self.policy.ordinary_attempts.min(range_count).max(1);
        while next_range < window {
            spawn_segment(
                &mut tasks,
                SegmentJob {
                    runtime: self.clone(),
                    activity: activity.handle(),
                    trace: Arc::clone(&trace),
                    url: url.to_string(),
                    dest: dest.to_path_buf(),
                    staging: staging.clone(),
                    auth_header: auth_header.clone(),
                    range: ranges[next_range],
                    expected_size,
                    initial_whole_request: is_whole_request,
                    segment_index: next_range,
                    priority,
                    downloaded: Arc::clone(&downloaded),
                    last_reported: Arc::clone(&last_reported),
                    last_reported_at: Arc::clone(&last_reported_at),
                    on_progress: Arc::clone(&on_progress),
                    streaming_hasher: streaming_hasher.clone(),
                },
            );
            next_range += 1;
        }

        let mut segment_reports = Vec::with_capacity(range_count);
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(report)) => {
                    segment_reports.push(report);
                    if next_range < range_count {
                        spawn_segment(
                            &mut tasks,
                            SegmentJob {
                                runtime: self.clone(),
                                activity: activity.handle(),
                                trace: Arc::clone(&trace),
                                url: url.to_string(),
                                dest: dest.to_path_buf(),
                                staging: staging.clone(),
                                auth_header: auth_header.clone(),
                                range: ranges[next_range],
                                expected_size,
                                initial_whole_request: false,
                                segment_index: next_range,
                                priority,
                                downloaded: Arc::clone(&downloaded),
                                last_reported: Arc::clone(&last_reported),
                                last_reported_at: Arc::clone(&last_reported_at),
                                on_progress: Arc::clone(&on_progress),
                                streaming_hasher: streaming_hasher.clone(),
                            },
                        );
                        next_range += 1;
                    }
                }
                Ok(Err(error)) => {
                    tasks.shutdown().await;
                    cleanup_emergency_files(dest).await;
                    return Err(self.transfer_failure(&trace, range_count as u32, 0.0, 0.0, error));
                }
                Err(error) => {
                    tasks.shutdown().await;
                    cleanup_emergency_files(dest).await;
                    return Err(self.transfer_failure(
                        &trace,
                        range_count as u32,
                        0.0,
                        0.0,
                        anyhow::anyhow!("download range task failed: {error}"),
                    ));
                }
            }
        }

        on_progress.as_ref()(expected_size);
        let used_emergency = segment_reports.iter().any(|report| report.emergencies > 0);
        let verify_started = Instant::now();
        trace.record(TransferEvent::simple("verification_started"));
        let actual = if !used_emergency {
            match streaming_hasher {
                Some(hasher) => {
                    let context = hasher
                        .lock()
                        .expect("streaming hash lock poisoned")
                        .take()
                        .expect("streaming hash context already consumed");
                    crate::hash::hex_lower(context.finish().as_ref())
                }
                None => staging.sha256(dest).await?,
            }
        } else {
            staging.sha256(dest).await?
        };
        let verify_seconds = verify_started.elapsed().as_secs_f64();
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            trace.record(TransferEvent::simple("verification_failed"));
            return Err(self.transfer_failure(
                &trace,
                range_count as u32,
                verify_seconds,
                0.0,
                Sha256Mismatch::new(url, expected_sha256, actual).into(),
            ));
        }
        trace.record(TransferEvent::simple("verification_completed"));
        let sync_started = Instant::now();
        staging.sync_all(dest).await?;
        let sync_seconds = sync_started.elapsed().as_secs_f64();
        trace.record(TransferEvent::simple("artifact_completed"));

        segment_reports.sort_by_key(|report| report.start);
        let diagnostics = trace.snapshot(
            range_count as u32,
            verify_seconds,
            sync_seconds,
            self.ordinary.active(),
            self.emergency_active(),
        );
        drop(activity);
        Ok(TransferReport {
            segments: segment_reports,
            staging,
            diagnostics,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_unknown_size_to_path<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_sha256: &str,
        auth_header: Option<(String, String)>,
        priority: u64,
        on_progress: Arc<F>,
        trace: Arc<TransferTrace>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let staging = StagingFile::create(dest, None).await?;
        let activity = self.register_artifact(priority, 1);
        trace.record(TransferEvent::artifact("artifact_started", priority, 1));
        let mut retries = 0_u32;
        let mut max_reported = 0_u64;
        loop {
            let queued = Instant::now();
            let permit = self
                .ordinary
                .acquire(priority)
                .await
                .ok_or_else(|| anyhow::anyhow!("download request budget closed"))?;
            let attempt_id = self.next_attempt_id();
            let mut transport = self.next_ordinary_transport(self.policy.target_part_size);
            trace.attempt_started(
                attempt_id,
                AttemptKind::from_retries(retries),
                ByteRange { start: 0, end: 0 },
                queued.elapsed(),
                self.counts(),
                Some(transport.assignment()),
            );
            let mut request = transport.client().get(url);
            if let Some((name, value)) = &auth_header {
                request = request.header(name, value);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    let failure = AttemptFailure::Request(error);
                    trace.attempt_failed(attempt_id, &failure, self.counts());
                    drop(transport);
                    drop(permit);
                    retries = retries.saturating_add(1);
                    let delay = self.policy.retry_delay(retries);
                    trace.retry_scheduled(attempt_id, delay, self.counts());
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            trace.response_headers(
                attempt_id,
                response.version(),
                response.status(),
                response_origin(response.url()),
                self.counts(),
            );
            if response.status() != StatusCode::OK {
                let retry_after = retry_after(response.headers());
                let failure = AttemptFailure::RetryableStatus {
                    status: response.status(),
                    retry_after,
                };
                trace.attempt_failed(attempt_id, &failure, self.counts());
                drop(transport);
                drop(permit);
                retries = retries.saturating_add(1);
                let delay = retry_after.unwrap_or_else(|| self.policy.retry_delay(retries));
                trace.retry_scheduled(attempt_id, delay, self.counts());
                tokio::time::sleep(delay).await;
                continue;
            }

            staging.truncate(dest).await?;
            let mut stream = response.bytes_stream();
            let mut received = 0_u64;
            let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
            let mut body_error = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        let chunk_len = chunk.len() as u64;
                        transport.record_bytes(chunk_len);
                        trace.wire_bytes.fetch_add(chunk_len, Ordering::Relaxed);
                        let mut event = TransferEvent::simple(if received == 0 {
                            "first_body_byte"
                        } else {
                            "attempt_progress"
                        });
                        event.attempt = Some(attempt_id);
                        event.bytes = Some(received + chunk_len);
                        event.ordinary_in_flight = self.ordinary.active();
                        event.emergency_in_flight = self.emergency_active();
                        trace.record(event);
                        let chunk = staging.write_all_at(dest, received, chunk).await?;
                        hasher.update(&chunk);
                        received += chunk.len() as u64;
                        if received > max_reported {
                            trace.logical_progress(received - max_reported);
                            max_reported = received;
                            on_progress.as_ref()(max_reported);
                        }
                    }
                    Err(error) => {
                        body_error = Some(AttemptFailure::Body(error));
                        break;
                    }
                }
            }
            drop(transport);
            drop(permit);
            if let Some(error) = body_error {
                trace.attempt_failed(attempt_id, &error, self.counts());
                retries = retries.saturating_add(1);
                let delay = self.policy.retry_delay(retries);
                trace.retry_scheduled(attempt_id, delay, self.counts());
                tokio::time::sleep(delay).await;
                continue;
            }
            trace.attempt_completed(attempt_id, received, self.counts());
            let actual = crate::hash::hex_lower(hasher.finish().as_ref());
            if !actual.eq_ignore_ascii_case(expected_sha256) {
                trace.record(TransferEvent::simple("verification_failed"));
                return Err(self.transfer_failure(
                    &trace,
                    1,
                    0.0,
                    0.0,
                    Sha256Mismatch::new(url, expected_sha256, actual).into(),
                ));
            }
            let sync_started = Instant::now();
            staging.sync_all(dest).await?;
            let sync_seconds = sync_started.elapsed().as_secs_f64();
            activity.mark_range_complete();
            trace.record(TransferEvent::simple("artifact_completed"));
            let diagnostics = trace.snapshot(
                1,
                0.0,
                sync_seconds,
                self.ordinary.active(),
                self.emergency_active(),
            );
            drop(activity);
            return Ok(TransferReport {
                segments: vec![SegmentReport {
                    start: 0,
                    end: received.saturating_sub(1),
                    attempts: retries.saturating_add(1),
                    emergencies: 0,
                    resumed: retries > 0,
                }],
                staging,
                diagnostics,
            });
        }
    }

    fn transfer_failure(
        &self,
        trace: &TransferTrace,
        segments: u32,
        verify_seconds: f64,
        sync_seconds: f64,
        source: anyhow::Error,
    ) -> anyhow::Error {
        TransferFailure {
            diagnostics: trace.snapshot(
                segments,
                verify_seconds,
                sync_seconds,
                self.ordinary.active(),
                self.emergency_active(),
            ),
            source,
        }
        .into()
    }

    fn register_artifact(&self, priority: u64, ranges: usize) -> ArtifactRegistration {
        let mut state = self
            .activity
            .lock()
            .expect("transfer activity lock poisoned");
        let id = state.next_artifact_id;
        state.next_artifact_id = state.next_artifact_id.wrapping_add(1);
        state.artifacts.insert(
            id,
            ActiveArtifact {
                priority,
                total_ranges: ranges,
                unfinished_ranges: ranges,
            },
        );
        ArtifactRegistration {
            handle: ArtifactActivity {
                runtime: self.clone(),
                id,
            },
            registered: true,
        }
    }

    fn tail_eligible(&self, id: u64) -> Option<TailEvidence> {
        let state = self
            .activity
            .lock()
            .expect("transfer activity lock poisoned");
        let artifact = state.artifacts.get(&id)?;
        let highest_priority = state.artifacts.values().map(|item| item.priority).min()?;
        let at_artifact_tail = artifact.unfinished_ranges <= 2;
        let established_multipart_tail = artifact.total_ranges > 2;
        let install_tail = state.artifacts.len() <= 2;
        (artifact.priority == highest_priority
            && at_artifact_tail
            && (established_multipart_tail || install_tail))
            .then_some(TailEvidence {
                unfinished_ranges: artifact.unfinished_ranges,
                active_artifacts: state.artifacts.len(),
            })
    }

    fn try_emergency(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.emergency).try_acquire_owned().ok()
    }

    fn emergency_active(&self) -> usize {
        self.policy
            .emergency_attempts
            .saturating_sub(self.emergency.available_permits())
    }

    fn counts(&self) -> RequestCounts {
        RequestCounts {
            ordinary: self.ordinary.active(),
            emergency: self.emergency_active(),
        }
    }

    fn next_attempt_id(&self) -> u64 {
        self.next_attempt_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn spawn_segment<F>(tasks: &mut JoinSet<Result<SegmentReport>>, job: SegmentJob<F>)
where
    F: Fn(u64) + Send + Sync + 'static,
{
    tasks.spawn(async move { run_segment(job).await });
}

#[derive(Debug, Default)]
struct RuntimeActivity {
    next_artifact_id: u64,
    artifacts: BTreeMap<u64, ActiveArtifact>,
}

#[derive(Debug)]
struct ActiveArtifact {
    priority: u64,
    total_ranges: usize,
    unfinished_ranges: usize,
}

struct ArtifactRegistration {
    handle: ArtifactActivity,
    registered: bool,
}

impl ArtifactRegistration {
    fn handle(&self) -> ArtifactActivity {
        self.handle.clone()
    }

    fn mark_range_complete(&self) {
        self.handle.mark_range_complete();
    }
}

impl Drop for ArtifactRegistration {
    fn drop(&mut self) {
        if self.registered {
            self.handle
                .runtime
                .activity
                .lock()
                .expect("transfer activity lock poisoned")
                .artifacts
                .remove(&self.handle.id);
            self.registered = false;
        }
    }
}

#[derive(Clone)]
struct ArtifactActivity {
    runtime: TransferRuntime,
    id: u64,
}

impl ArtifactActivity {
    fn mark_range_complete(&self) {
        if let Some(artifact) = self
            .runtime
            .activity
            .lock()
            .expect("transfer activity lock poisoned")
            .artifacts
            .get_mut(&self.id)
        {
            artifact.unfinished_ranges = artifact.unfinished_ranges.saturating_sub(1);
        }
    }

    fn tail_eligible(&self) -> Option<TailEvidence> {
        self.runtime.tail_eligible(self.id)
    }
}

#[derive(Debug, Clone, Copy)]
struct TailEvidence {
    unfinished_ranges: usize,
    active_artifacts: usize,
}

pub(crate) struct TransferRequest<'a> {
    pub(crate) artifact_id: &'a str,
    pub(crate) url: &'a str,
    pub(crate) dest: &'a Path,
    pub(crate) expected_sha256: &'a str,
    pub(crate) expected_size: Option<u64>,
    pub(crate) priority: u64,
}

#[derive(Debug, Clone)]
struct TransferPolicy {
    ordinary_attempts: usize,
    emergency_attempts: usize,
    emergency_per_range: u32,
    retry_base: Duration,
    retry_max: Duration,
    monitor_interval: Duration,
    emergency_warmup: Duration,
    stalled_for: Duration,
    pathological_remaining: Duration,
    max_emergency_rate: f64,
    rolling_window: Duration,
    multipart_threshold: u64,
    target_part_size: u64,
}

impl TransferPolicy {
    fn retry_delay(&self, retry: u32) -> Duration {
        let shift = retry.saturating_sub(1).min(8);
        let multiplier = 1_u32 << shift;
        let base = self
            .retry_base
            .saturating_mul(multiplier)
            .min(self.retry_max);
        base + Duration::from_millis((retry as u64 * 37) % 97)
    }
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            ordinary_attempts: 16,
            emergency_attempts: 2,
            emergency_per_range: 1,
            retry_base: Duration::from_millis(250),
            retry_max: Duration::from_secs(30),
            monitor_interval: Duration::from_millis(250),
            emergency_warmup: Duration::from_secs(3),
            stalled_for: Duration::from_secs(2),
            pathological_remaining: Duration::from_secs(10),
            max_emergency_rate: 128_000.0,
            rolling_window: Duration::from_secs(3),
            multipart_threshold: MIN_MULTIPART_SIZE,
            target_part_size: TARGET_PART_SIZE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }

    fn remaining_after(self, received: u64) -> Self {
        Self {
            start: self.start + received,
            end: self.end,
        }
    }

    fn header_value(self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AttemptKind {
    Initial,
    Retry,
    Emergency,
}

impl AttemptKind {
    fn from_retries(retries: u32) -> Self {
        if retries == 0 {
            Self::Initial
        } else {
            Self::Retry
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum AttemptFailure {
    #[error("request failed")]
    Request(#[source] reqwest::Error),
    #[error("response body failed")]
    Body(#[source] reqwest::Error),
    #[error("retryable HTTP status {status}")]
    RetryableStatus {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    #[error("response ended early: expected {expected} bytes, got {received}")]
    EarlyEof { expected: u64, received: u64 },
    #[error("response exceeded expected length {expected}")]
    Overlong { expected: u64 },
    #[error("invalid range response: {0}")]
    InvalidRange(String),
    #[error("local I/O failed: {0}")]
    LocalIo(String),
    #[error("download request budget closed")]
    BudgetClosed,
}

impl AttemptFailure {
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Request(_) | Self::Body(_) | Self::RetryableStatus { .. } | Self::EarlyEof { .. }
        )
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RetryableStatus { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("artifact transfer failed")]
pub(crate) struct TransferFailure {
    diagnostics: TransferDiagnostics,
    #[source]
    source: anyhow::Error,
}

impl TransferFailure {
    pub(crate) fn diagnostics(&self) -> &TransferDiagnostics {
        &self.diagnostics
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TransferReport {
    segments: Vec<SegmentReport>,
    staging: StagingFile,
    diagnostics: TransferDiagnostics,
}

impl TransferReport {
    pub(crate) fn diagnostics(&self) -> &TransferDiagnostics {
        &self.diagnostics
    }

    pub(crate) async fn validate_staging_path(&self, path: &Path) -> Result<()> {
        self.staging.validate_path_identity(path).await
    }

    pub(crate) fn validate(&self, expected_size: Option<u64>) -> Result<()> {
        if self.segments.is_empty() {
            bail!("transfer completed without range reports");
        }
        for segment in &self.segments {
            if segment.attempts == 0 || segment.emergencies > segment.attempts {
                bail!("transfer produced an invalid attempt summary");
            }
            if segment.resumed && segment.attempts < 2 {
                bail!("transfer marked a range retried without a second attempt");
            }
        }
        if let Some(expected_size) = expected_size {
            let mut next = 0_u64;
            for segment in &self.segments {
                if segment.start != next || segment.end < segment.start {
                    bail!("transfer produced non-contiguous range reports");
                }
                next = segment.end + 1;
            }
            if next != expected_size {
                bail!("transfer range reports cover {next} bytes, expected {expected_size}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SegmentReport {
    start: u64,
    end: u64,
    attempts: u32,
    emergencies: u32,
    resumed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TransferDiagnostics {
    artifact_id: String,
    transport_profile: String,
    transport_count: usize,
    segments: u32,
    attempts: u32,
    retries: u32,
    emergencies: u32,
    /// Compatibility alias for older trace analysis. New consumers should use `emergencies`.
    hedges: u32,
    /// Compatibility alias for the unique artifact byte count.
    bytes: u64,
    logical_bytes: u64,
    wire_bytes: u64,
    transfer_seconds: f64,
    /// Earliest observed request-relative first-body-byte latency, excluding admission queueing.
    first_byte_seconds: Option<f64>,
    /// Earliest body byte relative to the start of this artifact operation.
    first_body_at_seconds: Option<f64>,
    queue_seconds: f64,
    verify_seconds: f64,
    sync_seconds: f64,
    ordinary_in_flight_at_completion: usize,
    emergency_in_flight_at_completion: usize,
    events: Vec<TransferEvent>,
}

impl TransferDiagnostics {
    pub(crate) fn subphase_boundaries(&self) -> Option<[f64; 4]> {
        let first_body = self.first_body_at_seconds?;
        let body_complete =
            (self.transfer_seconds - self.verify_seconds - self.sync_seconds).max(first_body);
        let verify_complete = body_complete + self.verify_seconds;
        Some([
            first_body,
            body_complete,
            verify_complete,
            self.transfer_seconds,
        ])
    }
}

#[derive(Debug, Clone, Serialize)]
struct TransferEvent {
    event: String,
    at_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<AttemptKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<ByteRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_in_flight: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_outstanding_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_observed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_work_capacity_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_in_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rolling_bytes_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_remaining_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unfinished_ranges: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_artifacts: Option<usize>,
    ordinary_in_flight: usize,
    emergency_in_flight: usize,
}

impl TransferEvent {
    fn simple(event: &str) -> Self {
        Self {
            event: event.to_string(),
            at_seconds: 0.0,
            attempt: None,
            reason: None,
            range: None,
            bytes: None,
            queue_seconds: None,
            http_version: None,
            transport_id: None,
            transport_in_flight: None,
            transport_outstanding_bytes: None,
            transport_observed_bytes: None,
            transport_work_capacity_ratio: None,
            response_origin: None,
            status: None,
            error: None,
            retry_in_seconds: None,
            rolling_bytes_per_second: None,
            estimated_remaining_seconds: None,
            priority: None,
            unfinished_ranges: None,
            active_artifacts: None,
            ordinary_in_flight: 0,
            emergency_in_flight: 0,
        }
    }

    fn artifact(event: &str, priority: u64, ranges: usize) -> Self {
        Self {
            priority: Some(priority),
            unfinished_ranges: Some(ranges),
            ..Self::simple(event)
        }
    }

    fn attempt(event: &str, attempt: u64, reason: AttemptKind, range: ByteRange) -> Self {
        Self {
            attempt: Some(attempt),
            reason: Some(reason),
            range: Some(range),
            ..Self::simple(event)
        }
    }
}

#[derive(Debug)]
struct TransferTrace {
    artifact_id: String,
    transport_profile: String,
    transport_count: usize,
    started: Instant,
    events: Mutex<Vec<TransferEvent>>,
    attempts: AtomicU32,
    retries: AtomicU32,
    emergencies: AtomicU32,
    logical_bytes: AtomicU64,
    wire_bytes: AtomicU64,
    queue_micros: AtomicU64,
}

impl TransferTrace {
    fn new(artifact_id: &str, transport_count: usize, transport_profile: String) -> Arc<Self> {
        Arc::new(Self {
            artifact_id: artifact_id.to_string(),
            transport_profile,
            transport_count,
            started: Instant::now(),
            events: Mutex::new(Vec::new()),
            attempts: AtomicU32::new(0),
            retries: AtomicU32::new(0),
            emergencies: AtomicU32::new(0),
            logical_bytes: AtomicU64::new(0),
            wire_bytes: AtomicU64::new(0),
            queue_micros: AtomicU64::new(0),
        })
    }

    fn record(&self, mut event: TransferEvent) {
        event.at_seconds = self.started.elapsed().as_secs_f64();
        self.events
            .lock()
            .expect("transfer event lock poisoned")
            .push(event);
    }

    fn attempt_queued(&self, id: u64, kind: AttemptKind, range: ByteRange, counts: RequestCounts) {
        let mut event = TransferEvent::attempt("attempt_queued", id, kind, range);
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn attempt_started(
        &self,
        id: u64,
        kind: AttemptKind,
        range: ByteRange,
        queue: Duration,
        counts: RequestCounts,
        transport: Option<TransportAssignment>,
    ) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        if kind == AttemptKind::Retry {
            self.retries.fetch_add(1, Ordering::Relaxed);
        } else if kind == AttemptKind::Emergency {
            self.emergencies.fetch_add(1, Ordering::Relaxed);
        }
        self.queue_micros.fetch_add(
            queue.as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        let mut event = TransferEvent::attempt("attempt_started", id, kind, range);
        event.queue_seconds = Some(queue.as_secs_f64());
        if let Some(transport) = transport {
            event.transport_id = Some(transport.id);
            event.transport_in_flight = Some(transport.active);
            event.transport_outstanding_bytes = Some(transport.outstanding_bytes);
            event.transport_observed_bytes = Some(transport.observed_bytes);
            event.transport_work_capacity_ratio = Some(transport.work_capacity_ratio);
        }
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn response_headers(
        &self,
        id: u64,
        version: reqwest::Version,
        status: StatusCode,
        response_origin: Option<String>,
        counts: RequestCounts,
    ) {
        let mut event = TransferEvent::simple("response_headers");
        event.attempt = Some(id);
        event.http_version = Some(format!("{version:?}"));
        event.response_origin = response_origin;
        event.status = Some(status.as_u16());
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn logical_progress(&self, bytes: u64) {
        self.logical_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn attempt_completed(&self, id: u64, bytes: u64, counts: RequestCounts) {
        let mut event = TransferEvent::simple("attempt_completed");
        event.attempt = Some(id);
        event.bytes = Some(bytes);
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn attempt_failed(&self, id: u64, error: &AttemptFailure, counts: RequestCounts) {
        let mut event = TransferEvent::simple("attempt_failed");
        event.attempt = Some(id);
        event.error = Some(format!("{error:?}"));
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn attempt_cancelled(&self, id: u64, counts: RequestCounts) {
        let mut event = TransferEvent::simple("attempt_cancelled");
        event.attempt = Some(id);
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn retry_scheduled(&self, id: u64, delay: Duration, counts: RequestCounts) {
        let mut event = TransferEvent::simple("range_requeued");
        event.attempt = Some(id);
        event.retry_in_seconds = Some(delay.as_secs_f64());
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn emergency_triggered(
        &self,
        id: u64,
        range: ByteRange,
        priority: u64,
        tail: TailEvidence,
        health: AttemptHealthSnapshot,
        counts: RequestCounts,
    ) {
        let mut event =
            TransferEvent::attempt("emergency_triggered", id, AttemptKind::Emergency, range);
        event.priority = Some(priority);
        event.unfinished_ranges = Some(tail.unfinished_ranges);
        event.active_artifacts = Some(tail.active_artifacts);
        event.rolling_bytes_per_second = Some(health.rate);
        event.estimated_remaining_seconds = Some(health.estimated_remaining.as_secs_f64());
        event.ordinary_in_flight = counts.ordinary;
        event.emergency_in_flight = counts.emergency;
        self.record(event);
    }

    fn snapshot(
        &self,
        segments: u32,
        verify_seconds: f64,
        sync_seconds: f64,
        ordinary: usize,
        emergency: usize,
    ) -> TransferDiagnostics {
        let events = self
            .events
            .lock()
            .expect("transfer event lock poisoned")
            .clone();
        let attempts = self.attempts.load(Ordering::Relaxed);
        let retries = self.retries.load(Ordering::Relaxed);
        let emergencies = self.emergencies.load(Ordering::Relaxed);
        let logical_bytes = self.logical_bytes.load(Ordering::Relaxed);
        TransferDiagnostics {
            artifact_id: self.artifact_id.clone(),
            transport_profile: self.transport_profile.clone(),
            transport_count: self.transport_count,
            segments,
            attempts,
            retries,
            emergencies,
            hedges: emergencies,
            bytes: logical_bytes,
            logical_bytes,
            wire_bytes: self.wire_bytes.load(Ordering::Relaxed),
            transfer_seconds: self.started.elapsed().as_secs_f64(),
            first_byte_seconds: first_body_byte_latency(&events),
            first_body_at_seconds: events
                .iter()
                .filter(|event| event.event == "first_body_byte")
                .map(|event| event.at_seconds)
                .min_by(f64::total_cmp),
            queue_seconds: self.queue_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            verify_seconds,
            sync_seconds,
            ordinary_in_flight_at_completion: ordinary,
            emergency_in_flight_at_completion: emergency,
            events,
        }
    }
}

fn first_body_byte_latency(events: &[TransferEvent]) -> Option<f64> {
    let starts = events
        .iter()
        .filter(|event| event.event == "attempt_started")
        .filter_map(|event| event.attempt.map(|attempt| (attempt, event.at_seconds)))
        .collect::<BTreeMap<_, _>>();
    events
        .iter()
        .filter(|event| event.event == "first_body_byte")
        .filter_map(|event| {
            let started = starts.get(&event.attempt?)?;
            Some((event.at_seconds - started).max(0.0))
        })
        .min_by(f64::total_cmp)
}

#[derive(Debug, Clone, Copy)]
struct TransportAssignment {
    id: usize,
    active: usize,
    outstanding_bytes: u64,
    observed_bytes: u64,
    work_capacity_ratio: f64,
}

#[derive(Debug, Clone, Copy)]
struct RequestCounts {
    ordinary: usize,
    emergency: usize,
}

#[derive(Debug)]
struct AttemptProgress {
    request_started: Mutex<Option<Instant>>,
    last_progress: Mutex<Option<Instant>>,
    samples: Mutex<VecDeque<(Instant, u64)>>,
    received: AtomicU64,
    first_byte_seen: std::sync::atomic::AtomicBool,
    last_event_bytes: AtomicU64,
    last_event_at: Mutex<Instant>,
    rolling_window: Duration,
}

impl AttemptProgress {
    fn new(rolling_window: Duration) -> Self {
        Self {
            request_started: Mutex::new(None),
            last_progress: Mutex::new(None),
            samples: Mutex::new(VecDeque::new()),
            received: AtomicU64::new(0),
            first_byte_seen: std::sync::atomic::AtomicBool::new(false),
            last_event_bytes: AtomicU64::new(0),
            last_event_at: Mutex::new(Instant::now()),
            rolling_window,
        }
    }

    fn request_started(&self) {
        let now = Instant::now();
        *self
            .request_started
            .lock()
            .expect("attempt progress lock poisoned") = Some(now);
        *self
            .last_progress
            .lock()
            .expect("attempt progress lock poisoned") = Some(now);
        self.samples
            .lock()
            .expect("attempt samples lock poisoned")
            .push_back((now, 0));
    }

    fn progress(&self, bytes: u64) -> ProgressSample {
        let now = Instant::now();
        let total = self.received.fetch_add(bytes, Ordering::Relaxed) + bytes;
        *self
            .last_progress
            .lock()
            .expect("attempt progress lock poisoned") = Some(now);
        let mut samples = self.samples.lock().expect("attempt samples lock poisoned");
        samples.push_back((now, total));
        while samples.len() > 1
            && now.duration_since(samples.front().expect("sample exists").0) > self.rolling_window
        {
            samples.pop_front();
        }
        let first = !self.first_byte_seen.swap(true, Ordering::Relaxed);
        let previous = self.last_event_bytes.load(Ordering::Relaxed);
        let mut last_event_at = self
            .last_event_at
            .lock()
            .expect("attempt event lock poisoned");
        let report = first
            || total >= previous.saturating_add(PROGRESS_TRACE_BYTES)
            || last_event_at.elapsed() >= PROGRESS_TRACE_INTERVAL;
        if report {
            self.last_event_bytes.store(total, Ordering::Relaxed);
            *last_event_at = now;
        }
        ProgressSample {
            first,
            report,
            total,
        }
    }

    fn snapshot(&self, range: ByteRange, policy: &TransferPolicy) -> Option<AttemptHealthSnapshot> {
        let now = Instant::now();
        let started = (*self
            .request_started
            .lock()
            .expect("attempt progress lock poisoned"))?;
        if now.duration_since(started) < policy.emergency_warmup {
            return None;
        }
        let received = self.received.load(Ordering::Relaxed);
        let samples = self.samples.lock().expect("attempt samples lock poisoned");
        let (sample_time, sample_bytes) = samples.front().copied().unwrap_or((started, 0));
        let elapsed = now.duration_since(sample_time).as_secs_f64();
        let rate = if elapsed > 0.0 {
            received.saturating_sub(sample_bytes) as f64 / elapsed
        } else {
            0.0
        };
        let remaining = range.len().saturating_sub(received);
        let estimated_remaining = if rate > 0.0 {
            Duration::from_secs_f64(remaining as f64 / rate)
        } else {
            Duration::MAX
        };
        let last_progress = self
            .last_progress
            .lock()
            .expect("attempt progress lock poisoned")
            .unwrap_or(started);
        let stopped = now.duration_since(last_progress) >= policy.stalled_for;
        let dribbling = rate > 0.0
            && rate <= policy.max_emergency_rate
            && estimated_remaining >= policy.pathological_remaining;
        (remaining > 0 && (stopped || dribbling)).then_some(AttemptHealthSnapshot {
            rate,
            estimated_remaining,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ProgressSample {
    first: bool,
    report: bool,
    total: u64,
}

#[derive(Debug, Clone, Copy)]
struct AttemptHealthSnapshot {
    rate: f64,
    estimated_remaining: Duration,
}

#[derive(Debug, Clone)]
struct RequestBudget {
    inner: Arc<Mutex<RequestBudgetState>>,
}

#[derive(Debug)]
struct RequestBudgetState {
    limit: usize,
    available: usize,
    next_sequence: u64,
    waiters: BTreeMap<(u64, u64), oneshot::Sender<RequestPermit>>,
}

impl RequestBudget {
    fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            inner: Arc::new(Mutex::new(RequestBudgetState {
                limit,
                available: limit,
                next_sequence: 0,
                waiters: BTreeMap::new(),
            })),
        }
    }

    async fn acquire(&self, priority: u64) -> Option<RequestPermit> {
        let (sender, receiver) = oneshot::channel();
        let key = {
            let mut state = self.inner.lock().expect("request budget lock poisoned");
            let key = (priority, state.next_sequence);
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.waiters.insert(key, sender);
            dispatch_requests(&mut state, &self.inner);
            key
        };
        let mut cleanup = WaitingRequest {
            budget: self.clone(),
            key,
            armed: true,
        };
        let permit = receiver.await.ok()?;
        cleanup.armed = false;
        Some(permit)
    }

    fn active(&self) -> usize {
        let state = self.inner.lock().expect("request budget lock poisoned");
        state.limit.saturating_sub(state.available)
    }

    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.inner
            .lock()
            .expect("request budget lock poisoned")
            .waiters
            .len()
    }
}

fn dispatch_requests(state: &mut RequestBudgetState, budget: &Arc<Mutex<RequestBudgetState>>) {
    while state.available > 0 {
        let Some((_, sender)) = state.waiters.pop_first() else {
            break;
        };
        state.available -= 1;
        let permit = RequestPermit {
            budget: Arc::clone(budget),
            armed: true,
        };
        if let Err(mut permit) = sender.send(permit) {
            permit.armed = false;
            state.available += 1;
        }
    }
}

#[derive(Debug)]
struct RequestPermit {
    budget: Arc<Mutex<RequestBudgetState>>,
    armed: bool,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.budget.lock().expect("request budget lock poisoned");
        state.available += 1;
        dispatch_requests(&mut state, &self.budget);
    }
}

struct WaitingRequest {
    budget: RequestBudget,
    key: (u64, u64),
    armed: bool,
}

impl Drop for WaitingRequest {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .budget
            .inner
            .lock()
            .expect("request budget lock poisoned");
        state.waiters.remove(&self.key);
        dispatch_requests(&mut state, &self.budget.inner);
    }
}

fn validate_content_range(
    headers: &header::HeaderMap,
    range: ByteRange,
    expected_size: u64,
    url: &str,
) -> Result<()> {
    let value = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("range download for {url} missing Content-Range"))?;
    let parsed = parse_content_range(value).ok_or_else(|| {
        anyhow::anyhow!("range download for {url} returned invalid Content-Range {value:?}")
    })?;
    if parsed != (range.start, range.end, expected_size) {
        bail!(
            "range download for {url} returned Content-Range {value:?}, expected bytes {}-{}/{}",
            range.start,
            range.end,
            expected_size
        );
    }
    Ok(())
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

fn response_origin(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?;
    let mut origin = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn report_progress<F>(
    total: u64,
    last_reported: &AtomicU64,
    last_reported_at: &Mutex<Instant>,
    on_progress: &Arc<F>,
) where
    F: Fn(u64),
{
    let mut last = last_reported.load(Ordering::Relaxed);
    loop {
        let byte_threshold = should_report(total, last);
        let time_threshold = !byte_threshold
            && last_reported_at
                .lock()
                .expect("progress report time lock poisoned")
                .elapsed()
                >= PROGRESS_REPORT_INTERVAL;
        if !byte_threshold && !time_threshold {
            break;
        }
        match last_reported.compare_exchange_weak(last, total, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => {
                *last_reported_at
                    .lock()
                    .expect("progress report time lock poisoned") = Instant::now();
                on_progress.as_ref()(total);
                break;
            }
            Err(current) => last = current,
        }
    }
}

fn segment_ranges(size: u64, policy: &TransferPolicy) -> Vec<ByteRange> {
    if !should_use_multipart(size, policy) {
        return vec![ByteRange {
            start: 0,
            end: size - 1,
        }];
    }
    let part_count = size
        .div_ceil(policy.target_part_size)
        .max(MIN_MULTIPART_PARTS) as usize;
    (0..part_count)
        .map(|index| {
            let start = index as u64 * policy.target_part_size;
            let end = (start + policy.target_part_size).min(size) - 1;
            ByteRange { start, end }
        })
        .collect()
}

fn should_use_multipart(size: u64, policy: &TransferPolicy) -> bool {
    size > policy.multipart_threshold
}

fn should_report(accumulated: u64, last_reported: u64) -> bool {
    accumulated >= last_reported.saturating_add(PROGRESS_REPORT_BYTES)
}

const PROGRESS_REPORT_BYTES: u64 = 100_000;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(250);
const PROGRESS_TRACE_BYTES: u64 = 1024 * 1024;
const PROGRESS_TRACE_INTERVAL: Duration = Duration::from_secs(1);
const MIB: u64 = 1024 * 1024;
const MIN_MULTIPART_SIZE: u64 = 10 * MIB;
const TARGET_PART_SIZE: u64 = 10 * MIB;
const MIN_MULTIPART_PARTS: u64 = 2;

#[cfg(test)]
mod tests;
