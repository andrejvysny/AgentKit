//! Address blocks an operator adds to the public-address rule, such as the
//! host's own public addresses ([`crate::EgressConfig::extra_blocked`]).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An address block, `addr/prefix_len`. The address is kept with its host
/// bits cleared, so `10.1.2.3/8` and `10.0.0.0/8` are the same block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpNet {
    addr: IpAddr,
    prefix_len: u8,
}

impl IpNet {
    /// The block of `addr` with `prefix_len` leading bits; `None` when the
    /// prefix is longer than the address (32 bits for IPv4, 128 for IPv6).
    pub fn new(addr: IpAddr, prefix_len: u8) -> Option<Self> {
        let addr = match addr {
            IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from_bits(
                v4.to_bits() & u32::try_from(mask(prefix_len, 32)?).ok()?,
            )),
            IpAddr::V6(v6) => {
                IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & mask(prefix_len, 128)?))
            }
        };
        Some(Self { addr, prefix_len })
    }

    /// One address on its own (`/32` or `/128`).
    pub fn host(addr: IpAddr) -> Self {
        let prefix_len = if addr.is_ipv4() { 32 } else { 128 };
        Self { addr, prefix_len }
    }

    /// The first address of the block.
    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// Leading bits that every address of the block shares.
    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    /// True when `ip` is in the block. An IPv4 block never contains an
    /// IPv6 address or the reverse; the proxy also checks the IPv4 address
    /// an IPv6 one embeds (see [`crate::EgressConfig::extra_blocked`]).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = mask(self.prefix_len, 32).unwrap_or_default();
                u128::from(ip.to_bits()) & mask == u128::from(net.to_bits())
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = mask(self.prefix_len, 128).unwrap_or_default();
                ip.to_bits() & mask == net.to_bits()
            }
            _ => false,
        }
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix_len)
    }
}

/// The low `width` bits of a mask with `prefix_len` leading ones, or `None`
/// when the prefix is longer than `width`.
fn mask(prefix_len: u8, width: u32) -> Option<u128> {
    let prefix_len = u32::from(prefix_len);
    if prefix_len > width {
        return None;
    }
    let all = u128::MAX >> (128 - width);
    Some(all & !(all.checked_shr(prefix_len).unwrap_or(0)))
}

/// True when any block contains `ip`, or the IPv4 address `ip` carries
/// (IPv4-mapped, IPv4-compatible, SIIT, NAT64 `64:ff9b::/96` or 6to4), so
/// a blocked IPv4 address cannot be reached through an IPv6 spelling the
/// network translates back to it. The same embedded forms as
/// `agentkit_kernel::is_blocked`.
pub(crate) fn blocks(nets: &[IpNet], ip: IpAddr) -> bool {
    let embedded = match ip {
        IpAddr::V4(_) => None,
        IpAddr::V6(v6) => embedded_v4(v6).map(IpAddr::V4),
    };
    nets.iter()
        .any(|net| net.contains(ip) || embedded.is_some_and(|v4| net.contains(v4)))
}

fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let tail = |at: usize| {
        let [a, b] = s[at].to_be_bytes();
        let [c, d] = s[at + 1].to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    let zero = |range: std::ops::Range<usize>| s[range].iter().all(|&x| x == 0);
    let mapped_or_compatible = zero(0..5) && (s[5] == 0xFFFF || s[5] == 0);
    let siit = zero(0..4) && s[4] == 0xFFFF && s[5] == 0;
    let nat64 = s[0] == 0x0064 && s[1] == 0xFF9B && zero(2..6);
    if mapped_or_compatible || siit || nat64 {
        return Some(tail(6));
    }
    (s[0] == 0x2002).then(|| tail(1))
}

#[cfg(test)]
mod tests;
