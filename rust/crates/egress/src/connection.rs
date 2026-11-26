//! One guest connection: head, policy, limit, resolution, address rule,
//! connect, then (for CONNECT) the TLS server name, then the pipe. The
//! order is the ADR's: nothing is resolved for a target that is not
//! granted, nothing is connected that `agentkit_kernel::is_blocked`
//! refuses, and no client byte goes upstream before the checks pass.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::denials::DenialReason;
use crate::net::{self, IpNet};
use crate::proxy::{PendingPermit, Policy, Shared};
use crate::relay::{self, End, Tunnel};
use crate::request::{HeadError, Request, parse, read_head};
use crate::sni::{self, PeekError};
use crate::stats::bump;
use crate::target::Target;

const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

/// Serve one connection until it ends or the proxy shuts down; shutting
/// down drops whatever the connection is waiting on.
pub(crate) async fn serve(client: UnixStream, shared: Arc<Shared>, pending: PendingPermit) {
    let cancel = shared.cancel.clone();
    tokio::select! {
        () = handle(client, &shared, pending) => {}
        () = cancel.cancelled() => {}
    }
}

async fn handle(mut client: UnixStream, shared: &Shared, pending: PendingPermit) {
    let Some((request, early)) = receive(&mut client, shared).await else {
        return;
    };
    let target = request.target().clone();
    let mut policy = shared.policy.subscribe();
    if !policy.borrow_and_update().contains(&target) {
        return refuse(&mut client, shared, &target, DenialReason::NotGranted).await;
    }
    let Ok(_slot) = shared.tunnels.clone().try_acquire_owned() else {
        return refuse(&mut client, shared, &target, DenialReason::Limit).await;
    };
    // Every connection holds one of the two kinds of slot until it ends.
    drop(pending);
    let Some(mut upstream) = open(&mut client, shared, &target).await else {
        return;
    };
    // A revoke while resolving or connecting: nothing has been sent yet.
    if !policy.borrow_and_update().contains(&target) {
        return refuse(&mut client, shared, &target, DenialReason::NotGranted).await;
    }
    let start = establish(
        &mut client,
        &mut upstream,
        request,
        early,
        &mut policy,
        shared,
    );
    let Some((early, up_limit)) = start.await else {
        return;
    };
    bump(&shared.counters.tunnels_established);
    let tunnel = Tunnel {
        target: &target,
        policy,
        lifetime: shared.config.tunnel_lifetime,
        max_bytes: shared.config.max_bytes_per_tunnel,
        up_limit,
        counters: &shared.counters,
    };
    match relay::run(client, upstream, early, tunnel).await {
        End::Revoked => bump(&shared.counters.closed_revoked),
        End::Lifetime => bump(&shared.counters.closed_lifetime),
        End::Bytes => bump(&shared.counters.closed_bytes),
        End::Closed | End::Failed => {}
    }
}

/// The parsed head and the bytes after it, or `None` once the client has
/// been answered (or has gone).
async fn receive(client: &mut UnixStream, shared: &Shared) -> Option<(Request, Vec<u8>)> {
    let config = &shared.config;
    let read = timeout(
        config.head_timeout,
        read_head(client, config.head_max_bytes),
    )
    .await;
    let refusal = match read {
        Ok(Ok(head)) => match parse(&head.bytes) {
            Ok(request) => return Some((request, head.early)),
            Err(bad) => ("400 Bad Request", bad.message()),
        },
        Ok(Err(HeadError::TooLarge)) => ("400 Bad Request", "request head too large\n"),
        Ok(Err(HeadError::Closed)) => {
            bump(&shared.counters.bad_requests);
            return None;
        }
        Err(_) => ("408 Request Timeout", "request head too slow\n"),
    };
    bump(&shared.counters.bad_requests);
    reply(client, shared, refusal.0, refusal.1).await;
    None
}

/// Record the denial, then answer it.
async fn refuse(client: &mut UnixStream, shared: &Shared, target: &Target, reason: DenialReason) {
    shared.deny(target, reason);
    let (status, body) = match reason {
        DenialReason::NotGranted => ("403 Forbidden", "target not granted\n"),
        DenialReason::BlockedAddress => ("502 Bad Gateway", "target address refused\n"),
        DenialReason::Limit => ("503 Service Unavailable", "tunnel limit reached\n"),
        // Decided after the `200`, so never answered; kept for completeness.
        DenialReason::SniMismatch => ("403 Forbidden", "TLS server name not granted\n"),
    };
    reply(client, shared, status, body).await;
}

/// A complete response that closes the connection, so no client waits.
async fn reply(client: &mut UnixStream, shared: &Shared, status: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let send = async {
        client.write_all(text.as_bytes()).await?;
        client.shutdown().await
    };
    let _ = timeout(shared.config.head_timeout, send).await;
}

