//! P3 — shared state snapshots (`plan/02-contracts.md` §`src/state.rs`).
//!
//! [`SharedState`] owns the lock-free configuration snapshots the request path
//! reads: the loaded [`Config`], the compiled [`RoutingTable`], the per-SNI
//! certificate table and the per-site forward [`LbEngine`] pool. Every field is
//! a full-snapshot [`ArcSwap`], so a reader takes an atomic snapshot and never
//! blocks a writer.
//!
//! P3 creates only the `config` / `routing` / `lbs` fields fixed by the
//! contract; P4 adds `certs` and P7 adds `firewall` / `resolvers` as purely
//! additive fields.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::lb::{build_lb, LbEngine};
use crate::routing::RoutingTable;
use crate::tls::SniCertMap;

/// The process-wide shared state read on the request path.
pub struct SharedState {
    /// The loaded and validated site configuration.
    pub config: ArcSwap<Config>,
    /// The compiled host-routing table.
    pub routing: ArcSwap<RoutingTable>,
    /// The per-SNI certificate table served by the TLS listener (P4).
    pub certs: ArcSwap<SniCertMap>,
    /// Forward-site load balancers, keyed by [`crate::config::ForwardSite::name`].
    pub lbs: ArcSwap<HashMap<String, Arc<LbEngine>>>,
}

impl SharedState {
    /// Build the shared state, constructing one load balancer per forward site
    /// and the per-SNI certificate table from `config`.
    ///
    /// This constructor is infallible by contract: certificate parsing failures
    /// are logged and yield an empty table here. Startup (`main.rs`) builds the
    /// real [`crate::tls::SniCertSource`] separately and fails fast on a bad
    /// certificate, so this best-effort path only matters for reload.
    pub fn new(config: Config, routing: RoutingTable) -> Self {
        let lbs: HashMap<String, Arc<LbEngine>> = config
            .forward
            .iter()
            .map(|site| (site.name.clone(), build_lb(&site.backends, &site.lb_mode)))
            .collect();

        let certs = match SniCertMap::from_sites(&config) {
            Ok(map) => map,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to build certificate map; TLS SNI lookup table left empty"
                );
                SniCertMap::empty()
            }
        };

        Self {
            config: ArcSwap::from_pointee(config),
            routing: ArcSwap::from_pointee(routing),
            certs: ArcSwap::from_pointee(certs),
            lbs: ArcSwap::from_pointee(lbs),
        }
    }

    /// Load the load balancer for forward site `name`, if any.
    pub fn lb(&self, name: &str) -> Option<Arc<LbEngine>> {
        self.lbs.load().get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::config::{Backend, ForwardSite, LbMode};

    fn forward_site(name: &str, domain: &str, mode: LbMode) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            backends: vec![Backend {
                addr: SocketAddr::from(([127, 0, 0, 1], 18081)),
                weight: 1,
            }],
            lb_mode: mode,
            health_check: None,
        }
    }

    fn state_with(sites: Vec<ForwardSite>) -> SharedState {
        let config = Config {
            forward: sites,
            openstack: Vec::new(),
        };
        let routing = RoutingTable::try_from(&config).expect("routing table builds");
        SharedState::new(config, routing)
    }

    fn https_site(name: &str, domain: &str) -> ForwardSite {
        let mut site = forward_site(name, domain, LbMode::RoundRobin);
        site.https = true;
        site.cert_file = Some("tests/fixtures/certs/www.example.com.crt".into());
        site.key_file = Some("tests/fixtures/certs/www.example.com.key".into());
        site
    }

    #[test]
    fn builds_one_lb_per_forward_site() {
        let state = state_with(vec![
            forward_site("a", "a.example.com", LbMode::RoundRobin),
            forward_site("b", "b.example.com", LbMode::Ketama),
        ]);
        assert_eq!(state.lbs.load().len(), 2);
        assert!(state.lb("a").is_some());
        assert!(state.lb("b").is_some());
        assert!(state.lb("missing").is_none());
    }

    #[test]
    fn config_and_routing_snapshots_are_accessible() {
        let state = state_with(vec![forward_site("www", "www.example.com", LbMode::Random)]);
        assert_eq!(state.config.load().forward.len(), 1);
        assert_eq!(
            state.routing.load().lookup("www.example.com"),
            Some(crate::routing::RouteTarget::Forward(0))
        );
    }

    #[test]
    fn no_forward_sites_yields_empty_lb_map() {
        let state = state_with(Vec::new());
        assert!(state.lbs.load().is_empty());
    }

    #[test]
    fn http_only_sites_yield_empty_cert_map() {
        let state = state_with(vec![forward_site("www", "www.example.com", LbMode::RoundRobin)]);
        assert!(state.certs.load().lookup("www.example.com").is_none());
    }

    #[test]
    fn https_site_populates_cert_map() {
        let state = state_with(vec![https_site("www", "www.example.com")]);
        let cert = state
            .certs
            .load()
            .lookup("www.example.com")
            .expect("https site certificate is registered");
        assert_eq!(cert.site(), "www");
    }
}
