//! The request head: read with a size bound, then parsed strictly. Two forms
//! are accepted: `CONNECT host:port`, and an absolute `http://` URI on port
//! 80, which is rewritten to origin form for the upstream. A plain request
//! carries at most a `Content-Length` body; `Transfer-Encoding` is refused,
//! so the proxy always knows where the one request it forwards ends.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::target::Target;

const TERMINATOR: &[u8] = b"\r\n\r\n";

/// Headers never forwarded upstream: the proxy sets `Host` and
/// `Connection` itself, and the rest belong to the hop to the proxy.
/// Every `proxy-*` header is dropped as well.
const DROPPED_HEADERS: [&str; 4] = ["host", "connection", "keep-alive", "upgrade"];

/// A complete request head and the bytes the client sent after it.
#[derive(Debug)]
pub(crate) struct Head {
    pub(crate) bytes: Vec<u8>,
    pub(crate) early: Vec<u8>,
}

/// Why no head was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeadError {
    /// No terminator within the size bound.
    TooLarge,
    /// The client closed or failed before a full head.
    Closed,
}

/// Read until the blank line, at most `max` bytes in all.
pub(crate) async fn read_head<R: AsyncRead + Unpin>(
    stream: &mut R,
    max: usize,
) -> Result<Head, HeadError> {
    let mut buf = vec![0u8; max];
    let mut len = 0;
    loop {
        if len == max {
            return Err(HeadError::TooLarge);
        }
        let n = stream
            .read(&mut buf[len..])
            .await
            .map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(HeadError::Closed);
        }
        let from = len.saturating_sub(TERMINATOR.len() - 1);
        len += n;
        if let Some(at) = find(&buf[from..len], TERMINATOR) {
            let end = from + at + TERMINATOR.len();
            let early = buf[end..len].to_vec();
            buf.truncate(end);
            return Ok(Head { bytes: buf, early });
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A parsed request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// `CONNECT host:port`: answer `200` and pipe.
    Connect(Target),
    /// Plain HTTP on port 80: send `head` upstream, then the `body_len`
    /// bytes of its body, then nothing more from the client.
    Forward {
        target: Target,
        head: Vec<u8>,
        body_len: u64,
    },
}

impl Request {
    pub(crate) fn target(&self) -> &Target {
        match self {
            Self::Connect(target) | Self::Forward { target, .. } => target,
        }
    }
}

/// Why a head was refused with `400`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadRequest {
    /// Not well-formed HTTP/1.x, or an invalid host or port.
    Malformed,
    /// Neither CONNECT nor an absolute `http://` URI.
    Unsupported,
    /// An absolute `http://` URI on a port other than 80.
    PlainHttpPort,
    /// A plain request with a `Transfer-Encoding` header.
    TransferEncoding,
}

impl BadRequest {
    /// The body of the `400` reply.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Malformed => "malformed request\n",
            Self::Unsupported => "only CONNECT and absolute http:// requests are proxied\n",
            Self::PlainHttpPort => "plain HTTP is proxied on port 80 only; use CONNECT\n",
            Self::TransferEncoding => {
                "Transfer-Encoding is not proxied; send the body with Content-Length\n"
            }
        }
    }
}

struct Line<'a> {
    method: &'a str,
    target: &'a str,
    version: &'a str,
}

/// Parse a head returned by [`read_head`].
pub(crate) fn parse(head: &[u8]) -> Result<Request, BadRequest> {
    let body = head.strip_suffix(TERMINATOR).ok_or(BadRequest::Malformed)?;
    let mut lines = split_lines(body);
    let line = request_line(lines.next().ok_or(BadRequest::Malformed)?)?;
    let headers: Vec<&[u8]> = lines.collect();
    for header in &headers {
        check_header(header)?;
    }
    if line.method == "CONNECT" {
        let (host, port) = authority(line.target, None)?;
        let target = Target::new(host, port).map_err(|_| BadRequest::Malformed)?;
        return Ok(Request::Connect(target));
    }
    forward(&line, &headers)
}

