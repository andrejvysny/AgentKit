use super::{BadRequest, HeadError, Request, parse, read_head};
use crate::target::Target;

fn target(host: &str, port: u16) -> Target {
    Target::new(host, port).expect("valid target")
}

fn forwarded(head: &str) -> (Target, String) {
    match parse(head.as_bytes()) {
        Ok(Request::Forward { target, head, .. }) => {
            (target, String::from_utf8(head).expect("ascii head"))
        }
        other => panic!("not forwarded: {other:?}"),
    }
}

#[tokio::test]
async fn head_reader_splits_early_bytes_and_bounds_size() {
    let mut input: &[u8] = b"CONNECT a.example:443 HTTP/1.1\r\n\r\nEARLY";
    let head = read_head(&mut input, 64).await.expect("head");
    assert_eq!(head.bytes, b"CONNECT a.example:443 HTTP/1.1\r\n\r\n");
    assert_eq!(head.early, b"EARLY");

    let mut long: &[u8] = &[b'a'; 100];
    assert_eq!(
        read_head(&mut long, 64).await.err(),
        Some(HeadError::TooLarge)
    );
    let mut cut: &[u8] = b"CONNECT a.example:443 HTTP/1.1\r\n";
    assert_eq!(read_head(&mut cut, 64).await.err(), Some(HeadError::Closed));
}

#[test]
fn connect_targets_are_normalised() {
    let cases = [
        (
            "CONNECT PyPI.org.:443 HTTP/1.1\r\nHost: x\r\n\r\n",
            target("pypi.org", 443),
        ),
        (
            "CONNECT 93.184.216.34:8443 HTTP/1.0\r\n\r\n",
            target("93.184.216.34", 8443),
        ),
        (
            "CONNECT [2001:DB8::1]:443 HTTP/1.1\r\n\r\n",
            target("[2001:db8::1]", 443),
        ),
    ];
    for (head, want) in cases {
        assert_eq!(
            parse(head.as_bytes()),
            Ok(Request::Connect(want)),
            "{head:?}"
        );
    }
}

#[test]
fn malformed_heads_are_refused() {
    let cases = [
        "CONNECT a.example HTTP/1.1\r\n\r\n",
        "CONNECT a.example:0 HTTP/1.1\r\n\r\n",
        "CONNECT a.example:70000 HTTP/1.1\r\n\r\n",
        "CONNECT a.example:+443 HTTP/1.1\r\n\r\n",
        "CONNECT a.example: HTTP/1.1\r\n\r\n",
        "CONNECT *.example:443 HTTP/1.1\r\n\r\n",
        "CONNECT ::1:443 HTTP/1.1\r\n\r\n",
        "CONNECT [::1%lo]:443 HTTP/1.1\r\n\r\n",
        "CONNECT [::1]x:443 HTTP/1.1\r\n\r\n",
        "CONNECT a.example:443 HTTP/2.0\r\n\r\n",
        "CONNECT  a.example:443 HTTP/1.1\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1 x\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1\r\nNo-Colon\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1\r\n folded: x\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1\r\nBad Name: x\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1\r\nX: a\rb\r\n\r\n",
        "CONNECT a.example:443 HTTP/1.1\nX: a\r\n\r\n",
        "GET http://user@a.example/ HTTP/1.1\r\n\r\n",
        "GET http:///path HTTP/1.1\r\n\r\n",
        "GET http://a.example:/ HTTP/1.1\r\n\r\n",
        "\u{0}\r\n\r\n",
    ];
    for head in cases {
        assert_eq!(
            parse(head.as_bytes()),
            Err(BadRequest::Malformed),
            "{head:?}"
        );
    }
}

#[test]
fn other_forms_are_unsupported() {
    let cases = [
        "GET / HTTP/1.1\r\nHost: a.example\r\n\r\n",
        "GET https://a.example/ HTTP/1.1\r\n\r\n",
        "GET ftp://a.example/ HTTP/1.1\r\n\r\n",
        "connect a.example:443 HTTP/1.1\r\n\r\n",
    ];
    for head in cases {
        assert_eq!(
            parse(head.as_bytes()),
            Err(BadRequest::Unsupported),
            "{head:?}"
        );
    }
}

