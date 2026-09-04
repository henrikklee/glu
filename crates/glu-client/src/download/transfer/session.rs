use super::{
    report_progress, retry_after, validate_content_range, ArtifactActivity, AttemptFailure,
    AttemptKind, AttemptProgress, ByteRange, SegmentReport, StagingFile, TransferEvent,
    TransferRuntime, TransferSource, TransferTrace,
};
use anyhow::Result;
use futures_util::StreamExt;
use reqwest::{header, StatusCode};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{sync::OwnedSemaphorePermit, task::JoinSet};

pub(super) struct SegmentJob<F> {
    pub(super) runtime: TransferRuntime,
    pub(super) activity: ArtifactActivity,
    pub(super) trace: Arc<TransferTrace>,
    pub(super) source: TransferSource,
    pub(super) dest: PathBuf,
    pub(super) staging: StagingFile,

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
    result: std::result::Result<AttemptOutput, AttemptFailure>,
}

#[derive(Debug)]
enum AttemptOutput {
    Primary,
    Emergency {
        staging: StagingFile,
        start: u64,
        path: PathBuf,
    },
}

struct ActivePrimary {
    id: u64,
    handle: tokio::task::AbortHandle,
    progress: Arc<AttemptProgress>,
    range: ByteRange,
}

struct ActiveEmergency {
    id: u64,
    handle: tokio::task::AbortHandle,
    path: PathBuf,
}

struct EmergencyFileCleanup {
    paths: BTreeSet<PathBuf>,
}

