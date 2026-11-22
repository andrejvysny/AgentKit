//! SSRF guard: blocked address ranges, URL pre-checks and a DNS resolver that
//! hands the connector exactly the addresses it checked.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use super::{FetchConfig, FetchError};

const URL_MAX_BYTES: usize = 2048;

/// True when `ip` is in a private, local, reserved or otherwise non-public range.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => v6_blocked(v6),
    }
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0
        || a == 10
        || (a == 100 && b & 0xC0 == 64)
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && b & 0xF0 == 16)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && b & 0xFE == 18)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // 224/4 multicast and 240/4 reserved (incl. broadcast).
        || a >= 224
}

fn embedded_v4(s: [u16; 8], at: usize) -> Ipv4Addr {
    let [a, b] = s[at].to_be_bytes();
    let [c, d] = s[at + 1].to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

fn v6_blocked(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if ip.is_unspecified() || ip.is_loopback() {
        return true;
    }
    if s[0] & 0xFE00 == 0xFC00 || s[0] & 0xFFC0 == 0xFE80 || s[0] & 0xFF00 == 0xFF00 {
        return true;
    }
    // Documentation 2001:db8::/32, Teredo 2001::/32, site-local fec0::/10,
    // local-use NAT64 64:ff9b:1::/48.
    if (s[0] == 0x2001 && (s[1] == 0x0DB8 || s[1] == 0))
        || s[0] & 0xFFC0 == 0xFEC0
        || (s[0] == 0x0064 && s[1] == 0xFF9B && s[2] == 1)
    {
        return true;
    }
    // SIIT ::ffff:0:a.b.c.d.
    if s[..4].iter().all(|&x| x == 0) && s[4] == 0xFFFF && s[5] == 0 {
        return v4_blocked(embedded_v4(s, 6));
    }
    let zero_prefix = s[..5].iter().all(|&x| x == 0);
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d).
    if zero_prefix && (s[5] == 0xFFFF || s[5] == 0) {
        return v4_blocked(embedded_v4(s, 6));
    }
    // NAT64 well-known prefix 64:ff9b::/96.
    if s[0] == 0x0064 && s[1] == 0xFF9B && s[2..6].iter().all(|&x| x == 0) {
        return v4_blocked(embedded_v4(s, 6));
    }
    // 6to4 2002::/16 embeds the v4 address right after the prefix.
    if s[0] == 0x2002 {
        return v4_blocked(embedded_v4(s, 1));
    }
    false
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
