//! P3 — proxy request pipeline (`design.md` §4.3, `plan/06-p3-forward.md`).
//!
//! [`IrisProxy`] implements Pingora's [`ProxyHttp`] for the **forward** lane
//! only (`RouteTarget::Forward`). The OpenStack lane is out of scope until P7;
//! an OpenStack route is answered with `502` as an explicit placeholder (see
//! [`request_filter`](IrisProxy) below).
//!
//! The pure decision points — host normalisation + route lookup, forward-site
//! selection and the connect-retry policy — are factored into free functions so
//! they can be unit tested with injected [`SharedState`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::http::ResponseHeader;
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};

use crate::routing::{normalize_host, RouteTarget};
use crate::state::SharedState;
use crate::tls::TlsSni;

/// Value advertised in the `Server` response header.
const SERVER_HEADER: &str = "iris";
/// Connect timeout applied to every upstream peer.
const UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Read timeout applied to every upstream peer.
const UPSTREAM_READ_TIMEOUT_SECS: u64 = 30;
/// Extra upstream attempts allowed after the first connect failure.
///
/// `1` means exactly one retry (two attempts in total) before the request fails.
pub const MAX_CONNECT_RETRIES: u32 = 1;

/// Per-request context shared across the proxy filters (`plan/02-contracts.md`).
///
/// `retries` is an internal, additive field beyond the four contract fields:
/// `fail_to_connect` needs to count connect failures per request to honour the
/// "retry once" policy without relying on Pingora's 16-retry fail-safe.
#[derive(Debug, Default)]
pub struct Ctx {
    /// The resolved route target, if any.
    pub route: Option<RouteTarget>,
    /// The upstream address chosen for this request.
    pub upstream: Option<SocketAddr>,
    /// The resolved forward site name.
    pub site: Option<String>,
    /// The OpenStack instance name (unused in P3).
    pub instance: Option<String>,
    /// Number of upstream connect failures seen for this request.
    pub retries: u32,
}

/// The forward-lane proxy application.
pub struct IrisProxy {
    /// The shared configuration/routing/pool snapshots.
    pub shared: Arc<SharedState>,
}

impl IrisProxy {
    /// The `(https, site_name)` pair for a matched route, or `None` when the
    /// host matched no site.
    fn site_facts(&self, decision: &RouteDecision) -> Option<(bool, String)> {
        let config = self.shared.config.load();
        match decision {
            RouteDecision::Forward { site_idx, .. } => config
                .forward
                .get(*site_idx)
                .map(|site| (site.https, site.name.clone())),
            RouteDecision::OpenStack { site_idx, .. } => config
                .openstack
                .get(*site_idx)
                .map(|site| (site.https, site.name.clone())),
            RouteDecision::NotFound => None,
        }
    }
}

/// The routing decision for a request host, after normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// A `forward` site was matched: forward to its backend pool.
    Forward {
        /// The resolved forward site name (the `lbs` map key).
        site: String,
        /// Index into [`crate::config::Config::forward`].
        site_idx: usize,
    },
    /// An OpenStack instance route was matched (implemented in P7).
    OpenStack {
        /// Index into [`crate::config::Config::openstack`].
        site_idx: usize,
        /// The instance name extracted from the host.
        instance: String,
        /// The target port extracted from the host.
        port: u16,
    },
    /// No site matched: the request must be answered with `404`.
    NotFound,
}

/// Resolve a raw request host to a [`RouteDecision`].
///
/// Host normalisation follows `config.md` §8; matching order (exact then
/// wildcard) is owned by [`crate::routing::RoutingTable::lookup`].
pub fn decide_route(shared: &SharedState, raw_host: &str) -> RouteDecision {
    let host = normalize_host(raw_host);
    let target = shared.routing.load().lookup(&host);
    match target {
        None => RouteDecision::NotFound,
        Some(RouteTarget::Forward(idx)) => {
            let site = shared
                .config
                .load()
                .forward
                .get(idx)
                .map(|site| site.name.clone());
            match site {
                Some(site) => RouteDecision::Forward { site, site_idx: idx },
                None => RouteDecision::NotFound,
            }
        }
        Some(RouteTarget::OpenStack {
            site_idx,
            instance,
            port,
        }) => RouteDecision::OpenStack {
            site_idx,
            instance,
            port,
        },
    }
}

/// Select an upstream backend for `site` using `client` as the hash key.
pub fn select_backend(
    shared: &SharedState,
    site: &str,
    client: Option<IpAddr>,
) -> Option<SocketAddr> {
    let lbs = shared.lbs.load();
    lbs.get(site).and_then(|lb| lb.select(client))
}

/// Whether another upstream attempt is allowed after `failures` connect
/// failures, given `max_retries` extra attempts.
pub fn should_retry_connect(failures: u32, max_retries: u32) -> bool {
    failures <= max_retries
}

