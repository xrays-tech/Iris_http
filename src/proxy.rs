//! P3/P7 — proxy request pipeline (`design.md` §4.3, `plan/06-p3-forward.md`,
//! `plan/10-p7-orchestration.md`).
//!
//! [`IrisProxy`] implements Pingora's [`ProxyHttp`] for both lanes:
//! `RouteTarget::Forward` (P3) and `RouteTarget::OpenStack` (P7). The OpenStack
//! lane runs the per-site firewall ACL (`firewall.md` §3), then instance
//! discovery via the injected [`Resolver`](crate::openstack::Resolver) seam
//! (`discovery.md` §2/§7), and proxies to the floating address with a single
//! fixed-address fallback (`discovery.md` §5).
//!
//! The pure decision points — host normalisation + route lookup, forward-site
//! selection, the firewall→status mapping, the resolve-error→status mapping,
//! upstream address picking and both retry policies — are factored into free
//! functions so they can be unit tested with injected [`SharedState`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::http::ResponseHeader;
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};

use crate::firewall::{AclReason, AclVerdict, FirewallTable};
use crate::openstack::{ResolveError, ResolvedInstance};
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
///
/// P7 adds three additive fields for the OpenStack lane: the resolved
/// floating/fixed addresses, the target port and the fixed-fallback flag so
/// `upstream_peer` can recompute the upstream per attempt.
#[derive(Debug, Default)]
pub struct Ctx {
    /// The resolved route target, if any.
    pub route: Option<RouteTarget>,
    /// The upstream address chosen for this request.
    pub upstream: Option<SocketAddr>,
    /// The resolved forward site name (or openstack site name).
    pub site: Option<String>,
    /// The OpenStack instance name (set on the OpenStack lane).
    pub instance: Option<String>,
    /// Number of upstream connect failures seen for this request.
    pub retries: u32,
    /// The addresses resolved for an OpenStack instance (P7).
    pub resolved: Option<ResolvedInstance>,
    /// The target port derived from the OpenStack host prefix (P7).
    pub openstack_port: Option<u16>,
    /// Whether the next upstream attempt must use the fixed address (P7).
    pub use_fixed: bool,
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

/// Map a per-site firewall evaluation to an outbound HTTP status (P7).
///
/// * `site_has_firewall == false` → `None`: the site configured no
///   `firewall_dir`, so no ACL checks run (`firewall.md` §4).
/// * the site has a firewall and the ACL allows → `None` (proceed).
/// * the ACL denies → `Some(403)`: the uniform, detail-free rejection
///   (`firewall.md` §3/§6).
/// * the site configures a firewall but no table was loaded → `Some(403)`
///   (fail closed; this is an internal inconsistency — a site with a
///   `firewall_dir` always has a table after a successful build).
pub fn firewall_verdict_to_http(
    firewall: Option<&FirewallTable>,
    instance: &str,
    port: u16,
    client: IpAddr,
    site_has_firewall: bool,
) -> Option<u16> {
    if !site_has_firewall {
        return None;
    }
    match firewall {
        Some(table) => match table.check(instance, port, client) {
            AclVerdict::Allow => None,
            AclVerdict::Deny(_) => Some(403),
        },
        None => Some(403),
    }
}

/// The metric label for a firewall denial, or `None` when the request is not
/// denied (`firewall_denied_total{reason}` wiring, P8).
///
/// Mirrors [`firewall_verdict_to_http`]'s decision order exactly so the metric
/// and the HTTP status can never disagree.
pub fn firewall_denial_reason(
    firewall: Option<&FirewallTable>,
    instance: &str,
    port: u16,
    client: IpAddr,
    site_has_firewall: bool,
) -> Option<&'static str> {
    if !site_has_firewall {
        return None;
    }
    match firewall {
        Some(table) => match table.check(instance, port, client) {
            AclVerdict::Allow => None,
            AclVerdict::Deny(reason) => Some(match reason {
                AclReason::NoRole => "no_role",
                AclReason::PortNotAllowed(_) => "port_not_allowed",
                AclReason::Blacklisted(_) => "blacklisted",
                AclReason::NotWhitelisted(_) => "not_whitelisted",
            }),
        },
        None => Some("no_table"),
    }
}

