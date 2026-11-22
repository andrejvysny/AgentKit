//! `SafeHttpClient` against local stubs.

mod fetch_support;

use std::sync::Arc;
use std::time::Duration;

use agentkit_toolpacks_web::fetch::client::is_downgrade;
use agentkit_toolpacks_web::fetch::{FetchError, SystemResolver};
use fetch_support::{FakeResolver, Resp, client, local_config, serve};
use tokio_util::sync::CancellationToken;
use url::Url;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn fetcher(stub: &fetch_support::Stub) -> agentkit_toolpacks_web::fetch::SafeHttpClient {
    client(local_config(stub), FakeResolver::new(&[]))
}

#[tokio::test]
async fn fetches_html_and_sends_expected_headers() {
    let stub = serve(|path| match path {
        "/echo" => Resp::new(Some("text/plain"), "x"),
        _ => Resp::html("<p>hi</p>"),
    });
    let got = fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(got.content_type, "text/html");
    assert_eq!(got.charset.as_deref(), Some("utf-8"));
    assert_eq!(got.bytes, b"<p>hi</p>");
    assert!(!got.truncated_body);
    assert_eq!(got.final_url.path(), "/");
}

#[tokio::test]
async fn request_headers_identity_accept_and_ua() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx");
        String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
    });
    let stub = fetch_support::Stub { port };
    fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await
        .unwrap();
    let raw = seen.join().unwrap();
    assert!(raw.contains("accept-encoding: identity"), "{raw}");
    assert!(raw.contains("user-agent: agentkit-webfetch/0.1"), "{raw}");
    assert!(
        raw.contains("accept: text/html,application/xhtml+xml"),
        "{raw}"
    );
    assert!(
        !raw.contains("cookie") && !raw.contains("authorization"),
        "{raw}"
    );
}

#[tokio::test]
async fn follows_three_redirects() {
    let stub = serve(|path| match path {
        "/a" => Resp::redirect("/b"),
        "/b" => Resp::redirect("/c"),
        "/c" => Resp::redirect("/d?x=1"),
        _ => Resp::html("<p>done</p>"),
    });
    let got = fetcher(&stub)
        .get(url(&stub.url("/a")), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(got.final_url.path(), "/d");
    assert_eq!(got.final_url.query(), Some("x=1"));
}

#[tokio::test]
async fn six_redirects_is_too_many() {
    let stub = serve(|path| {
        let n: u32 = path.trim_start_matches("/r").parse().unwrap_or(0);
        Resp::redirect(&format!("/r{}", n + 1))
    });
    let res = fetcher(&stub)
        .get(url(&stub.url("/r0")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::TooManyRedirects)), "{res:?}");
}

#[tokio::test]
async fn five_redirects_is_the_limit() {
    let stub = serve(|path| {
        let n: u32 = path.trim_start_matches("/r").parse().unwrap_or(0);
        if n < 5 {
            Resp::redirect(&format!("/r{}", n + 1))
        } else {
            Resp::html("ok")
        }
    });
    let res = fetcher(&stub)
        .get(url(&stub.url("/r0")), &CancellationToken::new())
        .await;
    assert!(res.is_ok(), "{res:?}");
}

#[tokio::test]
async fn redirect_to_private_hostname_is_blocked() {
    let resolver = FakeResolver::new(&[("evil.test", &["10.0.0.5"])]);
    let stub = serve(|path| match path {
        "/go" => Resp::redirect(&format!(
            "http://evil.test:{}/x",
            PORT.get().copied().unwrap_or(0)
        )),
        _ => Resp::html("<p>secret</p>"),
    });
    PORT.set(stub.port).unwrap();
    let res = client(local_config(&stub), resolver)
        .get(url(&stub.url("/go")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
}

static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

#[tokio::test]
async fn redirect_to_private_ip_literal_is_blocked() {
    let stub = serve(|_| Resp::redirect("http://169.254.169.254/latest/meta-data"));
    let res = fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
}

#[tokio::test]
async fn redirect_to_userinfo_or_bad_port_is_refused() {
    let stub = serve(|path| match path {
        "/u" => Resp::redirect("http://user:pw@example.com/"),
        _ => Resp::redirect("http://example.com:8080/"),
    });
    let c = fetcher(&stub);
    for p in ["/u", "/p"] {
        let res = c.get(url(&stub.url(p)), &CancellationToken::new()).await;
        assert!(
            matches!(res, Err(FetchError::InvalidUrl(_))),
            "{p}: {res:?}"
        );
    }
}

#[test]
fn downgrade_detection() {
    let https = url("https://a.example/");
    let http = url("http://a.example/");
    assert!(is_downgrade(&https, &http));
    assert!(!is_downgrade(&http, &https));
    assert!(!is_downgrade(&https, &https));
    assert!(!is_downgrade(&http, &http));
}

#[tokio::test]
async fn unsupported_content_type() {
    let stub = serve(|_| Resp::new(Some("application/pdf"), "%PDF-1.7"));
    let res = fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await;
    assert!(
        matches!(&res, Err(FetchError::UnsupportedType(m)) if m == "application/pdf"),
        "{res:?}"
    );
}

#[tokio::test]
async fn missing_content_type_is_sniffed() {
    let stub = serve(|path| match path {
        "/h" => Resp::new(None, "  <!DOCTYPE HTML><html><body>x</body></html>"),
        _ => Resp::new(None, "just words"),
    });
    let c = fetcher(&stub);
    let html = c
        .get(url(&stub.url("/h")), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(html.content_type, "text/html");
    let text = c
        .get(url(&stub.url("/t")), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text.content_type, "text/plain");
}

#[tokio::test]
async fn non_success_status_is_http_error() {
    let stub = serve(|_| Resp::html("nope").status("404 Not Found"));
    let res = fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::Http(404))), "{res:?}");
}

