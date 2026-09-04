use super::{
    first_body_byte_latency, parse_content_range, segment_ranges, should_report,
    should_use_multipart, AttemptFailure, AttemptKind, AttemptProgress, ByteRange, RequestBudget,
    TransferEvent, TransferFailure, TransferPolicy, MIB,
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
fn chunking_starts_above_one_mib() {
    let policy = TransferPolicy::default();
    assert!(!should_use_multipart(MIB, &policy));
    assert!(should_use_multipart(MIB + 1, &policy));
}

#[test]
fn segment_ranges_stay_fixed_size_for_very_large_artifacts() {
    let ranges = segment_ranges(64 * MIB, &TransferPolicy::default());
    assert_eq!(ranges.len(), 64);
    assert_eq!(ranges.first().unwrap().start, 0);
    assert_eq!(ranges.last().unwrap().end, 64 * MIB - 1);
    assert!(ranges.iter().all(|range| range.len() <= MIB));
    assert_eq!(
        segment_ranges(164 * MIB, &TransferPolicy::default()).len(),
        164
    );
    let huge = segment_ranges(2_240 * MIB, &TransferPolicy::default());
    assert_eq!(huge.len(), 2_240);
    assert!(huge.iter().all(|range| range.len() <= MIB));
    assert_eq!(huge.last().unwrap().end, 2_240 * MIB - 1);
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
    assert_eq!(ranges.len(), 101);
    assert_eq!(ranges.first().unwrap().start, 0);
    assert_eq!(ranges.last().unwrap().end, 100 * MIB + 122);
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].end + 1, pair[1].start);
    }
}

#[test]
fn first_byte_latency_excludes_request_queue_time() {
    let mut started = TransferEvent::attempt(
        "attempt_started",
        7,
        AttemptKind::Initial,
        ByteRange { start: 0, end: 99 },
    );
    started.at_seconds = 5.0;
    started.queue_seconds = Some(4.0);
    let mut first = TransferEvent::simple("first_body_byte");
    first.attempt = Some(7);
    first.at_seconds = 5.25;

    assert_eq!(first_body_byte_latency(&[started, first]), Some(0.25));
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
    assert!(AttemptFailure::EarlyEof {
        expected: 10,
        received: 3,
    }
    .retryable());
    assert!(!AttemptFailure::InvalidRange("wrong range".into()).retryable());
    assert!(!AttemptFailure::LocalIo("disk full".into()).retryable());
}

#[test]
fn ordinary_progress_is_not_a_tail_emergency() {
    let policy = TransferPolicy {
        emergency_warmup: Duration::ZERO,
        pathological_remaining: Duration::from_millis(20),
        rolling_window: Duration::from_secs(1),
        ..TransferPolicy::default()
    };
    let progress = AttemptProgress::new(policy.rolling_window);
    progress.request_started();
    thread::sleep(Duration::from_millis(2));
    progress.progress(1_000_000);
    assert!(progress
        .snapshot(
            ByteRange {
                start: 0,
                end: 9_999_999,
            },
            &policy,
        )
        .is_none());
}

#[test]
fn no_progress_is_tail_emergency_evidence() {
    let policy = TransferPolicy {
        emergency_warmup: Duration::from_millis(1),
        stalled_for: Duration::from_millis(2),
        rolling_window: Duration::from_secs(1),
        ..TransferPolicy::default()
    };
    let progress = AttemptProgress::new(policy.rolling_window);
    progress.request_started();
    thread::sleep(Duration::from_millis(4));
    assert!(progress
        .snapshot(ByteRange { start: 0, end: 999 }, &policy)
        .is_some());
}

#[tokio::test]
async fn request_budget_runs_higher_priority_first() {
    let budget = RequestBudget::new(1);
    let held = budget.acquire(0).await.unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let low = budget.clone();
    let low_sender = sender.clone();
    tokio::spawn(async move {
        let _permit = low.acquire(10).await.unwrap();
        low_sender.send("low").unwrap();
    });
    while budget.waiting() != 1 {
        tokio::task::yield_now().await;
    }
    let high = budget.clone();
    tokio::spawn(async move {
        let _permit = high.acquire(1).await.unwrap();
        sender.send("high").unwrap();
    });
    while budget.waiting() != 2 {
        tokio::task::yield_now().await;
    }
    drop(held);
    assert_eq!(receiver.recv().await, Some("high"));
    assert_eq!(receiver.recv().await, Some("low"));
}

#[derive(Debug)]
struct TestRequest {
    sequence: usize,
    path: String,
    range: Option<ByteRange>,
    authorization: Option<String>,
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
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
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
    let authorization = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    });
    TestRequest {
        sequence,
        path,
        range,
        authorization,
    }
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

