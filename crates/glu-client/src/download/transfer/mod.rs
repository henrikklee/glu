use crate::{download::http::GhcrTransport, hash::Sha256Mismatch};
mod health;
mod request;
mod session;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use health::{AttemptHealthRegistry, HedgeDecision};
use request::RequestCoordinator;
use reqwest::{header, StatusCode};
use session::{cleanup_hedge_files, run_segment, SegmentJob};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    task::JoinSet,
};

#[derive(Debug, Clone)]
pub struct TransferManager {
    transport: GhcrTransport,
    ordinary_attempts: RequestCoordinator,
    health: AttemptHealthRegistry,
    next_attempt_id: Arc<AtomicU64>,
    policy: TransferPolicy,
}

impl TransferManager {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_policy(http, TransferPolicy::default())
    }

    fn with_policy(http: reqwest::Client, policy: TransferPolicy) -> Self {
        Self {
            transport: GhcrTransport::new(http),
            ordinary_attempts: RequestCoordinator::new(policy.ordinary_attempts),
            health: AttemptHealthRegistry::default(),
            next_attempt_id: Arc::new(AtomicU64::new(1)),
            policy,
        }
    }

    pub async fn configure_install_token<'a>(
        &self,
        urls: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        self.transport.configure_install_token(urls).await
    }

    pub async fn download_blob_to_path<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_sha256: &str,
        expected_size: Option<u64>,
        priority: u64,
        on_progress: F,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let (header_name, header_value) = self.transport.auth_header_for_blob_url(url).await?;
        self.download_with_priority_to_path(
            url,
            dest,
            expected_sha256,
            expected_size,
            TransferContext {
                auth_header: Some((header_name, header_value)),
                priority,
            },
            Arc::new(on_progress),
        )
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
        self.download_with_priority_to_path(
            url,
            dest,
            expected_sha256,
            expected_size,
            TransferContext {
                auth_header,
                priority: 0,
            },
            on_progress,
        )
        .await
    }

    async fn download_with_priority_to_path<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_sha256: &str,
        expected_size: Option<u64>,
        transfer: TransferContext,
        on_progress: Arc<F>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let TransferContext {
            auth_header,
            priority,
        } = transfer;
        let Some(expected_size) = expected_size else {
            return self
                .download_unknown_size_to_path(
                    url,
                    dest,
                    expected_sha256,
                    auth_header,
                    priority,
                    on_progress,
                )
                .await;
        };
        if expected_size == 0 {
            bail!("artifact {url} has an invalid expected size of zero");
        }

        // Create the staging artifact exclusively. It is pre-sized so disjoint segment writers
        // can seek without growing it or exposing partial bytes in the
        // admitted digest cache.
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        file.set_len(expected_size)
            .await
            .with_context(|| format!("sizing {}", dest.display()))?;
        drop(file);

        let ranges = segment_ranges(expected_size, &self.policy);
        let is_whole_request = ranges.len() == 1;
        // Preserve the no-redundant-read fast path for an uninterrupted or ordinarily resumed
        // single segment. Multipart and hedge winners still require an ordered full-file pass.
        let streaming_hasher = is_whole_request.then(|| {
            Arc::new(Mutex::new(Some(ring::digest::Context::new(
                &ring::digest::SHA256,
            ))))
        });
        let downloaded = Arc::new(AtomicU64::new(0));
        let last_reported = Arc::new(AtomicU64::new(0));
        let last_reported_at = Arc::new(Mutex::new(Instant::now()));
        let mut tasks = JoinSet::new();

        for (segment_index, range) in ranges.iter().copied().enumerate() {
            let job = SegmentJob {
                manager: self.clone(),
                url: url.to_string(),
                dest: dest.to_path_buf(),
                auth_header: auth_header.clone(),
                range,
                expected_size,
                initial_whole_request: is_whole_request,
                segment_index,
                priority,
                downloaded: Arc::clone(&downloaded),
                last_reported: Arc::clone(&last_reported),
                last_reported_at: Arc::clone(&last_reported_at),
                on_progress: Arc::clone(&on_progress),
                streaming_hasher: streaming_hasher.clone(),
            };
            tasks.spawn(async move { run_segment(job).await });
        }

        let mut segment_reports = Vec::with_capacity(ranges.len());
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(report)) => segment_reports.push(report),
                Ok(Err(error)) => {
                    tasks.shutdown().await;
                    cleanup_hedge_files(dest).await;
                    return Err(error);
                }
                Err(error) => {
                    tasks.shutdown().await;
                    cleanup_hedge_files(dest).await;
                    return Err(anyhow::anyhow!("download segment task failed: {error}"));
                }
            }
        }

        on_progress.as_ref()(expected_size);
        let used_hedge = segment_reports.iter().any(|report| report.hedges > 0);
        let actual = if !used_hedge {
            match streaming_hasher {
                Some(hasher) => {
                    let context = hasher
                        .lock()
                        .expect("streaming hash lock poisoned")
                        .take()
                        .expect("streaming hash context already consumed");
                    crate::hash::hex_lower(context.finish().as_ref())
                }
                None => sha256_file(dest).await?,
            }
        } else {
            sha256_file(dest).await?
        };
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(Sha256Mismatch::new(url, expected_sha256, actual).into());
        }
        sync_file(dest).await?;

        segment_reports.sort_by_key(|report| report.start);
        Ok(TransferReport {
            segments: segment_reports,
        })
    }

    async fn download_unknown_size_to_path<F>(
        &self,
        url: &str,
        dest: &Path,
        expected_sha256: &str,
        auth_header: Option<(String, String)>,
        priority: u64,
        on_progress: Arc<F>,
    ) -> Result<TransferReport>
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        // Unknown-size retries restart the whole response, but retain this exclusively created
        // descriptor so a pathname replacement cannot turn a retry into a symlink write.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        let mut retries = 0_u32;
        let mut max_reported = 0_u64;
        loop {
            let _permit = self
                .ordinary_attempts
                .acquire(priority)
                .await
                .ok_or_else(|| anyhow::anyhow!("download request coordinator closed"))?;
            let mut request = self.transport.client().get(url);
            if let Some((name, value)) = &auth_header {
                request = request.header(name, value);
            }
            let response = match request.send().await {
                Ok(response)
                    if matches!(
                        response.status(),
                        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
                    ) || response.status().is_server_error() =>
                {
                    if retries >= self.policy.max_retries {
                        bail!("download failed for {url}: HTTP {}", response.status());
                    }
                    let retry_after = response
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    retries += 1;
                    tokio::time::sleep(
                        retry_after.unwrap_or_else(|| self.policy.retry_delay(retries)),
                    )
                    .await;
                    continue;
                }
                Ok(response) => response
                    .error_for_status()
                    .with_context(|| format!("download failed for {url}"))?,
                Err(error) => {
                    if retries >= self.policy.max_retries {
                        return Err(error).with_context(|| format!("downloading {url}"));
                    }
                    retries += 1;
                    tokio::time::sleep(self.policy.retry_delay(retries)).await;
                    continue;
                }
            };

            file.set_len(0)
                .await
                .with_context(|| format!("truncating {}", dest.display()))?;
            file.seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking {}", dest.display()))?;
            let mut stream = response.bytes_stream();
            let mut received = 0_u64;
            let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
            let mut body_error = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        file.write_all(&chunk)
                            .await
                            .with_context(|| format!("writing {}", dest.display()))?;
                        hasher.update(&chunk);
                        received += chunk.len() as u64;
                        if received > max_reported {
                            max_reported = received;
                            on_progress.as_ref()(max_reported);
                        }
                    }
                    Err(error) => {
                        body_error = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = body_error {
                if retries >= self.policy.max_retries {
                    return Err(error)
                        .with_context(|| format!("reading download stream for {url}"));
                }
                retries += 1;
                tokio::time::sleep(self.policy.retry_delay(retries)).await;
                continue;
            }
            file.flush()
                .await
                .with_context(|| format!("flushing {}", dest.display()))?;
            drop(file);

            let actual = crate::hash::hex_lower(hasher.finish().as_ref());
            if !actual.eq_ignore_ascii_case(expected_sha256) {
                return Err(Sha256Mismatch::new(url, expected_sha256, actual).into());
            }
            sync_file(dest).await?;
            return Ok(TransferReport {
                segments: vec![SegmentReport {
                    start: 0,
                    end: received.saturating_sub(1),
                    attempts: retries + 1,
                    hedges: 0,
                    resumed: retries > 0,
                }],
            });
        }
    }

    fn next_attempt_id(&self) -> u64 {
        self.next_attempt_id.fetch_add(1, Ordering::Relaxed)
    }
}

