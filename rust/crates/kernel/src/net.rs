//! Pure public-address rules shared by sandbox and web egress paths.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True when the address is private, local, reserved or otherwise non-public:
/// the rule every sandbox and web egress path uses (spec NET-3).
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
