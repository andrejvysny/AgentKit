//! Association handshake (§3.3, §8.1, S16).
//!
//! The worker opens with `HelloAck`; the guest validates
//! `protocol_version == 1` (no downgrade, ever) and echoes the kernel-cmdline
//! boot nonce as base64 — channel association only, never attestation.
//! Nothing else from the wire is trusted.

use std::io;
use std::time::Instant;

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::api::{GuestToWorker, WorkerToGuest};
use agentkit_sandbox_contracts::protocol::FrameDecoder;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{GuestFail, code};

/// Fixed PATH used for capability probing (mirrors the exec PATH dirs).
pub const CAPABILITY_PATH_DIRS: [&str; 2] = ["/usr/bin", "/bin"];

/// True when `name` is executable under the fixed PATH dirs.
#[must_use]
pub fn binary_exists(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return false;
    }
    CAPABILITY_PATH_DIRS.iter().any(|dir| {
        let p = format!("{dir}/{name}");
        let c = match std::ffi::CString::new(p) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // SAFETY: access(X_OK) on a stack C string.
        unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
    })
}

/// Advertised capabilities: `search` is guest-owned and always on; the rest
/// reflect binaries under the fixed PATH.
#[must_use]
pub fn detect_capabilities() -> Vec<String> {
    let mut caps = vec!["search".to_string()];
    for (cap, bin) in [
        ("git", "git"),
        ("pip", "pip"),
        ("npm", "npm"),
        ("cargo", "cargo"),
    ] {
        if binary_exists(bin) {
            caps.push(cap.to_string());
        }
    }
    caps
}

/// Await the worker's `HelloAck`, validate it, and reply with `Hello`.
/// Any other frame before `HelloAck` (or a bad version) terminates the
/// association.
pub async fn negotiate<S>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
    boot_nonce: &[u8],
) -> Result<(), GuestFail>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1) await HelloAck (must be the very first frame)
    let frame = read_frame(stream, decoder).await?;
    let ack = match serde_json::from_value::<WorkerToGuest>(frame.body) {
        Ok(WorkerToGuest::HelloAck {
            protocol_version, ..
        }) => protocol_version,
        Ok(other) => {
            return Err(GuestFail::new(
                code::BAD_INPUT,
                format!(
                    "expected hello_ack before association, got {:?}",
                    other_name(&other)
                ),
            ));
        }
        Err(_) => {
            return Err(GuestFail::new(code::BAD_INPUT, "malformed hello_ack body"));
        }
    };
    if ack != PROTOCOL_VERSION {
        return Err(GuestFail::new(
            code::BAD_INPUT,
            format!("unsupported protocol version {ack}"),
        ));
    }

    // 2) reply Hello
    let hello = GuestToWorker::Hello {
        boot_nonce_b64: STANDARD.encode(boot_nonce),
        guest_agent_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_min: PROTOCOL_VERSION,
        protocol_max: PROTOCOL_VERSION,
        capabilities: detect_capabilities(),
    };
    let bytes = crate::server::encode_outbound("hello", &hello, false)?;
    stream.write_all(&bytes).await.map_err(GuestFail::from)?;
    Ok(())
}

/// Pull the next complete frame from the stream.
pub(crate) async fn read_frame<S>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
) -> Result<agentkit_sandbox_contracts::protocol::WireFrame, GuestFail>
where
    S: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if let Some(frame) = decoder
            .next_frame(now_ms())
            .map_err(|e| GuestFail::new(code::BAD_INPUT, e.to_string()))?
        {
            return Ok(frame);
        }
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| GuestFail::new(code::IO, io_error_kind(&e)))?;
        if n == 0 {
            return Err(GuestFail::new(code::IO, "connection closed"));
        }
        decoder.push(&buf[..n]);
    }
}

fn io_error_kind(e: &io::Error) -> String {
    format!("io:{}", e.kind() as u8)
}

fn other_name(m: &WorkerToGuest) -> &'static str {
    match m {
        WorkerToGuest::HelloAck { .. } => "hello_ack",
        WorkerToGuest::ExecSpawn { .. } => "exec_spawn",
        WorkerToGuest::ExecCancel { .. } => "exec_cancel",
        WorkerToGuest::FsOp { .. } => "fs_op",
        WorkerToGuest::BrokerChunk { .. } => "broker_chunk",
        WorkerToGuest::BrokerDeny { .. } => "broker_deny",
        WorkerToGuest::HealthPing {} => "health_ping",
        WorkerToGuest::Shutdown { .. } => "shutdown",
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn uptime_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// Load average (1 min) in per-mille, from /proc/loadavg; 0 if unreadable.
#[must_use]
pub fn load_permille() -> u32 {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(s) => {
            let first = s.split_whitespace().next().unwrap_or("0");
            let v: f64 = first.parse().unwrap_or(0.0);
            if v.is_finite() && v >= 0.0 {
                (v * 1000.0).min(u32::MAX as f64) as u32
            } else {
                0
            }
        }
        Err(_) => 0,
    }
}
