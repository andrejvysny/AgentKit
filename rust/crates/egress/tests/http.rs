//! Plain HTTP on port 80 and malformed requests.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use support::{PUBLIC, config, read_all, start, target, within};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// One-shot origin: hands back the bytes it received up to the end of the
/// head plus `body_len` bytes, then answers [`RESPONSE`] and closes.
async fn origin(body_len: usize) -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = stream.read(&mut buf).await.expect("read");
            seen.extend_from_slice(&buf[..n]);
            let head_end = seen.windows(4).position(|w| w == b"\r\n\r\n");
            if n == 0 || head_end.is_some_and(|at| seen.len() >= at + 4 + body_len) {
                break;
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&seen).into_owned());
        let _ = stream.write_all(RESPONSE.as_bytes()).await;
    });
    (addr, rx)
}

/// One-shot origin that keeps reading until the proxy has sent nothing for
/// a while (or closed), then hands back what it received and whether the
/// proxy closed its side, and answers [`RESPONSE`].
async fn collecting_origin() -> (SocketAddr, oneshot::Receiver<(String, bool)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        let idle = Duration::from_millis(500);
        let closed = loop {
            match tokio::time::timeout(idle, stream.read(&mut buf)).await {
                Ok(Ok(0) | Err(_)) => break true,
                Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
                Err(_) => break false,
            }
        };
        let _ = tx.send((String::from_utf8_lossy(&seen).into_owned(), closed));
        let _ = stream.write_all(RESPONSE.as_bytes()).await;
    });
    (addr, rx)
}

async fn send(h: &support::Harness, request: &str) -> String {
    let mut client = h.client().await;
    within(client.write_all(request.as_bytes()))
        .await
        .expect("send");
    read_all(&mut client).await
}

#[tokio::test]
async fn plain_http_is_forwarded_in_origin_form_with_connection_close() {
    let h = start(config());
    let (local, seen) = origin(4).await;
    h.route_name("example.test", 80, local);
    h.proxy.set_policy(vec![target("example.test", 80)]);
    let request = "POST http://Example.Test/upload?x=1 HTTP/1.1\r\n\
                   Host: other.test\r\n\
                   Proxy-Connection: keep-alive\r\n\
                   Proxy-Authorization: Basic c2VjcmV0\r\n\
                   Connection: keep-alive\r\n\
                   Content-Length: 4\r\n\r\nbody";
    let answer = send(&h, request).await;
    assert_eq!(answer, RESPONSE);
    let upstream = within(seen).await.expect("origin saw the request");
    assert_eq!(
        upstream,
        "POST /upload?x=1 HTTP/1.1\r\nHost: example.test\r\nContent-Length: 4\r\n\
         Connection: close\r\n\r\nbody"
    );
    let public: SocketAddr = format!("{PUBLIC}:80").parse().expect("addr");
    assert_eq!(h.connector.dialed(), [public]);
    assert_eq!(h.proxy.stats().tunnels_established, 1);
}

#[tokio::test]
async fn plain_http_needs_a_grant_on_port_80() {
    let h = start(config());
    let (local, _seen) = origin(0).await;
    h.route_name("example.test", 80, local);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let answer = send(&h, "GET http://example.test/ HTTP/1.1\r\n\r\n").await;
    assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!((denial.host.as_str(), denial.port), ("example.test", 80));
}

#[tokio::test]
async fn plain_http_on_another_port_is_refused_even_when_granted() {
    let h = start(config());
    h.proxy.set_policy(vec![target("example.test", 8080)]);
    let answer = send(&h, "GET http://example.test:8080/ HTTP/1.1\r\n\r\n").await;
    assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
    assert!(answer.contains("port 80 only"), "{answer}");
    assert!(h.resolver.asked().is_empty());
    assert_eq!(
        h.proxy.drain(0).cursor,
        0,
        "a malformed request is not a denial"
    );
}

#[tokio::test]
async fn anything_else_is_answered_400_and_closed() {
    let h = start(config());
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let requests = [
        "\u{16}\u{3}\u{1}\u{0}\u{a5}\r\n\r\n",
        "hello\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: example.test\r\n\r\n",
        "GET https://example.test/ HTTP/1.1\r\n\r\n",
        "CONNECT example.test HTTP/1.1\r\n\r\n",
        "CONNECT *.test:443 HTTP/1.1\r\n\r\n",
        "CONNECT example.test:443 HTTP/1.1\r\nX: a\u{0}b\r\n\r\n",
    ];
    for request in requests {
        let answer = send(&h, request).await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{request:?}: {answer}");
        assert!(answer.contains("Connection: close"), "{answer}");
    }
    assert!(h.resolver.asked().is_empty());
    assert!(h.connector.dialed().is_empty());
    let stats = h.proxy.stats();
    assert_eq!(
        (stats.bad_requests, stats.denials),
        (requests.len() as u64, 0)
    );
}

#[tokio::test]
async fn a_pipelined_request_sent_with_the_head_is_not_forwarded() {
    let h = start(config());
    let (local, seen) = collecting_origin().await;
    h.route_name("example.test", 80, local);
    h.proxy.set_policy(vec![target("example.test", 80)]);
    let answer = send(
        &h,
        "GET http://example.test/ HTTP/1.1\r\n\r\n\
         GET http://example.test/second HTTP/1.1\r\nHost: example.test\r\n\r\n",
    )
    .await;
    assert_eq!(answer, RESPONSE);
    let (upstream, closed) = within(seen).await.expect("origin report");
    assert_eq!(
        upstream,
        "GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n"
    );
    assert!(!closed, "the request side is left open for the response");
}

#[tokio::test]
async fn bytes_after_the_body_are_not_forwarded() {
    let h = start(config());
    let (local, seen) = collecting_origin().await;
    h.route_name("example.test", 80, local);
    h.proxy.set_policy(vec![target("example.test", 80)]);
    let mut client = h.client().await;
    let head = "POST http://example.test/up HTTP/1.1\r\nContent-Length: 4\r\n\r\nbo";
    within(client.write_all(head.as_bytes()))
        .await
        .expect("send head");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let rest = b"dyGET http://example.test/x HTTP/1.1\r\n\r\n";
    within(client.write_all(rest)).await.expect("send body");
    assert_eq!(read_all(&mut client).await, RESPONSE);
    let (upstream, _) = within(seen).await.expect("origin report");
    assert_eq!(
        upstream,
        "POST /up HTTP/1.1\r\nHost: example.test\r\nContent-Length: 4\r\n\
         Connection: close\r\n\r\nbody"
    );
    assert_eq!(h.proxy.stats().bytes_up, 4, "the body only");
}

#[tokio::test]
async fn chunked_and_ambiguous_bodies_are_refused() {
    let h = start(config());
    h.proxy.set_policy(vec![target("example.test", 80)]);
    let requests = [
        "POST http://example.test/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST http://example.test/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
    ];
    for request in requests {
        let answer = send(&h, request).await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{request:?}: {answer}");
    }
    assert!(h.resolver.asked().is_empty());
    assert!(h.connector.dialed().is_empty());
    let stats = h.proxy.stats();
    assert_eq!((stats.bad_requests, stats.denials), (2, 0));
}
