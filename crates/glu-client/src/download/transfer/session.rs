use super::{
    report_progress, validate_content_range, AttemptFailure, AttemptKind, ByteRange, HedgeDecision,
    SegmentReport, StagingFile, TransferManager,
};
use anyhow::{bail, Result};
use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::task::{AbortHandle, JoinSet};

pub(super) struct SegmentJob<F> {
    pub(super) manager: TransferManager,
    pub(super) url: String,
    pub(super) dest: PathBuf,
    pub(super) staging: StagingFile,
    pub(super) auth_header: Option<(String, String)>,
    pub(super) range: ByteRange,
    pub(super) expected_size: u64,
    pub(super) initial_whole_request: bool,
    pub(super) segment_index: usize,
    pub(super) priority: u64,
    pub(super) downloaded: Arc<AtomicU64>,
    pub(super) last_reported: Arc<AtomicU64>,
    pub(super) last_reported_at: Arc<Mutex<Instant>>,
    pub(super) on_progress: Arc<F>,
    pub(super) streaming_hasher: Option<Arc<Mutex<Option<ring::digest::Context>>>>,
}

#[derive(Debug)]
struct AttemptTaskResult {
    id: u64,
    kind: AttemptKind,
    hedge_start: Option<u64>,
    result: std::result::Result<Option<StagingFile>, AttemptFailure>,
}

struct HedgeFileCleanup {
    paths: BTreeSet<PathBuf>,
}