impl Drop for EmergencyFileCleanup {
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
    let mut cleanup = EmergencyFileCleanup {
        paths: BTreeSet::new(),
    };
    let mut tasks = JoinSet::new();
    let mut primary = Some(spawn_primary_attempt(
        &job,
        &committed,
        AttemptKind::Initial,
        &mut tasks,
    ));
    let mut emergency: Option<ActiveEmergency> = None;
    let mut attempts = 1_u32;
    let mut retries = 0_u32;
    let mut emergencies = 0_u32;
    let mut emergency_used = false;
    let mut last_primary_error: Option<AttemptFailure> = None;
    let mut monitor = tokio::time::interval(job.runtime.policy.monitor_interval);
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            joined = tasks.join_next(), if !tasks.is_empty() => {
                let Some(joined) = joined else { continue };
                let outcome = joined.map_err(|error| anyhow::anyhow!("range attempt task failed: {error}"))?;
                match outcome.kind {
                    AttemptKind::Initial | AttemptKind::Retry => {
                        primary = None;
                        match outcome.result {
                            Ok(AttemptOutput::Primary) => {
                                if let Some(active) = &emergency {
                                    job.trace.attempt_cancelled(active.id, job.runtime.counts());
                                    active.handle.abort();
                                }
                                abort_and_drain(&mut tasks).await;
                                if let Some(active) = emergency.take() {
                                    cleanup.paths.remove(&active.path);
                                    let _ = tokio::fs::remove_file(active.path).await;
                                }
                                job.activity.mark_range_complete();
                                record_range_completed(&job, outcome.id);
                                return Ok(SegmentReport {
                                    start: job.range.start,
                                    end: job.range.end,
                                    attempts,
                                    emergencies,
                                    resumed: retries > 0,
                                });
                            }
                            Ok(AttemptOutput::Emergency { .. }) => unreachable!("primary returned emergency output"),
                            Err(error) => {
                                let retryable = error.retryable();
                                last_primary_error = Some(error);
                                if emergency.is_none() {
                                    if retryable && committed.load(Ordering::Acquire) >= job.range.len() {
                                        job.activity.mark_range_complete();
                                        record_range_completed(&job, outcome.id);
                                        return Ok(SegmentReport {
                                            start: job.range.start,
                                            end: job.range.end,
                                            attempts,
                                            emergencies,
                                            resumed: retries > 0,
                                        });
                                    }
                                    if !retryable {
                                        return Err(range_failure(
                                            &job,
                                            attempts,
                                            last_primary_error.take().expect("primary error exists"),
                                        ));
                                    }
                                    retries = retries.saturating_add(1);
                                    let delay = last_primary_error
                                        .as_ref()
                                        .and_then(AttemptFailure::retry_after)
                                        .unwrap_or_else(|| job.runtime.policy.retry_delay(retries));
                                    job.trace.retry_scheduled(outcome.id, delay, job.runtime.counts());
                                    tokio::time::sleep(delay).await;
                                    primary = Some(spawn_primary_attempt(
                                        &job,
                                        &committed,
                                        AttemptKind::Retry,
                                        &mut tasks,
                                    ));
                                    attempts = attempts.saturating_add(1);
                                }
                            }
                        }
                    }
                    AttemptKind::Emergency => {
                        let Some(active) = emergency.take() else { continue };
                        cleanup.paths.remove(&active.path);
                        match outcome.result {
                            Ok(AttemptOutput::Emergency { staging, start, path }) => {
                                if let Some(active) = &primary {
                                    job.trace.attempt_cancelled(active.id, job.runtime.counts());
                                    active.handle.abort();
                                }
                                abort_and_drain(&mut tasks).await;
                                commit_emergency(&job, &committed, &staging, start).await?;
                                let mut event = TransferEvent::simple("emergency_won");
                                event.attempt = Some(outcome.id);
                                event.range = Some(ByteRange { start, end: job.range.end });
                                event.ordinary_in_flight = job.runtime.ordinary_http.active();
                                event.emergency_in_flight = job.runtime.emergency_active();
                                job.trace.record(event);
                                let _ = tokio::fs::remove_file(path).await;
                                job.activity.mark_range_complete();
                                record_range_completed(&job, outcome.id);
                                return Ok(SegmentReport {
                                    start: job.range.start,
                                    end: job.range.end,
                                    attempts,
                                    emergencies,
                                    resumed: retries > 0,
                                });
                            }
                            Ok(AttemptOutput::Primary) => unreachable!("emergency returned primary output"),
                            Err(_) => {
                                let _ = tokio::fs::remove_file(active.path).await;
                                if primary.is_none() {
                                    let error = last_primary_error.take().ok_or_else(|| {
                                        anyhow::anyhow!("all range attempts ended without a result")
                                    })?;
                                    if !error.retryable() {
                                        return Err(range_failure(&job, attempts, error));
                                    }
                                    if committed.load(Ordering::Acquire) >= job.range.len() {
                                        job.activity.mark_range_complete();
                                        record_range_completed(&job, outcome.id);
                                        return Ok(SegmentReport {
                                            start: job.range.start,
                                            end: job.range.end,
                                            attempts,
                                            emergencies,
                                            resumed: retries > 0,
                                        });
                                    }
                                    retries = retries.saturating_add(1);
                                    let delay = error
                                        .retry_after()
                                        .unwrap_or_else(|| job.runtime.policy.retry_delay(retries));
                                    job.trace.retry_scheduled(outcome.id, delay, job.runtime.counts());
                                    tokio::time::sleep(delay).await;
                                    primary = Some(spawn_primary_attempt(
                                        &job,
                                        &committed,
                                        AttemptKind::Retry,
                                        &mut tasks,
                                    ));
                                    attempts = attempts.saturating_add(1);
                                }
                            }
                        }
                    }
                }
            }
            _ = monitor.tick(), if primary.is_some()
                && emergency.is_none()
                && !emergency_used
                && emergencies < job.runtime.policy.emergency_per_range =>
            {
                let Some(active) = &primary else { continue };
                let Some(tail) = job.activity.tail_eligible() else { continue };
                let Some(health) = active.progress.snapshot(active.range, &job.runtime.policy) else {
                    continue;
                };
                let Some(permit) = job.runtime.try_emergency() else { continue };
                let received = committed.load(Ordering::Acquire);
                if received >= job.range.len() {
                    drop(permit);
                    continue;
                }
                let remaining = job.range.remaining_after(received);
                let id = job.runtime.next_attempt_id();
                job.trace.emergency_triggered(
                    id,
                    remaining,
                    job.priority,
                    tail,
                    health,
                    job.runtime.counts(),
                );
                let active = spawn_emergency_attempt(
                    &job,
                    remaining,
                    id,
                    permit,
                    &mut tasks,
                );
                cleanup.paths.insert(active.path.clone());
                emergency = Some(active);
                emergency_used = true;
                emergencies += 1;
                attempts = attempts.saturating_add(1);
            }
        }
    }
}