fn write_redirect(stream: &mut TcpStream, location: &str) {
    write!(
        stream,
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
}

fn sha256(bytes: &[u8]) -> String {
    crate::hash::hex_lower(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn test_manager(policy: TransferPolicy) -> super::TransferRuntime {
    let client = || {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    };
    let resolver = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .read_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    super::TransferRuntime::with_policy(resolver, vec![client()], client(), policy)
}

#[test]
fn transport_assignment_explores_equal_unknown_pools() {
    let clients = (0..4)
        .map(|_| reqwest::Client::builder().build().unwrap())
        .collect();
    let transports = super::OrdinaryTransports::new(clients, 4);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let first = runtime.block_on(async {
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(transports.acquire(0, 100).await.unwrap());
        }
        leases
    });

    assert_eq!(
        first.iter().map(|lease| lease.id).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn transport_assignment_remains_fixed_round_robin_after_observation() {
    let clients = (0..2)
        .map(|_| reqwest::Client::builder().build().unwrap())
        .collect();
    let transports = super::OrdinaryTransports::new(clients, 4);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut first = transports.acquire(0, 100).await.unwrap();
        let mut second = transports.acquire(0, 100).await.unwrap();
        first.record_bytes(100);
        second.record_bytes(10);
        drop(first);
        drop(second);

        assert_eq!(transports.acquire(0, 100).await.unwrap().id, 0);
        assert_eq!(transports.acquire(0, 100).await.unwrap().id, 1);
    });
}

#[test]
fn dropped_attempt_removes_unreceived_transport_work() {
    let clients = vec![reqwest::Client::builder().build().unwrap()];
    let transports = super::OrdinaryTransports::new(clients, 1);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut attempt = runtime.block_on(transports.acquire(0, 100)).unwrap();
    attempt.record_bytes(40);
    drop(attempt);

    assert_eq!(
        transports.pools[0]
            .outstanding_bytes
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(transports.pools[0].active.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn fixed_transport_lanes_hold_four_attempts_each() {
    let clients = (0..4)
        .map(|_| reqwest::Client::builder().build().unwrap())
        .collect();
    let transports = super::OrdinaryTransports::new(clients, 4);
    let mut leases = Vec::new();
    for _ in 0..16 {
        leases.push(transports.acquire(0, 100).await.unwrap());
    }

    assert_eq!(transports.active(), 16);
    assert!(transports
        .pools
        .iter()
        .all(|pool| pool.active.load(Ordering::Relaxed) == 4));

    let waiting = transports.clone();
    let waiter = tokio::spawn(async move { waiting.acquire(0, 100).await.unwrap() });
    while transports.pools[0].budget.waiting() != 1 {
        tokio::task::yield_now().await;
    }
    let released = leases.iter().position(|lease| lease.id == 0).unwrap();
    drop(leases.swap_remove(released));
    let replacement = waiter.await.unwrap();
    assert_eq!(replacement.id, 0);
    assert_eq!(replacement.active_at_admission, 4);
}

#[test]
fn only_the_highest_priority_artifact_enters_emergency_tail() {
    let manager = test_manager(TransferPolicy::default());
    let high = manager.register_artifact(1, 5);
    let low = manager.register_artifact(10, 1);

    assert!(high.handle.tail_eligible().is_none());
    assert!(low.handle.tail_eligible().is_none());
    for _ in 0..3 {
        high.handle.mark_range_complete();
    }
    assert!(high.handle.tail_eligible().is_some());
    assert!(low.handle.tail_eligible().is_none());
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
    assert_eq!(report.segments[0].emergencies, 0);
    assert_eq!(report.diagnostics.logical_bytes, data.len() as u64);
    assert_eq!(report.diagnostics.wire_bytes, data.len() as u64);
    assert_eq!(report.diagnostics.transport_profile, "test");
    assert_eq!(report.diagnostics.transport_count, 1);
    let [first_body, body_complete, verify_complete, sync_complete] =
        report.diagnostics.subphase_boundaries().unwrap();
    assert!(first_body <= body_complete);
    assert!(body_complete <= verify_complete);
    assert!(verify_complete <= sync_complete);
    assert!(report
        .diagnostics
        .events
        .iter()
        .any(|event| { event.event == "attempt_started" && event.transport_id == Some(0) }));
    assert!(report.diagnostics.events.iter().any(|event| {
        event.event == "response_headers"
            && event.http_version.is_some()
            && event
                .response_origin
                .as_deref()
                .is_some_and(|origin| origin.starts_with("http://127.0.0.1:"))
    }));
    assert!(report
        .diagnostics
        .events
        .iter()
        .any(|event| event.event == "first_body_byte"));
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
        .all(|segment| segment.attempts == 1 && segment.emergencies == 0));
}

#[tokio::test]
async fn resolves_one_redirect_then_reuses_the_cdn_url_for_all_ranges() {
    let data = Arc::new(vec![67_u8; 300_000]);
    let redirects = Arc::new(AtomicUsize::new(0));
    let cdn_requests = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let redirects = Arc::clone(&redirects);
        let cdn_requests = Arc::clone(&cdn_requests);
        move |request, stream| {
            if request.path == "/artifact" {
                redirects.fetch_add(1, Ordering::Relaxed);
                assert_eq!(request.authorization.as_deref(), Some("Bearer test-token"));
                assert_eq!(request.range, None);
                write_redirect(stream, "/cdn");
                return;
            }
            assert_eq!(request.path, "/cdn");
            assert_eq!(request.authorization, None);
            cdn_requests.fetch_add(1, Ordering::Relaxed);
            let range = request.range.expect("direct CDN request must use Range");
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
        multipart_threshold: 100_000,
        target_part_size: 100_000,
        emergency_per_range: 0,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_after_redirect_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            ("Authorization".into(), "Bearer test-token".into()),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(redirects.load(Ordering::Relaxed), 1);
    assert_eq!(cdn_requests.load(Ordering::Relaxed), 3);
    assert!(report
        .diagnostics
        .events
        .iter()
        .any(|event| event.event == "source_url_resolved" && event.status == Some(307)));
}

#[tokio::test]
async fn rejected_signed_url_is_resolved_again_and_retried() {
    let data = Arc::new(vec![71_u8; 50_000]);
    let redirects = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let redirects = Arc::clone(&redirects);
        move |request, stream| match request.path.as_str() {
            "/artifact" => {
                let redirect = redirects.fetch_add(1, Ordering::Relaxed);
                write_redirect(stream, if redirect == 0 { "/expired" } else { "/cdn" });
            }
            "/expired" => write_response(stream, "403 Forbidden", 0, None, &[]),
            "/cdn" => {
                let range = request.range.expect("retry resumes with a range request");
                write_response(
                    stream,
                    "206 Partial Content",
                    range.len() as usize,
                    Some((range, data.len())),
                    &data[range.start as usize..=range.end as usize],
                );
            }
            path => panic!("unexpected request path {path}"),
        }
    });
    let policy = TransferPolicy {
        retry_base: Duration::ZERO,
        emergency_per_range: 0,
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let report = manager
        .download_after_redirect_with_header_to_path(
            &server.url,
            &path,
            &sha256(&data),
            Some(data.len() as u64),
            ("Authorization".into(), "Bearer test-token".into()),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(path).await.unwrap(), *data);
    assert_eq!(redirects.load(Ordering::Relaxed), 2);
    assert_eq!(report.segments[0].attempts, 2);
    assert_eq!(report.diagnostics.retries, 1);
}

#[tokio::test]
async fn uniformly_slow_bulk_transfers_do_not_start_emergencies() {
    let data = Arc::new(vec![47_u8; 100_000]);
    let requests = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let requests = Arc::clone(&requests);
        move |request, stream| {
            assert!(request.range.is_none());
            requests.fetch_add(1, Ordering::Relaxed);
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
                thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let policy = TransferPolicy {
        ordinary_attempts: 3,
        emergency_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        max_emergency_rate: 200_000.0,
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let digest = sha256(&data);
    let first_path = temp.path().join("first.tmp");
    let second_path = temp.path().join("second.tmp");
    let third_path = temp.path().join("third.tmp");
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
    let third = manager.download_with_header_to_path(
        &server.url,
        &third_path,
        &digest,
        Some(data.len() as u64),
        None,
        Arc::new(|_| {}),
    );

    assert!(tokio::time::timeout(Duration::from_millis(100), async {
        let _ = tokio::join!(first, second, third);
    })
    .await
    .is_err());
    assert_eq!(requests.load(Ordering::Relaxed), 3);
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
        emergency_per_range: 0,
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
async fn complete_expected_bytes_survive_a_trailing_body_error() {
    let data = Arc::new(vec![53_u8; 100_000]);
    let requests = Arc::new(AtomicUsize::new(0));
    let server = test_server({
        let data = Arc::clone(&data);
        let requests = Arc::clone(&requests);
        move |request, stream| {
            assert!(request.range.is_none());
            requests.fetch_add(1, Ordering::Relaxed);
            write_response(stream, "200 OK", data.len() + 1, None, &data);
        }
    });
    let policy = TransferPolicy {
        emergency_per_range: 0,
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
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert_eq!(report.segments[0].attempts, 1);
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
async fn retryable_statuses_keep_retrying_beyond_the_old_attempt_limit() {
    let data = Arc::new(
        (0..200_000)
            .map(|value| (value % 227) as u8)
            .collect::<Vec<_>>(),
    );
    let server = test_server({
        let data = Arc::clone(&data);
        move |request, stream| {
            if request.sequence < 5 {
                let status = match request.sequence {
                    0 => "408 Request Timeout",
                    1 => "429 Too Many Requests",
                    _ => "503 Service Unavailable",
                };
                write_response(stream, status, 0, None, &[]);
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
        emergency_per_range: 0,
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
    assert_eq!(report.segments[0].attempts, 6);
    assert_eq!(report.diagnostics.retries, 5);
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

        emergency_per_range: 0,
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
async fn multipart_keeps_one_priority_waiter_behind_each_busy_lane() {
    let data = Arc::new(vec![17_u8; 300_000]);
    let server = test_server(move |request, stream| {
        let range = request.range.expect("multipart request must use Range");
        let headers = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/300000\r\nConnection: close\r\n\r\n",
            range.len(), range.start, range.end
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(500));
    });
    let policy = TransferPolicy {
        ordinary_attempts: 1,
        multipart_threshold: 100_000,
        target_part_size: 100_000,
        emergency_warmup: Duration::from_secs(5),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let task_manager = manager.clone();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("artifact.tmp");
    let url = server.url.clone();
    let digest = sha256(&data);
    let task = tokio::spawn(async move {
        task_manager
            .download_with_header_to_path(
                &url,
                &path,
                &digest,
                Some(data.len() as u64),
                None,
                Arc::new(|_| {}),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pool = &manager.ordinary_http.pools[0];
            if pool.active.load(Ordering::Relaxed) == 1 && pool.budget.waiting() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one active range retains one queued lookahead range");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn final_pathological_range_uses_one_emergency() {
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

        emergency_per_range: 1,
        monitor_interval: Duration::from_millis(10),
        emergency_warmup: Duration::from_millis(50),
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
    assert_eq!(slow.emergencies, 1);
    assert!(slow.attempts >= 2);
    assert!(report
        .diagnostics
        .events
        .iter()
        .any(|event| event.event == "emergency_won"));
    assert!(report
        .diagnostics
        .events
        .iter()
        .any(|event| event.event == "attempt_cancelled"));
}

#[tokio::test]
async fn final_pathological_single_stream_uses_one_emergency() {
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
                let range = request.range.expect("emergency request must use Range");
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
        emergency_per_range: 1,
        monitor_interval: Duration::from_millis(10),
        emergency_warmup: Duration::from_millis(50),
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
    assert_eq!(report.segments[0].emergencies, 1);
}

#[tokio::test]
async fn slow_original_remains_the_fallback_when_emergency_fails() {
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
        emergency_per_range: 1,
        monitor_interval: Duration::from_millis(10),
        emergency_warmup: Duration::from_millis(30),
        stalled_for: Duration::from_millis(60),
        pathological_remaining: Duration::from_millis(50),
        max_emergency_rate: 1_000_000.0,
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
    assert_eq!(report.segments[0].emergencies, 1);
}

#[tokio::test]
async fn stalled_single_launches_only_one_emergency_attempt() {
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
        emergency_per_range: 1,
        monitor_interval: Duration::from_millis(5),
        emergency_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        manager.download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        ),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(requests.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn two_final_stalled_ranges_have_one_emergency_each() {
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

        monitor_interval: Duration::from_millis(10),
        emergency_warmup: Duration::from_millis(20),
        stalled_for: Duration::from_millis(30),
        pathological_remaining: Duration::from_millis(30),
        rolling_window: Duration::from_millis(100),
        ..TransferPolicy::default()
    };
    let manager = test_manager(policy);
    let temp = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        manager.download_with_header_to_path(
            &server.url,
            &temp.path().join("artifact.tmp"),
            &sha256(&data),
            Some(data.len() as u64),
            None,
            Arc::new(|_| {}),
        ),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(request_count.load(Ordering::Relaxed), 4);
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
        emergency_per_range: 0,
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

    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("sha256 mismatch"));
    let failure = error
        .downcast_ref::<TransferFailure>()
        .expect("terminal transfer retains diagnostics");
    assert!(failure
        .diagnostics()
        .events
        .iter()
        .any(|event| event.event == "verification_failed"));
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

        emergency_per_range: 0,
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
async fn cancellation_removes_in_flight_emergency_files() {
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
                let range = request.range.expect("emergency request must use Range");
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
        emergency_per_range: 1,
        monitor_interval: Duration::from_millis(5),
        emergency_warmup: Duration::from_millis(20),
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
        names.iter().all(|name| !name.ends_with(".emergency")),
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

        emergency_warmup: Duration::from_secs(5),
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
    assert_eq!(manager.ordinary_http.active(), 0);
    assert_eq!(manager.emergency_active(), 0);
    assert!(manager
        .activity
        .lock()
        .expect("transfer activity lock poisoned")
        .artifacts
        .is_empty());
}