impl Drop for HedgeFileCleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(super) async fn run_segment<F>(job: SegmentJob<F>) -> Result<SegmentReport>
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let committed = Arc::new(AtomicU64::new(0));
    // Declared before the JoinSet so task handles are dropped and aborted before synchronous
    // cancellation cleanup unlinks any open hedge files.
    let mut hedge_cleanup = HedgeFileCleanup {
        paths: BTreeSet::new(),
    };
    let mut tasks = JoinSet::new();
    let mut primary: Option<(u64, AbortHandle)> = None;
    let mut active_hedges = BTreeMap::<u64, (AbortHandle, PathBuf)>::new();
    let mut attempts = 0_u32;
    let mut retries = 0_u32;
    let mut hedges = 0_u32;
    let mut last_primary_error: Option<AttemptFailure> = None;

    spawn_primary_attempt(
        &job,
        &committed,
        AttemptKind::Original,
        &mut tasks,
        &mut primary,
    );
    attempts += 1;
    let mut monitor = tokio::time::interval(job.manager.policy.monitor_interval);
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            joined = tasks.join_next(), if !tasks.is_empty() => {
                let Some(joined) = joined else { continue };
                let outcome = joined.map_err(|error| anyhow::anyhow!("segment attempt task failed: {error}"))?;
                match outcome.kind {
                    AttemptKind::Original | AttemptKind::Resume => {
                        primary = None;
                        match outcome.result {
                            Ok(None) => {
                                abort_and_drain(&mut tasks).await;
                                cleanup_paths(active_hedges.into_values().map(|(_, path)| path)).await;
                                return Ok(SegmentReport {
                                    start: job.range.start,
                                    end: job.range.end,
                                    attempts,
                                    hedges,
                                    resumed: retries > 0,
                                });
                            }
                            Ok(Some(_)) => unreachable!("primary attempt returned a hedge file"),
                            Err(error) => {
                                let retryable = error.retryable();
                                let retry_after = error.retry_after();
                                last_primary_error = Some(error);
                                if !retryable && active_hedges.is_empty() {
                                    abort_and_drain(&mut tasks).await;
                                    cleanup_paths(active_hedges.into_values().map(|(_, path)| path)).await;
                                    return Err(segment_failure(&job, attempts, last_primary_error.take().unwrap()));
                                }
                                if active_hedges.is_empty() {
                                    if retries >= job.manager.policy.max_retries || !retryable {
                                        return Err(segment_failure(&job, attempts, last_primary_error.take().unwrap()));
                                    }
                                    retries += 1;
                                    tokio::time::sleep(
                                        retry_after.unwrap_or_else(|| job.manager.policy.retry_delay(retries)),
                                    )
                                    .await;
                                    spawn_primary_attempt(
                                        &job,
                                        &committed,
                                        AttemptKind::Resume,
                                        &mut tasks,
                                        &mut primary,
                                    );
                                    attempts += 1;
                                }
                            }
                        }
                    }
                    AttemptKind::Diagnostic | AttemptKind::Hedge => {
                        let Some((_, hedge_path)) = active_hedges.remove(&outcome.id) else {
                            continue;
                        };
                        match outcome.result {
                            Ok(Some(hedge_file)) => {
                                if let Some((_, handle)) = &primary {
                                    handle.abort();
                                }
                                for (handle, _) in active_hedges.values() {
                                    handle.abort();
                                }
                                abort_and_drain(&mut tasks).await;
                                let hedge_start = outcome.hedge_start.expect("hedge has a start offset");
                                commit_hedge(&job, &committed, &hedge_file, hedge_start).await?;
                                let mut cleanup = active_hedges.into_values().map(|(_, path)| path).collect::<Vec<_>>();
                                cleanup.push(hedge_path);
                                cleanup_paths(cleanup).await;
                                return Ok(SegmentReport {
                                    start: job.range.start,
                                    end: job.range.end,
                                    attempts,
                                    hedges,
                                    resumed: retries > 0,
                                });
                            }
                            Ok(None) => unreachable!("hedge attempt returned no hedge file"),
                            Err(_) => {
                                let _ = tokio::fs::remove_file(&hedge_path).await;
                                if primary.is_none() && active_hedges.is_empty() {
                                    let Some(error) = last_primary_error.take() else {
                                        bail!("all attempts ended without a segment result");
                                    };
                                    if retries >= job.manager.policy.max_retries || !error.retryable() {
                                        return Err(segment_failure(&job, attempts, error));
                                    }
                                    let retry_after = error.retry_after();
                                    retries += 1;
                                    tokio::time::sleep(
                                        retry_after.unwrap_or_else(|| job.manager.policy.retry_delay(retries)),
                                    )
                                    .await;
                                    spawn_primary_attempt(
                                        &job,
                                        &committed,
                                        AttemptKind::Resume,
                                        &mut tasks,
                                        &mut primary,
                                    );
                                    attempts += 1;
                                }
                            }
                        }
                    }
                }
            }
            _ = monitor.tick(), if primary.is_some() && hedges < job.manager.policy.max_hedges => {
                let Some((primary_id, _)) = &primary else { continue };
                let existing_hedge_id = active_hedges.keys().next().copied();
                let target_id = existing_hedge_id.unwrap_or(*primary_id);
                let decision = job.manager.health.hedge_decision(target_id, &job.manager.policy);
                let kind = if existing_hedge_id.is_some() {
                    (decision == HedgeDecision::Localized).then_some(AttemptKind::Hedge)
                } else {
                    decision.attempt_kind()
                };
                if let Some(kind) = kind {
                    let received = committed.load(Ordering::Acquire);
                    if received < job.range.len() {
                        let (id, handle, path) = spawn_hedge_attempt(
                            &job,
                            received,
                            kind,
                            &mut tasks,
                        );
                        hedge_cleanup.paths.insert(path.clone());
                        active_hedges.insert(id, (handle, path));
                        hedges += 1;
                        attempts += 1;
                    }
                }
            }
        }
    }
}

fn spawn_primary_attempt<F>(
    job: &SegmentJob<F>,
    committed: &Arc<AtomicU64>,
    kind: AttemptKind,
    tasks: &mut JoinSet<AttemptTaskResult>,
    primary: &mut Option<(u64, AbortHandle)>,
) where
    F: Fn(u64) + Send + Sync + 'static,
{
    let id = job.manager.next_attempt_id();
    let attempt = PrimaryAttempt {
        id,
        kind,
        manager: job.manager.clone(),
        url: job.url.clone(),
        dest: job.dest.clone(),
        staging: job.staging.clone(),
        auth_header: job.auth_header.clone(),
        range: job.range,
        expected_size: job.expected_size,
        whole_request: kind == AttemptKind::Original && job.initial_whole_request,
        committed: Arc::clone(committed),
        downloaded: Arc::clone(&job.downloaded),
        last_reported: Arc::clone(&job.last_reported),
        last_reported_at: Arc::clone(&job.last_reported_at),
        on_progress: Arc::clone(&job.on_progress),
        streaming_hasher: job.streaming_hasher.clone(),
        priority: job.priority,
    };
    let handle = tasks.spawn(async move {
        let result = run_primary_attempt(attempt).await.map(|()| None);
        AttemptTaskResult {
            id,
            kind,
            hedge_start: None,
            result,
        }
    });
    *primary = Some((id, handle));
}

