use super::{
    parse_content_range, segment_ranges, should_report, should_use_multipart, AttemptFailure,
    AttemptHealthRegistry, AttemptKind, ByteRange, HedgeDecision, RequestCoordinator,
    TransferPolicy, MIB,
};
use ring::digest;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn reports_every_tenth_of_a_megabyte() {
    assert!(!should_report(50_000, 0));
    assert!(should_report(100_000, 0));
    assert!(should_report(250_000, 100_000));
    assert!(!should_report(199_999, 100_000));
}

#[test]
fn chunking_starts_above_ten_mib() {
    let policy = TransferPolicy::default();
    assert!(!should_use_multipart(10 * MIB, &policy));
    assert!(should_use_multipart(10 * MIB + 1, &policy));
}

#[test]
fn segment_ranges_use_target_size_and_cap() {
    let ranges = segment_ranges(64 * MIB, &TransferPolicy::default());
    assert_eq!(ranges.len(), 7);
    assert_eq!(ranges.first().unwrap().start, 0);
    assert_eq!(ranges.last().unwrap().end, 64 * MIB - 1);
    assert!(ranges.iter().all(|range| range.len() <= 10 * MIB));
    assert_eq!(
        segment_ranges(164 * MIB, &TransferPolicy::default()).len(),
        16
    );
    assert_eq!(
        segment_ranges(2_240 * MIB, &TransferPolicy::default()).len(),
        16
    );
}

#[test]
fn small_artifact_is_one_segment() {
    assert_eq!(
        segment_ranges(1_000, &TransferPolicy::default()),
        vec![ByteRange { start: 0, end: 999 }]
    );
}

#[test]
fn segment_ranges_cover_odd_sizes_without_overlap() {
    let ranges = segment_ranges(100 * MIB + 123, &TransferPolicy::default());
    assert_eq!(ranges.len(), 11);
    assert_eq!(ranges.first().unwrap().start, 0);
    assert_eq!(ranges.last().unwrap().end, 100 * MIB + 122);
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].end + 1, pair[1].start);
    }
}

#[test]
fn parses_content_range() {
    assert_eq!(
        parse_content_range("bytes 0-99/163797476"),
        Some((0, 99, 163797476))
    );
    assert_eq!(parse_content_range("bytes 0-99/*"), None);
    assert_eq!(parse_content_range("garbage"), None);
}

#[test]
fn retry_classification_is_typed() {
    assert!(AttemptFailure::Request("offline".into()).retryable());
    assert!(AttemptFailure::EarlyEof {
        expected: 10,
        received: 3,
    }
    .retryable());
    assert!(!AttemptFailure::InvalidRange("wrong range".into()).retryable());
    assert!(!AttemptFailure::LocalIo("disk full".into()).retryable());
}

#[test]
fn health_classifier_distinguishes_localized_and_global_stalls() {
    let registry = AttemptHealthRegistry::default();
    let policy = TransferPolicy {
        hedge_warmup: Duration::from_millis(10),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(20),
        rolling_window: Duration::from_secs(1),
        ..TransferPolicy::default()
    };
    let slow = registry.register(
        1,
        AttemptKind::Original,
        ByteRange {
            start: 0,
            end: 999_999,
        },
        policy.rolling_window,
    );
    let fast = registry.register(
        2,
        AttemptKind::Original,
        ByteRange {
            start: 0,
            end: 999_999,
        },
        policy.rolling_window,
    );
    thread::sleep(Duration::from_millis(12));
    slow.progress(100);
    fast.progress(200_000);
    assert_eq!(
        registry.hedge_decision(1, &policy),
        HedgeDecision::Localized
    );

    thread::sleep(Duration::from_millis(35));
    assert_eq!(registry.hedge_decision(1, &policy), HedgeDecision::None);
    drop(fast);
    assert_eq!(registry.hedge_decision(1, &policy), HedgeDecision::None);
    thread::sleep(Duration::from_millis(35));
    assert_eq!(
        registry.hedge_decision(1, &policy),
        HedgeDecision::Ambiguous
    );
}

