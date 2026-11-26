use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use super::{Malformed, PeekError, admits, read_record, server_name};
use crate::target::Target;

/// Captured from `openssl s_client -servername Example.COM` (OpenSSL 4.1:
/// TLS 1.3, ML-KEM key share, 1574 bytes).
const OPENSSL_HELLO: &[u8] = include_bytes!("../../tests/fixtures/openssl-client-hello.bin");
/// Captured from macOS `curl https://pypi.org:<port>/`.
const CURL_HELLO: &[u8] = include_bytes!("../../tests/fixtures/curl-client-hello.bin");

type Extension = (u16, Vec<u8>);

fn u16_len(bytes: &[u8]) -> [u8; 2] {
    u16::try_from(bytes.len()).expect("fits u16").to_be_bytes()
}

/// A `server_name` extension with one entry per `(name_type, name)`.
fn sni_ext(entries: &[(u8, &[u8])]) -> Extension {
    let mut list = Vec::new();
    for (kind, name) in entries {
        list.push(*kind);
        list.extend(u16_len(name));
        list.extend_from_slice(name);
    }
    let mut data = u16_len(&list).to_vec();
    data.extend(list);
    (0, data)
}

fn named(name: &str) -> Extension {
    sni_ext(&[(0, name.as_bytes())])
}

/// The ClientHello body; `None` leaves out the extensions block.
fn hello_body(extensions: Option<&[Extension]>) -> Vec<u8> {
    let mut body = vec![3, 3];
    body.extend([7u8; 32]);
    body.push(0);
    body.extend([0, 2, 0x13, 0x01]);
    body.extend([1, 0]);
    if let Some(extensions) = extensions {
        let mut block = Vec::new();
        for (kind, data) in extensions {
            block.extend(kind.to_be_bytes());
            block.extend(u16_len(data));
            block.extend_from_slice(data);
        }
        body.extend(u16_len(&block));
        body.extend(block);
    }
    body
}

/// `body` as a ClientHello handshake message.
fn handshake_of(body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("fits").to_be_bytes();
    let mut handshake = vec![1, len[1], len[2], len[3]];
    handshake.extend_from_slice(body);
    handshake
}

/// One handshake record with `payload`.
fn wrap(payload: &[u8]) -> Vec<u8> {
    let mut record = vec![22, 3, 1];
    record.extend(u16_len(payload));
    record.extend_from_slice(payload);
    record
}

/// One handshake record holding `body` as a ClientHello.
fn record_of(body: &[u8]) -> Vec<u8> {
    wrap(&handshake_of(body))
}

fn record(extensions: &[Extension]) -> Vec<u8> {
    record_of(&hello_body(Some(extensions)))
}

fn target(host: &str) -> Target {
    Target::new(host, 443).expect("valid target")
}

#[test]
fn real_client_hellos_parse() {
    assert_eq!(server_name(OPENSSL_HELLO), Ok(Some(&b"Example.COM"[..])));
    assert!(admits(Some(b"Example.COM"), &target("example.com")));
    assert_eq!(server_name(CURL_HELLO), Ok(Some(&b"pypi.org"[..])));
}

#[test]
fn the_name_is_found_among_other_extensions() {
    let extensions = [
        (0x002b, vec![2, 3, 4]),
        named("pypi.org"),
        (0xfe0d, vec![0, 1, 2, 3]),
    ];
    assert_eq!(
        server_name(&record(&extensions)),
        Ok(Some(&b"pypi.org"[..]))
    );
    assert_eq!(server_name(&record(&[(0x002b, vec![2, 3, 4])])), Ok(None));
    assert_eq!(server_name(&record(&[])), Ok(None));
    assert_eq!(server_name(&record_of(&hello_body(None))), Ok(None));
}

#[test]
fn only_the_granted_host_is_admitted() {
    let pypi = target("pypi.org");
    for name in ["pypi.org", "PyPI.Org", "pypi.org."] {
        assert!(admits(Some(name.as_bytes()), &pypi), "{name}");
    }
    let refused: [&[u8]; 8] = [
        b"files.pythonhosted.org",
        b"pypi.org..",
        b"pypi.org.evil.example",
        b"evil-pypi.org",
        b"ypi.org",
        b"pypi.org\0",
        b"pypi.org:443",
        b"",
    ];
    for name in refused {
        assert!(!admits(Some(name), &pypi), "{name:?}");
    }
    assert!(!admits(None, &pypi), "a named target needs a name");
}

#[test]
fn an_ip_literal_target_admits_no_name_or_its_own_text_only() {
    let literal = target("93.184.216.34");
    assert!(admits(None, &literal));
    assert!(admits(Some(b"93.184.216.34"), &literal));
    assert!(!admits(Some(b"pypi.org"), &literal));
    assert!(!admits(Some(b"93.184.216.35"), &literal));
}

