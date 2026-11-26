//! The address rule (NET-3, NET-7): a granted name or literal is refused
//! when every address it resolves to is local, private or reserved, and
//! the connector is never asked for such an address.

mod support;

use std::net::SocketAddr;

use agentkit_egress::{DenialReason, EgressConfig, IpNet};
use support::{PUBLIC, config, echo_upstream, greet, ip, read_all, round_trip, start, target};

const REFUSED: [&str; 12] = [
    "10.0.0.1",
    "127.0.0.1",
    "169.254.169.254",
    "100.64.0.1",
    "192.168.1.1",
    "224.0.0.1",
    "0.0.0.0",
    "::1",
    "fc00::1",
    "fe80::1",
    "::ffff:127.0.0.1",
    "64:ff9b::a00:1",
];

#[tokio::test]
async fn a_granted_name_resolving_only_to_refused_addresses_is_denied() {
    let h = start(config());
    h.proxy.set_current_operation(Some("op-9".into()));
    h.proxy.set_policy(vec![target("inside.test", 443)]);
    for addr in REFUSED {
        h.resolver.set("inside.test", vec![ip(addr)]);
        let (mut client, head) = h.connect("inside.test:443").await;
        assert!(head.starts_with("HTTP/1.1 502 "), "{addr}: {head}");
        read_all(&mut client).await;
    }
    assert!(
        h.connector.dialed().is_empty(),
        "the check runs before any connect"
    );
    let batch = h.proxy.drain(0);
    assert_eq!(batch.denials.len(), REFUSED.len());
    for denial in &batch.denials {
        assert_eq!(denial.reason, DenialReason::BlockedAddress);
        assert_eq!((denial.host.as_str(), denial.port), ("inside.test", 443));
        assert_eq!(denial.operation_id.as_deref(), Some("op-9"));
    }
    assert_eq!(h.proxy.stats().open_tunnels, 0);
}

#[tokio::test]
async fn every_address_of_the_answer_is_refused_together() {
    let h = start(config());
    h.proxy.set_policy(vec![target("inside.test", 443)]);
    h.resolver
        .set("inside.test", REFUSED.iter().map(|a| ip(a)).collect());
    let (_client, head) = h.connect("inside.test:443").await;
    assert!(head.starts_with("HTTP/1.1 502 "), "{head}");
    assert!(h.connector.dialed().is_empty());
}

#[tokio::test]
async fn a_mixed_answer_connects_to_the_first_public_address() {
    let h = start(config());
    let echo = echo_upstream().await;
    let public: SocketAddr = format!("{PUBLIC}:443").parse().expect("addr");
    h.connector.route(public, echo);
    h.proxy.set_policy(vec![target("mixed.test", 443)]);
    let answer = vec![ip("10.0.0.1"), ip("::1"), ip(PUBLIC), ip("1.1.1.1")];
    h.resolver.set("mixed.test", answer);
    let (mut client, head) = h.connect("mixed.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    greet(&mut client, Some("mixed.test")).await;
    assert_eq!(round_trip(&mut client, b"m").await, b"m");
    assert_eq!(h.connector.dialed(), [public]);
}

#[tokio::test]
async fn a_mapped_public_address_is_connected_as_ipv4() {
    let h = start(config());
    let echo = echo_upstream().await;
    let public: SocketAddr = format!("{PUBLIC}:443").parse().expect("addr");
    h.connector.route(public, echo);
    h.proxy.set_policy(vec![target("mapped.test", 443)]);
    h.resolver
        .set("mapped.test", vec![ip(&format!("::ffff:{PUBLIC}"))]);
    let (_client, head) = h.connect("mapped.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_eq!(h.connector.dialed(), [public]);
}

#[tokio::test]
async fn granted_refused_literals_are_denied_too() {
    let h = start(config());
    let literals = [
        "127.0.0.1",
        "169.254.169.254",
        "10.0.0.1",
        "[::1]",
        "[fc00::1]",
    ];
    h.proxy
        .set_policy(literals.iter().map(|l| target(l, 443)).collect());
    for literal in literals {
        let (_client, head) = h.connect(&format!("{literal}:443")).await;
        assert!(head.starts_with("HTTP/1.1 502 "), "{literal}: {head}");
    }
    assert!(h.resolver.asked().is_empty());
    assert!(h.connector.dialed().is_empty());
    let batch = h.proxy.drain(0);
    let hosts: Vec<&str> = batch.denials.iter().map(|d| d.host.as_str()).collect();
    assert_eq!(hosts, literals);
    assert!(
        batch
            .denials
            .iter()
            .all(|d| d.reason == DenialReason::BlockedAddress)
    );
}

#[tokio::test]
async fn operator_blocks_refuse_addresses_the_kernel_rule_allows() {
    let h = start(EgressConfig {
        extra_blocked: vec![IpNet::host(ip(PUBLIC))],
        ..config()
    });
    let echo = echo_upstream().await;
    h.connector.route(SocketAddr::new(ip(PUBLIC), 443), echo);
    h.proxy.set_policy(vec![
        target("own.test", 443),
        target("nat64.test", 443),
        target(PUBLIC, 443),
    ]);
    h.resolver.set("own.test", vec![ip(PUBLIC)]);
    h.resolver
        .set("nat64.test", vec![ip(&format!("64:ff9b::{PUBLIC}"))]);
    for authority in ["own.test:443", "nat64.test:443", &format!("{PUBLIC}:443")] {
        let (_client, head) = h.connect(authority).await;
        assert!(head.starts_with("HTTP/1.1 502 "), "{authority}: {head}");
    }
    assert!(h.connector.dialed().is_empty());
    let batch = h.proxy.drain(0);
    assert_eq!(batch.denials.len(), 3);
    assert!(
        batch
            .denials
            .iter()
            .all(|d| d.reason == DenialReason::BlockedAddress)
    );
}