#[tokio::test]
async fn request_coordinator_runs_higher_priority_first() {
    let coordinator = RequestCoordinator::new(1);
    let held = coordinator.acquire(0).await.unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let low = coordinator.clone();
    let low_sender = sender.clone();
    tokio::spawn(async move {
        let _permit = low.acquire(10).await.unwrap();
        low_sender.send("low").unwrap();
    });
    while coordinator.waiting() != 1 {
        tokio::task::yield_now().await;
    }
    let high = coordinator.clone();
    tokio::spawn(async move {
        let _permit = high.acquire(1).await.unwrap();
        sender.send("high").unwrap();
    });
    while coordinator.waiting() != 2 {
        tokio::task::yield_now().await;
    }
    drop(held);
    assert_eq!(receiver.recv().await, Some("high"));
    assert_eq!(receiver.recv().await, Some("low"));
}

#[test]
fn healthy_cdn_rate_variation_does_not_trigger_a_hedge() {
    let registry = AttemptHealthRegistry::default();
    let policy = TransferPolicy {
        hedge_warmup: Duration::from_millis(10),
        pathological_remaining: Duration::from_millis(20),
        rolling_window: Duration::from_secs(1),
        ..TransferPolicy::default()
    };
    let slower = registry.register(
        1,
        AttemptKind::Original,
        ByteRange {
            start: 0,
            end: 9_999_999,
        },
        policy.rolling_window,
    );
    let faster = registry.register(
        2,
        AttemptKind::Original,
        ByteRange {
            start: 0,
            end: 9_999_999,
        },
        policy.rolling_window,
    );
    thread::sleep(Duration::from_millis(20));
    slower.progress(1_000_000);
    faster.progress(4_000_000);

    assert_eq!(registry.hedge_decision(1, &policy), HedgeDecision::None);
}

#[derive(Debug)]
struct TestRequest {
    sequence: usize,
    range: Option<ByteRange>,
}

struct TestServer {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(address) = self.url.trim_start_matches("http://").parse() {
            let _ = TcpStream::connect_timeout(&address, Duration::from_millis(50));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn test_server(
    handler: impl Fn(TestRequest, &mut TcpStream) + Send + Sync + 'static,
) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let sequence = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);
    let thread = thread::spawn(move || {
        let mut children = Vec::new();
        while !stop_for_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if stop_for_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = stream.set_nonblocking(false);
                    let handler = Arc::clone(&handler);
                    let request_sequence = sequence.fetch_add(1, Ordering::Relaxed);
                    children.push(thread::spawn(move || {
                        let request = read_request(&mut stream, request_sequence);
                        handler(request, &mut stream);
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
        for child in children {
            let _ = child.join();
        }
    });
    TestServer {
        url: format!("http://{address}/artifact"),
        stop,
        thread: Some(thread),
    }
}

fn read_request(stream: &mut TcpStream, sequence: usize) -> TestRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let headers = String::from_utf8_lossy(&bytes);
    let range = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let value = value.trim().strip_prefix("bytes=")?;
        let (start, end) = value.split_once('-')?;
        Some(ByteRange {
            start: start.parse().ok()?,
            end: end.parse().ok()?,
        })
    });
    TestRequest { sequence, range }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    declared_length: usize,
    content_range: Option<(ByteRange, usize)>,
    body: &[u8],
) {
    let mut headers =
        format!("HTTP/1.1 {status}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n");
    if let Some((range, total)) = content_range {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            range.start, range.end, total
        ));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).unwrap();
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn sha256(bytes: &[u8]) -> String {
    crate::hash::hex_lower(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn test_manager(policy: TransferPolicy) -> super::TransferManager {
    let client = reqwest::Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(1))
        .read_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    super::TransferManager::with_policy(client, policy)
}

#[tokio::test]
async fn healthy_single_stream_completes_without_speculation() {
    let data = Arc::new(vec![19_u8; 50_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            assert_eq!(request.range, None);
            write_response(stream, "200 OK", data.len(), None, &data);
        }
    });
    let manager = test_manager(TransferPolicy::default());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments.len(), 1);
    assert_eq!(report.segments[0].attempts, 1);
    assert_eq!(report.segments[0].hedges, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn cache_admission_rejects_a_post_transfer_path_replacement() {
    use std::os::unix::fs::symlink;

    let data = Arc::new(vec![27_u8; 50_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            assert_eq!(request.range, None);
            write_response(stream, "200 OK", data.len(), None, &data);
        }
    });
    let manager = test_manager(TransferPolicy::default());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    let replacement = temp.path().join("replacement");
    std::fs::write(&replacement, &*data).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&replacement, &path).unwrap();
    let error = report.validate_staging_path(&path).await.unwrap_err();
    assert!(format!("{error:#}").contains("staging path identity changed"));
}

