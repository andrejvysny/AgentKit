//! Shared 127.0.0.1 HTTP stub for the backend integration tests.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Resp {
    pub status: &'static str,
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: String,
}

pub fn ok(body: &str) -> Resp {
    status("200 OK", body)
}

pub fn status(status: &'static str, body: &str) -> Resp {
    Resp {
        status,
        headers: vec![],
        body: body.to_owned(),
    }
}

pub struct Stub {
    pub base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    /// Raw requests (head and body) in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= end + 4 + len {
                break;
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// Serves `responses` to sequential connections; the thread is detached.
pub fn serve(responses: Vec<Resp>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    std::thread::spawn(move || {
        for resp in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let raw = read_request(&mut stream);
            log.lock().unwrap().push(raw);
            let mut head = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                resp.status,
                resp.body.len()
            );
            for (k, v) in resp.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(resp.body.as_bytes());
        }
    });
    Stub { base, requests }
}

/// Lower-cased header lookup in a raw request.
pub fn header(raw: &str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    raw.lines()
        .take_while(|l| !l.is_empty())
        .find(|l| l.to_ascii_lowercase().starts_with(&prefix))
        .map(|l| l[prefix.len()..].trim().to_owned())
}

pub fn request_line(raw: &str) -> &str {
    raw.lines().next().unwrap_or("")
}

pub fn body(raw: &str) -> &str {
    raw.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}
