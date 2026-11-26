//! Bounds (NET-8): concurrent tunnels, lifetime, bytes, and the request
//! head's size and time.

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use agentkit_egress::{DenialReason, EgressConfig};
use support::{
    client_hello, closes_within, config, echo_upstream, greet, read_all, round_trip, sink_upstream,
    start, target, within,
};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn one_tunnel_over_the_limit_is_refused_and_recorded() {
    let h = start(EgressConfig {
        max_tunnels: 1,
        ..config()
    });
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut first, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    greet(&mut first, Some("example.test")).await;

    let (_second, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 503 "), "{head}");
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!(denial.reason, DenialReason::Limit);
    assert_eq!((denial.host.as_str(), denial.port), ("example.test", 443));
    assert_eq!(round_trip(&mut first, b"still").await, b"still");

    drop(first);
    let started = Instant::now();
    loop {
        let (mut third, head) = h.connect("example.test:443").await;
        if head.starts_with("HTTP/1.1 200 ") {
            greet(&mut third, Some("example.test")).await;
            assert_eq!(round_trip(&mut third, b"3").await, b"3");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "slot never freed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_tunnel_ends_at_its_lifetime() {
    let h = start(EgressConfig {
        tunnel_lifetime: Duration::from_millis(200),
        ..config()
    });
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut client, _) = h.connect("example.test:443").await;
    greet(&mut client, Some("example.test")).await;
    assert_eq!(round_trip(&mut client, b"a").await, b"a");
    assert!(closes_within(&mut client, Duration::from_secs(2)).await);
    assert_eq!(h.proxy.stats().closed_lifetime, 1);
}

#[tokio::test]
async fn a_tunnel_never_carries_more_than_its_byte_bound() {
    let h = start(EgressConfig {
        max_bytes_per_tunnel: 10,
        ..config()
    });
    let (sink, received) = sink_upstream().await;
    h.route_name("example.test", 443, sink);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut client, _) = h.connect("example.test:443").await;
    let hello = client_hello(Some("example.test"));
    assert!(hello.len() > 10, "the hello alone is over the bound");
    let _ = within(client.write_all(&hello)).await;
    assert!(closes_within(&mut client, Duration::from_secs(2)).await);
    let deadline = Instant::now() + Duration::from_secs(2);
    while received.load(Ordering::SeqCst) < 10 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(received.load(Ordering::SeqCst), 10);
    let stats = h.proxy.stats();
    assert_eq!((stats.closed_bytes, stats.bytes_up), (1, 10));
}

#[tokio::test]
async fn an_oversized_head_is_refused() {
    let h = start(EgressConfig {
        head_max_bytes: 64,
        ..config()
    });
    let mut client = h.client().await;
    let long = format!("CONNECT {}.test:443 HTTP/1.1\r\n\r\n", "a".repeat(60));
    let _ = within(client.write_all(long.as_bytes())).await;
    let answer = read_all(&mut client).await;
    assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
    assert_eq!(h.proxy.stats().bad_requests, 1);
}

#[tokio::test]
async fn a_slow_head_is_closed() {
    let h = start(EgressConfig {
        head_timeout: Duration::from_millis(200),
        ..config()
    });
    let mut client = h.client().await;
    within(client.write_all(b"CONNECT example.te"))
        .await
        .expect("send");
    let started = Instant::now();
    let answer = read_all(&mut client).await;
    assert!(answer.starts_with("HTTP/1.1 408 "), "{answer}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn connections_beyond_max_pending_wait_for_a_head_slot() {
    let h = start(EgressConfig {
        max_pending: 1,
        head_timeout: Duration::from_millis(800),
        ..config()
    });
    let mut idle = h.client().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    let (_client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "served before the idle head timed out"
    );
    let answer = read_all(&mut idle).await;
    assert!(answer.starts_with("HTTP/1.1 408 "), "{answer}");
}
