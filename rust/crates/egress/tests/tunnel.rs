//! CONNECT tunnels: default deny, grants, live policy changes (NET-1,
//! NET-2, NET-4, NET-5, NET-6).

mod support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use agentkit_egress::DenialReason;
use support::{
    NOW_MS, PUBLIC, client_hello, closes_within, config, echo_upstream, greet, ip, read_all,
    round_trip, start, target, within,
};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn nothing_leaves_without_a_grant_and_the_denial_names_the_operation() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_current_operation(Some("op-1".into()));
    let (mut client, head) = h.connect("Example.Test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(head.contains("Content-Length: "), "{head}");
    read_all(&mut client).await;
    assert!(
        h.resolver.asked().is_empty(),
        "an ungranted name is not resolved"
    );
    assert!(h.connector.dialed().is_empty());

    let batch = h.proxy.drain(0);
    assert_eq!(batch.cursor, 1);
    let denial = &batch.denials[0];
    assert_eq!((denial.host.as_str(), denial.port), ("example.test", 443));
    assert_eq!(denial.reason, DenialReason::NotGranted);
    assert_eq!(denial.operation_id.as_deref(), Some("op-1"));
    assert_eq!((denial.seq, denial.at_ms), (1, NOW_MS));
}

#[tokio::test]
async fn a_granted_target_tunnels_bytes_both_ways() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut client, head) = h.connect("example.test:443").await;
    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");
    greet(&mut client, Some("example.test")).await;
    assert_eq!(round_trip(&mut client, b"hello").await, b"hello");
    assert_eq!(
        round_trip(&mut client, &[7u8; 50_000]).await,
        vec![7u8; 50_000]
    );
    assert_eq!(h.resolver.asked(), ["example.test"]);
    let public: SocketAddr = format!("{PUBLIC}:443").parse().expect("addr");
    assert_eq!(
        h.connector.dialed(),
        [public],
        "resolved address, granted port"
    );

    let stats = h.proxy.stats();
    assert_eq!((stats.tunnels_established, stats.open_tunnels), (1, 1));
    let carried = 50_005 + client_hello(Some("example.test")).len() as u64;
    assert_eq!((stats.bytes_up, stats.bytes_down), (carried, carried));
    assert_eq!(h.proxy.drain(0).cursor, 0, "no denial");
}