#[test]
fn plain_http_only_on_port_80() {
    let head = "GET http://a.example:8080/ HTTP/1.1\r\n\r\n";
    assert_eq!(parse(head.as_bytes()), Err(BadRequest::PlainHttpPort));
    let (t, _) = forwarded("GET http://a.example:80/ HTTP/1.1\r\n\r\n");
    assert_eq!(t, target("a.example", 80));
}

#[test]
fn plain_http_is_rewritten_to_origin_form() {
    let head = "GET HTTP://A.Example/p/q?x=1#frag HTTP/1.1\r\n\
                Host: evil.example\r\n\
                Proxy-Connection: keep-alive\r\n\
                Proxy-Authorization: Basic Zm9v\r\n\
                Connection: keep-alive, Upgrade\r\n\
                Upgrade: websocket\r\n\
                Keep-Alive: 5\r\n\
                Accept: */*\r\n\
                Content-Length: 3\r\n\r\n";
    let (t, out) = forwarded(head);
    assert_eq!(t, target("a.example", 80));
    assert_eq!(
        out,
        "GET /p/q?x=1 HTTP/1.1\r\nHost: a.example\r\nAccept: */*\r\n\
         Content-Length: 3\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn origin_form_paths() {
    let cases = [
        ("http://a.example", "/"),
        ("http://a.example?x", "/?x"),
        ("http://a.example#f", "/"),
        ("http://[2001:db8::1]/v6", "/v6"),
    ];
    for (uri, want) in cases {
        let (_, out) = forwarded(&format!("HEAD {uri} HTTP/1.0\r\n\r\n"));
        let first = out.lines().next().expect("request line");
        assert_eq!(first, format!("HEAD {want} HTTP/1.0"), "{uri}");
    }
    let (_, out) = forwarded("GET http://[2001:db8::1]/ HTTP/1.1\r\n\r\n");
    assert!(out.contains("\r\nHost: [2001:db8::1]\r\n"), "{out}");
}

fn body_len(head: &str) -> Result<u64, BadRequest> {
    match parse(head.as_bytes())? {
        Request::Forward { body_len, .. } => Ok(body_len),
        Request::Connect(_) => panic!("not a plain request"),
    }
}

#[test]
fn plain_bodies_are_delimited_by_content_length_only() {
    let get = "GET http://a.example/ HTTP/1.1\r\n\r\n";
    assert_eq!(body_len(get), Ok(0), "no Content-Length: no body");
    let cases = [
        ("Content-Length: 4", 4),
        ("content-length:0", 0),
        ("CONTENT-LENGTH: \t 1234 \t", 1234),
        (
            "Content-Length: 9999999999999999999",
            9_999_999_999_999_999_999,
        ),
    ];
    for (header, want) in cases {
        let head = format!("POST http://a.example/ HTTP/1.1\r\n{header}\r\n\r\n");
        assert_eq!(body_len(&head), Ok(want), "{header:?}");
    }
}

#[test]
fn ambiguous_bodies_are_refused() {
    let refused = [
        ("Transfer-Encoding: chunked", BadRequest::TransferEncoding),
        ("transfer-encoding: identity", BadRequest::TransferEncoding),
        (
            "Content-Length: 3\r\nTransfer-Encoding: chunked",
            BadRequest::TransferEncoding,
        ),
        (
            "Content-Length: 3\r\nContent-Length: 3",
            BadRequest::Malformed,
        ),
        ("Content-Length: 3, 3", BadRequest::Malformed),
        ("Content-Length: +3", BadRequest::Malformed),
        ("Content-Length: -1", BadRequest::Malformed),
        ("Content-Length: 0x10", BadRequest::Malformed),
        ("Content-Length:", BadRequest::Malformed),
        (
            "Content-Length: 99999999999999999999",
            BadRequest::Malformed,
        ),
        (
            "Content-Length: 18446744073709551616",
            BadRequest::Malformed,
        ),
    ];
    for (headers, want) in refused {
        let head = format!("POST http://a.example/ HTTP/1.1\r\n{headers}\r\n\r\n");
        assert_eq!(parse(head.as_bytes()), Err(want), "{headers:?}");
    }
    // A CONNECT head's headers are not a body: the tunnel is opaque.
    let connect = "CONNECT a.example:443 HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
    assert!(matches!(parse(connect.as_bytes()), Ok(Request::Connect(_))));
}
