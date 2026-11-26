//! SSRF matrix: blocked ranges, URL forms and the guarded resolver.

mod fetch_support;

use std::net::IpAddr;

use agentkit_toolpacks_web::fetch::ssrf::check_url;
use agentkit_toolpacks_web::fetch::{FetchConfig, FetchError};
use fetch_support::{FakeResolver, Resp, client, local_config, serve};
use tokio_util::sync::CancellationToken;
use url::Url;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
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