fn spawn_primary_attempt<F>(
    job: &SegmentJob<F>,
    committed: &Arc<AtomicU64>,
    kind: AttemptKind,
    tasks: &mut JoinSet<AttemptTaskResult>,
) -> ActivePrimary
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let id = job.runtime.next_attempt_id();
    let already_received = committed.load(Ordering::Acquire);
    let request_range = job.range.remaining_after(already_received);
    let progress = Arc::new(AttemptProgress::new(job.runtime.policy.rolling_window));
    let attempt = PrimaryAttempt {
        id,
        kind,
        runtime: job.runtime.clone(),
        trace: Arc::clone(&job.trace),
        source: job.source.clone(),
        dest: job.dest.clone(),
        staging: job.staging.clone(),

        range: request_range,
        expected_size: job.expected_size,
        whole_request: kind == AttemptKind::Initial && job.initial_whole_request,
        committed: Arc::clone(committed),
        downloaded: Arc::clone(&job.downloaded),
        last_reported: Arc::clone(&job.last_reported),
        last_reported_at: Arc::clone(&job.last_reported_at),
        on_progress: Arc::clone(&job.on_progress),
        streaming_hasher: job.streaming_hasher.clone(),
        priority: job.priority,
        progress: Arc::clone(&progress),
    };
    job.trace
        .attempt_queued(id, kind, request_range, job.runtime.counts());
    let handle = tasks.spawn(async move {
        let result = run_primary_attempt(attempt).await;
        AttemptTaskResult {
            id,
            kind,
            result: result.map(|()| AttemptOutput::Primary),
        }
    });
    ActivePrimary {
        id,
        handle,
        progress,
        range: request_range,
    }
}

fn spawn_emergency_attempt<F>(
    job: &SegmentJob<F>,
    range: ByteRange,
    id: u64,
    permit: OwnedSemaphorePermit,
    tasks: &mut JoinSet<AttemptTaskResult>,
) -> ActiveEmergency
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let path = emergency_path(&job.dest, job.segment_index, id);
    let attempt = EmergencyAttempt {
        id,
        runtime: job.runtime.clone(),
        trace: Arc::clone(&job.trace),
        source: job.source.clone(),
        path: path.clone(),

        range,
        expected_size: job.expected_size,
        progress: Arc::new(AttemptProgress::new(job.runtime.policy.rolling_window)),
        _permit: permit,
    };
    job.trace
        .attempt_queued(id, AttemptKind::Emergency, range, job.runtime.counts());
    let task_path = path.clone();
    let handle = tasks.spawn(async move {
        let result = run_emergency_attempt(attempt)
            .await
            .map(|staging| AttemptOutput::Emergency {
                staging,
                start: range.start,
                path: task_path,
            });
        AttemptTaskResult {
            id,
            kind: AttemptKind::Emergency,
            result,
        }
    });
    ActiveEmergency { id, handle, path }
}

struct PrimaryAttempt<F> {
    id: u64,
    kind: AttemptKind,
    runtime: TransferRuntime,
    trace: Arc<TransferTrace>,
    source: TransferSource,
    dest: PathBuf,
    staging: StagingFile,

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
    progress: Arc<AttemptProgress>,
}

async fn run_primary_attempt<F>(
    attempt: PrimaryAttempt<F>,
) -> std::result::Result<(), AttemptFailure>
where
    F: Fn(u64) + Send + Sync + 'static,
{
    let queued = Instant::now();
    let mut transport = attempt
        .runtime
        .next_ordinary_transport(attempt.priority, attempt.range.len())
        .await
        .ok_or(AttemptFailure::BudgetClosed)?;
    attempt.progress.request_started();
    attempt.trace.attempt_started(
        attempt.id,
        attempt.kind,
        attempt.range,
        queued.elapsed(),
        attempt.runtime.counts(),
        Some(transport.assignment()),
    );
    let (request_url, request_header) = attempt.source.request().await;
    let response = match send_attempt_request(
        transport.client(),
        &request_url,
        request_header.as_ref(),
        (!attempt.whole_request).then_some(attempt.range),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            attempt
                .trace
                .attempt_failed(attempt.id, &error, attempt.runtime.counts());
            return Err(error);
        }
    };
    attempt.trace.response_headers(
        attempt.id,
        response.version(),
        response.status(),
        super::response_origin(response.url()),
        attempt.runtime.counts(),
    );
    if attempt
        .source
        .refresh_if_rejected(&request_url, response.status())
        .await
        .map_err(AttemptFailure::SourceResolution)?
    {
        let error = AttemptFailure::RetryableStatus {
            status: response.status(),
            retry_after: retry_after(response.headers()),
        };
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }
    if let Err(error) = validate_attempt_response(
        &response,
        (!attempt.whole_request).then_some(attempt.range),
        attempt.expected_size,
        attempt.source.display_url(),
    ) {
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }

    let mut stream = response.bytes_stream();
    let mut received = 0_u64;
    let mut pending_write = None;
    loop {
        let next = stream.next().await;
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(source)) => {
                finish_primary_write(&attempt, pending_write.take()).await?;
                let error = AttemptFailure::Body(source.without_url());
                attempt
                    .trace
                    .attempt_failed(attempt.id, &error, attempt.runtime.counts());
                return Err(error);
            }
            None => break,
        };
        let offset = attempt.range.start + received;
        received += chunk.len() as u64;
        if received > attempt.range.len() {
            finish_primary_write(&attempt, pending_write.take()).await?;
            let error = AttemptFailure::Overlong {
                expected: attempt.range.len(),
            };
            attempt
                .trace
                .attempt_failed(attempt.id, &error, attempt.runtime.counts());
            return Err(error);
        }
        transport.record_bytes(chunk.len() as u64);
        record_network_chunk(
            &attempt.runtime,
            &attempt.trace,
            attempt.id,
            attempt.range,
            &attempt.progress,
            chunk.len() as u64,
        );
        finish_primary_write(&attempt, pending_write.take()).await?;
        pending_write = Some(attempt.staging.start_write_all_at(offset, chunk));
    }
    finish_primary_write(&attempt, pending_write.take()).await?;
    if received != attempt.range.len() {
        let error = AttemptFailure::EarlyEof {
            expected: attempt.range.len(),
            received,
        };
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }
    attempt
        .trace
        .attempt_completed(attempt.id, received, attempt.runtime.counts());
    Ok(())
}

