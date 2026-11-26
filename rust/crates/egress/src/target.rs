//! Host and port pairs: the unit of a grant (NET-2) and of a request. Grants
//! and requests go through the same normalisation, so a policy match is an
//! exact string comparison and a name grant never covers an address literal.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Longest accepted host name (DNS limit).
const HOST_MAX_BYTES: usize = 253;
const LABEL_MAX_BYTES: usize = 63;

/// One host and one TCP port. The host is a lowercase DNS name without a
/// trailing dot, an IPv4 literal, or a bracketed IPv6 literal in canonical
/// form (`[2001:db8::1]`). Wildcards are refused (NET-2). Spelling only:
/// addresses are checked after resolution (NET-3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    host: String,
    port: u16,
}

/// Why a host and port is not a valid [`Target`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    /// The host is empty.
    #[error("host is empty")]
    EmptyHost,
    /// The host is longer than 253 bytes.
    #[error("host is longer than 253 bytes")]
    HostTooLong,
    /// The host contains `*`.
    #[error("wildcard hosts are not allowed")]
    Wildcard,
    /// The host has a character outside `a-z`, `0-9`, `-` and `.`.
    #[error("host contains `{0}`, which is not allowed in a host name")]
    InvalidCharacter(char),
    /// A label is empty, longer than 63 bytes, or starts or ends with `-`.
    #[error("host label `{0}` is not a valid DNS label")]
    InvalidLabel(String),
    /// A bracketed host is not an IPv6 address (zone ids are refused).
    #[error("bracketed host is not an IPv6 address")]
    InvalidAddress,
    /// Port 0.
    #[error("port must be between 1 and 65535")]
    PortZero,
}

impl Target {
    /// Validate and normalise: ASCII lowercase, one trailing dot removed,
    /// IPv6 literals (`[..]`) parsed and re-printed.
    pub fn new(host: &str, port: u16) -> Result<Self, TargetError> {
        if port == 0 {
            return Err(TargetError::PortZero);
        }
        Ok(Self {
            host: normalize_host(host)?,
            port,
        })
    }

    /// The normalised host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The address when the host is an IP literal; such a host is never
    /// resolved.
    pub(crate) fn literal(&self) -> Option<IpAddr> {
        if let Some(inner) = self.host.strip_prefix('[') {
            let inner = inner.strip_suffix(']')?;
            return inner.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
        }
        self.host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// The one host normalisation for grants and requests.
fn normalize_host(raw: &str) -> Result<String, TargetError> {
    if let Some(inner) = raw.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or(TargetError::InvalidAddress)?;
        let addr: Ipv6Addr = inner.parse().map_err(|_| TargetError::InvalidAddress)?;
        return Ok(format!("[{addr}]"));
    }
    let host = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    check_name(&host)?;
    Ok(host)
}

fn check_name(host: &str) -> Result<(), TargetError> {
    if host.is_empty() {
        return Err(TargetError::EmptyHost);
    }
    if host.len() > HOST_MAX_BYTES {
        return Err(TargetError::HostTooLong);
    }
    if host.contains('*') {
        return Err(TargetError::Wildcard);
    }
    let allowed =
        |ch: &char| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '.');
    if let Some(bad) = host.chars().find(|ch| !allowed(ch)) {
        return Err(TargetError::InvalidCharacter(bad));
    }
    for label in host.split('.') {
        let bad = label.is_empty()
            || label.len() > LABEL_MAX_BYTES
            || label.starts_with('-')
            || label.ends_with('-');
        if bad {
            return Err(TargetError::InvalidLabel(label.to_owned()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
