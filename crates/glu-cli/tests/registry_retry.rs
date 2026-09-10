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
fn unknown_package_has_concise_error_and_preserves_last_trace() {
    for json in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let body = r#"{"error":"not_found","name":"pyton@3.12","suggestions":["python@3.12","python@3.11","python@3.13"]}"#;
            write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let prefix = tempfile::tempdir().unwrap();
        let traces = prefix.path().join("var/glu/traces");
        fs::create_dir_all(&traces).unwrap();
        fs::write(traces.join("previous.json"), "{}\n").unwrap();
        std::os::unix::fs::symlink("previous.json", traces.join("last.json")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_glu"));
        if json {
            command.arg("--json");
        }
        let output = command
            .args(["install", "pyton@3.12"])
            .env("GLU_REGISTRY", format!("http://{address}"))
            .env("GLU_PREFIX", prefix.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let message = "package 'pyton@3.12' not found\n       Did you mean any of: 'python@3.12', 'python@3.11', 'python@3.13'?";
        if json {
            let envelope: serde_json::Value = serde_json::from_str(&stderr).unwrap();
            assert_eq!(envelope["error"]["code"], "package_not_found");
            assert_eq!(envelope["error"]["message"], message);
        } else {
            assert_eq!(stderr, format!("Error: {message}\n"));
        }
        assert_eq!(
            fs::read_link(traces.join("last.json")).unwrap(),
            std::path::Path::new("previous.json")
        );
        assert_eq!(fs::read(traces.join("previous.json")).unwrap(), b"{}\n");
        assert_eq!(fs::read_dir(&traces).unwrap().count(), 2);
    }
}

#[test]
fn sigint_during_registry_request_exits_promptly_without_trace() {
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
    assert!(!envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Trace:"));
    assert!(!prefix.path().join("var/glu/traces").exists());
}