#[tokio::test]
async fn body_over_cap_is_truncated_not_failed() {
    let stub = serve(|_| Resp::new(Some("text/plain"), vec![b'a'; 10_000]));
    let mut cfg = local_config(&stub);
    cfg.max_body_bytes = 4096;
    let got = client(cfg, FakeResolver::new(&[]))
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await
        .unwrap();
    assert!(got.truncated_body);
    assert_eq!(got.bytes.len(), 4096);
}

#[tokio::test]
async fn body_at_cap_is_not_truncated() {
    let stub = serve(|_| Resp::new(Some("text/plain"), vec![b'a'; 4096]));
    let mut cfg = local_config(&stub);
    cfg.max_body_bytes = 4096;
    let got = client(cfg, FakeResolver::new(&[]))
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await
        .unwrap();
    assert!(!got.truncated_body);
}

#[tokio::test]
async fn cancellation_aborts_the_request() {
    let stub = serve(|_| Resp::html("slow").delay(Duration::from_secs(3)));
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        child.cancel();
    });
    let started = std::time::Instant::now();
    let res = fetcher(&stub).get(url(&stub.url("/")), &token).await;
    assert!(matches!(res, Err(FetchError::Cancelled)), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn slow_server_times_out() {
    let stub = serve(|_| Resp::html("slow").delay(Duration::from_millis(1500)));
    let mut cfg = local_config(&stub);
    cfg.timeout = Duration::from_millis(300);
    let res = client(cfg, FakeResolver::new(&[]))
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::Timeout)), "{res:?}");
}

#[tokio::test]
async fn system_resolver_blocks_localhost_by_name() {
    let stub = serve(|_| Resp::html("x"));
    let mut cfg = local_config(&stub);
    cfg.extra_allowed_ips.clear();
    let res = client(cfg, Arc::new(SystemResolver))
        .get(
            url(&format!("http://localhost:{}/", stub.port)),
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(res, Err(FetchError::Blocked(_))), "{res:?}");
}

#[tokio::test]
async fn total_timeout_bounds_a_redirect_chain() {
    let stub = serve(|path| match path {
        "/a" => Resp::redirect("/b").delay(Duration::from_millis(300)),
        "/b" => Resp::redirect("/c").delay(Duration::from_millis(300)),
        _ => Resp::html("x").delay(Duration::from_millis(300)),
    });
    let mut cfg = local_config(&stub);
    cfg.total_timeout = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let res = client(cfg, FakeResolver::new(&[]))
        .get(url(&stub.url("/a")), &CancellationToken::new())
        .await;
    assert!(matches!(res, Err(FetchError::Timeout)), "{res:?}");
    assert!(started.elapsed() < Duration::from_millis(900));
}

#[tokio::test]
async fn echoed_content_type_is_sanitized() {
    let long = format!("application/{}; charset=x", "a".repeat(200));
    let stub = serve(move |_| Resp::new(Some(&long), "x"));
    let res = fetcher(&stub)
        .get(url(&stub.url("/")), &CancellationToken::new())
        .await;
    let Err(FetchError::UnsupportedType(mime)) = res else {
        panic!("{res:?}");
    };
    assert_eq!(mime.len(), 64);
    assert!(mime.starts_with("application/aaa") && !mime.contains(';'));
}