#[test]
fn ambiguous_or_malformed_hellos_are_refused() {
    let mut wrong_type = record(&[named("a.test")]);
    wrong_type[5] = 2;
    let mut long_session = hello_body(Some(&[named("a.test")]));
    long_session[34] = 33;
    long_session.splice(35..35, [0u8; 33]);
    let hello = handshake_of(&hello_body(Some(&[named("a.test")])));
    let trailing = wrap(&[hello.as_slice(), &[0]].concat());
    let after_extensions = record_of(&[hello_body(Some(&[named("a.test")])), vec![0]].concat());
    let mut short_header = record(&[named("a.test")]);
    short_header[4] -= 1;
    let mut long_header = record(&[named("a.test")]);
    long_header[4] += 1;
    let mut split_hello = record(&[named("a.test")]);
    split_hello[8] += 1;
    let mut not_handshake = record(&[named("a.test")]);
    not_handshake[0] = 23;
    let mut list_too_long = named("a.test");
    list_too_long.1[1] += 1;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "two server_name extensions",
            record(&[named("a.test"), named("b.test")]),
        ),
        (
            "a repeated other extension",
            record(&[(5, vec![]), named("a.test"), (5, vec![])]),
        ),
        (
            "two names",
            record(&[sni_ext(&[(0, b"a.test"), (0, b"b.test")])]),
        ),
        (
            "a non-host name type",
            record(&[sni_ext(&[(1, b"a.test")])]),
        ),
        ("an empty name", record(&[sni_ext(&[(0, b"")])])),
        ("an empty name list", record(&[(0, vec![0, 0])])),
        ("an empty extension", record(&[(0, vec![])])),
        (
            "bytes after the list",
            record(&[(0, [named("a.test").1, vec![0]].concat())]),
        ),
        ("a list longer than its extension", record(&[list_too_long])),
        ("not a ClientHello", wrong_type),
        ("a long session id", record_of(&long_session)),
        (
            "no cipher suites",
            record_of(&[&[3, 3][..], &[7; 32], &[0, 0, 0, 1, 0]].concat()),
        ),
        (
            "odd cipher suites",
            record_of(&[&[3, 3][..], &[7; 32], &[0, 0, 1, 1, 1, 0]].concat()),
        ),
        (
            "no compression",
            record_of(&[&[3, 3][..], &[7; 32], &[0, 0, 2, 0x13, 1, 0]].concat()),
        ),
        ("a byte after the hello", trailing),
        ("a byte after the extensions", after_extensions),
        ("a header shorter than the record", short_header),
        ("a header longer than the record", long_header),
        ("a hello longer than its record", split_hello),
        ("not a handshake record", not_handshake),
    ];
    for (what, bytes) in cases {
        assert_eq!(server_name(&bytes), Err(Malformed), "{what}");
    }
}

/// Tiny xorshift64 so the fuzz loop needs no extra dependency.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).expect("fits")
    }
}

#[test]
fn truncated_and_mutated_hellos_never_panic() {
    for cut in 0..OPENSSL_HELLO.len() {
        assert_eq!(server_name(&OPENSSL_HELLO[..cut]), Err(Malformed), "{cut}");
    }
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..5_000 {
        let mut bytes = CURL_HELLO.to_vec();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bytes.len());
            bytes[at] = rng.next().to_be_bytes()[0];
        }
        let _ = server_name(&bytes);
    }
}

/// Hands out one byte per read, like a client sending a byte at a time.
struct Trickle<'a>(&'a [u8]);

impl AsyncRead for Trickle<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some((first, rest)) = self.0.split_first() {
            buf.put_slice(&[*first]);
            self.0 = rest;
        }
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_record_arriving_a_byte_at_a_time_is_read_to_its_end_only() {
    let hello = record(&[named("a.test")]);
    let wire = [hello.as_slice(), b"after"].concat();
    let mut stream = Trickle(&wire[2..]);
    let peeked = read_record(&mut stream, wire[..2].to_vec())
        .await
        .expect("record");
    assert_eq!(peeked.record(), hello.as_slice());
    assert_eq!(peeked.bytes, hello, "nothing read past the record");
    assert_eq!(stream.0, b"after");
}

#[tokio::test]
async fn bytes_sent_with_the_head_may_hold_the_record_and_more() {
    let hello = record(&[named("a.test")]);
    let early = [hello.as_slice(), b"0-RTT"].concat();
    let mut stream = Trickle(b"unread");
    let peeked = read_record(&mut stream, early.clone())
        .await
        .expect("record");
    assert_eq!(peeked.record(), hello.as_slice());
    assert_eq!(peeked.bytes, early);
    assert_eq!(stream.0, b"unread");
}

#[tokio::test]
async fn headers_that_are_not_tls_or_too_long_are_refused_before_the_body() {
    let max_ok = [22, 3, 1, 0x40, 0x00];
    let mut stream = Trickle(&[]);
    assert_eq!(
        read_record(&mut stream, max_ok.to_vec()).await.err(),
        Some(PeekError::Closed),
        "a 16 KiB record is waited for"
    );
    let refused: [&[u8]; 6] = [
        &[22, 3, 1, 0x40, 0x01],
        &[22, 3, 1, 0, 0],
        &[22, 3, 5, 0, 10],
        &[22, 2, 0, 0, 10],
        &[23, 3, 3, 0, 10],
        b"SSH-2.0-OpenSSH_9.6\r\n",
    ];
    for header in refused {
        let mut stream = Trickle(b"never read");
        let peeked = read_record(&mut stream, header.to_vec()).await;
        assert_eq!(peeked.err(), Some(PeekError::NotTls), "{header:?}");
        assert_eq!(stream.0, b"never read");
    }
    let mut short = Trickle(&[22, 3]);
    let peeked = read_record(&mut short, Vec::new()).await;
    assert_eq!(peeked.err(), Some(PeekError::Closed));
}
