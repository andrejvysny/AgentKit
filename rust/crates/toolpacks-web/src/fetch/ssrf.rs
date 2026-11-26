//! SSRF guard: blocked address ranges, URL pre-checks and a DNS resolver that
//! hands the connector exactly the addresses it checked.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use super::{FetchConfig, FetchError};

const URL_MAX_BYTES: usize = 2048;

/// True when `ip` is in a private, local, reserved or otherwise non-public range.
pub fn is_blocked(ip: IpAddr) -> bool {
    agentkit_kernel::is_blocked(ip)
}

/// Pre-flight checks that need no network: scheme, userinfo, port, length and
/// IP-literal hosts (which never reach the DNS resolver).
pub fn check_url(url: &Url, cfg: &FetchConfig) -> Result<(), FetchError> {
    if url.as_str().len() > URL_MAX_BYTES {
        return Err(FetchError::InvalidUrl(format!(
            "URL is longer than {URL_MAX_BYTES} characters."
        )));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(FetchError::InvalidUrl(
            "only http and https URLs are supported.".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::InvalidUrl(
            "URLs with credentials are not allowed.".into(),
        ));
    }
    let port = url.port_or_known_default().unwrap_or(0);
    if !cfg.allowed_ports.contains(&port) {
        return Err(FetchError::InvalidUrl(format!(
            "port {port} is not allowed."
        )));
    }
    let ip = match url.host() {
        Some(Host::Ipv4(v4)) => IpAddr::V4(v4),
        Some(Host::Ipv6(v6)) => IpAddr::V6(v6),
        Some(Host::Domain(_)) => return Ok(()),
        None => return Err(FetchError::InvalidUrl("URL has no host.".into())),
    };
    if is_blocked(ip) && !cfg.extra_allowed_ips.contains(&ip) {
        return Err(FetchError::Blocked(ip.to_string()));
    }
    Ok(())
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub trait HostResolver: Send + Sync + 'static {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<io::Result<Vec<SocketAddr>>>;
}

/// Resolves through the operating system.
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
        let host = host.to_owned();
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
            Ok(addrs.collect())
        })
    }
}

/// Marker error: the client maps it to `FetchError::Blocked`.
#[derive(Debug)]
pub(super) struct BlockedHost(pub String);

impl std::fmt::Display for BlockedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host {} resolves to a blocked address", self.0)
    }
}

impl std::error::Error for BlockedHost {}

/// reqwest resolver that drops the whole answer when any address is blocked,
/// so the connection uses exactly the checked set (no DNS-rebinding window).
pub struct GuardedResolver {
    inner: Arc<dyn HostResolver>,
    extra_allowed: Vec<IpAddr>,
}

impl GuardedResolver {
    pub fn new(inner: Arc<dyn HostResolver>, extra_allowed: Vec<IpAddr>) -> Self {
        Self {
            inner,
            extra_allowed,
        }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let inner = self.inner.clone();
        let extra = self.extra_allowed.clone();
        Box::pin(async move {
            let addrs = inner.resolve(&host, 0).await?;
            if addrs.is_empty() {
                let err = io::Error::new(io::ErrorKind::NotFound, "no addresses for host");
                return Err(err.into());
            }
            if addrs
                .iter()
                .any(|a| is_blocked(a.ip()) && !extra.contains(&a.ip()))
            {
                return Err(Box::new(BlockedHost(host)) as _);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}
