//! Fakes for the proxy's ports and small socket helpers. The fake connector
//! is the only stand-in for the network: it maps public test addresses to
//! local listeners, after the proxy has applied the address rule.

#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_egress::{Connector, EgressConfig, EgressPorts, EgressProxy, Resolver, Target};
use agentkit_kernel::Runtime;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// A public address no test ever reaches: the fake connector maps it.
pub const PUBLIC: &str = "93.184.216.34";
/// Fixed clock of every test proxy.
pub const NOW_MS: i64 = 1_700_000_000_000;
const WAIT: Duration = Duration::from_secs(5);

pub fn ip(text: &str) -> IpAddr {
    text.parse().expect("address")
}

pub fn target(host: &str, port: u16) -> Target {
    Target::new(host, port).expect("valid target")
}

/// Await with a bound, so a regression fails instead of hanging.
pub async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, fut).await.expect("timed out")
}

struct FixedClock;

impl Runtime for FixedClock {
    fn now_ms(&self) -> i64 {
        NOW_MS
    }

    fn id(&self) -> String {
        "fixed".into()
    }
}

/// Answers from a table; records every name asked.
#[derive(Default)]
pub struct FakeResolver {
    answers: Mutex<HashMap<String, Vec<IpAddr>>>,
    hangs: Mutex<Vec<String>>,
    pub asked: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub fn set(&self, host: &str, addrs: Vec<IpAddr>) {
        self.answers.lock().unwrap().insert(host.into(), addrs);
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    /// `host` never answers.
    pub fn hang(&self, host: &str) {
        self.hangs.lock().unwrap().push(host.into());
    }
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.asked.lock().unwrap().push(host.into());
        if self.hangs.lock().unwrap().iter().any(|h| h == host) {
            std::future::pending::<()>().await;
        }
        let answer = self.answers.lock().unwrap().get(host).cloned();
        answer.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such name"))
    }
}

/// Maps a public test address to a local listener; records every address
/// the proxy asked for. Unmapped addresses are refused.
#[derive(Default)]
pub struct FakeConnector {
    routes: Mutex<HashMap<SocketAddr, SocketAddr>>,
    delay: Mutex<Duration>,
    pub dialed: Mutex<Vec<SocketAddr>>,
}

impl FakeConnector {
    pub fn route(&self, public: SocketAddr, local: SocketAddr) {
        self.routes.lock().unwrap().insert(public, local);
    }

    pub fn dialed(&self) -> Vec<SocketAddr> {
        self.dialed.lock().unwrap().clone()
    }

    /// Every later connect waits `delay` first.
    pub fn slow_down(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }
}

#[async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.dialed.lock().unwrap().push(addr);
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        let local = self.routes.lock().unwrap().get(&addr).copied();
        match local {
            Some(local) => TcpStream::connect(local).await,
            None => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "unrouted")),
        }
    }
}

/// A proxy on a socket in its own temporary directory.
pub struct Harness {
    pub proxy: EgressProxy,
    pub resolver: Arc<FakeResolver>,
    pub connector: Arc<FakeConnector>,
    pub dir: tempfile::TempDir,
}

pub fn start(config: EgressConfig) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let resolver = Arc::new(FakeResolver::default());
    let connector = Arc::new(FakeConnector::default());
    let ports = EgressPorts {
        resolver: resolver.clone(),
        connector: connector.clone(),
        runtime: Arc::new(FixedClock),
    };
    let path = dir.path().join("egress.sock");
    let proxy = EgressProxy::bind(&path, config, ports).expect("bind");
    Harness {
        proxy,
        resolver,
        connector,
        dir,
    }
}

impl Harness {
    pub async fn client(&self) -> UnixStream {
        within(UnixStream::connect(self.proxy.path()))
            .await
            .expect("connect to proxy")
    }

    /// `name` resolves to [`PUBLIC`], whose `port` reaches `local`.
    pub fn route_name(&self, name: &str, port: u16, local: SocketAddr) {
        self.resolver.set(name, vec![ip(PUBLIC)]);
        self.connector
            .route(SocketAddr::new(ip(PUBLIC), port), local);
    }

    /// Send a CONNECT and read the response head.
    pub async fn connect(&self, authority: &str) -> (UnixStream, String) {
        let mut client = self.client().await;
        let head = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        within(client.write_all(head.as_bytes()))
            .await
            .expect("send head");
        let response = read_head(&mut client).await;
        (client, response)
    }
}

/// Read up to the blank line, one byte at a time so nothing after it is
/// consumed. Empty if the peer closed first.
pub async fn read_head(stream: &mut UnixStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match within(stream.read(&mut byte)).await {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Everything until the peer closes (or resets).
pub async fn read_all(stream: &mut UnixStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match within(stream.read(&mut buf)).await {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True when the peer closes within `bound` and sends nothing first.
pub async fn closes_within(stream: &mut UnixStream, bound: Duration) -> bool {
    let mut buf = [0u8; 64];
    matches!(
        tokio::time::timeout(bound, stream.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// A local TCP echo server.
pub async fn echo_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    addr
}

/// A local TCP server that discards input and counts the bytes.
pub async fn sink_upstream() -> (SocketAddr, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(AtomicU64::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = stream.read(&mut buf).await {
                    seen.fetch_add(n as u64, Ordering::SeqCst);
                }
            });
        }
    });
    (addr, count)
}

/// Write `data` and read the same number of bytes back.
pub async fn round_trip(stream: &mut UnixStream, data: &[u8]) -> Vec<u8> {
    within(stream.write_all(data)).await.expect("send");
    let mut back = vec![0u8; data.len()];
    within(stream.read_exact(&mut back)).await.expect("echo");
    back
}

/// One TLS handshake record holding a minimal ClientHello that names
/// `name` (no `server_name` extension for `None`). The proxy reads only
/// the record framing and the server name.
pub fn client_hello(name: Option<&str>) -> Vec<u8> {
    let be16 = |n: usize| u16::try_from(n).expect("fits u16").to_be_bytes();
    let mut extensions = vec![0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04];
    if let Some(name) = name {
        let name = name.as_bytes();
        extensions.extend([0, 0]);
        extensions.extend(be16(name.len() + 5));
        extensions.extend(be16(name.len() + 3));
        extensions.push(0);
        extensions.extend(be16(name.len()));
        extensions.extend_from_slice(name);
    }
    let mut body = vec![3, 3];
    body.extend([7u8; 32]);
    body.extend([0, 0, 2, 0x13, 0x01, 1, 0]);
    body.extend(be16(extensions.len()));
    body.extend(extensions);
    let len = u32::try_from(body.len()).expect("fits").to_be_bytes();
    let mut record = vec![22, 3, 1];
    record.extend(be16(body.len() + 4));
    record.extend([1, len[1], len[2], len[3]]);
    record.extend(body);
    record
}

/// Open an established tunnel through an echo upstream: send a ClientHello
/// naming `name` and read it back.
pub async fn greet(stream: &mut UnixStream, name: Option<&str>) {
    let hello = client_hello(name);
    assert_eq!(round_trip(stream, &hello).await, hello, "hello echoed");
}

/// The test config: small bounds so limit tests are fast.
pub fn config() -> EgressConfig {
    EgressConfig {
        head_timeout: Duration::from_millis(500),
        connect_timeout: Duration::from_millis(500),
        ..EgressConfig::default()
    }
}
