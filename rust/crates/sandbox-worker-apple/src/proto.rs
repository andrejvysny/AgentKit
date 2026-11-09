//! Helper wire protocol (§12.2): length-prefixed JSON over the helper
//! child's stdin/stdout. The Rust adapter is the only caller; the Swift
//! helper (macOS) and the fake helper (L1 contract tests) speak the same
//! frames. Cap 1 MiB; malformed input fails closed (no partial trust).

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Error, Debug)]
pub enum HelperError {
    #[error("io: {0}: {1}")]
    Io(String, std::io::Error),
    #[error("helper protocol: {0}")]
    Protocol(String),
    #[error("helper reported failure: {0}")]
    Failed(String),
    #[error("helper unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRequest {
    pub id: u64,
    pub cmd: HelperCmd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", deny_unknown_fields)]
pub enum HelperCmd {
    #[serde(rename = "status")]
    Status {},
    #[serde(rename = "stop")]
    Stop {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// VM manifest handed to the helper via `--manifest <path>` (admin-derived
/// paths only; the worker/adapter derives every path — S20, S03).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmManifest {
    pub schema: u32,
    pub vm_id: String,
    pub kernel: String,
    pub boot_args: String,
    pub rootfs: String,
    pub data_disk: String,
    pub cpus: u32,
    pub memory_mib: u64,
    /// Unix socket the helper must serve the guest vsock relay on
    /// (CONNECT-handshake compatible with the worker's guestlink).
    pub relay_socket: String,
    pub guest_vsock_port: u32,
}

/// `helper probe` JSON (separate one-shot invocation, no VM).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperProbe {
    pub hardware_virtualization: bool,
    pub entitled: bool,
    pub helper_version: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    value: &impl Serialize,
) -> Result<(), HelperError> {
    let json =
        serde_json::to_vec(value).map_err(|e| HelperError::Protocol(format!("encode: {e}")))?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(HelperError::Protocol("frame exceeds cap".into()));
    }
    let len = (json.len() as u32).to_be_bytes();
    w.write_all(&len)
        .await
        .map_err(|e| HelperError::Io("write len".into(), e))?;
    w.write_all(&json)
        .await
        .map_err(|e| HelperError::Io("write body".into(), e))?;
    w.flush()
        .await
        .map_err(|e| HelperError::Io("flush".into(), e))?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, HelperError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(HelperError::Io("read len".into(), e)),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME_BYTES {
        return Err(HelperError::Protocol(format!("bad frame length {n}")));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)
        .await
        .map_err(|e| HelperError::Io("read body".into(), e))?;
    Ok(Some(buf))
}

pub async fn read_json<R, T>(r: &mut R) -> Result<Option<T>, HelperError>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    match read_frame(r).await? {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| HelperError::Protocol(format!("decode: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_and_length_cap() {
        let req = HelperRequest {
            id: 7,
            cmd: HelperCmd::Status {},
        };
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &req).await.unwrap();
        let got: HelperRequest = read_json(&mut buf.as_slice()).await.unwrap().unwrap();
        assert_eq!(got.id, 7);

        // length prefix over cap is refused without allocating the body.
        let mut evil = vec![0xFF, 0xFF, 0xFF, 0x7F];
        evil.extend_from_slice(b"junk");
        let err = read_frame(&mut evil.as_slice()).await.unwrap_err();
        assert!(matches!(err, HelperError::Protocol(_)), "{err}");

        // clean EOF reads as None (helper exited).
        let empty: Vec<u8> = Vec::new();
        assert!(read_json::<_, HelperResponse>(&mut empty.as_slice())
            .await
            .unwrap()
            .is_none());
    }
}
