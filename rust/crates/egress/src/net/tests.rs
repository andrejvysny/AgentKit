use std::net::IpAddr;

use super::{IpNet, blocks};

fn ip(text: &str) -> IpAddr {
    text.parse().expect("address")
}

fn net(text: &str, prefix_len: u8) -> IpNet {
    IpNet::new(ip(text), prefix_len).expect("valid block")
}

#[test]
fn prefixes_longer_than_the_address_are_refused() {
    assert!(IpNet::new(ip("1.2.3.4"), 33).is_none());
    assert!(IpNet::new(ip("::1"), 129).is_none());
    assert!(IpNet::new(ip("1.2.3.4"), 32).is_some());
    assert!(IpNet::new(ip("::1"), 128).is_some());
}

#[test]
fn host_bits_are_cleared() {
    assert_eq!(net("10.1.2.3", 8), net("10.0.0.0", 8));
    assert_eq!(net("10.1.2.3", 8).addr(), ip("10.0.0.0"));
    assert_eq!(net("2001:db8::77", 32).to_string(), "2001:db8::/32");
    assert_eq!(net("1.2.3.4", 0).addr(), ip("0.0.0.0"));
    assert_eq!(IpNet::host(ip("1.2.3.4")), net("1.2.3.4", 32));
    assert_eq!(IpNet::host(ip("::1")).prefix_len(), 128);
}

#[test]
fn contains_matches_the_prefix_bits_only() {
    let v4 = net("203.0.112.0", 23);
    assert!(v4.contains(ip("203.0.112.1")));
    assert!(v4.contains(ip("203.0.113.255")));
    assert!(!v4.contains(ip("203.0.114.0")));
    assert!(!v4.contains(ip("::ffff:203.0.112.1")), "families never mix");
    let one = net("198.51.100.7", 32);
    assert!(one.contains(ip("198.51.100.7")));
    assert!(!one.contains(ip("198.51.100.8")));
    assert!(net("0.0.0.0", 0).contains(ip("255.255.255.255")));
    let v6 = net("2001:db8:aa::", 48);
    assert!(v6.contains(ip("2001:db8:aa:ffff::1")));
    assert!(!v6.contains(ip("2001:db8:ab::1")));
    assert!(!v6.contains(ip("203.0.112.1")));
    assert!(net("::", 0).contains(ip("ffff::1")));
}

#[test]
fn an_ipv4_block_catches_the_ipv6_spellings_of_its_addresses() {
    let nets = [net("198.51.100.7", 32)];
    for spelling in [
        "198.51.100.7",
        "::ffff:198.51.100.7",
        "::198.51.100.7",
        "::ffff:0:198.51.100.7",
        "64:ff9b::198.51.100.7",
        "2002:c633:6407::1",
    ] {
        assert!(blocks(&nets, ip(spelling)), "{spelling}");
    }
    for other in [
        "198.51.100.8",
        "64:ff9b::198.51.100.8",
        "2001:db8::c633:6407",
    ] {
        assert!(!blocks(&nets, ip(other)), "{other}");
    }
    assert!(!blocks(&[], ip("198.51.100.7")));
}