#[tokio::test]
async fn the_port_is_part_of_the_grant() {
    let h = start(config());
    h.route_name("example.test", 8443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (_client, head) = h.connect("example.test:8443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    h.proxy.set_policy(vec![target("example.test", 8443)]);
    let (mut client, head) = h.connect("example.test:8443").await;
    assert!(
        head.starts_with("HTTP/1.1 200 "),
        "any port can be granted: {head}"
    );
    greet(&mut client, Some("example.test")).await;
    assert_eq!(round_trip(&mut client, b"x").await, b"x");
}

#[tokio::test]
async fn bytes_sent_with_the_head_reach_upstream() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let mut client = h.client().await;
    let hello = client_hello(Some("example.test"));
    let mut request = b"CONNECT example.test:443 HTTP/1.1\r\n\r\n".to_vec();
    request.extend_from_slice(&hello);
    request.extend_from_slice(b"early");
    within(client.write_all(&request)).await.expect("send");
    let head = support::read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let mut back = vec![0u8; hello.len() + 5];
    within(tokio::io::AsyncReadExt::read_exact(&mut client, &mut back))
        .await
        .expect("early bytes echoed");
    assert_eq!(back, [hello.as_slice(), b"early"].concat());
}

#[tokio::test]
async fn a_new_grant_applies_without_a_rebind() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    let (_client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    greet(&mut client, Some("example.test")).await;
    assert_eq!(round_trip(&mut client, b"ok").await, b"ok");
}

#[tokio::test]
async fn a_revoke_closes_open_tunnels_to_that_target_only() {
    let h = start(config());
    let echo = echo_upstream().await;
    h.route_name("example.test", 443, echo);
    h.route_name("other.test", 443, echo);
    let (a, b) = (target("example.test", 443), target("other.test", 443));
    h.proxy.set_policy(vec![a.clone(), b.clone()]);
    let (mut to_a, _) = h.connect("example.test:443").await;
    let (mut to_b, _) = h.connect("other.test:443").await;
    greet(&mut to_a, Some("example.test")).await;
    greet(&mut to_b, Some("other.test")).await;
    assert_eq!(round_trip(&mut to_a, b"1").await, b"1");

    h.proxy.set_policy(vec![b]);
    let revoked_at = Instant::now();
    assert!(
        closes_within(&mut to_a, Duration::from_secs(1)).await,
        "revoked tunnel still open"
    );
    assert!(revoked_at.elapsed() < Duration::from_secs(1));
    assert_eq!(
        round_trip(&mut to_b, b"2").await,
        b"2",
        "a still-granted tunnel stays"
    );
    assert_eq!(h.proxy.stats().closed_revoked, 1);

    let (_client, head) = h.connect("example.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
}

#[tokio::test]
async fn an_ip_literal_is_matched_as_text_and_never_resolved() {
    let h = start(config());
    let echo = echo_upstream().await;
    h.route_name("example.test", 443, echo);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (_client, head) = h.connect(&format!("{PUBLIC}:443")).await;
    assert!(
        head.starts_with("HTTP/1.1 403 "),
        "a name grant covers no literal: {head}"
    );

    h.proxy.set_policy(vec![target(PUBLIC, 443)]);
    let (mut client, head) = h.connect(&format!("{PUBLIC}:443")).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    greet(&mut client, None).await;
    assert_eq!(round_trip(&mut client, b"lit").await, b"lit");
    assert!(h.resolver.asked().is_empty(), "literals are not resolved");
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!(
        (denial.host.as_str(), denial.reason),
        (PUBLIC, DenialReason::NotGranted)
    );
}

#[tokio::test]
async fn resolve_and_connect_failures_answer_502_without_a_denial() {
    let h = start(config());
    h.proxy
        .set_policy(vec![target("nowhere.test", 443), target("down.test", 443)]);
    let (_client, head) = h.connect("nowhere.test:443").await;
    assert!(head.starts_with("HTTP/1.1 502 "), "{head}");
    h.resolver.set("down.test", vec![ip("93.184.216.35")]);
    let (_client, head) = h.connect("down.test:443").await;
    assert!(head.starts_with("HTTP/1.1 502 "), "{head}");
    assert_eq!(h.proxy.drain(0).cursor, 0);
    let stats = h.proxy.stats();
    assert_eq!((stats.resolve_failures, stats.connect_failures), (1, 1));
    assert_eq!(stats.open_tunnels, 0, "a failed attempt frees its slot");
}

#[tokio::test]
async fn a_revoke_while_connecting_refuses_before_any_byte_flows() {
    let h = std::sync::Arc::new(start(config()));
    let (sink, received) = support::sink_upstream().await;
    h.route_name("example.test", 443, sink);
    h.connector.slow_down(Duration::from_millis(300));
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let pending = tokio::spawn({
        let h = h.clone();
        async move {
            let mut client = h.client().await;
            let request = b"CONNECT example.test:443 HTTP/1.1\r\n\r\nearly";
            within(client.write_all(request)).await.expect("send");
            read_all(&mut client).await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.connector.dialed().len(), 1, "connect under way");
    h.proxy.set_policy(Vec::new());
    let answer = within(pending).await.expect("client task");
    assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
    let denial = &h.proxy.drain(0).denials[0];
    assert_eq!(denial.reason, DenialReason::NotGranted);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(h.proxy.stats().tunnels_established, 0);
}

#[tokio::test]
async fn a_resolver_that_never_answers_is_bounded() {
    let h = start(config());
    h.resolver.hang("slow.test");
    h.proxy.set_policy(vec![target("slow.test", 443)]);
    let started = Instant::now();
    let (_client, head) = h.connect("slow.test:443").await;
    assert!(head.starts_with("HTTP/1.1 502 "), "{head}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let stats = h.proxy.stats();
    assert_eq!((stats.resolve_failures, stats.open_tunnels), (1, 0));
}
