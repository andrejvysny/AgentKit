//! The TLS server name of CONNECT tunnels: a grant reaches the granted
//! host, not every site at its address.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentkit_egress::{DenialReason, EgressConfig};
use support::{
    Harness, PUBLIC, client_hello, closes_within, config, echo_upstream, greet, round_trip,
    sink_upstream, start, target, within,
};
use tokio::io::AsyncWriteExt;

const CURL_HELLO: &[u8] = include_bytes!("fixtures/curl-client-hello.bin");

/// A proxy granting `example.test:443`, routed to a byte-counting sink.
async fn granted(config: EgressConfig) -> (Harness, Arc<AtomicU64>) {
    let h = start(config);
    let (sink, received) = sink_upstream().await;
    h.route_name("example.test", 443, sink);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    h.proxy.set_current_operation(Some("op-7".into()));
    (h, received)
}

/// CONNECT, then `bytes`; true when the proxy closed the tunnel.
async fn refused_after(h: &Harness, bytes: &[u8]) -> bool {
    let (mut client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    within(client.write_all(bytes)).await.expect("send");
    closes_within(&mut client, Duration::from_secs(2)).await
}

/// The one denial recorded, which must be an `sni_mismatch` for the
/// CONNECT target; and nothing reached the upstream.
async fn assert_sni_denial(h: &Harness, received: &AtomicU64) {
    let batch = h.proxy.drain(0);
    assert_eq!(batch.denials.len(), 1, "{batch:?}");
    let denial = &batch.denials[0];
    assert_eq!(denial.reason, DenialReason::SniMismatch);
    assert_eq!(denial.reason.as_str(), "sni_mismatch");
    assert_eq!((denial.host.as_str(), denial.port), ("example.test", 443));
    assert_eq!(denial.operation_id.as_deref(), Some("op-7"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(received.load(Ordering::SeqCst), 0, "nothing went upstream");
    let stats = h.proxy.stats();
    assert_eq!((stats.tunnels_established, stats.bytes_up), (0, 0));
    assert_eq!(stats.open_tunnels, 0, "the slot is freed");
}

#[tokio::test]
async fn a_hello_naming_the_granted_host_is_forwarded() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    for name in ["example.test", "Example.TEST", "example.test."] {
        let (mut client, _) = h.connect("example.test:443").await;
        greet(&mut client, Some(name)).await;
        assert_eq!(round_trip(&mut client, b"data").await, b"data");
    }
    assert_eq!(h.proxy.stats().tunnels_established, 3);
    assert_eq!(h.proxy.drain(0).cursor, 0, "no denial");
}

#[tokio::test]
async fn a_real_client_hello_is_admitted() {
    let h = start(config());
    h.route_name("pypi.org", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("pypi.org", 443)]);
    let (mut client, _) = h.connect("pypi.org:443").await;
    assert_eq!(round_trip(&mut client, CURL_HELLO).await, CURL_HELLO);
}

#[tokio::test]
async fn another_name_on_the_same_address_is_refused() {
    let (h, received) = granted(config()).await;
    let hello = client_hello(Some("other.test"));
    assert!(refused_after(&h, &hello).await, "tunnel left open");
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn a_hello_without_a_name_is_refused_for_a_named_target() {
    let (h, received) = granted(config()).await;
    assert!(refused_after(&h, &client_hello(None)).await);
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn an_ip_literal_target_takes_no_name_or_its_own_only() {
    let h = start(config());
    let echo = echo_upstream().await;
    h.route_name("example.test", 443, echo);
    h.proxy.set_policy(vec![target(PUBLIC, 443)]);
    let literal = format!("{PUBLIC}:443");
    for name in [None, Some(PUBLIC)] {
        let (mut client, _) = h.connect(&literal).await;
        greet(&mut client, name).await;
    }
    let (mut client, head) = h.connect(&literal).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    within(client.write_all(&client_hello(Some("example.test"))))
        .await
        .expect("send");
    assert!(closes_within(&mut client, Duration::from_secs(2)).await);
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!(
        (denial.host.as_str(), denial.reason),
        (PUBLIC, DenialReason::SniMismatch)
    );
}

#[tokio::test]
async fn bytes_that_are_not_tls_are_refused() {
    let (h, received) = granted(config()).await;
    assert!(refused_after(&h, b"SSH-2.0-OpenSSH_9.6\r\n").await);
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn a_malformed_hello_is_refused() {
    let (h, received) = granted(config()).await;
    // The handshake claims one byte less than the record holds.
    let mut hello = client_hello(Some("example.test"));
    hello[8] -= 1;
    assert!(refused_after(&h, &hello).await);
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn an_oversized_record_is_refused_without_waiting_for_it() {
    let (h, received) = granted(EgressConfig {
        head_timeout: Duration::from_secs(5),
        ..config()
    })
    .await;
    let started = Instant::now();
    assert!(refused_after(&h, &[22, 3, 1, 0x40, 0x01]).await);
    assert!(started.elapsed() < Duration::from_secs(2), "not waited");
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn a_hello_split_across_writes_is_reassembled() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let hello = client_hello(Some("example.test"));
    let mut client = h.client().await;
    let mut head = b"CONNECT example.test:443 HTTP/1.1\r\n\r\n".to_vec();
    head.extend_from_slice(&hello[..2]);
    within(client.write_all(&head)).await.expect("send head");
    assert!(
        support::read_head(&mut client)
            .await
            .starts_with("HTTP/1.1 200 ")
    );
    for piece in [&hello[2..3], &hello[3..5], &hello[5..20], &hello[20..]] {
        tokio::time::sleep(Duration::from_millis(20)).await;
        within(client.write_all(piece)).await.expect("send piece");
    }
    let mut back = vec![0u8; hello.len()];
    within(tokio::io::AsyncReadExt::read_exact(&mut client, &mut back))
        .await
        .expect("echo");
    assert_eq!(back, hello);
    assert_eq!(h.proxy.drain(0).cursor, 0);
}

#[tokio::test]
async fn no_hello_within_the_head_timeout_is_refused() {
    let (h, received) = granted(EgressConfig {
        head_timeout: Duration::from_millis(200),
        ..config()
    })
    .await;
    let started = Instant::now();
    assert!(refused_after(&h, &[22, 3]).await);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_sni_denial(&h, &received).await;
}

#[tokio::test]
async fn a_client_that_leaves_before_its_hello_is_not_a_denial() {
    let (h, received) = granted(config()).await;
    let (mut client, _) = h.connect("example.test:443").await;
    within(client.write_all(&[22, 3, 1])).await.expect("send");
    client.shutdown().await.expect("half-close");
    assert!(closes_within(&mut client, Duration::from_secs(2)).await);
    let stats = h.proxy.stats();
    assert_eq!((stats.denials, stats.bad_requests), (0, 1));
    assert_eq!(received.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_revoke_while_waiting_for_the_hello_closes_at_once() {
    let (h, received) = granted(EgressConfig {
        head_timeout: Duration::from_secs(5),
        ..config()
    })
    .await;
    let (mut client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    h.proxy.set_policy(Vec::new());
    let revoked_at = Instant::now();
    assert!(closes_within(&mut client, Duration::from_secs(1)).await);
    assert!(revoked_at.elapsed() < Duration::from_secs(1));
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!(denial.reason, DenialReason::NotGranted);
    assert_eq!(received.load(Ordering::SeqCst), 0);
}
