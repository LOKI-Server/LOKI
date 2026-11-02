//! Outgoing HTTP(S) requests from apps with the `outbound` capability, to the hosts
//! their manifest lists. Everything else is refused, as for apps without it.
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Semaphore;
use wasmtime_wasi_http::p2::{
    HttpResult, WasiHttpHooks,
    bindings::http::types::ErrorCode,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, OutgoingRequestConfig},
};

/// Requests one app may have waiting for a response at once.
pub(super) const IN_FLIGHT: usize = 8;
const CONNECT_TIME: Duration = Duration::from_secs(10);
const RESPONSE_TIME: Duration = Duration::from_secs(30);

/// Which requests an app may send. `None` refuses them all.
#[derive(Clone, Default)]
pub(crate) struct Outbound(Option<Arc<Policy>>);
pub(crate) struct Policy {
    pub hosts: Vec<String>,
    pub in_flight: Arc<Semaphore>,
}
impl Outbound {
    pub fn new(policy: Option<Arc<Policy>>) -> Self {
        Self(policy)
    }
}
impl Policy {
    /// Whether `host:port` matches an entry: `host` exactly or `*.host` for its
    /// subdomains, on the entry's port or, without one, 80 and 443.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.hosts.iter().any(|entry| {
            let (pattern, wanted) = match entry.rsplit_once(':') {
                Some((pattern, port)) => (pattern, port.parse::<u16>().ok()),
                None => (entry.as_str(), None),
            };
            let port_ok = wanted.map_or(port == 80 || port == 443, |p| p == port);
            let host_ok = match pattern.strip_prefix("*.") {
                Some(domain) => host
                    .strip_suffix(domain)
                    .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
                None => host == pattern,
            };
            port_ok && host_ok
        })
    }
    /// Whether the manifest names this IP address itself, which lets an app reach a
    /// local address on purpose.
    fn names(&self, ip: IpAddr) -> bool {
        self.hosts.iter().any(|entry| {
            let host = entry.rsplit_once(':').map_or(entry.as_str(), |(h, _)| h);
            host.parse::<IpAddr>().is_ok_and(|listed| listed == ip)
        })
    }
}
/// Whether `ip` is on the public internet, not this machine or a private network.
pub(crate) fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

impl WasiHttpHooks for Outbound {
    fn send_request(
        &mut self,
        mut request: axum::http::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let Some(policy) = self.0.clone() else {
            return Err(ErrorCode::HttpRequestDenied.into());
        };
        let authority = request
            .uri()
            .authority()
            .cloned()
            .ok_or(ErrorCode::HttpRequestUriInvalid)?;
        let host = authority.host().trim_start_matches('[').trim_end_matches(']').to_string();
        let port = authority
            .port_u16()
            .unwrap_or(if config.use_tls { 443 } else { 80 });
        if !policy.allows(&host, port) {
            return Err(ErrorCode::HttpRequestDenied.into());
        }
        let permit = policy
            .in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| ErrorCode::InternalError(Some("Too many outgoing requests.".into())))?;
        let config = OutgoingRequestConfig {
            use_tls: config.use_tls,
            connect_timeout: config.connect_timeout.min(CONNECT_TIME),
            first_byte_timeout: config.first_byte_timeout.min(RESPONSE_TIME),
            between_bytes_timeout: config.between_bytes_timeout.min(RESPONSE_TIME),
        };
        let handle = wasmtime_wasi::runtime::spawn(async move {
            let _permit = permit;
            let addresses: Vec<SocketAddr> = match tokio::time::timeout(
                CONNECT_TIME,
                tokio::net::lookup_host((host.as_str(), port)),
            )
            .await
            {
                Ok(Ok(addresses)) => addresses.collect(),
                _ => return Ok(Err(ErrorCode::DnsTimeout)),
            };
            // Local and private addresses only when the manifest names them.
            let Some(address) = addresses
                .into_iter()
                .find(|a| public(a.ip()) || policy.names(a.ip()))
            else {
                return Ok(Err(ErrorCode::HttpRequestDenied));
            };
            // Plain HTTP connects to the address just checked, so a second lookup
            // can't point it elsewhere. HTTPS checks the certificate for the name.
            if !config.use_tls {
                request.headers_mut().insert(
                    axum::http::header::HOST,
                    axum::http::header::HeaderValue::from_str(authority.as_str())
                        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?,
                );
                let mut parts = request.uri().clone().into_parts();
                parts.authority = Some(
                    address
                        .to_string()
                        .parse()
                        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?,
                );
                *request.uri_mut() =
                    axum::http::Uri::from_parts(parts).map_err(|_| ErrorCode::HttpRequestUriInvalid)?;
            }
            Ok(wasmtime_wasi_http::p2::default_send_request_handler(request, config).await)
        });
        Ok(HostFutureIncomingResponse::pending(handle))
    }
    fn outgoing_body_chunk_size(&mut self) -> usize {
        65536
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(hosts: &[&str]) -> Policy {
        Policy {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            in_flight: Arc::new(Semaphore::new(1)),
        }
    }
    #[test]
    fn hosts_match_exactly_or_by_subdomain_on_allowed_ports() {
        let p = policy(&["api.example.com", "*.cdn.example.org", "127.0.0.1:8080"]);
        assert!(p.allows("api.example.com", 443));
        assert!(p.allows("API.example.com.", 80));
        assert!(!p.allows("api.example.com", 8443));
        assert!(!p.allows("evil-api.example.com", 443));
        assert!(!p.allows("example.com", 443));
        assert!(p.allows("a.cdn.example.org", 443));
        assert!(p.allows("a.b.cdn.example.org", 443));
        assert!(!p.allows("cdn.example.org", 443));
        assert!(!p.allows("xcdn.example.org", 443));
        assert!(p.allows("127.0.0.1", 8080));
        assert!(!p.allows("127.0.0.1", 80));
        assert!(p.names("127.0.0.1".parse().unwrap()));
        assert!(!p.names("10.0.0.1".parse().unwrap()));
    }
    #[test]
    fn only_public_addresses_are_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.5",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }
}
