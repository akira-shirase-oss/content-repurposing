//! The SSRF policy: which URLs may be fetched and which addresses may be dialled.
//!
//! A URL passes [`check_url`] before anything is resolved; every address a name
//! resolves to passes [`check_ip`] before it is dialled, and the dialler connects to
//! that checked address, never to the name again (no second lookup to rebind). A
//! redirect target is a new URL and passes both checks again.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

use crate::error::Refusal;

/// The longest URL accepted, in bytes.
pub const MAX_URL_BYTES: usize = 2048;

/// What the fetcher is allowed to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Plain `http` is allowed (many blogs still serve it); `https` always is.
    pub allow_http: bool,
    /// Test fixtures only: loopback addresses and any port. Unreachable without the
    /// `test-support` feature.
    loopback_fixture: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow_http: true,
            loopback_fixture: false,
        }
    }
}

impl Policy {
    /// A policy that may also reach `127.0.0.1` on any port — fixture servers only.
    #[cfg(feature = "test-support")]
    pub fn loopback_fixture() -> Self {
        Self {
            allow_http: true,
            loopback_fixture: true,
        }
    }

    /// Whether `ip` may be dialled.
    pub fn allows_ip(self, ip: IpAddr) -> bool {
        if self.loopback_fixture && ip == IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return true;
        }
        check_ip(ip).is_ok()
    }
}

/// Parses and checks a URL a person pasted or a server redirected to.
pub fn check_url(raw: &str, policy: Policy) -> Result<Url, Refusal> {
    if raw.len() > MAX_URL_BYTES {
        return Err(Refusal::UrlTooLong);
    }
    let url = Url::parse(raw.trim()).map_err(|_| Refusal::BadUrl)?;
    if url.as_str().len() > MAX_URL_BYTES {
        return Err(Refusal::UrlTooLong);
    }
    check_parsed(&url, policy)?;
    Ok(url)
}

/// Checks an already-parsed URL (a redirect target joined onto its base).
pub fn check_parsed(url: &Url, policy: Policy) -> Result<(), Refusal> {
    match url.scheme() {
        "https" => {}
        "http" if policy.allow_http => {}
        _ => return Err(Refusal::Scheme),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::Credentials);
    }
    let port = url.port_or_known_default().ok_or(Refusal::BadUrl)?;
    if !policy.loopback_fixture && port != 80 && port != 443 {
        return Err(Refusal::Port);
    }
    match url.host() {
        None => Err(Refusal::BadUrl),
        Some(Host::Ipv4(v4)) => allow(IpAddr::V4(v4), policy),
        Some(Host::Ipv6(v6)) => allow(IpAddr::V6(v6), policy),
        Some(Host::Domain(name)) => check_name(name),
    }
}

fn allow(ip: IpAddr, policy: Policy) -> Result<(), Refusal> {
    if policy.allows_ip(ip) {
        Ok(())
    } else {
        check_ip(ip)
    }
}

/// Names that never leave the machine or the cluster, refused before any lookup.
fn check_name(name: &str) -> Result<(), Refusal> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let local = name == "localhost"
        || !name.contains('.')
        || [
            ".localhost",
            ".local",
            ".internal",
            ".localdomain",
            ".home.arpa",
            ".arpa",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix));
    if local {
        Err(Refusal::LocalName)
    } else {
        Ok(())
    }
}

/// IPv4 blocks never dialled: the IANA special-purpose registry plus multicast and
/// reserved space. `169.254.0.0/16` holds the cloud metadata endpoints.
const DENY_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::UNSPECIFIED, 8),           // "this network"
    (Ipv4Addr::new(10, 0, 0, 0), 8),      // private
    (Ipv4Addr::new(100, 64, 0, 0), 10),   // shared address space (CGNAT)
    (Ipv4Addr::new(127, 0, 0, 0), 8),     // loopback
    (Ipv4Addr::new(169, 254, 0, 0), 16),  // link-local, cloud metadata
    (Ipv4Addr::new(172, 16, 0, 0), 12),   // private
    (Ipv4Addr::new(192, 0, 0, 0), 24),    // IETF protocol assignments
    (Ipv4Addr::new(192, 0, 2, 0), 24),    // TEST-NET-1
    (Ipv4Addr::new(192, 88, 99, 0), 24),  // 6to4 relay anycast
    (Ipv4Addr::new(192, 168, 0, 0), 16),  // private
    (Ipv4Addr::new(198, 18, 0, 0), 15),   // benchmarking
    (Ipv4Addr::new(198, 51, 100, 0), 24), // TEST-NET-2
    (Ipv4Addr::new(203, 0, 113, 0), 24),  // TEST-NET-3
    (Ipv4Addr::new(224, 0, 0, 0), 4),     // multicast
    (Ipv4Addr::new(240, 0, 0, 0), 4),     // reserved, broadcast
];

