#![cfg(target_os = "linux")]
//! Association-level tests against the real guest binary over the test-only
//! TCP listener: frames before HelloAck are refused, a wrong protocol version
//! disconnects, a valid handshake echoes the cmdline nonce, and a missing
//! nonce exits 3 without binding.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::api::{GuestToWorker, WorkerToGuest};
use agentkit_sandbox_contracts::protocol::{FrameDecoder, FrameHeader, encode_frame};
use base64::{Engine as _, engine::general_purpose::STANDARD};

const NONCE_HEX: &str = "00112233445566778899aabbccddee";

struct Guest {
    child: Child,
    port: u16,
    _cmdline: tempfile::TempDir,
}

impl Guest {
    fn start(cmdline: impl AsRef<str>) -> Self {
        let dir = tempfile::tempdir().expect("tmpdir");
        let ws = dir.path().join("workspace");
        let home = dir.path().join("home");
        let gstmp = dir.path().join("tmp");
        for d in [&ws, &home, &gstmp] {
            std::fs::create_dir_all(d).expect("root dirs");
        }
        let cmdline_path = dir.path().join("cmdline");
        std::fs::write(
            &cmdline_path,
            format!("{} agentkit.workspace={}", cmdline.as_ref(), ws.display()),
        )
        .expect("write cmdline");
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_agentkit-sandbox-guest"))
            .args([
                "--cmdline-file",
                cmdline_path.to_str().expect("utf8"),
                "--test-tcp",
                "--listen-port",
                &port.to_string(),
                "--test-home",
                home.to_str().expect("utf8"),
                "--test-tmp",
                gstmp.to_str().expect("utf8"),
            ])
            .env("AGENTKIT_GUEST_TEST_TCP", "1")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn guest");
        Self {
            child,
            port,
            _cmdline: dir,
        }
    }

