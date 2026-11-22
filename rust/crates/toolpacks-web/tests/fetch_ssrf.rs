//! SSRF matrix: blocked ranges, URL forms and the guarded resolver.

mod fetch_support;

use std::net::IpAddr;

use agentkit_toolpacks_web::fetch::ssrf::{check_url, is_blocked};
use agentkit_toolpacks_web::fetch::{FetchConfig, FetchError};
use fetch_support::{FakeResolver, Resp, client, local_config, serve};
use tokio_util::sync::CancellationToken;
use url::Url;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn blocked_v4_ranges() {
    for s in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "10.255.255.255",
        "100.64.0.1",
        "100.127.255.254",
        "127.0.0.1",
        "127.255.0.9",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.8",
        "192.0.2.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.7",
        "203.0.113.9",
        "224.0.0.1",
        "239.255.255.250",
        "240.0.0.1",
        "255.255.255.255",
    ] {
        assert!(is_blocked(ip(s)), "{s}");
    }
}

#[test]
fn allowed_v4_neighbours() {
    for s in [
        "8.8.8.8",
        "1.1.1.1",
        "100.63.255.255",
        "100.128.0.1",
        "172.15.255.255",
        "172.32.0.1",
        "192.0.1.1",
        "192.167.0.1",
        "192.169.0.1",
        "198.17.0.1",
        "198.20.0.1",
        "198.51.101.1",
        "203.0.112.1",
        "223.255.255.255",
        "9.255.255.255",
        "11.0.0.1",
    ] {
        assert!(!is_blocked(ip(s)), "{s}");
    }
}

#[test]
fn blocked_v6_ranges_and_embedded_v4() {
    for s in [
        "::",
        "::1",
        "fc00::1",
        "fdff::1",
        "fe80::1",
        "febf::1",
        "ff02::1",
        "2001:db8::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:192.168.0.1",
        "::10.0.0.1",
        "::127.0.0.1",
        "64:ff9b::10.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "64:ff9b::7f00:1",
        "2002:0a00:0001::1",
    ] {
        assert!(is_blocked(ip(s)), "{s}");
    }
}

#[test]
fn public_addresses_allowed() {
    for s in [
        "8.8.8.8",
        "2606:4700::1111",
        "2001:4860:4860::8888",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
        "2001:db9::1",
        "fbff::1",
    ] {
        assert!(!is_blocked(ip(s)), "{s}");
    }
}

fn check(url: &str) -> Result<(), FetchError> {
    check_url(&Url::parse(url).unwrap(), &FetchConfig::default())
}

#[test]
fn ip_literal_forms_are_blocked() {
    for u in [
        "http://127.0.0.1/",
        "http://2130706433/",
        "http://0x7f.0.0.1/",
        "http://0177.0.0.1/",
        "http://127.1/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://[::ffff:7f00:1]/",
        "https://169.254.169.254/latest/meta-data",
        "http://[fd00::1]/",
        "http://10.1.2.3/",
        "http://3232235777/",
    ] {
        assert!(matches!(check(u), Err(FetchError::Blocked(_))), "{u}");
    }
    assert!(check("http://8.8.8.8/").is_ok());
    assert!(check("https://[2606:4700::1111]/").is_ok());
    assert!(check("https://example.com/").is_ok());
}

#[test]
fn rejects_userinfo_scheme_port_and_length() {
    for u in [
        "http://user:pw@example.com/",
        "http://user@example.com/",
        "ftp://example.com/",
        "file:///etc/passwd",
        "http://example.com:8080/",
        "https://example.com:22/",
    ] {
        assert!(matches!(check(u), Err(FetchError::InvalidUrl(_))), "{u}");
    }
    let long = format!("https://example.com/{}", "a".repeat(2100));
    assert!(matches!(check(&long), Err(FetchError::InvalidUrl(_))));
    assert!(check("http://example.com:80/").is_ok());
    assert!(check("https://example.com:443/").is_ok());
}

#[test]
fn extra_allowed_ip_and_port_are_honoured() {
    let cfg = FetchConfig {
        allowed_ports: vec![8080],
        extra_allowed_ips: vec![ip("127.0.0.1")],
        ..FetchConfig::default()
    };
    assert!(check_url(&Url::parse("http://127.0.0.1:8080/").unwrap(), &cfg).is_ok());
    assert!(check_url(&Url::parse("http://127.0.0.2:8080/").unwrap(), &cfg).is_err());
}

async fn get(
    resolver: std::sync::Arc<FakeResolver>,
    url: &str,
    extra: bool,
) -> Result<(), FetchError> {
    let stub = serve(|_| Resp::html("<p>ok</p>"));
    let mut cfg = local_config(&stub);
    if !extra {
        cfg.extra_allowed_ips.clear();
    }
    // Rewrite the stub port into the URL under test.
    let url = Url::parse(&url.replace("PORT", &stub.port.to_string())).unwrap();
    client(cfg, resolver)
        .get(url, &CancellationToken::new())
        .await
        .map(|_| ())
}

#[tokio::test]
async fn resolver_blocks_mixed_answer() {
    let r = FakeResolver::new(&[("mixed.test", &["127.0.0.1", "10.0.0.1"])]);
    let res = get(r, "http://mixed.test:PORT/", true).await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
}

#[tokio::test]
async fn resolver_blocks_metadata_only_answer() {
    let r = FakeResolver::new(&[("meta.test", &["169.254.169.254"])]);
    let res = get(r, "http://meta.test:PORT/", true).await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
}

#[tokio::test]
async fn loopback_needs_the_opt_in() {
    let r = FakeResolver::new(&[("local.test", &["127.0.0.1"])]);
    let res = get(r.clone(), "http://local.test:PORT/", false).await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
    let res = get(r.clone(), "http://127.0.0.1:PORT/", false).await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
    assert!(get(r, "http://local.test:PORT/", true).await.is_ok());
}

#[tokio::test]
async fn unresolvable_host_is_a_network_error() {
    let r = FakeResolver::new(&[]);
    let res = get(r, "http://nowhere.test:PORT/", true).await;
    assert!(matches!(res, Err(FetchError::Network(_))), "{res:?}");
}

#[test]
fn transition_and_legacy_ranges_are_blocked() {
    for s in [
        "2001::1",
        "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        "2001:0:ffff::1",
        "64:ff9b:1::1",
        "64:ff9b:1::808:808",
        "64:ff9b:1:ffff::1",
        "fec0::1",
        "fedf::1",
        "feff:ffff::1",
        "::ffff:0:10.0.0.1",
        "::ffff:0:127.0.0.1",
        "192.88.99.0",
        "192.88.99.255",
    ] {
        assert!(is_blocked(ip(s)), "{s}");
    }
}

#[test]
fn neighbours_of_new_ranges_stay_allowed() {
    for s in [
        "2001:1::1",
        "2000::1",
        "64:ff9b:2::1",
        "::ffff:0:8.8.8.8",
        "192.88.98.1",
        "192.88.100.1",
        "fe00::1",
    ] {
        assert!(!is_blocked(ip(s)), "{s}");
    }
}