/// Resolve, apply the address rule, connect. `None` once answered.
async fn open(client: &mut UnixStream, shared: &Shared, target: &Target) -> Option<TcpStream> {
    let addr = address(client, shared, target).await?;
    let connect = shared.ports.connector.connect(addr);
    match timeout(shared.config.connect_timeout, connect).await {
        Ok(Ok(stream)) => Some(stream),
        _ => {
            bump(&shared.counters.connect_failures);
            reply(
                client,
                shared,
                "502 Bad Gateway",
                "upstream connect failed\n",
            )
            .await;
            None
        }
    }
}

/// The first permitted address with the granted port. An IP literal is
/// never resolved but passes the same rule (NET-3).
async fn address(client: &mut UnixStream, shared: &Shared, target: &Target) -> Option<SocketAddr> {
    let candidates = match target.literal() {
        Some(ip) => vec![ip],
        None => {
            let resolve = shared.ports.resolver.resolve(target.host());
            match timeout(shared.config.connect_timeout, resolve).await {
                Ok(Ok(addrs)) if !addrs.is_empty() => addrs,
                _ => {
                    bump(&shared.counters.resolve_failures);
                    reply(client, shared, "502 Bad Gateway", "name did not resolve\n").await;
                    return None;
                }
            }
        }
    };
    match first_permitted(&candidates, &shared.config.extra_blocked) {
        Some(ip) => Some(SocketAddr::new(ip, target.port())),
        None => {
            refuse(client, shared, target, DenialReason::BlockedAddress).await;
            None
        }
    }
}

/// The address to connect to: the first candidate whose own form and
/// canonical form (IPv4-mapped IPv6 unwrapped) both pass the rule and lie
/// in none of the operator's `extra` blocks; the canonical form is the one
/// connected.
fn first_permitted(candidates: &[IpAddr], extra: &[IpNet]) -> Option<IpAddr> {
    candidates.iter().find_map(|ip| {
        let canonical = ip.to_canonical();
        let refused = agentkit_kernel::is_blocked(*ip)
            || agentkit_kernel::is_blocked(canonical)
            || net::blocks(extra, *ip)
            || net::blocks(extra, canonical);
        (!refused).then_some(canonical)
    })
}

/// Answer `200` to a CONNECT and admit its ClientHello, or send the
/// rewritten head of a plain HTTP request upstream. Answers the bytes to
/// send upstream first and the most client bytes the tunnel forwards (the
/// request body), or `None` once the connection is done with.
async fn establish(
    client: &mut UnixStream,
    upstream: &mut TcpStream,
    request: Request,
    early: Vec<u8>,
    policy: &mut watch::Receiver<Policy>,
    shared: &Shared,
) -> Option<(Vec<u8>, Option<u64>)> {
    let bound = shared.config.head_timeout;
    match request {
        Request::Connect(target) => {
            let sent = timeout(bound, client.write_all(ESTABLISHED)).await;
            if !matches!(sent, Ok(Ok(()))) {
                return None;
            }
            let hello = screen_hello(client, &target, early, policy, shared).await?;
            Some((hello, None))
        }
        Request::Forward { head, body_len, .. } => {
            let sent = timeout(bound, upstream.write_all(&head)).await;
            matches!(sent, Ok(Ok(()))).then_some((early, Some(body_len)))
        }
    }
}

/// Read the client's first TLS record within the head timeout and admit
/// the tunnel only if it is a ClientHello naming the granted host
/// ([`sni::admits`]). Answers the bytes read, which go upstream first.
///
/// The `200` is already sent, so a refusal closes the connection without
/// a reply. Not TLS, too large, malformed, unnamed, misnamed, or nothing
/// whole in time: denial `sni_mismatch`. A revoke meanwhile: denial
/// `not_granted`, as before the `200`. A client that closes first is only
/// counted as a bad request.
async fn screen_hello(
    client: &mut UnixStream,
    target: &Target,
    early: Vec<u8>,
    policy: &mut watch::Receiver<Policy>,
    shared: &Shared,
) -> Option<Vec<u8>> {
    let read = timeout(shared.config.head_timeout, sni::read_record(client, early));
    let read = tokio::select! {
        read = read => read,
        () = relay::revoked(policy, target) => {
            shared.deny(target, DenialReason::NotGranted);
            return None;
        }
    };
    let peeked = match read {
        Ok(Ok(peeked)) => peeked,
        Ok(Err(PeekError::Closed)) => {
            bump(&shared.counters.bad_requests);
            return None;
        }
        Ok(Err(PeekError::NotTls)) | Err(_) => {
            shared.deny(target, DenialReason::SniMismatch);
            return None;
        }
    };
    let named = sni::server_name(peeked.record()).is_ok_and(|name| sni::admits(name, target));
    if !named {
        shared.deny(target, DenialReason::SniMismatch);
        return None;
    }
    // A revoke that landed as the read finished: nothing has gone upstream.
    if !policy.borrow_and_update().contains(target) {
        shared.deny(target, DenialReason::NotGranted);
        return None;
    }
    Some(peeked.bytes)
}

#[cfg(test)]
mod tests;
