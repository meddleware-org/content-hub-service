//! Which hosts and addresses count as public. The admin API refuses URLs naming anything else (an edge check on
//! the host as written); the worker's HTTP client applies [`is_public_ip`] to every address a name resolves to,
//! and connects only to the addresses it checked, so a public name that resolves to a private address is refused
//! too (ARCHITECTURE.md §11, SSRF).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A host name that can only resolve inside a network: `localhost`, a single label, or a name under `.local`,
/// `.internal`, `.localhost`, `.lan`, `.home.arpa` or a cluster's `.svc.cluster.local`.
pub fn is_public_name(host: &str) -> bool {
    let d = host.trim_end_matches('.').to_ascii_lowercase();
    d.contains('.')
        && ![".local", ".internal", ".localhost", ".lan", ".home.arpa"].iter().any(|t| d.ends_with(t))
        && !d.ends_with(".svc.cluster.local")
}

/// Whether an address is globally routable: not private, loopback, link-local, shared (100.64/10), benchmarking
/// (198.18/15), documentation, multicast, broadcast, unspecified, reserved (240/4), unique-local (fc00::/7),
/// NAT64 (64:ff9b::/96) or an IPv4-mapped private address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_public_v4(a),
        IpAddr::V6(a) => is_public_v6(a),
    }
}

fn is_public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_private()
        || a.is_loopback()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_documentation()
        || a.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1]))
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240)
}

fn is_public_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = a.segments();
    !(a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x0064 && s[1] == 0xff9b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["example.org", "forum.example.org.", "mastodon.social"] {
            assert!(is_public_name(ok), "{ok}");
        }
        for bad in [
            "localhost",
            "intranet",
            "printer.local",
            "db.internal",
            "a.localhost",
            "postgres.database.svc.cluster.local",
        ] {
            assert!(!is_public_name(bad), "{bad}");
        }
    }

    #[test]
    fn addresses() {
        for ok in ["8.8.8.8", "1.1.1.1", "2606:4700::1", "162.159.135.232"] {
            assert!(is_public_ip(ok.parse().expect("ip")), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.20",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "240.0.0.1",
            "224.0.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "2001:db8::1",
            "64:ff9b::a00:1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(bad.parse().expect("ip")), "{bad}");
        }
    }
}