/// Whether a TLS-terminated request must be refused with `421 Misdirected
/// Request`.
///
/// Only called for requests that arrived over TLS *and* matched a configured
/// site (`plan/07` §421 semantics):
/// * a site that does not serve HTTPS (`https == false`) is unreachable on the
///   TLS listener, and
/// * a site that does serve HTTPS is only valid when it owns the certificate
///   the SNI selected (`same_site_as_sni`).
pub fn should_421(site_https: bool, same_site_as_sni: bool) -> bool {
    !site_https || !same_site_as_sni
}

/// The TLS SNI of the current connection, or `None` for a plain-HTTP request.
///
/// [`SniCertSource`](crate::tls::SniCertSource) attaches the SNI to the
/// connection digest's TLS extension; its presence also marks the request as
/// having arrived over TLS.
fn tls_sni(session: &Session) -> Option<String> {
    let digest = session.as_downstream().digest()?;
    let ssl = digest.ssl_digest.as_ref()?;
    ssl.extension.get::<TlsSni>().map(|sni| sni.as_str().to_owned())
}

/// The downstream client IP, if the session exposes one.
fn client_ip(session: &Session) -> Option<IpAddr> {
    session
        .as_downstream()
        .client_addr()
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.ip())
}

/// The request authority (the caller normalises it).
///
/// HTTP/1.x carries it in the `Host` header; HTTP/2 carries it in the request
/// URI (`:authority`) with no `Host` header, so that is the fallback.
fn request_host(session: &Session) -> String {
    let req = session.as_downstream().req_header();
    if let Some(host) = req.headers.get("host").and_then(|value| value.to_str().ok()) {
        return host.to_owned();
    }
    req.uri
        .authority()
        .map(|authority| authority.as_str().to_owned())
        .unwrap_or_default()
}

/// Insert a response header, logging (never panicking) on failure.
fn set_header(response: &mut ResponseHeader, name: &'static str, value: &'static str) {
    if let Err(err) = response.insert_header(name, value) {
        tracing::debug!(header = name, error = %err, "failed to set response header");
    }
}

