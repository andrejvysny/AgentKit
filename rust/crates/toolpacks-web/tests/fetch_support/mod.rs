//! Shared helpers for the `fetch_*` integration tests: a threaded HTTP stub on
//! 127.0.0.1 and a fake host resolver.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use agentkit_toolpacks_web::fetch::ssrf::BoxFuture;
use agentkit_toolpacks_web::fetch::{FetchConfig, HostResolver, SafeHttpClient};

pub struct Resp {
    pub status: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay: Duration,
}

impl Resp {
    pub fn new(content_type: Option<&str>, body: impl Into<Vec<u8>>) -> Self {
        let headers = content_type
            .map(|ct| vec![("Content-Type".to_owned(), ct.to_owned())])
            .unwrap_or_default();
        Self {
            status: "200 OK",
            headers,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn html(body: &str) -> Self {
        Self::new(Some("text/html; charset=utf-8"), body)
    }

    pub fn redirect(to: &str) -> Self {
        let mut r = Self::new(None, "");
        r.status = "302 Found";
        r.headers.push(("Location".into(), to.into()));
        r
    }

    pub fn status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub struct Stub {
    pub port: u16,
}

impl Stub {
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn read_path(stream: &mut std::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&raw);
    text.split_whitespace().nth(1).unwrap_or("/").to_owned()
}

fn write_resp(mut stream: std::net::TcpStream, resp: Resp) {
    std::thread::sleep(resp.delay);
    let mut head = format!("HTTP/1.1 {}\r\n", resp.status);
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        resp.body.len()
    ));
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp.body);
}

/// Serves every connection with `handler(path)`; threads are detached.
pub fn serve(handler: impl Fn(&str) -> Resp + Send + Sync + 'static) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let path = read_path(&mut stream);
                write_resp(stream, handler(&path));
            });
        }
    });
    Stub { port }
}

pub const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Maps host names to fixed answers; unknown hosts fail to resolve.
pub struct FakeResolver(pub HashMap<String, Vec<IpAddr>>);

impl FakeResolver {
    pub fn new(entries: &[(&str, &[&str])]) -> Arc<Self> {
        let map = entries
            .iter()
            .map(|(host, ips)| {
                let ips = ips.iter().map(|ip| ip.parse().unwrap()).collect();
                ((*host).to_owned(), ips)
            })
            .collect();
        Arc::new(Self(map))
    }
}

impl HostResolver for FakeResolver {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<std::io::Result<Vec<SocketAddr>>> {
        let answer = self.0.get(host).cloned();
        Box::pin(async move {
            let ips = answer.ok_or_else(|| std::io::Error::other("unknown host"))?;
            Ok(ips
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect())
        })
    }
}

/// Config that lets the client reach the local stub.
pub fn local_config(stub: &Stub) -> FetchConfig {
    FetchConfig {
        allowed_ports: vec![80, 443, stub.port],
        extra_allowed_ips: vec![LOCAL],
        ..FetchConfig::default()
    }
}

pub fn client(cfg: FetchConfig, resolver: Arc<dyn HostResolver>) -> SafeHttpClient {
    SafeHttpClient::new(cfg, resolver).unwrap()
}
