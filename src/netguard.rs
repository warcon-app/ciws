//! `block_private`: for a relay whose targets other people choose (the hosted service), no target
//! may reach a private, loopback, link-local or otherwise internal address. Literal addresses are
//! refused when the configuration is read; names are checked every time they are resolved, so a
//! name that later points somewhere internal (DNS rebinding) is refused at connect time.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_public_v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(a),
        },
    }
}

fn is_public_v4(a: Ipv4Addr) -> bool {
    let [b0, b1, ..] = a.octets();
    !(a.is_private()
        || a.is_loopback()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_documentation()
        || a.is_unspecified()
        || a.is_multicast()
        || b0 == 0
        || (b0 == 100 && (64..128).contains(&b1)) // shared address space (CGNAT)
        || (b0 == 192 && b1 == 0 && a.octets()[2] == 0) // IETF protocol assignments
        || (b0 == 198 && (18..20).contains(&b1)) // benchmarking
        || b0 >= 240) // reserved
}

fn is_public_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    !(a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        || a.is_unique_local()
        || a.is_unicast_link_local()
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64, which reaches IPv4 space unchecked
        || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)) // discard-only
}

/// Whether a target URL's host is acceptable before any lookup: a literal address must be public
/// and `localhost` is refused outright. Names are left to [`PublicOnly`].
pub fn check_literal_host(url: &Url) -> Result<(), &'static str> {
    match url.host() {
        Some(Host::Ipv4(a)) if !is_public(IpAddr::V4(a)) => Err("is a private or internal address"),
        Some(Host::Ipv6(a)) if !is_public(IpAddr::V6(a)) => Err("is a private or internal address"),
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost" || d.ends_with(".localhost") {
                Err("is localhost")
            } else {
                Ok(())
            }
        }
        Some(_) => Ok(()),
        None => Err("has no host"),
    }
}

/// A resolver that drops every internal address from an answer and fails when none is left.
pub struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(
                    "the target's name resolves only to private or internal addresses".into(),
                );
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(s: &str) -> bool {
        is_public(s.parse().unwrap())
    }

    #[test]
    fn internal_addresses_are_refused() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
        ] {
            assert!(!public(a), "{a} should be internal");
        }
    }

    #[test]
    fn public_addresses_pass() {
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "104.16.0.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(public(a), "{a} should be public");
        }
    }

    #[test]
    fn literal_hosts() {
        let ok = |u: &str| check_literal_host(&Url::parse(u).unwrap()).is_ok();
        assert!(ok("https://console.warcon.app"));
        assert!(ok("https://1.1.1.1/x"));
        assert!(!ok("http://127.0.0.1:3000"));
        assert!(!ok("http://[::1]/"));
        assert!(!ok("http://localhost:3000"));
        assert!(!ok("http://api.localhost/"));
        assert!(!ok("http://169.254.169.254/latest/meta-data"));
    }
}
