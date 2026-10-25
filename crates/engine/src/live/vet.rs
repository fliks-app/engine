//! Address vetting of vetted upstreams: loopback, link-local, private and other internal
//! destinations are refused. Direct fetches resolve once and connect to what passed.

use hyper_util::client::legacy::connect::dns::Name;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::Service;

fn v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
}

fn v6(ip: Ipv6Addr) -> bool {
    if let Some(m) = ip.to_ipv4_mapped() {
        return v4(m);
    }
    let s = ip.segments();
    // `::a.b.c.d` and NAT64 `64:ff9b::/96` carry an IPv4 address in their last 32 bits.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..6] == [0; 6] {
        let [a, b, c, d] = [s[6] >> 8, s[6] & 0xff, s[7] >> 8, s[7] & 0xff].map(|x| x as u8);
        return v4(Ipv4Addr::new(a, b, c, d)) || s[0] == 0;
    }
    ip.is_multicast()
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] & 0xfe00) == 0xfc00
        || s[0] == 0x2002
        || (s[0] == 0x2001 && (s[1] == 0 || s[1] == 0x0db8))
}

/// Any address outside the public unicast space.
pub(super) fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => v6(a),
    }
}

/// The host of `scheme://[user@]host[:port]/...`, brackets removed.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let host = match auth.strip_prefix('[') {
        Some(r) => r.split(']').next()?,
        None => auth.rsplit_once(':').map_or(auth, |(h, _)| h),
    };
    (!host.is_empty()).then_some(host)
}

fn refused(host: &str, ip: IpAddr) -> String {
    format!("Refused: \"{host}\" is an internal address ({ip})")
}

/// Refuses an IP-literal host; a name is left to the connector's resolver.
pub(super) fn check_literal(url: &str) -> Result<(), String> {
    let host = host_of(url).ok_or("Refused: no host")?;
    match host.parse::<IpAddr>() {
        Ok(ip) if is_internal(ip) => Err(refused(host, ip)),
        _ => Ok(()),
    }
}

/// Resolves the host and refuses it when any address is internal (ffmpeg resolves again itself).
pub(super) async fn check(url: &str) -> Result<(), String> {
    check_literal(url)?;
    let host = host_of(url).ok_or("Refused: no host")?;
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|_| format!("Could not resolve \"{host}\""))?;
    for a in addrs {
        if is_internal(a.ip()) {
            return Err(refused(host, a.ip()));
        }
    }
    Ok(())
}

/// The connector's DNS: one lookup whose addresses are the ones dialed; `vetted` refuses a name
/// with any internal address.
#[derive(Clone, Copy)]
pub(super) struct Resolver {
    pub vetted: bool,
}

impl Service<Name> for Resolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let vetted = self.vetted;
        Box::pin(async move {
            let host = name.as_str();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
            if vetted && let Some(a) = addrs.iter().find(|a| is_internal(a.ip())) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    refused(host, a.ip()),
                ));
            }
            Ok(addrs.into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_ranges() {
        for ip in [
            "0.1.2.3",
            "10.1.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.5",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "224.0.0.1",
            "239.1.1.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "fec0::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "::7f00:1",
            "2002:7f00:1::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "192.0.1.1",
            "198.17.0.1",
            "198.20.0.1",
            "223.255.255.255",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("http://u:p@a.b:80/x?y"), Some("a.b"));
        assert_eq!(host_of("udp://@239.1.1.1:1234"), Some("239.1.1.1"));
        assert_eq!(host_of("http://[::1]:80/"), Some("::1"));
        assert_eq!(host_of("http://x@127.0.0.1#f@8.8.8.8"), Some("127.0.0.1"));
        assert!(check_literal("http://169.254.169.254/latest").is_err());
        assert!(check_literal("rtmp://[fe80::1]/live").is_err());
        assert!(check_literal("http://8.8.8.8/").is_ok());
    }

    #[tokio::test]
    async fn a_name_resolving_to_loopback_is_refused() {
        let name = || "localhost".parse::<Name>().unwrap();
        assert!(check("http://localhost:9/x").await.is_err());
        assert!(Resolver { vetted: true }.call(name()).await.is_err());
        assert!(Resolver { vetted: false }.call(name()).await.is_ok());
    }
}