/// Map a discovery failure to an outbound HTTP status (`discovery.md` §7).
///
/// `NotFound` → `404`; `AuthFailed` / `DiscoveryUnavailable` / `NoAddress` →
/// `502`.
pub fn resolve_error_to_status(err: &ResolveError) -> u16 {
    match err {
        ResolveError::NotFound => 404,
        ResolveError::AuthFailed
        | ResolveError::DiscoveryUnavailable(_)
        | ResolveError::NoAddress => 502,
    }
}

/// Pick the upstream `{ip}:{port}` for an OpenStack instance.
///
/// The first attempt uses the primary address (floating, else fixed); the
/// fallback attempt (`use_fixed`) uses the fixed address. Returns `None` when
/// the requested address is absent (`discovery.md` §5).
pub fn pick_upstream(
    instance: &ResolvedInstance,
    port: u16,
    use_fixed: bool,
) -> Option<SocketAddr> {
    let ip = if use_fixed {
        instance.fixed
    } else {
        instance.primary()
    };
    ip.map(|ip| SocketAddr::new(ip, port))
}

/// Whether an OpenStack request should fall back to the fixed address after
/// `failures` connect failures (`discovery.md` §5).
///
/// A fallback is possible only when the instance has a distinct fixed address
/// to try (i.e. not fixed-only, not floating-only, and floating ≠ fixed), and
/// only within the single-retry budget.
pub fn should_fallback_to_fixed(resolved: &ResolvedInstance, failures: u32) -> bool {
    failures <= MAX_CONNECT_RETRIES
        && resolved.floating.is_some()
        && resolved.fixed.is_some()
        && resolved.floating != resolved.fixed
}

/// The outcome of the OpenStack gate for one request (P7 orchestration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenStackOutcome {
    /// Reject with the given HTTP status (403 / 404 / 502).
    Deny(u16),
    /// Proceed to the upstream with the resolved addresses.
    Serve {
        /// The openstack site name (the `resolvers`/`firewall` map key).
        site: String,
        /// The target port from the host prefix.
        port: u16,
        /// The resolved floating/fixed addresses.
        resolved: ResolvedInstance,
    },
}