fn spawn_hedge_attempt<F>(
    job: &SegmentJob<F>,
    received: u64,
    kind: AttemptKind,
    tasks: &mut JoinSet<AttemptTaskResult>,
) -> (u64, AbortHandle, PathBuf)
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let id = job.manager.next_attempt_id();
    let path = hedge_path(&job.dest, job.segment_index, id);
    let attempt = HedgeAttempt {
        id,
        kind,
        manager: job.manager.clone(),
        url: job.url.clone(),
        path: path.clone(),
        auth_header: job.auth_header.clone(),
        range: job.range.remaining_after(received),
        expected_size: job.expected_size,
    };
    let hedge_start = job.range.start + received;
    let handle = tasks.spawn(async move {
        let result = run_hedge_attempt(attempt).await.map(Some);
        AttemptTaskResult {
            id,
            kind,
            hedge_start: Some(hedge_start),
            result,
        }
    });
    (id, handle, path)
}

struct PrimaryAttempt<F> {
    id: u64,
    kind: AttemptKind,
    manager: TransferManager,
    url: String,
    dest: PathBuf,
    staging: StagingFile,
    auth_header: Option<(String, String)>,
    range: ByteRange,
    expected_size: u64,
    whole_request: bool,
    committed: Arc<AtomicU64>,
    downloaded: Arc<AtomicU64>,
    last_reported: Arc<AtomicU64>,
    last_reported_at: Arc<Mutex<Instant>>,
    on_progress: Arc<F>,
    streaming_hasher: Option<Arc<Mutex<Option<ring::digest::Context>>>>,
    priority: u64,
}

async fn run_primary_attempt<F>(
    attempt: PrimaryAttempt<F>,
) -> std::result::Result<(), AttemptFailure>
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let _permit = attempt
        .manager
        .ordinary_attempts
        .acquire(attempt.priority)
        .await
        .ok_or(AttemptFailure::CoordinatorClosed)?;
    let already_received = attempt.committed.load(Ordering::Acquire);
    if already_received >= attempt.range.len() {
        return Ok(());
    }
    let request_range = attempt.range.remaining_after(already_received);
    let mut health = attempt.manager.health.register(
        attempt.id,
        attempt.kind,
        request_range,
        attempt.manager.policy.rolling_window,
    );
    let response = send_attempt_request(
        attempt.manager.transport.client(),
        &attempt.url,
        attempt.auth_header.as_ref(),
        (!attempt.whole_request).then_some(request_range),
    )
    .await?;
    validate_attempt_response(
        &response,
        (!attempt.whole_request).then_some(request_range),
        attempt.expected_size,
        &attempt.url,
    )?;

    let mut stream = response.bytes_stream();
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AttemptFailure::Body(error.to_string()))?;
        let offset = request_range.start + received;
        received += chunk.len() as u64;
        if received > request_range.len() {
            return Err(AttemptFailure::Overlong {
                expected: request_range.len(),
            });
        }
        let chunk = attempt
            .staging
            .write_all_at(&attempt.dest, offset, chunk)
            .await
            .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
        if let Some(hasher) = &attempt.streaming_hasher {
            hasher
                .lock()
                .expect("streaming hash lock poisoned")
                .as_mut()
                .expect("streaming hash context missing")
                .update(&chunk);
        }
        attempt
            .committed
            .fetch_add(chunk.len() as u64, Ordering::Release);
        health.progress(chunk.len() as u64);
        let total = attempt
            .downloaded
            .fetch_add(chunk.len() as u64, Ordering::Relaxed)
            + chunk.len() as u64;
        report_progress(
            total,
            &attempt.last_reported,
            &attempt.last_reported_at,
            &attempt.on_progress,
        );
    }
    if received != request_range.len() {
        return Err(AttemptFailure::EarlyEof {
            expected: request_range.len(),
            received,
        });
    }
    health.complete();
    Ok(())
}

struct HedgeAttempt {
    id: u64,
    kind: AttemptKind,
    manager: TransferManager,
    url: String,
    path: PathBuf,
    auth_header: Option<(String, String)>,
    range: ByteRange,
    expected_size: u64,
}

