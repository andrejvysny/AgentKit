use std::net::IpAddr;

use super::first_permitted;
use crate::net::IpNet;

fn ip(text: &str) -> IpAddr {
    text.parse().expect("address")
}

#[test]
fn refused_addresses_are_skipped_and_public_ones_kept_in_order() {
    let mixed = [ip("10.0.0.1"), ip("93.184.216.34"), ip("1.1.1.1")];
    assert_eq!(first_permitted(&mixed, &[]), Some(ip("93.184.216.34")));
    let refused = [
        ip("127.0.0.1"),
        ip("10.1.2.3"),
        ip("169.254.169.254"),
        ip("100.64.0.1"),
        ip("224.0.0.1"),
        ip("::1"),
        ip("fc00::1"),
        ip("fe80::1"),
        ip("::ffff:127.0.0.1"),
        ip("::ffff:10.0.0.1"),
        ip("0.0.0.0"),
        ip("::"),
    ];
    assert_eq!(first_permitted(&refused, &[]), None);
    assert_eq!(first_permitted(&[], &[]), None);
}

#[test]
fn mapped_public_addresses_are_connected_as_ipv4() {
    let mapped = [ip("::ffff:93.184.216.34")];
    assert_eq!(first_permitted(&mapped, &[]), Some(ip("93.184.216.34")));
    let v6 = [ip("2606:2800:220:1:248:1893:25c8:1946")];
    assert_eq!(first_permitted(&v6, &[]), Some(v6[0]));
}

#[test]
fn extra_blocks_refuse_addresses_the_kernel_rule_allows() {
    let own = [
        IpNet::host(ip("93.184.216.34")),
        IpNet::new(ip("2606:2800::"), 32).expect("block"),
    ];
    let answer = [
        ip("93.184.216.34"),
        ip("::ffff:93.184.216.34"),
        ip("64:ff9b::93.184.216.34"),
        ip("2606:2800:220:1:248:1893:25c8:1946"),
        ip("1.1.1.1"),
    ];
    assert_eq!(first_permitted(&answer, &own), Some(ip("1.1.1.1")));
    assert_eq!(first_permitted(&answer[..4], &own), None);
    assert_eq!(
        first_permitted(&answer, &[]),
        Some(ip("93.184.216.34")),
        "no extra blocks by default"
    );
}