async fn finish_primary_write<F, B>(
    attempt: &PrimaryAttempt<F>,
    pending: Option<tokio::task::JoinHandle<std::io::Result<B>>>,
) -> std::result::Result<(), AttemptFailure>
where
    F: Fn(u64) + Send + Sync + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    let Some(pending) = pending else {
        return Ok(());
    };
    let chunk = StagingFile::finish_write_all_at(&attempt.dest, pending)
        .await
        .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
    if let Some(hasher) = &attempt.streaming_hasher {
        hasher
            .lock()
            .expect("streaming hash lock poisoned")
            .as_mut()
            .expect("streaming hash context missing")
            .update(chunk.as_ref());
    }
    let len = chunk.as_ref().len() as u64;
    attempt.committed.fetch_add(len, Ordering::Release);
    attempt.trace.logical_progress(len);
    let total = attempt.downloaded.fetch_add(len, Ordering::Relaxed) + len;
    report_progress(
        total,
        &attempt.last_reported,
        &attempt.last_reported_at,
        &attempt.on_progress,
    );
    Ok(())
}

struct EmergencyAttempt {
    id: u64,
    runtime: TransferRuntime,
    trace: Arc<TransferTrace>,
    source: TransferSource,
    path: PathBuf,

    range: ByteRange,
    expected_size: u64,
    progress: Arc<AttemptProgress>,
    _permit: OwnedSemaphorePermit,
}

async fn run_emergency_attempt(
    attempt: EmergencyAttempt,
) -> std::result::Result<StagingFile, AttemptFailure> {
    attempt.progress.request_started();
    attempt.trace.attempt_started(
        attempt.id,
        AttemptKind::Emergency,
        attempt.range,
        Duration::ZERO,
        attempt.runtime.counts(),
        None,
    );
    let (request_url, request_header) = attempt.source.request().await;
    let response = match send_attempt_request(
        &attempt.runtime.emergency_http,
        &request_url,
        request_header.as_ref(),
        Some(attempt.range),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            attempt
                .trace
                .attempt_failed(attempt.id, &error, attempt.runtime.counts());
            return Err(error);
        }
    };
    attempt.trace.response_headers(
        attempt.id,
        response.version(),
        response.status(),
        super::response_origin(response.url()),
        attempt.runtime.counts(),
    );
    if attempt
        .source
        .refresh_if_rejected(&request_url, response.status())
        .await
        .map_err(AttemptFailure::SourceResolution)?
    {
        let error = AttemptFailure::RetryableStatus {
            status: response.status(),
            retry_after: retry_after(response.headers()),
        };
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }
    if let Err(error) = validate_attempt_response(
        &response,
        Some(attempt.range),
        attempt.expected_size,
        attempt.source.display_url(),
    ) {
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }

    let staging = StagingFile::create(&attempt.path, None)
        .await
        .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(source) => {
                let error = AttemptFailure::Body(source.without_url());
                attempt
                    .trace
                    .attempt_failed(attempt.id, &error, attempt.runtime.counts());
                return Err(error);
            }
        };
        let offset = received;
        received += chunk.len() as u64;
        if received > attempt.range.len() {
            let error = AttemptFailure::Overlong {
                expected: attempt.range.len(),
            };
            attempt
                .trace
                .attempt_failed(attempt.id, &error, attempt.runtime.counts());
            return Err(error);
        }
        record_network_chunk(
            &attempt.runtime,
            &attempt.trace,
            attempt.id,
            attempt.range,
            &attempt.progress,
            chunk.len() as u64,
        );
        staging
            .write_all_at(&attempt.path, offset, chunk)
            .await
            .map_err(|error| AttemptFailure::LocalIo(format!("{error:#}")))?;
    }
    if received != attempt.range.len() {
        let error = AttemptFailure::EarlyEof {
            expected: attempt.range.len(),
            received,
        };
        attempt
            .trace
            .attempt_failed(attempt.id, &error, attempt.runtime.counts());
        return Err(error);
    }
    attempt
        .trace
        .attempt_completed(attempt.id, received, attempt.runtime.counts());
    Ok(staging)
}