struct TransferContext {
    auth_header: Option<(String, String)>,
    priority: u64,
}

#[derive(Debug, Clone)]
struct TransferPolicy {
    ordinary_attempts: usize,
    max_retries: u32,
    max_hedges: u32,
    retry_base: Duration,
    monitor_interval: Duration,
    hedge_warmup: Duration,
    stalled_for: Duration,
    pathological_remaining: Duration,
    max_hedge_rate: f64,
    rolling_window: Duration,
    multipart_threshold: u64,
    target_part_size: u64,
    max_multipart_parts: u64,
}

impl TransferPolicy {
    fn retry_delay(&self, retry: u32) -> Duration {
        let shift = retry.saturating_sub(1).min(4);
        let multiplier = 1_u32 << shift;
        let base = self.retry_base.saturating_mul(multiplier);
        // Deterministic jitter avoids adding a random-number dependency while ensuring concurrent
        // failed ranges do not all wake on exactly the same boundary.
        base + Duration::from_millis((retry as u64 * 37) % 97)
    }
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            ordinary_attempts: 16,
            max_retries: 3,
            max_hedges: 2,
            retry_base: Duration::from_millis(250),
            monitor_interval: Duration::from_millis(250),
            hedge_warmup: Duration::from_secs(3),
            stalled_for: Duration::from_secs(2),
            pathological_remaining: Duration::from_secs(10),
            // A peer being three times faster is normal under shared CDN congestion. Rate-based
            // hedging is reserved for true dribbling tails; complete lack of progress is handled
            // separately by `stalled_for` and is not subject to this ceiling.
            max_hedge_rate: 128_000.0,
            rolling_window: Duration::from_secs(3),
            multipart_threshold: MIN_MULTIPART_SIZE,
            target_part_size: TARGET_PART_SIZE,
            max_multipart_parts: MAX_MULTIPART_PARTS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    Original,
    Resume,
    Diagnostic,
    Hedge,
}