#[cfg(unix)]
async fn assert_path_replacement_cannot_redirect_writes(expected_size: Option<u64>) {
    use std::os::unix::fs::symlink;

    let data = Arc::new(vec![29_u8; 50_000]);
    let request_seen = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let server = test_server({
        let data = Arc::clone(&data);
        let request_seen = Arc::clone(&request_seen);
        let release = Arc::clone(&release);
        move |request, stream| {
            assert_eq!(request.range, None);
            request_seen.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            write_response(stream, "200 OK", data.len(), None, &data);
        }
    });
    let manager = test_manager(TransferPolicy::default());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let victim = temp.path().join("victim");
    let original_victim = b"victim remains unchanged";
    std::fs::write(&victim, original_victim).unwrap();
    let url = server.url.clone();
    let digest = sha256(&data);
    let download_path = path.clone();
    let task = tokio::spawn(async move {
        manager
            .download_with_header_to_path(
                &url,
                &download_path,
                &digest,
                expected_size,
                None,
                Arc::new(|_| {}),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        while !request_seen.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server saw the request");
    std::fs::remove_file(&path).unwrap();
    symlink(&victim, &path).unwrap();
    release.store(true, Ordering::Release);

    let report = task.await.unwrap().unwrap();
    let error = report.validate_staging_path(&path).await.unwrap_err();
    assert!(format!("{error:#}").contains("staging path identity changed"));
    assert_eq!(std::fs::read(victim).unwrap(), original_victim);
}

#[cfg(unix)]
#[tokio::test]
async fn known_size_download_retains_its_original_inode() {
    assert_path_replacement_cannot_redirect_writes(Some(50_000)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unknown_size_download_retains_its_original_inode() {
    assert_path_replacement_cannot_redirect_writes(None).await;
}

#[tokio::test]
async fn healthy_multipart_completes_without_speculation() {
    let data = Arc::new(vec![23_u8; 300_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            let range = request.range.expect("multipart request must use Range");
            write_response(
                stream,
                "206 Partial Content",
                range.len() as usize,
                Some((range, data.len())),
                &data[range.start as usize..=range.end as usize],
            );
        }
    });
    let policy = TransferPolicy {
        multipart_threshold: 200_000,
        target_part_size: 150_000,
        max_multipart_parts: 2,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments.len(), 2);
    assert!(report
        .segments
        .iter()
        .all(|segment| segment.attempts == 1 && segment.hedges == 0));
}

#[tokio::test]
async fn same_digest_transfers_use_distinct_staging_files_and_converge() {
    let data = Arc::new(vec![31_u8; 80_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            assert_eq!(request.range, None);
            write_response(stream, "200 OK", data.len(), None, &data);
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let prefix = glu_core::Prefix(temp.path().to_path_buf());
    let cache = crate::download::cache::ArtifactCache::new(&prefix);
    cache.ensure_dirs().await.unwrap();
    let digest = sha256(&data);
    let first_path = cache.temp_path_for_sha256(&digest).unwrap();
    let second_path = cache.temp_path_for_sha256(&digest).unwrap();
    assert_ne!(first_path, second_path);

    let manager = test_manager(TransferPolicy::default());
    let first = manager.download_with_header_to_path(
        &server.url,
        &first_path,
        &digest,
        Some(data.len() as u64),
        None,
        Arc::new(|_| {}),
    );
    let second = manager.download_with_header_to_path(
        &server.url,
        &second_path,
        &digest,
        Some(data.len() as u64),
        None,
        Arc::new(|_| {}),
    );
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    first.validate_staging_path(&first_path).await.unwrap();
    second.validate_staging_path(&second_path).await.unwrap();

    let cached = cache.path_for_sha256(&digest);
    tokio::fs::rename(&first_path, &cached).await.unwrap();
    tokio::fs::rename(&second_path, &cached).await.unwrap();
    assert_eq!(tokio::fs::read(&cached).await.unwrap(), *data);
}

#[tokio::test]
async fn interrupted_body_resumes_exact_unwritten_suffix() {
    let data = Arc::new(
        (0..300_000)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = test_server({
        let data = Arc::clone(&data);
        let requests = Arc::clone(&requests);
        move |request, stream| {
            requests.lock().unwrap().push(request.range);
            if request.sequence == 0 {
                write_response(stream, "200 OK", data.len(), None, &data[..120_000]);
            } else {
                let range = request.range.expect("retry must use Range");
                write_response(
                    stream,
                    "206 Partial Content",
                    range.len() as usize,
                    Some((range, data.len())),
                    &data[range.start as usize..=range.end as usize],
                );
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 0,
        retry_base: Duration::ZERO,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let progress = Arc::new(Mutex::new(Vec::new()));
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            {
                let progress = Arc::clone(&progress);
                Arc::new(move |bytes| progress.lock().unwrap().push(bytes))
            },
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&path).await.unwrap(), *data);
    assert_eq!(report.segments[0].attempts, 2);
    assert!(report.segments[0].resumed);
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0], None);
    assert_eq!(requests[1].unwrap().start, 120_000);
    let progress = progress.lock().unwrap();
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(progress.last().copied(), Some(data.len() as u64));
}

#[tokio::test]
async fn unknown_size_failure_restarts_whole_object_safely() {
    let data = Arc::new(vec![41_u8; 100_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            assert!(request.range.is_none());
            if request.sequence == 0 {
                write_response(stream, "200 OK", data.len(), None, &data[..20_000]);
            } else {
                write_response(stream, "200 OK", data.len(), None, &data);
            }
        }
    });
    let policy = TransferPolicy {
        retry_base: Duration::ZERO,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            None,
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments[0].attempts, 2);
    assert!(report.segments[0].resumed);
}

#[tokio::test]
async fn retryable_status_retries_as_a_range_attempt() {
    let data = Arc::new(
        (0..200_000)
            .map(|value| (value % 227) as u8)
            .collect::<Vec<_>>(),
    );
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence == 0 {
                write_response(stream, "503 Service Unavailable", 0, None, &[]);
            } else {
                let range = request.range.expect("retry must use Range");
                write_response(
                    stream,
                    "206 Partial Content",
                    range.len() as usize,
                    Some((range, data.len())),
                    &data[range.start as usize..=range.end as usize],
                );
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 0,
        retry_base: Duration::ZERO,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments[0].attempts, 2);
}

#[tokio::test]
async fn multipart_requests_respect_the_shared_attempt_budget() {
    let data = Arc::new(vec![13_u8; 400_000]);
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        move |request, stream| {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now_active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(40));
            let range = request.range.expect("multipart request must use Range");
            write_response(
                stream,
                "206 Partial Content",
                range.len() as usize,
                Some((range, data.len())),
                &data[range.start as usize..=range.end as usize],
            );
            active.fetch_sub(1, Ordering::SeqCst);
        }
    });
    let policy = TransferPolicy {
        ordinary_attempts: 2,
        multipart_threshold: 100_000,
        target_part_size: 50_000,
        max_multipart_parts: 8,
        max_hedges: 0,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert!(peak.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn pathological_segment_is_hedged_before_siblings_finish() {
    let data = Arc::new(
        (0..300_000)
            .map(|value| (value % 239) as u8)
            .collect::<Vec<_>>(),
    );
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            let range = request.range.expect("multipart request must use Range");
            let body = &data[range.start as usize..=range.end as usize];
            let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    body.len(), range.start, range.end, data.len()
                );
            stream.write_all(headers.as_bytes()).unwrap();
            if range.start == 0 {
                for chunk in body.chunks(1_000) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            } else if range.start == 150_000 {
                for chunk in body.chunks(10_000) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            } else {
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        }
    });
    let policy = TransferPolicy {
        multipart_threshold: 200_000,
        target_part_size: 150_000,
        max_multipart_parts: 2,
        max_hedges: 1,
        monitor_interval: Duration::from_millis(10),
        hedge_warmup: Duration::from_millis(50),
        stalled_for: Duration::from_millis(80),
        pathological_remaining: Duration::from_millis(100),
        rolling_window: Duration::from_millis(200),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let started = Instant::now();
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(tokio::fs::read(&path).await.unwrap(), *data);
    let slow = report
        .segments
        .iter()
        .find(|segment| segment.start == 0)
        .unwrap();
    assert_eq!(slow.hedges, 1);
    assert!(slow.attempts >= 2);
}

#[tokio::test]
async fn pathological_single_stream_uses_a_diagnostic_hedge() {
    let data = Arc::new(
        (0..300_000)
            .map(|value| (value % 229) as u8)
            .collect::<Vec<_>>(),
    );
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence == 0 {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                for chunk in data.chunks(1_000) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            } else {
                let range = request.range.expect("diagnostic request must use Range");
                write_response(
                    stream,
                    "206 Partial Content",
                    range.len() as usize,
                    Some((range, data.len())),
                    &data[range.start as usize..=range.end as usize],
                );
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 1,
        monitor_interval: Duration::from_millis(10),
        hedge_warmup: Duration::from_millis(50),
        stalled_for: Duration::from_millis(80),
        pathological_remaining: Duration::from_millis(100),
        rolling_window: Duration::from_millis(200),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let started = Instant::now();
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments[0].hedges, 1);
}

#[tokio::test]
async fn slow_original_remains_the_fallback_when_hedge_fails() {
    let data = Arc::new(vec![11_u8; 100_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence == 0 {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                for chunk in data.chunks(10_000) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(30));
                }
            } else {
                assert!(request.range.is_some());
                write_response(stream, "503 Service Unavailable", 0, None, &[]);
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 1,
        monitor_interval: Duration::from_millis(10),
        hedge_warmup: Duration::from_millis(30),
        stalled_for: Duration::from_millis(60),
        pathological_remaining: Duration::from_millis(50),
        max_hedge_rate: 1_000_000.0,
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(report.segments[0].hedges, 1);
}

#[tokio::test]
async fn stalled_single_and_diagnostic_do_not_multiply_attempts() {
    let data = Arc::new(vec![29_u8; 100_000]);
    let requests = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let requests = Arc::clone(&requests);
        move |request, stream| {
            requests.fetch_add(1, Ordering::Relaxed);
            let (status, range) = match request.range {
                Some(range) => ("206 Partial Content", Some((range, data.len()))),
                None => ("200 OK", None),
            };
            let expected = request.range.map_or(data.len() as u64, ByteRange::len);
            let mut headers =
                format!("HTTP/1.1 {status}\r\nContent-Length: {expected}\r\nConnection: close\r\n");
            if let Some((range, total)) = range {
                headers.push_str(&format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    range.start, range.end, total
                ));
            }
            headers.push_str("\r\n");
            stream.write_all(headers.as_bytes()).unwrap();
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(150));
        }
    });
    let policy = TransferPolicy {
        max_retries: 0,
        max_hedges: 2,
        monitor_interval: Duration::from_millis(5),
        hedge_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = manager
        .download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(requests.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn simultaneous_segment_stalls_do_not_fan_out_hedges() {
    let data = Arc::new(vec![7_u8; 300_000]);
    let request_count = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let request_count = Arc::clone(&request_count);
        move |request, stream| {
            request_count.fetch_add(1, Ordering::Relaxed);
            let range = request.range.expect("multipart request must use Range");
            let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    range.len(), range.start, range.end, data.len()
                );
            stream.write_all(headers.as_bytes()).unwrap();
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(150));
        }
    });
    let policy = TransferPolicy {
        multipart_threshold: 200_000,
        target_part_size: 150_000,
        max_multipart_parts: 2,
        max_retries: 0,
        monitor_interval: Duration::from_millis(10),
        hedge_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = manager
        .download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await;

    assert!(result.is_err());
    assert!(request_count.load(Ordering::Relaxed) <= 2);
}

#[tokio::test]
async fn ignored_range_is_never_spliced_into_partial_artifact() {
    let data = Arc::new(
        (0..300_000)
            .map(|value| (value % 233) as u8)
            .collect::<Vec<_>>(),
    );
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence == 0 {
                write_response(stream, "200 OK", data.len(), None, &data[..120_000]);
            } else {
                assert!(request.range.is_some());
                write_response(stream, "200 OK", data.len(), None, &data);
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 0,
        retry_base: Duration::ZERO,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = manager
        .download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await;

    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("expected HTTP 206"), "{error}");
}

#[tokio::test]
async fn corrupt_complete_artifact_fails_digest_verification() {
    let expected = Arc::new(vec![31_u8; 50_000]);
    let corrupt = Arc::new(vec![32_u8; 50_000]);
    let server = test_server({
        let corrupt = Arc::clone(&corrupt);
        move |request, stream| {
            assert!(request.range.is_none());
            write_response(stream, "200 OK", corrupt.len(), None, &corrupt);
        }
    });
    let manager = test_manager(TransferPolicy::default());
    let temp = tempfile::tempdir().unwrap();
    let result = manager
        .download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&expected),
            Some(expected.len() as u64),
            None,
            Arc::new(|_| {}),
        )
        .await;

    assert!(format!("{:#}", result.unwrap_err()).contains("sha256 mismatch"));
}