fn record_network_chunk(
    runtime: &TransferRuntime,
    trace: &TransferTrace,
    attempt_id: u64,
    range: ByteRange,
    progress: &AttemptProgress,
    bytes: u64,
) {
    let sample = progress.progress(bytes);
    trace.wire_bytes.fetch_add(bytes, Ordering::Relaxed);
    if sample.first || sample.report {
        let mut event = TransferEvent::simple(if sample.first {
            "first_body_byte"
        } else {
            "attempt_progress"
        });
        event.attempt = Some(attempt_id);
        event.range = Some(range);
        event.bytes = Some(sample.total);
        event.ordinary_in_flight = runtime.ordinary_http.active();
        event.emergency_in_flight = runtime.emergency_active();
        trace.record(event);
    }
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
        .map_err(|error| AttemptFailure::Request(error.without_url()))
}

fn validate_attempt_response(
    response: &reqwest::Response,
    range: Option<ByteRange>,
    expected_size: u64,
    url: &str,
) -> std::result::Result<(), AttemptFailure> {
    let status = response.status();
    if let Some(range) = range {
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(if status.is_success() {
                AttemptFailure::InvalidRange(format!(
                    "expected HTTP 206 for bytes {}-{}, got {status}",
                    range.start, range.end
                ))
            } else {
                AttemptFailure::RetryableStatus {
                    status,
                    retry_after: retry_after(response.headers()),
                }
            });
        }
        validate_content_range(response.headers(), range, expected_size, url)
            .map_err(|error| AttemptFailure::InvalidRange(format!("{error:#}")))?;
    } else if status != StatusCode::OK {
        return Err(AttemptFailure::RetryableStatus {
            status,
            retry_after: retry_after(response.headers()),
        });
    }
    Ok(())
}

async fn commit_emergency<F>(
    job: &SegmentJob<F>,
    committed: &AtomicU64,
    emergency_file: &StagingFile,
    emergency_start: u64,
) -> Result<()>
where
    F: Fn(u64),
{
    job.staging.wait_for_pending_writes().await?;
    emergency_file
        .copy_to_at(&job.staging, &job.dest, emergency_start)
        .await?;
    let old = committed.swap(job.range.len(), Ordering::AcqRel);
    let newly_committed = job.range.len().saturating_sub(old);
    if newly_committed > 0 {
        job.trace.logical_progress(newly_committed);
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

fn record_range_completed<F>(job: &SegmentJob<F>, attempt: u64)
where
    F: Fn(u64),
{
    let mut event = TransferEvent::simple("range_completed");
    event.attempt = Some(attempt);
    event.range = Some(job.range);
    event.ordinary_in_flight = job.runtime.ordinary_http.active();
    event.emergency_in_flight = job.runtime.emergency_active();
    job.trace.record(event);
}

#[derive(Debug, thiserror::Error)]
#[error("downloading range {start}-{end} for {url} failed after {attempts} attempt(s)")]
struct RangeTransferFailed {
    start: u64,
    end: u64,
    url: String,
    attempts: u32,
    #[source]
    source: AttemptFailure,
}

fn range_failure<F>(job: &SegmentJob<F>, attempts: u32, source: AttemptFailure) -> anyhow::Error {
    RangeTransferFailed {
        start: job.range.start,
        end: job.range.end,
        url: job.source.display_url().to_string(),
        attempts,
        source,
    }
    .into()
}

async fn abort_and_drain<T: 'static>(tasks: &mut JoinSet<T>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn emergency_path(dest: &Path, segment_index: usize, attempt_id: u64) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "artifact.tmp".into());
    name.push(format!(
        ".range-{segment_index}.attempt-{attempt_id}.emergency"
    ));
    dest.with_file_name(name)
}

pub(super) async fn cleanup_emergency_files(dest: &Path) {
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
        if name.starts_with(prefix) && name.ends_with(".emergency") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}
