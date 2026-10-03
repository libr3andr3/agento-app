//! Which IP is talking to us. Only a reverse proxy on the same host is
//! allowed to say "this came from elsewhere": a forwarded header from an
//! arbitrary peer would let anyone pick the address their rate limits are
//! keyed on.

use http::HeaderMap;
use std::net::{IpAddr, SocketAddr};

/// The client IP for rate limiting. `X-Forwarded-For`'s first hop is used
/// only when the direct peer is loopback (i.e. our own proxy); otherwise
/// the peer address itself.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.parse::<IpAddr>().is_ok());
    match (peer, forwarded) {
        (Some(p), Some(f)) if p.ip().is_loopback() => f.to_string(),
        (Some(p), _) => p.ip().to_string(),
        (None, Some(f)) => f.to_string(),
        (None, None) => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(xff: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("x-forwarded-for", xff.parse().unwrap());
        m
    }

    #[test]
    fn trusts_forwarded_only_from_loopback() {
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let remote: SocketAddr = "203.0.113.9:1".parse().unwrap();
        assert_eq!(client_ip(&h("198.51.100.4, 10.0.0.1"), Some(lo)), "198.51.100.4");
        assert_eq!(client_ip(&h("198.51.100.4"), Some(remote)), "203.0.113.9");
        assert_eq!(client_ip(&h("not-an-ip"), Some(lo)), "127.0.0.1");
        assert_eq!(client_ip(&HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn loopback_v6_proxy_is_trusted() {
        let lo6: SocketAddr = "[::1]:1".parse().unwrap();
        assert_eq!(client_ip(&h("2001:db8::7"), Some(lo6)), "2001:db8::7");
    }

    #[test]
    fn forwarded_without_peer_is_used() {
        assert_eq!(client_ip(&h("198.51.100.4"), None), "198.51.100.4");
        assert_eq!(client_ip(&h("junk"), None), "unknown");
    }

    #[test]
    fn empty_and_whitespace_first_hop_fall_back_to_peer() {
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(client_ip(&h(" , 198.51.100.4"), Some(lo)), "127.0.0.1");
        assert_eq!(client_ip(&h("  198.51.100.4  ,x"), Some(lo)), "198.51.100.4");
    }

    #[test]
    fn remote_peer_without_header() {
        let remote: SocketAddr = "203.0.113.9:443".parse().unwrap();
        assert_eq!(client_ip(&HeaderMap::new(), Some(remote)), "203.0.113.9");
    }

    #[test]
    fn non_utf8_header_is_ignored() {
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut m = HeaderMap::new();
        m.insert("x-forwarded-for", http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        assert_eq!(client_ip(&m, Some(lo)), "127.0.0.1");
    }
}
