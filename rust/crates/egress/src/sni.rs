//! The server name of a CONNECT tunnel's TLS ClientHello. A tunnel is
//! opaque, so a grant would otherwise reach every site served at the
//! address its name resolves to (a CDN): the first record the client sends
//! must be a TLS handshake record holding a whole ClientHello whose
//! `server_name` is the granted host.
//!
//! The parser is strict, so it cannot read a different name than the
//! server will: every length must fill its container exactly, no extension
//! may appear twice, and the name list holds exactly one `host_name`. A
//! ClientHello split over several records is refused; clients send it in
//! one, and a record's 16 KiB fits any real one.
//!
//! Encrypted Client Hello (`encrypted_client_hello`, extension 0xfe0d):
//! only the outer ClientHello is readable, so its `server_name` is the name
//! checked. Which site a client-facing server then serves for the
//! encrypted inner hello is not visible here. Neither is a second
//! ClientHello after a HelloRetryRequest, nor the HTTP `Host` a client
//! sends inside the TLS session.

use std::collections::HashSet;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::target::Target;

/// Bytes of a TLS record header: content type, version, length.
const HEADER_BYTES: usize = 5;
/// Longest record payload TLS allows for plaintext (RFC 8446 5.1).
const RECORD_MAX_BYTES: usize = 1 << 14;
const CONTENT_HANDSHAKE: u8 = 22;
const HANDSHAKE_CLIENT_HELLO: u8 = 1;
const EXTENSION_SERVER_NAME: u16 = 0;
const NAME_TYPE_HOST: u8 = 0;
const RANDOM_BYTES: usize = 32;
const SESSION_ID_MAX_BYTES: usize = 32;

/// The bytes read from the client up to the end of its first record.
#[derive(Debug)]
pub(crate) struct Peeked {
    /// Everything read, in order: the record, then any bytes the client
    /// sent with its CONNECT head after it. Forwarded upstream first.
    pub(crate) bytes: Vec<u8>,
    record_len: usize,
}

impl Peeked {
    /// The first record, header included.
    pub(crate) fn record(&self) -> &[u8] {
        &self.bytes[..self.record_len]
    }
}

/// Why no first record was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PeekError {
    /// The first bytes are not the header of a TLS handshake record, or
    /// the record is longer than TLS allows.
    NotTls,
    /// The client closed or failed before a whole record.
    Closed,
}

/// Read until `buf` (the bytes sent after the CONNECT head, then more from
/// `stream`) holds the client's first record. Reads nothing past the
/// record's end, so at most 16 KiB and 5 bytes beyond what `buf` held.
pub(crate) async fn read_record<R: AsyncRead + Unpin>(
    stream: &mut R,
    mut buf: Vec<u8>,
) -> Result<Peeked, PeekError> {
    fill(stream, &mut buf, HEADER_BYTES).await?;
    let payload = payload_len(&buf[..HEADER_BYTES]).ok_or(PeekError::NotTls)?;
    let record_len = HEADER_BYTES + payload;
    fill(stream, &mut buf, record_len).await?;
    Ok(Peeked {
        bytes: buf,
        record_len,
    })
}

async fn fill<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut Vec<u8>,
    want: usize,
) -> Result<(), PeekError> {
    while buf.len() < want {
        let have = buf.len();
        buf.resize(want, 0);
        match stream.read(&mut buf[have..]).await {
            Ok(n) if n > 0 => buf.truncate(have + n),
            _ => return Err(PeekError::Closed),
        }
    }
    Ok(())
}

/// The payload length a record header announces, if it is a handshake
/// record of SSL 3 to TLS 1.3 within the TLS bound.
fn payload_len(header: &[u8]) -> Option<usize> {
    let [kind, major, minor, high, low] = <[u8; HEADER_BYTES]>::try_from(header).ok()?;
    let len = usize::from(u16::from_be_bytes([high, low]));
    let ok = kind == CONTENT_HANDSHAKE
        && major == 3
        && minor <= 4
        && (1..=RECORD_MAX_BYTES).contains(&len);
    ok.then_some(len)
}

/// `record` is not one record holding exactly one well-formed ClientHello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Malformed;