    fn connect(&mut self) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("rt");
                    s.set_write_timeout(Some(Duration::from_secs(5)))
                        .expect("wt");
                    return s;
                }
                Err(e)
                    if Instant::now() < deadline
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::WouldBlock
                        ) =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("connect failed: {e}"),
            }
        }
    }

    fn wait_exit(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(st) = self.child.try_wait().expect("try_wait") {
                return st.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "guest did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let p = l.local_addr().expect("addr").port();
    drop(l);
    p
}

fn frame(op: &str, body: &serde_json::Value, seq: u64) -> Vec<u8> {
    let header = FrameHeader {
        protocol_version: PROTOCOL_VERSION,
        request_id: format!("t-{seq}"),
        sandbox_id: None,
        instance_generation: None,
        execution_id: None,
        operation: op.to_string(),
        sequence: seq,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    };
    encode_frame(header, body, false).expect("encode")
}

fn hello_ack_body(version: u32) -> serde_json::Value {
    serde_json::to_value(WorkerToGuest::HelloAck {
        session_digest: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(b"sess"),
        protocol_version: version,
        boot_nonce_b64: STANDARD.encode(NONCE_HEX),
        workspace_root: agentkit_sandbox_contracts::path::GuestPath::parse("/workspace").unwrap(),
    })
    .expect("json")
}

fn read_one_frame(s: &mut TcpStream) -> Option<GuestToWorker> {
    let mut d = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match s.read(&mut buf) {
            Ok(0) => return None, // EOF
            Ok(n) => {
                d.push(&buf[..n]);
                while let Ok(Some(f)) = d.next_frame(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |x| x.as_millis() as u64),
                ) {
                    if let Ok(g) = serde_json::from_value::<GuestToWorker>(f.body) {
                        return Some(g);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    None
}

fn expect_eof(s: &mut TcpStream) {
    let mut buf = [0u8; 16];
    match s.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("expected EOF, got {n} bytes"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            panic!("expected prompt disconnect, timed out instead")
        }
        Err(_) => {}
    }
}

#[test]
fn missing_nonce_exits_3_without_binding() {
    let mut g = Guest::start("quiet console=ttyS0");
    assert_eq!(g.wait_exit(), 3);
    // Listener must never have existed.
    assert!(TcpStream::connect(("127.0.0.1", g.port)).is_err());
}

#[test]
fn malformed_nonce_exits_3() {
    let mut g = Guest::start("agentkit.boot_nonce=not-hex-at-all");
    assert_eq!(g.wait_exit(), 3);
}

#[test]
fn test_tcp_requires_env_gate() {
    // Without AGENTKIT_GUEST_TEST_TCP=1 the hidden flag must be rejected.
    let dir = tempfile::tempdir().expect("tmpdir");
    let cmdline_path = dir.path().join("cmdline");
    std::fs::write(&cmdline_path, format!("agentkit.boot_nonce={NONCE_HEX}")).expect("w");
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentkit-sandbox-guest"))
        .args([
            "--cmdline-file",
            cmdline_path.to_str().expect("utf8"),
            "--test-tcp",
            "--listen-port",
            &port.to_string(),
        ])
        .env_remove("AGENTKIT_GUEST_TEST_TCP")
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            assert_eq!(st.code(), Some(2), "env-gated flag must exit 2");
            break;
        }
        assert!(Instant::now() < deadline, "should exit promptly");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
}

#[test]
fn frame_before_helloack_is_refused() {
    let mut g = Guest::start(format!("agentkit.boot_nonce={NONCE_HEX}"));
    let mut s = g.connect();
    // An FsOp request arrives before any HelloAck: terminate the association.
    let body = serde_json::to_value(WorkerToGuest::HealthPing {}).expect("json");
    s.write_all(&frame("health_ping", &body, 0)).expect("write");
    s.flush().expect("flush");
    expect_eof(&mut s);
}

#[test]
fn wrong_protocol_version_disconnects() {
    let mut g = Guest::start(format!("agentkit.boot_nonce={NONCE_HEX}"));
    let mut s = g.connect();
    s.write_all(&frame("hello_ack", &hello_ack_body(99), 0))
        .expect("write");
    s.flush().expect("flush");
    expect_eof(&mut s);
}

#[test]
fn valid_handshake_echoes_nonce_and_bounds() {
    let mut g = Guest::start(format!(
        "agentkit.boot_nonce={NONCE_HEX} ro agentkit.workspace=/workspace"
    ));
    let mut s = g.connect();
    s.write_all(&frame("hello_ack", &hello_ack_body(PROTOCOL_VERSION), 0))
        .expect("write");
    s.flush().expect("flush");
    let hello = read_one_frame(&mut s).expect("hello frame");
    match hello {
        GuestToWorker::Hello {
            boot_nonce_b64,
            guest_agent_version,
            protocol_min,
            protocol_max,
            capabilities,
        } => {
            let echoed = STANDARD.decode(&boot_nonce_b64).expect("b64");
            let expected: Vec<u8> = (0..NONCE_HEX.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&NONCE_HEX[i..i + 2], 16).expect("hex"))
                .collect();
            assert_eq!(echoed, expected, "nonce echo mismatch");
            assert_eq!(guest_agent_version, env!("CARGO_PKG_VERSION"));
            assert_eq!(protocol_min, 1);
            assert_eq!(protocol_max, 1);
            assert!(capabilities.iter().any(|c| c == "search"));
        }
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn health_ping_answers_health() {
    let mut g = Guest::start(format!("agentkit.boot_nonce={NONCE_HEX}"));
    let mut s = g.connect();
    s.write_all(&frame("hello_ack", &hello_ack_body(PROTOCOL_VERSION), 0))
        .expect("write");
    let mut d = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    // drain hello
    let _ = read_one_frame(&mut s).expect("hello");
    let ping = serde_json::to_value(WorkerToGuest::HealthPing {}).expect("json");
    s.write_all(&frame("health_ping", &ping, 1)).expect("write");
    s.flush().expect("flush");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                d.push(&buf[..n]);
                while let Ok(Some(f)) = d.next_frame(now_ms()) {
                    if let Ok(GuestToWorker::Health {
                        uptime_ms,
                        load_permille,
                    }) = serde_json::from_value::<GuestToWorker>(f.body)
                    {
                        let _ = load_permille;
                        assert!(uptime_ms < 60_000, "uptime sane");
                        return;
                    }
                }
            }
            Err(_) => break,
        }
    }
    panic!("no Health reply");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |x| x.as_millis() as u64)
}
