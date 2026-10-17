//! P3 — shared state snapshots (`plan/02-contracts.md` §`src/state.rs`).
//!
//! [`SharedState`] owns the lock-free configuration snapshots the request path
//! reads: the loaded [`Config`], the compiled [`RoutingTable`], the per-SNI
//! certificate table, the per-site forward [`LbEngine`] pool, the per-openstack-
//! site [`FirewallTable`] map and the per-openstack-site [`Resolver`] map. Every
//! field is a full-snapshot [`ArcSwap`], so a reader takes an atomic snapshot
//! and never blocks a writer.
//!
//! P3 created the `config` / `routing` / `lbs` fields fixed by the contract; P4
//! added `certs`; P7 adds `firewall` / `resolvers` as purely additive fields.
//!
//! [`SharedState::try_new`] replaces P3's infallible `new`: building the
//! per-site firewall tables can fail (a bad `.role` file must fail startup with
//! context, `firewall.md` §2), so the constructor is fallible. The `lbs` /
//! `certs` / `routing` construction semantics are unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::{Config, ConfigError};
use crate::firewall::{build_firewall, FirewallTable};
use crate::lb::{build_lb, LbEngine};
use crate::openstack::{build_resolvers, Resolver};
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
    /// Per-openstack-site firewall tables, keyed by
    /// [`crate::config::OpenStackSite::name`] (P7).
    ///
    /// A site **without** `firewall_dir` has no entry here, which means "no ACL
    /// checks" for that site (`firewall.md` §4). The per-site map (rather than a
    /// single merged table) is required so one site can enforce rules while
    /// another skips them.
    pub firewall: ArcSwap<HashMap<String, Arc<FirewallTable>>>,
    /// Per-openstack-site instance resolvers, keyed by
    /// [`crate::config::OpenStackSite::name`] (P7).
    pub resolvers: ArcSwap<HashMap<String, Arc<dyn Resolver>>>,
}

impl SharedState {
    /// Build the shared state from a loaded [`Config`].
    ///
    /// Constructs the routing table, one load balancer per forward site, the
    /// per-SNI certificate table (best-effort — see below) and, per openstack
    /// site, either a firewall table (fail-fast: a bad `.role` file fails the
    /// whole build) or a `WARN` log when the site has no `firewall_dir`
    /// (`firewall.md` §4), plus one instance resolver.
    ///
    /// Certificate parsing failures are logged and yield an empty table here:
    /// startup (`main.rs`) builds the real [`crate::tls::SniCertSource`]
    /// separately and fails fast on a bad certificate, so this best-effort path
    /// only matters for reload.
    pub fn try_new(config: Config) -> Result<Self, ConfigError> {
        let routing = RoutingTable::try_from(&config)
            .map_err(|err| ConfigError::Validate(err.to_string()))?;

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

        // Build one firewall table per openstack site that configures a
        // `firewall_dir`; a malformed `.role` file fails the whole build. A
        // site without `firewall_dir` is skipped (no ACL checks, one WARN).
        let mut firewall: HashMap<String, Arc<FirewallTable>> =
            HashMap::with_capacity(config.openstack.len());
        for site in &config.openstack {
            match &site.firewall_dir {
                Some(dir) => {
                    let table = build_firewall(dir)?;
                    firewall.insert(site.name.clone(), Arc::new(table));
                }
                None => {
                    tracing::warn!(
                        site = %site.name,
                        "openstack site has no firewall_dir; ACL checks are disabled for it"
                    );
                }
            }
        }

        let resolvers = build_resolvers(&config);

        Ok(Self {
            config: ArcSwap::from_pointee(config),
            routing: ArcSwap::from_pointee(routing),
            certs: ArcSwap::from_pointee(certs),
            lbs: ArcSwap::from_pointee(lbs),
            firewall: ArcSwap::from_pointee(firewall),
            resolvers: ArcSwap::from_pointee(resolvers),
        })
    }

    /// Load the load balancer for forward site `name`, if any.
    pub fn lb(&self, name: &str) -> Option<Arc<LbEngine>> {
        self.lbs.load().get(name).cloned()
    }

    /// Load the firewall table for openstack site `name`, if any.
    pub fn firewall_for(&self, name: &str) -> Option<Arc<FirewallTable>> {
        self.firewall.load().get(name).cloned()
    }

    /// Load the resolver for openstack site `name`, if any.
    pub fn resolver_for(&self, name: &str) -> Option<Arc<dyn Resolver>> {
        self.resolvers.load().get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::Path;

    use url::Url;

    use crate::config::{
        AuthConfig, AuthCredentials, Backend, Config, ForwardSite, LbMode, NovaConfig,
        OpenStackConfig, OpenStackSite,
    };

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

    fn openstack_site(name: &str, base_domain: &str, firewall_dir: Option<&Path>) -> OpenStackSite {
        OpenStackSite {
            name: name.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            firewall_dir: firewall_dir.map(Path::to_path_buf),
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

    fn state_with(sites: Vec<ForwardSite>) -> SharedState {
        let config = Config {
            forward: sites,
            openstack: Vec::new(),
        };
        SharedState::try_new(config).expect("shared state builds")
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

    // ----- P7: firewall / resolvers wiring -----

    #[test]
    fn openstack_site_without_firewall_dir_skips_acl_and_still_builds_resolver() {
        let config = Config {
            forward: Vec::new(),
            openstack: vec![openstack_site("rent", "rent.do.top", None)],
        };
        let state = SharedState::try_new(config).expect("state builds");
        assert!(
            state.firewall.load().is_empty(),
            "a site without firewall_dir must not appear in the firewall map"
        );
        assert!(state.firewall_for("rent").is_none());
        assert!(state.resolver_for("rent").is_some());
        assert_eq!(state.resolvers.load().len(), 1);
    }

    #[test]
    fn openstack_site_with_firewall_dir_builds_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80]\n").expect("write role");

        let config = Config {
            forward: Vec::new(),
            openstack: vec![openstack_site("rent", "rent.do.top", Some(dir.path()))],
        };
        let state = SharedState::try_new(config).expect("state builds");
        let table = state.firewall_for("rent").expect("firewall table present");
        assert_eq!(
            table.check("web", 80, std::net::IpAddr::from([127, 0, 0, 1])),
            crate::firewall::AclVerdict::Allow
        );
    }

    #[test]
    fn openstack_site_with_bad_role_file_fails_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bad.role"), "allowed_ports: [80\n").expect("write role");

        let config = Config {
            forward: Vec::new(),
            openstack: vec![openstack_site("rent", "rent.do.top", Some(dir.path()))],
        };
        let err = match SharedState::try_new(config) {
            Ok(_) => panic!("bad role file must fail the build"),
            Err(err) => err,
        };
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }
}