/// Run the OpenStack orchestration for one request: firewall ACL, then instance
/// discovery (`plan/10` §目标).
///
/// This is the injectable seam used by `request_filter`: it reads the firewall
/// table and [`Resolver`](crate::openstack::Resolver) from `shared`, so unit
/// tests exercise the real mapping with a fake resolver and a real firewall
/// table.
pub async fn openstack_gate(
    shared: &SharedState,
    site_idx: usize,
    instance: &str,
    port: u16,
    client: Option<IpAddr>,
) -> OpenStackOutcome {
    let config = shared.config.load();
    let Some(site) = config.openstack.get(site_idx) else {
        // A routing table entry that no longer maps to a site: treat as no
        // such route target.
        return OpenStackOutcome::Deny(404);
    };
    let site_name = site.name.clone();
    let has_firewall = site.firewall_dir.is_some();
    drop(config);

    // Firewall ACL first: a denial must not reach discovery (`firewall.md` §1).
    let firewall = shared.firewall.load();
    let table = firewall.get(&site_name).map(|table| table.as_ref());
    let status = match client {
        Some(ip) => {
            let verdict = firewall_verdict_to_http(table, instance, port, ip, has_firewall);
            if verdict == Some(403) {
                if let Some(reason) =
                    firewall_denial_reason(table, instance, port, ip, has_firewall)
                {
                    crate::reload::record_firewall_denied(reason);
                }
            }
            verdict
        }
        // With a firewall configured but no client address (e.g. a unix
        // socket), fail closed.
        None if has_firewall => {
            crate::reload::record_firewall_denied("no_client");
            Some(403)
        }
        None => None,
    };
    if let Some(code) = status {
        tracing::warn!(
            site = %site_name,
            instance = %instance,
            port,
            status = code,
            "openstack ACL denied request"
        );
        return OpenStackOutcome::Deny(code);
    }
    drop(firewall);

    let Some(resolver) = shared.resolvers.load().get(&site_name).cloned() else {
        tracing::error!(site = %site_name, "no resolver configured for openstack site");
        return OpenStackOutcome::Deny(502);
    };

    match resolver.resolve(instance).await {
        Ok(resolved) => OpenStackOutcome::Serve {
            site: site_name,
            port,
            resolved,
        },
        Err(err) => {
            let status = resolve_error_to_status(&err);
            tracing::warn!(
                site = %site_name,
                instance = %instance,
                error = %err,
                status,
                "openstack instance resolution failed"
            );
            OpenStackOutcome::Deny(status)
        }
    }
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
            // OpenStack lane (P7): firewall ACL → discovery → upstream.
            RouteDecision::OpenStack {
                site_idx,
                instance,
                port,
            } => {
                match openstack_gate(&self.shared, site_idx, &instance, port, client_ip(session))
                    .await
                {
                    OpenStackOutcome::Deny(code) => {
                        session.respond_error(code).await?;
                        Ok(true)
                    }
                    OpenStackOutcome::Serve {
                        site,
                        port,
                        resolved,
                    } => {
                        ctx.route = Some(RouteTarget::OpenStack {
                            site_idx,
                            instance: instance.clone(),
                            port,
                        });
                        ctx.site = Some(site);
                        ctx.instance = Some(instance);
                        ctx.openstack_port = Some(port);
                        ctx.resolved = Some(resolved);
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        // OpenStack peers recompute per attempt so the fixed-address fallback
        // (`ctx.use_fixed`, set by `fail_to_connect`) takes effect on retry.
        let addr = match ctx.route.clone() {
            Some(RouteTarget::OpenStack { .. }) => {
                let resolved = ctx.resolved.as_ref().ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(502),
                        "openstack route without a resolved instance",
                    )
                })?;
                let port = ctx.openstack_port.ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(502),
                        "openstack route without a target port",
                    )
                })?;
                pick_upstream(resolved, port, ctx.use_fixed).ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(502),
                        "no usable openstack upstream address",
                    )
                })?
            }
            _ => {
                let site = ctx.site.clone().ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(502),
                        "no forward site resolved for request",
                    )
                })?;

                // Selection runs here (not in `request_filter`) so each retry
                // re-runs load balancing and can land on a different backend.
                select_backend(&self.shared, &site, client_ip(session)).ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(502),
                        format!("no healthy upstream for forward site '{site}'"),
                    )
                })?
            }
        };
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
        let retry = match &ctx.route {
            // OpenStack: retry once, switching to the fixed address when the
            // floating address is unreachable (`discovery.md` §5).
            Some(RouteTarget::OpenStack { .. }) => {
                let fallback = ctx
                    .resolved
                    .as_ref()
                    .map(|resolved| should_fallback_to_fixed(resolved, ctx.retries))
                    .unwrap_or(false);
                if fallback {
                    ctx.use_fixed = true;
                }
                fallback
            }
            _ => should_retry_connect(ctx.retries, MAX_CONNECT_RETRIES),
        };
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
        SharedState::try_new(config).expect("shared state builds")
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

    // ----- P7: firewall -> status mapping -----

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::firewall::build_firewall;

    fn ip(octets: [u8; 4]) -> IpAddr {
        IpAddr::from(octets)
    }

    fn firewall_with(instance: &str, body: &str) -> FirewallTable {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(format!("{instance}.role")), body).expect("write role");
        build_firewall(dir.path()).expect("firewall builds")
    }

    #[test]
    fn firewall_verdict_skips_site_without_firewall() {
        assert_eq!(
            firewall_verdict_to_http(None, "web", 80, ip([127, 0, 0, 1]), false),
            None
        );
    }

    #[test]
    fn firewall_verdict_allows_open_port() {
        let table = firewall_with("web", "allowed_ports: [80]\n");
        assert_eq!(
            firewall_verdict_to_http(Some(&table), "web", 80, ip([127, 0, 0, 1]), true),
            None
        );
    }

    #[test]
    fn firewall_verdict_denies_unopened_port() {
        let table = firewall_with("web", "allowed_ports: [80]\n");
        assert_eq!(
            firewall_verdict_to_http(Some(&table), "web", 8080, ip([127, 0, 0, 1]), true),
            Some(403)
        );
    }

    #[test]
    fn firewall_verdict_denies_instance_without_role() {
        let table = firewall_with("web", "allowed_ports: [80]\n");
        assert_eq!(
            firewall_verdict_to_http(Some(&table), "ghost", 80, ip([127, 0, 0, 1]), true),
            Some(403)
        );
    }

    #[test]
    fn firewall_verdict_denies_whitelisted_out_client() {
        let table = firewall_with("web", "allowed_ports: [80]\nwhitelist: [10.0.0.0/8]\n");
        assert_eq!(
            firewall_verdict_to_http(Some(&table), "web", 80, ip([127, 0, 0, 1]), true),
            Some(403)
        );
    }

    #[test]
    fn firewall_verdict_denies_blacklisted_client() {
        let table = firewall_with("web", "allowed_ports: [80]\nblacklist: [127.0.0.0/8]\n");
        assert_eq!(
            firewall_verdict_to_http(Some(&table), "web", 80, ip([127, 0, 0, 1]), true),
            Some(403)
        );
    }

    #[test]
    fn firewall_verdict_fails_closed_when_site_has_firewall_but_no_table() {
        assert_eq!(
            firewall_verdict_to_http(None, "web", 80, ip([127, 0, 0, 1]), true),
            Some(403)
        );
    }

    // ----- P8: firewall denial metric label -----

    #[test]
    fn firewall_denial_reason_labels_match_the_verdict() {
        let table = firewall_with("web", "allowed_ports: [80]\n");
        // Allowed and no-firewall cases produce no metric.
        assert_eq!(
            firewall_denial_reason(Some(&table), "web", 80, ip([127, 0, 0, 1]), true),
            None
        );
        assert_eq!(
            firewall_denial_reason(None, "web", 80, ip([127, 0, 0, 1]), false),
            None
        );
        // Each denial reason maps to a distinct label.
        assert_eq!(
            firewall_denial_reason(Some(&table), "ghost", 80, ip([127, 0, 0, 1]), true),
            Some("no_role")
        );
        assert_eq!(
            firewall_denial_reason(Some(&table), "web", 8080, ip([127, 0, 0, 1]), true),
            Some("port_not_allowed")
        );
        let banned = firewall_with("web", "allowed_ports: [80]\nblacklist: [127.0.0.0/8]\n");
        assert_eq!(
            firewall_denial_reason(Some(&banned), "web", 80, ip([127, 0, 0, 1]), true),
            Some("blacklisted")
        );
        let locked = firewall_with("web", "allowed_ports: [80]\nwhitelist: [10.0.0.0/8]\n");
        assert_eq!(
            firewall_denial_reason(Some(&locked), "web", 80, ip([127, 0, 0, 1]), true),
            Some("not_whitelisted")
        );
        assert_eq!(
            firewall_denial_reason(None, "web", 80, ip([127, 0, 0, 1]), true),
            Some("no_table")
        );
    }

    // ----- P7: resolve error -> status mapping -----

    #[test]
    fn resolve_error_status_mapping() {
        assert_eq!(resolve_error_to_status(&ResolveError::NotFound), 404);
        assert_eq!(resolve_error_to_status(&ResolveError::AuthFailed), 502);
        assert_eq!(
            resolve_error_to_status(&ResolveError::DiscoveryUnavailable("boom".to_owned())),
            502
        );
        assert_eq!(resolve_error_to_status(&ResolveError::NoAddress), 502);
    }

    // ----- P7: upstream address picking -----

    fn resolved(floating: Option<[u8; 4]>, fixed: Option<[u8; 4]>) -> ResolvedInstance {
        ResolvedInstance {
            floating: floating.map(ip),
            fixed: fixed.map(ip),
        }
    }

    #[test]
    fn pick_upstream_prefers_floating_then_fixed() {
        let inst = resolved(Some([203, 0, 113, 10]), Some([192, 168, 1, 10]));
        assert_eq!(
            pick_upstream(&inst, 80, false),
            Some(SocketAddr::from(([203, 0, 113, 10], 80)))
        );
        assert_eq!(
            pick_upstream(&inst, 80, true),
            Some(SocketAddr::from(([192, 168, 1, 10], 80)))
        );
    }

    #[test]
    fn pick_upstream_falls_back_to_fixed_when_floating_absent() {
        let inst = resolved(None, Some([192, 168, 1, 20]));
        assert_eq!(
            pick_upstream(&inst, 8080, false),
            Some(SocketAddr::from(([192, 168, 1, 20], 8080)))
        );
    }

    #[test]
    fn pick_upstream_none_when_requested_address_absent() {
        let floating_only = resolved(Some([203, 0, 113, 11]), None);
        assert_eq!(pick_upstream(&floating_only, 80, true), None);
    }

    // ----- P7: floating -> fixed fallback decision -----

    #[test]
    fn fallback_allowed_once_when_fixed_differs() {
        let inst = resolved(Some([203, 0, 113, 10]), Some([192, 168, 1, 10]));
        assert!(should_fallback_to_fixed(&inst, 1));
        assert!(!should_fallback_to_fixed(&inst, 2), "one retry only");
    }

    #[test]
    fn fallback_disabled_without_distinct_fixed() {
        let fixed_only = resolved(None, Some([192, 168, 1, 20]));
        assert!(!should_fallback_to_fixed(&fixed_only, 1));
        let floating_only = resolved(Some([203, 0, 113, 11]), None);
        assert!(!should_fallback_to_fixed(&floating_only, 1));
        let same = resolved(Some([10, 0, 0, 1]), Some([10, 0, 0, 1]));
        assert!(!should_fallback_to_fixed(&same, 1));
    }

    // ----- P7: openstack_gate orchestration (fake Resolver injected) -----

    /// A fake resolver: the sanctioned external-boundary stub for the trait
    /// injected through `SharedState::resolvers`.
    struct FakeResolver {
        reply: Result<ResolvedInstance, ResolveError>,
        calls: AtomicUsize,
    }

    impl FakeResolver {
        fn ok(resolved: ResolvedInstance) -> Self {
            Self {
                reply: Ok(resolved),
                calls: AtomicUsize::new(0),
            }
        }

        fn err(err: ResolveError) -> Self {
            Self {
                reply: Err(err),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl crate::openstack::Resolver for FakeResolver {
        async fn resolve(&self, _instance: &str) -> Result<ResolvedInstance, ResolveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.reply.clone()
        }
    }

    fn openstack_site_with_firewall(
        name: &str,
        base_domain: &str,
        dir: Option<&std::path::Path>,
    ) -> OpenStackSite {
        let mut site = openstack_site(name, base_domain);
        site.firewall_dir = dir.map(std::path::Path::to_path_buf);
        site
    }

    fn inject_resolver(shared: &SharedState, site: &str, resolver: Arc<dyn crate::openstack::Resolver>) {
        let mut map = (**shared.resolvers.load()).clone();
        map.insert(site.to_owned(), resolver);
        shared.resolvers.store(Arc::new(map));
    }

    #[tokio::test]
    async fn gate_serves_after_acl_allow_and_resolve() {
        let shared = state(vec![], vec![openstack_site("rent", "rent.do.top")]);
        let fake = Arc::new(FakeResolver::ok(resolved(
            Some([203, 0, 113, 10]),
            Some([192, 168, 1, 10]),
        )));
        inject_resolver(&shared, "rent", fake.clone());

        let outcome = openstack_gate(&shared, 0, "web", 80, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(
            outcome,
            OpenStackOutcome::Serve {
                site: "rent".to_owned(),
                port: 80,
                resolved: resolved(Some([203, 0, 113, 10]), Some([192, 168, 1, 10])),
            }
        );
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn gate_denies_403_on_acl_and_skips_discovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80]\n").expect("write role");
        let shared = state(
            vec![],
            vec![openstack_site_with_firewall(
                "rent",
                "rent.do.top",
                Some(dir.path()),
            )],
        );
        let fake = Arc::new(FakeResolver::ok(resolved(Some([203, 0, 113, 10]), None)));
        inject_resolver(&shared, "rent", fake.clone());

        // Port 8080 is not open for `web`.
        let outcome = openstack_gate(&shared, 0, "web", 8080, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(outcome, OpenStackOutcome::Deny(403));
        assert_eq!(fake.calls(), 0, "ACL denial must not reach discovery");
    }

    #[tokio::test]
    async fn gate_denies_403_for_instance_without_role() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80]\n").expect("write role");
        let shared = state(
            vec![],
            vec![openstack_site_with_firewall(
                "rent",
                "rent.do.top",
                Some(dir.path()),
            )],
        );
        inject_resolver(&shared, "rent", Arc::new(FakeResolver::ok(resolved(Some([10, 0, 0, 1]), None))));

        let outcome = openstack_gate(&shared, 0, "ghost", 80, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(outcome, OpenStackOutcome::Deny(403));
    }

    #[tokio::test]
    async fn gate_denies_403_for_whitelisted_out_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("web.role"),
            "allowed_ports: [80]\nwhitelist: [10.0.0.0/8]\n",
        )
        .expect("write role");
        let shared = state(
            vec![],
            vec![openstack_site_with_firewall(
                "rent",
                "rent.do.top",
                Some(dir.path()),
            )],
        );
        inject_resolver(&shared, "rent", Arc::new(FakeResolver::ok(resolved(Some([10, 0, 0, 1]), None))));

        let outcome = openstack_gate(&shared, 0, "web", 80, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(outcome, OpenStackOutcome::Deny(403));
    }

    #[tokio::test]
    async fn gate_maps_resolve_errors_to_status() {
        let shared = state(vec![], vec![openstack_site("rent", "rent.do.top")]);
        for (err, want) in [
            (ResolveError::NotFound, 404),
            (ResolveError::AuthFailed, 502),
            (
                ResolveError::DiscoveryUnavailable("down".to_owned()),
                502,
            ),
            (ResolveError::NoAddress, 502),
        ] {
            inject_resolver(&shared, "rent", Arc::new(FakeResolver::err(err)));
            let outcome = openstack_gate(&shared, 0, "web", 80, Some(ip([127, 0, 0, 1]))).await;
            assert_eq!(outcome, OpenStackOutcome::Deny(want));
        }
    }

    #[tokio::test]
    async fn gate_denies_404_when_site_index_is_missing() {
        let shared = state(vec![], vec![]);
        let outcome = openstack_gate(&shared, 3, "web", 80, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(outcome, OpenStackOutcome::Deny(404));
    }

    #[tokio::test]
    async fn gate_denies_502_when_no_resolver_configured() {
        let shared = state(vec![], vec![openstack_site("rent", "rent.do.top")]);
        // Drop the auto-built resolver for this site.
        shared.resolvers.store(Arc::new(std::collections::HashMap::new()));
        let outcome = openstack_gate(&shared, 0, "web", 80, Some(ip([127, 0, 0, 1]))).await;
        assert_eq!(outcome, OpenStackOutcome::Deny(502));
    }
}