#[async_trait]
impl ProxyHttp for IrisProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        Ctx::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let host = request_host(session);
        let decision = decide_route(&self.shared, &host);

        // Misdirected-request guard (P4): only a TLS connection can be
        // misdirected. A site that does not serve HTTPS, or one whose
        // certificate the SNI did not select, must be answered with 421.
        if let Some(sni) = tls_sni(session) {
            if let Some((site_https, site_name)) = self.site_facts(&decision) {
                let same_site_as_sni = self
                    .shared
                    .certs
                    .load()
                    .lookup(&sni)
                    .map(|cert| cert.site() == site_name.as_str())
                    .unwrap_or(false);
                if should_421(site_https, same_site_as_sni) {
                    tracing::debug!(
                        sni = %sni,
                        host = %host,
                        site = %site_name,
                        "misdirected request over TLS; answering 421"
                    );
                    session.respond_error(421).await?;
                    return Ok(true);
                }
            }
        }

        match decision {
            RouteDecision::Forward { site, site_idx } => {
                ctx.route = Some(RouteTarget::Forward(site_idx));
                ctx.site = Some(site);
                Ok(false)
            }
            RouteDecision::NotFound => {
                session.respond_error(404).await?;
                Ok(true)
            }
            // OpenStack routing lands in P7; answer with an explicit gateway
            // error instead of misrouting. See module docs.
            RouteDecision::OpenStack { .. } => {
                session.respond_error(502).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        let site = ctx.site.clone().ok_or_else(|| {
            Error::explain(
                ErrorType::HTTPStatus(502),
                "no forward site resolved for request",
            )
        })?;

        // Selection runs here (not in `request_filter`) so each retry re-runs
        // load balancing and can land on a different backend.
        let addr = select_backend(&self.shared, &site, client_ip(session)).ok_or_else(|| {
            Error::explain(
                ErrorType::HTTPStatus(502),
                format!("no healthy upstream for forward site '{site}'"),
            )
        })?;
        ctx.upstream = Some(addr);

        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.connection_timeout = Some(Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS));
        peer.options.read_timeout = Some(Duration::from_secs(UPSTREAM_READ_TIMEOUT_SECS));
        Ok(Box::new(peer))
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        peer: &HttpPeer,
        ctx: &mut Ctx,
        mut e: Box<Error>,
    ) -> Box<Error> {
        ctx.retries = ctx.retries.saturating_add(1);
        let retry = should_retry_connect(ctx.retries, MAX_CONNECT_RETRIES);
        e.set_retry(retry);
        tracing::warn!(
            upstream = ?peer._address,
            attempt = ctx.retries,
            retry,
            "upstream connect failure"
        );
        e
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        set_header(upstream_response, "Server", SERVER_HEADER);
        set_header(upstream_response, "X-Content-Type-Options", "nosniff");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use url::Url;

    use crate::config::{
        AuthConfig, AuthCredentials, Backend, Config, ForwardSite, LbMode, NovaConfig,
        OpenStackConfig, OpenStackSite,
    };
    use crate::routing::RoutingTable;

    fn forward_site(name: &str, domain: &str, ports: &[(u16, u32)]) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            backends: ports
                .iter()
                .map(|(port, weight)| Backend {
                    addr: SocketAddr::from(([127, 0, 0, 1], *port)),
                    weight: *weight,
                })
                .collect(),
            lb_mode: LbMode::RoundRobin,
            health_check: None,
        }
    }

    fn openstack_site(name: &str, base_domain: &str) -> OpenStackSite {
        OpenStackSite {
            name: name.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            firewall_dir: None,
            openstack: OpenStackConfig {
                cache_ttl_secs: 86_400,
                timeout_secs: 5,
                auth: AuthConfig {
                    api_url: Url::parse("https://controller.example.com:5000/v3")
                        .expect("valid test URL"),
                    credentials: AuthCredentials::ApplicationCredential {
                        id: "test-id".to_owned(),
                        secret: "test-secret".to_owned(),
                    },
                },
                nova: NovaConfig {
                    api_url: None,
                    all_tenants: true,
                },
            },
        }
    }

    fn state(forward: Vec<ForwardSite>, openstack: Vec<OpenStackSite>) -> SharedState {
        let config = Config { forward, openstack };
        let routing = RoutingTable::try_from(&config).expect("routing table builds");
        SharedState::new(config, routing)
    }

    // ----- decide_route (host normalisation -> route / 404) -----

    #[test]
    fn decide_route_matches_forward_site_case_insensitively() {
        let state = state(vec![forward_site("www", "www.example.com", &[(18081, 1)])], vec![]);
        assert_eq!(
            decide_route(&state, "WWW.Example.COM:80"),
            RouteDecision::Forward {
                site: "www".to_owned(),
                site_idx: 0,
            }
        );
    }

    #[test]
    fn decide_route_returns_not_found_for_unknown_host() {
        let state = state(vec![forward_site("www", "www.example.com", &[(18081, 1)])], vec![]);
        assert_eq!(decide_route(&state, "unknown.test"), RouteDecision::NotFound);
        assert_eq!(decide_route(&state, ""), RouteDecision::NotFound);
    }

    #[test]
    fn decide_route_matches_openstack_wildcard() {
        let state = state(vec![], vec![openstack_site("rent", "rent.do.top")]);
        assert_eq!(
            decide_route(&state, "my-app-8080.rent.do.top"),
            RouteDecision::OpenStack {
                site_idx: 0,
                instance: "my-app".to_owned(),
                port: 8080,
            }
        );
        // Bare base domain is not routable.
        assert_eq!(decide_route(&state, "rent.do.top"), RouteDecision::NotFound);
    }

    // ----- select_backend -----

    #[test]
    fn select_backend_returns_a_pool_member() {
        let state = state(
            vec![forward_site("www", "www.example.com", &[(18081, 1), (18082, 1)])],
            vec![],
        );
        let selected = select_backend(&state, "www", Some(IpAddr::from([10, 0, 0, 1])));
        assert!(
            selected == Some(SocketAddr::from(([127, 0, 0, 1], 18081)))
                || selected == Some(SocketAddr::from(([127, 0, 0, 1], 18082)))
        );
    }

    #[test]
    fn select_backend_unknown_site_returns_none() {
        let state = state(vec![forward_site("www", "www.example.com", &[(18081, 1)])], vec![]);
        assert_eq!(select_backend(&state, "missing", None), None);
    }

    // ----- retry policy -----

    #[test]
    fn should_retry_connect_allows_exactly_one_retry() {
        // failures counted after the increment.
        assert!(should_retry_connect(1, 1), "first failure retries once");
        assert!(!should_retry_connect(2, 1), "second failure gives up");
    }

    #[test]
    fn should_retry_connect_with_zero_max_never_retries() {
        assert!(!should_retry_connect(1, 0));
    }

    // ----- 421 misdirected-request decision -----

    #[test]
    fn should_421_rejects_http_only_site_over_tls() {
        // A site that does not serve HTTPS is unreachable on the TLS listener.
        assert!(should_421(false, true));
        assert!(should_421(false, false));
    }

    #[test]
    fn should_421_rejects_https_site_when_cert_belongs_elsewhere() {
        assert!(should_421(true, false));
    }

    #[test]
    fn should_421_allows_matching_https_site() {
        assert!(!should_421(true, true));
    }
}
