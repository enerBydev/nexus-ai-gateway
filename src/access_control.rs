//! IP allowlist access control (Issue #78, Solution B).
//!
//! Defense-in-depth layer that restricts which client IPs may reach the proxy,
//! independent of the bind address. This complements the secure-by-default bind
//! (Solution A): even when an operator opts into `BIND_ADDR=0.0.0.0` for LAN
//! access, they can scope *who* may connect with `ALLOWED_IPS`.
//!
//! Opt-in: when `ALLOWED_IPS` is empty/unset the middleware is not mounted and
//! every client is allowed (the bind address governs exposure). When set, only
//! listed networks — plus loopback, always — may reach the proxy; everyone else
//! receives `403 Forbidden`.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};

/// Parsed allowlist of permitted client networks (Issue #78).
#[derive(Debug, Clone, Default)]
pub struct IpAllowlist {
    nets: Vec<IpNet>,
}

impl IpAllowlist {
    /// Parse a comma-separated list of CIDRs and/or bare IPs.
    ///
    /// - `192.168.1.0/24` -> network match.
    /// - `203.0.113.7` (bare IP) -> host route (`/32` for IPv4, `/128` for IPv6).
    /// - Invalid entries are skipped with a warning (parse, don't validate).
    pub fn parse(raw: &str) -> Self {
        let mut nets = Vec::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Ok(net) = entry.parse::<IpNet>() {
                nets.push(net);
            } else if let Ok(ip) = entry.parse::<IpAddr>() {
                // Bare IP -> host network with full prefix length.
                nets.push(IpNet::from(ip));
            } else {
                tracing::warn!("ALLOWED_IPS: ignoring invalid entry '{}'", entry);
            }
        }
        Self { nets }
    }

    /// True when no networks are configured (no restriction).
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    /// Number of configured networks (for startup logging).
    pub fn len(&self) -> usize {
        self.nets.len()
    }

    /// Whether a client IP is permitted.
    ///
    /// Loopback (`127.0.0.0/8`, `::1`) is **always** allowed regardless of the
    /// configured list. This guarantees local health checks, Prometheus scrapes,
    /// and the Claude Code client (which connects via `localhost`) keep working
    /// even if the operator forgets to add loopback to `ALLOWED_IPS`.
    pub fn allows(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() {
            return true;
        }
        self.nets.iter().any(|net| net.contains(&ip))
    }
}

/// Axum middleware enforcing the [`IpAllowlist`] against the peer socket address.
///
/// The client IP is taken from `ConnectInfo<SocketAddr>`, which is populated by
/// `into_make_service_with_connect_info::<SocketAddr>()` in `main`. Requests from
/// non-allowed IPs are rejected with `403 Forbidden` before reaching any handler.
pub async fn ip_allowlist_middleware(
    State(allowlist): State<IpAllowlist>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if allowlist.allows(addr.ip()) {
        Ok(next.run(request).await)
    } else {
        tracing::warn!(
            "[access_control] Rejected {} — not in ALLOWED_IPS (loopback always permitted)",
            addr.ip()
        );
        Err(StatusCode::FORBIDDEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP must parse")
    }

    #[test]
    fn empty_input_yields_empty_allowlist() {
        assert!(IpAllowlist::parse("").is_empty());
        assert!(IpAllowlist::parse("   ").is_empty());
        assert!(IpAllowlist::parse(" , , ").is_empty());
    }

    #[test]
    fn loopback_always_allowed_even_when_not_listed() {
        let al = IpAllowlist::parse("192.168.1.0/24");
        assert!(al.allows(ip("127.0.0.1")));
        assert!(al.allows(ip("127.0.0.5")));
        assert!(al.allows(ip("::1")));
    }

    #[test]
    fn loopback_allowed_on_empty_list_but_others_denied() {
        // allows() is the predicate; the middleware is only mounted when non-empty.
        let al = IpAllowlist::parse("");
        assert!(al.allows(ip("127.0.0.1")));
        assert!(!al.allows(ip("8.8.8.8")));
    }

    #[test]
    fn cidr_membership() {
        let al = IpAllowlist::parse("192.168.1.0/24");
        assert!(al.allows(ip("192.168.1.1")));
        assert!(al.allows(ip("192.168.1.254")));
        assert!(!al.allows(ip("192.168.2.1")));
        assert!(!al.allows(ip("8.8.8.8")));
    }

    #[test]
    fn bare_ipv4_becomes_host_route() {
        let al = IpAllowlist::parse("203.0.113.7");
        assert!(al.allows(ip("203.0.113.7")));
        assert!(!al.allows(ip("203.0.113.8")));
    }

    #[test]
    fn multiple_entries_with_ipv6_and_whitespace() {
        let al = IpAllowlist::parse(" 10.0.0.0/8 , 2001:db8::/32 ,192.168.1.1 ");
        assert!(al.allows(ip("10.255.1.2")));
        assert!(al.allows(ip("2001:db8::dead:beef")));
        assert!(al.allows(ip("192.168.1.1")));
        assert!(!al.allows(ip("172.16.0.1")));
        assert!(!al.allows(ip("2001:dead::1")));
        assert_eq!(al.len(), 3);
    }

    #[test]
    fn invalid_entries_are_skipped_not_fatal() {
        let al = IpAllowlist::parse("garbage, 192.168.1.0/24, 999.999.999.999, also-bad");
        assert_eq!(al.len(), 1);
        assert!(al.allows(ip("192.168.1.5")));
        assert!(!al.allows(ip("1.2.3.4")));
    }
}

#[cfg(test)]
mod http_tests {
    //! End-to-end middleware tests: drive a real router through the allowlist
    //! layer with a synthetic `ConnectInfo`, asserting 200 vs 403 without a socket.
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use tower::ServiceExt; // ServiceExt::oneshot

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn test_app(allowed: &str) -> Router {
        let allowlist = IpAllowlist::parse(allowed);
        Router::new()
            .route("/", get(ok_handler))
            .layer(axum::middleware::from_fn_with_state(allowlist, ip_allowlist_middleware))
    }

    async fn status_from(app: Router, client_ip: &str) -> StatusCode {
        let addr: SocketAddr = format!("{client_ip}:12345").parse().expect("valid socket addr");
        let mut req = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
        // Replicates what into_make_service_with_connect_info inserts at runtime.
        req.extensions_mut().insert(ConnectInfo(addr));
        app.oneshot(req).await.expect("router responds").status()
    }

    #[tokio::test]
    async fn allowed_ip_passes() {
        assert_eq!(status_from(test_app("192.168.1.0/24"), "192.168.1.50").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn disallowed_ip_is_forbidden() {
        assert_eq!(status_from(test_app("192.168.1.0/24"), "8.8.8.8").await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn loopback_passes_even_when_not_listed() {
        assert_eq!(status_from(test_app("192.168.1.0/24"), "127.0.0.1").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_allowlist_mounted_fails_closed() {
        // CodeRabbit #109: when ALLOWED_IPS is set but parses to no valid entries, main
        // mounts the middleware anyway (fail closed) -> only loopback may pass.
        assert_eq!(status_from(test_app(""), "8.8.8.8").await, StatusCode::FORBIDDEN);
        assert_eq!(status_from(test_app(""), "127.0.0.1").await, StatusCode::OK);
    }
}