fn split_lines(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(body);
    std::iter::from_fn(move || {
        let current = rest?;
        match find(current, b"\r\n") {
            Some(at) => {
                rest = Some(&current[at + 2..]);
                Some(&current[..at])
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

fn request_line(raw: &[u8]) -> Result<Line<'_>, BadRequest> {
    let visible = |b: &u8| b.is_ascii_graphic() || *b == b' ';
    if !raw.iter().all(visible) {
        return Err(BadRequest::Malformed);
    }
    let text = std::str::from_utf8(raw).map_err(|_| BadRequest::Malformed)?;
    let mut parts = text.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(BadRequest::Malformed);
    };
    if !is_token(method) || target.is_empty() || !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(BadRequest::Malformed);
    }
    Ok(Line {
        method,
        target,
        version,
    })
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `name: value`, no folding, no control bytes but tab in the value.
fn check_header(line: &[u8]) -> Result<(), BadRequest> {
    let colon = line
        .iter()
        .position(|b| *b == b':')
        .ok_or(BadRequest::Malformed)?;
    let name = std::str::from_utf8(&line[..colon]).map_err(|_| BadRequest::Malformed)?;
    let value_ok = line[colon + 1..]
        .iter()
        .all(|b| *b == b'\t' || (*b >= 0x20 && *b != 0x7F));
    if is_token(name) && value_ok {
        Ok(())
    } else {
        Err(BadRequest::Malformed)
    }
}

/// `host:port` or `[v6]:port`; the port may be left out only when a
/// default is given.
fn authority(text: &str, default_port: Option<u16>) -> Result<(&str, u16), BadRequest> {
    let (host, port) = if text.starts_with('[') {
        let close = text.find(']').ok_or(BadRequest::Malformed)?;
        let rest = &text[close + 1..];
        let port = match rest.strip_prefix(':') {
            Some(port) => Some(port),
            None if rest.is_empty() => None,
            None => return Err(BadRequest::Malformed),
        };
        (&text[..=close], port)
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    let port = match port {
        Some(digits) => parse_port(digits)?,
        None => default_port.ok_or(BadRequest::Malformed)?,
    };
    Ok((host, port))
}

fn parse_port(digits: &str) -> Result<u16, BadRequest> {
    if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BadRequest::Malformed);
    }
    digits.parse().map_err(|_| BadRequest::Malformed)
}

/// An absolute `http://` request rewritten to origin form, with the URI's
/// host as `Host` and `Connection: close` (one request per connection).
fn forward(line: &Line<'_>, headers: &[&[u8]]) -> Result<Request, BadRequest> {
    let scheme = line.target.get(..7).ok_or(BadRequest::Unsupported)?;
    if !scheme.eq_ignore_ascii_case("http://") {
        return Err(BadRequest::Unsupported);
    }
    let rest = &line.target[7..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (auth, path) = rest.split_at(end);
    if auth.is_empty() || auth.contains('@') {
        return Err(BadRequest::Malformed);
    }
    let (host, port) = authority(auth, Some(80))?;
    let target = Target::new(host, port).map_err(|_| BadRequest::Malformed)?;
    if port != 80 {
        return Err(BadRequest::PlainHttpPort);
    }
    let path = path.split('#').next().unwrap_or_default();
    let path = match path.as_bytes().first() {
        None => "/".to_owned(),
        Some(b'?') => format!("/{path}"),
        Some(_) => path.to_owned(),
    };
    let body_len = body_len(headers)?;
    let head = rewrite(line, &path, target.host(), headers);
    Ok(Request::Forward {
        target,
        head,
        body_len,
    })
}

/// The request body's length: `Content-Length`, or 0 without one (RFC 9112
/// 6.3). Any `Transfer-Encoding`, a repeated `Content-Length` or one that is
/// not plain digits is refused, so the proxy and the origin cannot disagree
/// on where the request ends.
fn body_len(headers: &[&[u8]]) -> Result<u64, BadRequest> {
    let mut length = None;
    for header in headers {
        let (name, value) = split_header(header);
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return Err(BadRequest::TransferEncoding);
        }
        if !name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let digits = value.trim_ascii();
        let valid =
            !digits.is_empty() && digits.len() <= 19 && digits.iter().all(u8::is_ascii_digit);
        if !valid || length.is_some() {
            return Err(BadRequest::Malformed);
        }
        let text = std::str::from_utf8(digits).map_err(|_| BadRequest::Malformed)?;
        length = Some(text.parse().map_err(|_| BadRequest::Malformed)?);
    }
    Ok(length.unwrap_or(0))
}

/// Name and value of a header [`check_header`] accepted.
fn split_header(header: &[u8]) -> (&[u8], &[u8]) {
    let colon = header.iter().position(|b| *b == b':').unwrap_or(0);
    (&header[..colon], &header[colon + 1..])
}

fn rewrite(line: &Line<'_>, path: &str, host: &str, headers: &[&[u8]]) -> Vec<u8> {
    let mut out = format!(
        "{} {path} {}\r\nHost: {host}\r\n",
        line.method, line.version
    )
    .into_bytes();
    for header in headers {
        if !dropped(header) {
            out.extend_from_slice(header);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

fn dropped(header: &[u8]) -> bool {
    let colon = header.iter().position(|b| *b == b':').unwrap_or(0);
    let name = String::from_utf8_lossy(&header[..colon]).to_ascii_lowercase();
    name.starts_with("proxy-") || DROPPED_HEADERS.contains(&name.as_str())
}

#[cfg(test)]
mod tests;