/// IPv6 blocks never dialled. Every form that carries an IPv4 address (mapped,
/// compatible, NAT64, 6to4, Teredo) is refused whole: a gateway turns it into an IPv4
/// connection the v4 table never saw.
const DENY_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::UNSPECIFIED, 96), // unspecified, loopback, IPv4-compatible
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96), // IPv4-mapped
    (Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, 0, 0), 96), // IPv4-translated
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96), // NAT64 well-known
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48), // NAT64 local-use
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64), // discard-only
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23), // IETF assignments, Teredo
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32), // documentation
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16), // 6to4
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20), // documentation
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16), // SRv6 SIDs
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7), // unique-local (incl. fd00:ec2::254)
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10), // link-local
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10), // site-local
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8), // multicast
];

fn in_v4(ip: Ipv4Addr, (net, len): (Ipv4Addr, u8)) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(net) & mask
}

fn in_v6(ip: Ipv6Addr, (net, len): (Ipv6Addr, u8)) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
    u128::from(ip) & mask == u128::from(net) & mask
}

/// Whether an address may be dialled under the production policy.
pub fn check_ip(ip: IpAddr) -> Result<(), Refusal> {
    let denied = match ip {
        IpAddr::V4(v4) => DENY_V4.iter().any(|&block| in_v4(v4, block)),
        IpAddr::V6(v6) => DENY_V6.iter().any(|&block| in_v6(v6, block)),
    };
    if denied {
        Err(Refusal::Address)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }

    #[test]
    fn refuses_every_internal_range() {
        for text in [
            "127.0.0.1",
            "127.255.255.254",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.170.2",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:8.8.8.8",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001::1",
            "ff02::1",
        ] {
            assert_eq!(check_ip(ip(text)), Err(Refusal::Address), "{text}");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for text in ["8.8.8.8", "1.1.1.1", "93.184.215.14", "2606:4700::1111"] {
            assert_eq!(check_ip(ip(text)), Ok(()), "{text}");
        }
    }

    #[test]
    fn refuses_schemes_ports_credentials_and_local_names() {
        let p = Policy::default();
        let cases = [
            ("file:///etc/passwd", Refusal::Scheme),
            ("gopher://example.com/", Refusal::Scheme),
            ("ftp://example.com/", Refusal::Scheme),
            ("javascript:alert(1)", Refusal::Scheme),
            ("https://user:pw@example.com/", Refusal::Credentials),
            ("https://example.com:6379/", Refusal::Port),
            ("http://localhost/", Refusal::LocalName),
            ("http://metadata.google.internal/", Refusal::LocalName),
            ("http://intranet/", Refusal::LocalName),
            ("http://169.254.169.254/latest/meta-data/", Refusal::Address),
            ("http://[::ffff:a9fe:a9fe]/", Refusal::Address),
            ("http://2130706433/", Refusal::Address),
            ("http://0x7f.1/", Refusal::Address),
            ("not a url", Refusal::BadUrl),
        ];
        for (raw, want) in cases {
            assert_eq!(check_url(raw, p).map(|_| ()), Err(want), "{raw}");
        }
        assert_eq!(
            check_url(&format!("https://example.com/{}", "a".repeat(3000)), p).map(|_| ()),
            Err(Refusal::UrlTooLong)
        );
        // Short as typed, too long once percent-encoded: the stored form is what counts.
        let encoded = format!("https://example.com/{}", "é".repeat(800));
        assert!(encoded.len() <= MAX_URL_BYTES);
        assert_eq!(check_url(&encoded, p).map(|_| ()), Err(Refusal::UrlTooLong));
        let fits = format!("https://example.com/{}", "é".repeat(300));
        assert!(check_url(&fits, p).is_ok_and(|u| u.as_str().len() <= MAX_URL_BYTES));
        assert!(check_url("https://example.com/a?b=c", p).is_ok());
        assert!(check_url("http://example.com/", p).is_ok());
        let strict = Policy {
            allow_http: false,
            ..Policy::default()
        };
        assert_eq!(
            check_url("http://example.com/", strict).map(|_| ()),
            Err(Refusal::Scheme)
        );
    }

    #[test]
    fn the_fixture_policy_only_adds_ipv4_loopback() {
        let p = Policy::loopback_fixture();
        assert!(p.allows_ip(ip("127.0.0.1")));
        assert!(!p.allows_ip(ip("127.0.0.2")));
        assert!(!p.allows_ip(ip("::1")));
        assert!(!p.allows_ip(ip("169.254.169.254")));
        assert!(check_url("http://127.0.0.1:63499/", p).is_ok());
    }
}
