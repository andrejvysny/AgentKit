//! Byte pipe of an established tunnel, bounded in bytes and lifetime and
//! closed when the grant goes away. Bytes are counted, never inspected or
//! kept (NET-9); a plain HTTP request's tunnel also stops reading the
//! client once the request's body has gone upstream.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::watch;

use crate::proxy::Policy;
use crate::stats::Counters;
use crate::target::Target;

const CHUNK: usize = 16 * 1024;

/// Why a tunnel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// Both sides finished.
    Closed,
    /// A read or write failed.
    Failed,
    /// The target's grant was revoked.
    Revoked,
    /// The lifetime bound passed.
    Lifetime,
    /// The byte bound was reached.
    Bytes,
}

/// The bounds and signals of one tunnel.
pub(crate) struct Tunnel<'a> {
    pub(crate) target: &'a Target,
    /// Marked seen at the tunnel's last policy check.
    pub(crate) policy: watch::Receiver<Policy>,
    pub(crate) lifetime: Duration,
    pub(crate) max_bytes: u64,
    /// Most client bytes sent upstream, early bytes included: the body of
    /// a plain HTTP request. `None` for a CONNECT tunnel.
    pub(crate) up_limit: Option<u64>,
    pub(crate) counters: &'a Counters,
}

/// Pipe both ways until both sides finish or a bound ends the tunnel.
/// `early` (bytes the client sent after its head) goes upstream first.
/// Ending drops both copies at once, whatever they are waiting on. The
/// write halves live until then, so a client direction that stops at its
/// limit does not half-close the upstream (an origin may read that as an
/// aborted request).
pub(crate) async fn run(
    client: UnixStream,
    upstream: TcpStream,
    early: Vec<u8>,
    tunnel: Tunnel<'_>,
) -> End {
    let budget = Budget::new(tunnel.max_bytes);
    let (client_read, mut client_write) = client.into_split();
    let (upstream_read, mut upstream_write) = upstream.into_split();
    let counters = tunnel.counters;
    let up = pump(
        client_read,
        &mut upstream_write,
        early,
        Bounds {
            budget: &budget,
            limit: tunnel.up_limit,
        },
        &counters.bytes_up,
    );
    let down = pump(
        upstream_read,
        &mut client_write,
        Vec::new(),
        Bounds {
            budget: &budget,
            limit: None,
        },
        &counters.bytes_down,
    );
    let stop = stop(tunnel.policy, tunnel.target, tunnel.lifetime);
    tokio::pin!(up, down, stop);
    let (mut up_open, mut down_open) = (true, true);
    while up_open || down_open {
        let flow = tokio::select! {
            flow = &mut up, if up_open => {
                up_open = false;
                flow
            }
            flow = &mut down, if down_open => {
                down_open = false;
                flow
            }
            end = &mut stop => return end,
        };
        match flow {
            Flow::Eof | Flow::Done => {}
            Flow::Spent => return End::Bytes,
            Flow::Failed => return End::Failed,
        }
    }
    End::Closed
}

/// Resolves when the target leaves the policy or `lifetime` has passed
/// since the call (a huge lifetime saturates instead of overflowing).
async fn stop(mut policy: watch::Receiver<Policy>, target: &Target, lifetime: Duration) -> End {
    tokio::select! {
        () = revoked(&mut policy, target) => End::Revoked,
        () = tokio::time::sleep(lifetime) => End::Lifetime,
    }
}

/// Resolves once a policy change after the receiver's last look leaves
/// `target` out; never while the proxy lives and still grants it.
pub(crate) async fn revoked(policy: &mut watch::Receiver<Policy>, target: &Target) {
    loop {
        if policy.changed().await.is_err() {
            // The proxy is gone; its cancellation ends the connection.
            std::future::pending::<()>().await;
        }
        if !policy.borrow_and_update().contains(target) {
            return;
        }
    }
}

/// How one direction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// The source closed; the destination's write side was shut.
    Eof,
    /// The direction's limit was reached; the source is no longer read and
    /// the destination is left open.
    Done,
    /// The tunnel's byte bound was reached.
    Spent,
    /// A read or write failed.
    Failed,
}

/// What one direction may carry: the tunnel's shared byte budget and, for
/// the client side of a plain request, its own limit.
struct Bounds<'a> {
    budget: &'a Budget,
    limit: Option<u64>,
}

async fn pump<R, W>(
    mut from: R,
    mut to: W,
    early: Vec<u8>,
    bounds: Bounds<'_>,
    moved: &AtomicU64,
) -> Flow
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut left = bounds.limit;
    if let Some(flow) = forward(&mut to, &early, &bounds, &mut left, moved).await {
        return flow;
    }
    let mut buf = vec![0u8; CHUNK];
    loop {
        match from.read(&mut buf).await {
            Ok(0) => {
                let _ = to.shutdown().await;
                return Flow::Eof;
            }
            Ok(n) => {
                let chunk = &buf[..n];
                if let Some(flow) = forward(&mut to, chunk, &bounds, &mut left, moved).await {
                    return flow;
                }
            }
            Err(_) => return Flow::Failed,
        }
    }
}

/// Write as much of `chunk` as the limit and the budget allow; `Some` ends
/// the pump. Bytes past the limit are dropped, never written.
async fn forward<W>(
    to: &mut W,
    chunk: &[u8],
    bounds: &Bounds<'_>,
    left: &mut Option<u64>,
    moved: &AtomicU64,
) -> Option<Flow>
where
    W: AsyncWrite + Unpin,
{
    let chunk = match *left {
        Some(left) => &chunk[..chunk.len().min(usize::try_from(left).unwrap_or(usize::MAX))],
        None => chunk,
    };
    let allowed = bounds.budget.take(chunk.len());
    if allowed > 0 {
        if to.write_all(&chunk[..allowed]).await.is_err() {
            return Some(Flow::Failed);
        }
        moved.fetch_add(allowed as u64, Ordering::Relaxed);
    }
    if bounds.budget.spent() {
        return Some(Flow::Spent);
    }
    let left = left.as_mut()?;
    *left -= allowed as u64;
    (*left == 0).then_some(Flow::Done)
}

/// Bytes one tunnel may still carry, shared by both directions. A byte is
/// reserved before it is written, so the bound is never exceeded.
struct Budget {
    limit: u64,
    used: AtomicU64,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    /// Reserve up to `want` bytes; answers how many were reserved.
    fn take(&self, want: usize) -> usize {
        let want = u64::try_from(want).unwrap_or(u64::MAX);
        let mut granted = 0;
        let _ = self
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                granted = want.min(self.limit - used);
                Some(used + granted)
            });
        usize::try_from(granted).unwrap_or(usize::MAX)
    }

    fn spent(&self) -> bool {
        self.used.load(Ordering::Relaxed) >= self.limit
    }
}

#[cfg(test)]
mod tests;