/// The `server_name` host name of the ClientHello that `record` (one whole
/// record, header included) holds, as sent; `None` when it names none.
pub(crate) fn server_name(record: &[u8]) -> Result<Option<&[u8]>, Malformed> {
    let mut record = Reader(record);
    let header = record.take(HEADER_BYTES)?;
    if payload_len(header) != Some(record.0.len()) {
        return Err(Malformed);
    }
    if record.u8()? != HANDSHAKE_CLIENT_HELLO {
        return Err(Malformed);
    }
    let mut hello = record.vec24()?;
    record.end()?;
    let _legacy_version = hello.u16()?;
    hello.take(RANDOM_BYTES)?;
    let session_id = hello.vec8()?;
    let suites = hello.vec16()?;
    let compression = hello.vec8()?;
    let shape_ok = session_id.0.len() <= SESSION_ID_MAX_BYTES
        && !suites.0.is_empty()
        && suites.0.len() % 2 == 0
        && !compression.0.is_empty();
    if !shape_ok {
        return Err(Malformed);
    }
    if hello.0.is_empty() {
        // No extensions block at all (allowed before TLS 1.3).
        return Ok(None);
    }
    let extensions = hello.vec16()?;
    hello.end()?;
    find_server_name(extensions)
}

fn find_server_name(mut extensions: Reader<'_>) -> Result<Option<&[u8]>, Malformed> {
    let mut seen = HashSet::new();
    let mut name = None;
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let data = extensions.vec16()?;
        if !seen.insert(kind) {
            return Err(Malformed);
        }
        if kind == EXTENSION_SERVER_NAME {
            name = Some(host_name(data)?);
        }
    }
    Ok(name)
}

/// The one `host_name` entry of a `server_name` extension (RFC 6066 3).
fn host_name(mut data: Reader<'_>) -> Result<&[u8], Malformed> {
    let mut list = data.vec16()?;
    data.end()?;
    if list.u8()? != NAME_TYPE_HOST {
        return Err(Malformed);
    }
    let name = list.vec16()?;
    list.end()?;
    if name.0.is_empty() {
        return Err(Malformed);
    }
    Ok(name.0)
}

/// True when a ClientHello naming `name` may open a tunnel to `target`:
/// the name is the target's host, ASCII case ignored and one trailing dot
/// dropped. A hello without a name is admitted only for an IP-literal
/// target, for which clients send none.
pub(crate) fn admits(name: Option<&[u8]>, target: &Target) -> bool {
    match name {
        Some(name) => {
            let name = name.strip_suffix(b".").unwrap_or(name);
            name.eq_ignore_ascii_case(target.host().as_bytes())
        }
        None => target.literal().is_some(),
    }
}

/// A bounds-checked cursor over TLS wire bytes.
#[derive(Clone, Copy)]
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let (head, rest) = self.0.split_at_checked(n).ok_or(Malformed)?;
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        let [byte] = <[u8; 1]>::try_from(self.take(1)?).map_err(|_| Malformed)?;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        let bytes = <[u8; 2]>::try_from(self.take(2)?).map_err(|_| Malformed)?;
        Ok(u16::from_be_bytes(bytes))
    }

    /// A vector with a one-byte length.
    fn vec8(&mut self) -> Result<Reader<'a>, Malformed> {
        let len = self.u8()?;
        self.take(usize::from(len)).map(Reader)
    }

    /// A vector with a two-byte length.
    fn vec16(&mut self) -> Result<Reader<'a>, Malformed> {
        let len = self.u16()?;
        self.take(usize::from(len)).map(Reader)
    }

    /// A vector with a three-byte length.
    fn vec24(&mut self) -> Result<Reader<'a>, Malformed> {
        let [a, b, c] = <[u8; 3]>::try_from(self.take(3)?).map_err(|_| Malformed)?;
        let len = u32::from_be_bytes([0, a, b, c]);
        self.take(usize::try_from(len).map_err(|_| Malformed)?)
            .map(Reader)
    }

    /// Nothing may follow.
    fn end(&self) -> Result<(), Malformed> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Malformed)
        }
    }
}

#[cfg(test)]
mod tests;
