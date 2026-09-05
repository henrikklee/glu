#![cfg(all(unix, feature = "dev-registry"))]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[test]
fn sigint_during_registry_request_exits_promptly_and_keeps_trace() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        received_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let _ = stream.write_all(b"");
    });

    let prefix = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_glu"))
        .args(["--json", "install", "vips"])
        .env("GLU_REGISTRY", format!("http://{address}"))
        .env("GLU_PREFIX", prefix.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    received_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SIGINT timed out"
        );
        thread::sleep(Duration::from_millis(10));
    };
    release_tx.send(()).unwrap();
    server.join().unwrap();

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(130), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(1));
    let envelope: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(envelope["error"]["code"], "interrupted");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Trace:"));

    let trace_dir = prefix.path().join("var/glu/traces");
    let trace_path = fs::read_dir(trace_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("trace-"))
        })
        .expect("resolution trace");
    let trace: serde_json::Value = serde_json::from_slice(&fs::read(trace_path).unwrap()).unwrap();
    assert_eq!(trace["status"], "failed");
    assert!(trace["error"]
        .as_str()
        .unwrap()
        .contains("interrupted while waiting for the registry"));
}