#[derive(Debug, thiserror::Error)]
enum AttemptFailure {
    #[error("request failed: {0}")]
    Request(String),
    #[error("response body failed: {0}")]
    Body(String),
    #[error("retryable HTTP status {status}")]
    RetryableStatus {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    #[error("HTTP status {0}")]
    HttpStatus(StatusCode),
    #[error("response ended early: expected {expected} bytes, got {received}")]
    EarlyEof { expected: u64, received: u64 },
    #[error("response exceeded expected length {expected}")]
    Overlong { expected: u64 },
    #[error("invalid range response: {0}")]
    InvalidRange(String),
    #[error("local I/O failed: {0}")]
    LocalIo(String),
    #[error("request coordinator closed")]
    CoordinatorClosed,
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

#[derive(Debug, Clone)]
pub(crate) struct TransferReport {
    segments: Vec<SegmentReport>,
}

impl TransferReport {
    pub(crate) fn validate(&self, expected_size: Option<u64>) -> Result<()> {
        if self.segments.is_empty() {
            bail!("transfer completed without segment reports");
        }
        for segment in &self.segments {
            if segment.attempts == 0 || segment.hedges > segment.attempts {
                bail!("transfer produced an invalid attempt summary");
            }
            if segment.resumed && segment.attempts < 2 {
                bail!("transfer marked a segment resumed without a second attempt");
            }
        }
        if let Some(expected_size) = expected_size {
            let mut next = 0_u64;
            for segment in &self.segments {
                if segment.start != next || segment.end < segment.start {
                    bail!("transfer produced non-contiguous segment reports");
                }
                next = segment.end + 1;
            }
            if next != expected_size {
                bail!("transfer segment reports cover {next} bytes, expected {expected_size}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SegmentReport {
    pub start: u64,
    pub end: u64,
    pub attempts: u32,
    pub hedges: u32,
    pub resumed: bool,
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

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0_u8; 1024 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(crate::hash::hex_lower(hasher.finish().as_ref()))
}

async fn sync_file(path: &Path) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .await
        .with_context(|| format!("opening {} for sync", path.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("syncing {}", path.display()))
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
        .clamp(MIN_MULTIPART_PARTS, policy.max_multipart_parts) as usize;
    let part_size = size.div_ceil(part_count as u64);
    (0..part_count)
        .map(|index| {
            let start = index as u64 * part_size;
            let end = ((index as u64 + 1) * part_size).min(size) - 1;
            ByteRange { start, end }
        })
        .collect()
}

fn should_use_multipart(size: u64, policy: &TransferPolicy) -> bool {
    size > policy.multipart_threshold
}

/// Progress reports fire every 0.1 MB of committed artifact bytes. The footer renders at 12fps,
/// so more frequent callbacks would not improve visible smoothness.
fn should_report(accumulated: u64, last_reported: u64) -> bool {
    accumulated >= last_reported.saturating_add(PROGRESS_REPORT_BYTES)
}

const PROGRESS_REPORT_BYTES: u64 = 100_000;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(250);
const MIB: u64 = 1024 * 1024;
const MIN_MULTIPART_SIZE: u64 = 10 * MIB;
const TARGET_PART_SIZE: u64 = 10 * MIB;
const MIN_MULTIPART_PARTS: u64 = 2;
const MAX_MULTIPART_PARTS: u64 = 16;

#[cfg(test)]
mod tests;
