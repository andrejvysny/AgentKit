//! What the proxy needs from its host: name resolution, outbound TCP and a
//! clock. Injected so tests can stand in for the network; the address rule
//! (`agentkit_kernel::is_blocked`) is applied by the proxy between the two
//! and cannot be replaced.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use agentkit_kernel::{Runtime, SystemRuntime};
use async_trait::async_trait;
use tokio::net::TcpStream;

/// Resolves a host name on the host side (NET-3). Never asked for an IP
/// literal. The answer is filtered by the proxy before any address is used;
/// the port always comes from the grant.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Every address of `host`, in preference order. `host` is a fully
    /// qualified name written without its trailing dot (a normalised
    /// [`crate::Target`] host): resolve it as is, never relative to a
    /// search domain, or a grant could reach a name nobody granted.
    async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// Resolution through the operating system (`getaddrinfo`), always of the
/// absolute name (`host.`), so `resolv.conf` search domains never apply.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let name = absolute(host);
        let addrs = tokio::net::lookup_host((name.as_str(), 0)).await?;
        Ok(addrs.map(|addr| addr.ip()).collect())
    }
}

/// `host` with exactly one trailing dot: the absolute form of the name.
fn absolute(host: &str) -> String {
    format!("{}.", host.strip_suffix('.').unwrap_or(host))
}

/// Opens the upstream TCP connection. Only ever called with an address
/// that passed the public-address rule and the granted port.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Connect to `addr`.
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream>;
}

/// Plain outbound TCP.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpConnector;

#[async_trait]
impl Connector for TcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect(addr).await
    }
}

/// The host services one proxy uses.
#[derive(Clone)]
pub struct EgressPorts {
    /// Name resolution.
    pub resolver: Arc<dyn Resolver>,
    /// Outbound TCP.
    pub connector: Arc<dyn Connector>,
    /// Clock for denial timestamps.
    pub runtime: Arc<dyn Runtime>,
}

impl EgressPorts {
    /// The operating system's resolver, plain TCP and the system clock.
    pub fn system() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
            connector: Arc::new(TcpConnector),
            runtime: Arc::new(SystemRuntime),
        }
    }
}

impl std::fmt::Debug for EgressPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressPorts").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