async fn run_hedge_attempt(
    attempt: HedgeAttempt,
) -> std::result::Result<StagingFile, AttemptFailure> {
    // Emergency attempts intentionally bypass the ordinary semaphore. They are bounded by the
    // segment supervisor and exist only after the health classifier declares an emergency.
    let mut health = attempt.manager.health.register(
        attempt.id,
        attempt.kind,
        attempt.range,
        attempt.manager.policy.rolling_window,
    );
    let response = send_attempt_request(
        attempt.manager.transport.client(),
        &attempt.url,
        attempt.auth_header.as_ref(),
        Some(attempt.range),
    )
    .await?;
    validate_attempt_response(
        &response,
        Some(attempt.range),
        attempt.expected_size,
        &attempt.url,
    )?;

    let staging = StagingFile::create(&attempt.path, None)
        .await
        .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AttemptFailure::Body(error.to_string()))?;
        let offset = received;
        received += chunk.len() as u64;
        if received > attempt.range.len() {
            return Err(AttemptFailure::Overlong {
                expected: attempt.range.len(),
            });
        }
        let chunk = staging
            .write_all_at(&attempt.path, offset, chunk)
            .await
            .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
        health.progress(chunk.len() as u64);
    }
    if received != attempt.range.len() {
        return Err(AttemptFailure::EarlyEof {
            expected: attempt.range.len(),
            received,
        });
    }
    health.complete();
    Ok(staging)
}

async fn send_attempt_request(
    http: &reqwest::Client,
    url: &str,
    auth_header: Option<&(String, String)>,
    range: Option<ByteRange>,
) -> std::result::Result<reqwest::Response, AttemptFailure> {
    let mut request = http.get(url);
    if let Some((name, value)) = auth_header {
        request = request.header(name, value);
    }
    if let Some(range) = range {
        request = request.header(header::RANGE, range.header_value());
    }
    request
        .send()
        .await
        .map_err(|error| AttemptFailure::Request(error.to_string()))
}

fn validate_attempt_response(
    response: &reqwest::Response,
    range: Option<ByteRange>,
    expected_size: u64,
    url: &str,
) -> std::result::Result<(), AttemptFailure> {
    let status = response.status();
    if matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
    {
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(AttemptFailure::RetryableStatus {
            status,
            retry_after,
        });
    }
    if let Some(range) = range {
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(if status.is_success() {
                AttemptFailure::InvalidRange(format!(
                    "expected HTTP 206 for bytes {}-{}, got {status}",
                    range.start, range.end
                ))
            } else {
                AttemptFailure::HttpStatus(status)
            });
        }
        validate_content_range(response.headers(), range, expected_size, url)
            .map_err(|error| AttemptFailure::InvalidRange(format!("{error:#}")))?;
    } else if status != StatusCode::OK {
        return Err(AttemptFailure::HttpStatus(status));
    }
    Ok(())
}

async fn commit_hedge<F>(
    job: &SegmentJob<F>,
    committed: &AtomicU64,
    hedge_file: &StagingFile,
    hedge_start: u64,
) -> Result<()>
where
    F: Fn(u64),
{
    hedge_file
        .copy_to_at(&job.staging, &job.dest, hedge_start)
        .await?;

    let old = committed.swap(job.range.len(), Ordering::AcqRel);
    let newly_committed = job.range.len().saturating_sub(old);
    if newly_committed > 0 {
        let total = job.downloaded.fetch_add(newly_committed, Ordering::Relaxed) + newly_committed;
        report_progress(
            total,
            &job.last_reported,
            &job.last_reported_at,
            &job.on_progress,
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("downloading segment {start}-{end} for {url} failed after {attempts} attempt(s)")]
struct SegmentTransferFailed {
    start: u64,
    end: u64,
    url: String,
    attempts: u32,
    #[source]
    source: AttemptFailure,
}

fn segment_failure<F>(job: &SegmentJob<F>, attempts: u32, source: AttemptFailure) -> anyhow::Error {
    SegmentTransferFailed {
        start: job.range.start,
        end: job.range.end,
        url: job.url.clone(),
        attempts,
        source,
    }
    .into()
}

async fn abort_and_drain<T: 'static>(tasks: &mut JoinSet<T>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn hedge_path(dest: &Path, segment_index: usize, attempt_id: u64) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "artifact.tmp".into());
    name.push(format!(
        ".segment-{segment_index}.attempt-{attempt_id}.hedge"
    ));
    dest.with_file_name(name)
}

async fn cleanup_paths(paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        let _ = tokio::fs::remove_file(path).await;
    }
}

pub(super) async fn cleanup_hedge_files(dest: &Path) {
    let Some(parent) = dest.parent() else {
        return;
    };
    let Some(prefix) = dest.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(".hedge") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}