#[tokio::test]
async fn overlong_range_fails_before_cache_admission() {
    let data = Arc::new(vec![37_u8; 300_001]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            let range = request.range.expect("multipart request must use Range");
            if range.start == 0 {
                let body = &data[range.start as usize..=range.end as usize + 1];
                write_response(
                    stream,
                    "206 Partial Content",
                    body.len(),
                    Some((range, data.len() - 1)),
                    body,
                );
            } else {
                write_response(
                    stream,
                    "206 Partial Content",
                    range.len() as usize,
                    Some((range, data.len() - 1)),
                    &data[range.start as usize..=range.end as usize],
                );
            }
        }
    });
    let policy = TransferPolicy {
        multipart_threshold: 200_000,
        target_part_size: 150_000,
        max_multipart_parts: 2,
        max_retries: 0,
        max_hedges: 0,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = manager
        .download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data[..300_000]),
            Some(300_000),
            None,
            Arc::new(|_| {}),
        )
        .await;

    assert!(format!("{:#}", result.unwrap_err()).contains("exceeded expected length"));
}

#[tokio::test]
async fn cancellation_removes_in_flight_hedge_files() {
    let data = Arc::new(vec![17_u8; 100_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence == 0 {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                for chunk in data.chunks(1_000) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            } else {
                let range = request.range.expect("diagnostic request must use Range");
                let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    range.len(), range.start, range.end, data.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                stream
                    .write_all(&data[range.start as usize..range.start as usize + 1])
                    .unwrap();
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(500));
            }
        }
    });
    let policy = TransferPolicy {
        max_hedges: 1,
        monitor_interval: Duration::from_millis(5),
        hedge_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let expected_sha256 = sha256(&data);
    let future = manager.download_with_header_to_path(
        &server.url,
        &path,
        &expected_sha256,
        Some(data.len() as u64),
        None,
        Arc::new(|_| {}),
    );
    assert!(tokio::time::timeout(Duration::from_millis(100), future)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(20)).await;
    let names = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| !name.ends_with(".hedge")),
        "{names:?}"
    );
}

#[tokio::test]
async fn dropping_artifact_future_aborts_owned_attempts() {
    let data = Arc::new(vec![9_u8; 300_000]);
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            let range = request.range.expect("multipart request must use Range");
            let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    range.len(), range.start, range.end, data.len()
                );
            stream.write_all(headers.as_bytes()).unwrap();
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(500));
        }
    });
    let policy = TransferPolicy {
        multipart_threshold: 200_000,
        target_part_size: 150_000,
        max_multipart_parts: 2,
        hedge_warmup: Duration::from_secs(5),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let expected_sha256 = sha256(&data);
    let future = manager.download_with_header_to_path(
        &server.url,
        &path,
        &expected_sha256,
        Some(data.len() as u64),
        None,
        Arc::new(|_| {}),
    );
    assert!(tokio::time::timeout(Duration::from_millis(50), future)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(manager.health.active_count(), 0);
}
