//! P3 — shared state snapshot (`plan/02-contracts.md` §`src/state.rs`).
//!
//! [`SharedState`] owns **one** [`arc_swap::ArcSwap`] of [`Snapshot`]: the whole
//! request-path configuration — the loaded [`Config`], the compiled
//! [`RoutingTable`], the per-SNI certificate table, the per-site forward
//! [`LbEngine`] pool, the per-openstack-site [`FirewallTable`] map, the
//! per-openstack-site [`Resolver`] map and the shared forward
//! [`HealthRegistry`] — is published and replaced as a single value.
//!
//! A reader takes one atomic [`SharedState::snapshot`] and every filter of a
//! request reads that same snapshot, so a reload can never be observed
//! half-applied: the request sees either all of the old configuration or all of
//! the new one (FIX #3). Because the certificate table lives inside the same
//! snapshot, the TLS listener's [`crate::tls::SniCertSource`] reads the *same*
//! store the proxy reads, so a reloaded certificate takes effect immediately
//! (FIX #1).
//!
//! The forward [`HealthRegistry`] is shared across every engine and therefore
//! does **not** change when the snapshot does; it is carried inside the snapshot
//! so the request path (and reload) can reach it, but it is the same object
//! across reloads (FIX #2). See [`crate::lb`].
//!
//! [`SharedState::try_new`] is fallible: a bad `.role` file (`firewall.md` §2)
//! and a missing/unparseable certificate both fail startup with context.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use arc_swap::ArcSwap;
use pingora_load_balancing::HealthRegistry;

use crate::config::{Config, ConfigError, ForwardSite};
use crate::firewall::{build_firewall, FirewallTable};
use crate::lb::{build_forward_backends, ForwardBackends, ProbeRegistry};
use crate::openstack::{build_env_resolvers, Resolver};
use crate::routing::RoutingTable;
use crate::tls::SniCertMap;

/// One indivisible generation of the request-path configuration.
///
/// Everything a request needs is reachable from here, and every field is an
/// `Arc` so cloning a snapshot (or a [`shallow`](Self::shallow) copy) is cheap
/// and never deep-copies the pools.
pub struct Snapshot {
    /// The loaded and validated site configuration.
    pub config: Arc<Config>,
    /// The compiled host-routing table.
    pub routing: Arc<RoutingTable>,
    /// Forward-site backend pools, keyed by
    /// [`crate::config::ForwardSite::name`].
    ///
    /// Each entry is either the static Pingora engine
    /// ([`ForwardBackends::Static`]) or the instance-driven [`InstancePool`]
    /// ([`ForwardBackends::Instance`]) once any backend references an instance.
    ///
    /// [`InstancePool`]: crate::lb::InstancePool
    pub lbs: Arc<HashMap<String, Arc<ForwardBackends>>>,
    /// The per-SNI certificate table served by the TLS listener (P4).
    pub certs: Arc<SniCertMap>,
    /// Per-openstack-site firewall tables, keyed by
    /// [`crate::config::OpenStackSite::name`] (P7).
    ///
    /// Every openstack site carries a required `firewall_dir` (P16), so every
    /// site always has an entry: ACLs are always enforced (`firewall.md` §4).
    pub firewall: Arc<HashMap<String, Arc<FirewallTable>>>,
    /// Per-openstack-**environment** instance resolvers, keyed by environment
    /// name (`Config::openstack_envs` key, C3).
    ///
    /// Every consumer of an environment — openstack wildcard sites and forward
    /// sites with instance backends — shares one resolver, and thus one cache /
    /// single-flight / breaker / last-known-good state.
    pub resolvers: Arc<HashMap<String, Arc<dyn Resolver>>>,
    /// The process-wide forward health registry, shared by every
    /// [`LbEngine`] (FIX #2). The same object across reloads.
    pub registry: Arc<HealthRegistry>,
    /// Stable content fingerprint of the loaded configuration (P8).
    pub fingerprint: Arc<String>,
}

/// Build the forward-site backend pool map: [`ForwardBackends::Static`] for an
/// all-`addr` site, [`ForwardBackends::Instance`] for one with instance
/// backends.
///
/// `handle` is the process-wide snapshot store the instance pools read (for
/// their env resolver).
fn build_lbs(
    sites: &[ForwardSite],
    registry: &Arc<HealthRegistry>,
    handle: Arc<ArcSwap<Snapshot>>,
) -> HashMap<String, Arc<ForwardBackends>> {
    sites
        .iter()
        .map(|site| {
            (
                site.name.clone(),
                build_forward_backends(site, registry, Arc::clone(&handle)),
            )
        })
        .collect()
}

impl Snapshot {
    /// A copy that shares every `Arc`; callers replace the field(s) they change.
    ///
    /// Used to derive a locally-modified snapshot (the registry/health handle is
    /// never rebuilt this way in production; tests use it to inject a resolver).
    pub fn shallow(&self) -> Snapshot {
        Snapshot {
            config: Arc::clone(&self.config),
            routing: Arc::clone(&self.routing),
            lbs: Arc::clone(&self.lbs),
            certs: Arc::clone(&self.certs),
            firewall: Arc::clone(&self.firewall),
            resolvers: Arc::clone(&self.resolvers),
            registry: Arc::clone(&self.registry),
            fingerprint: Arc::clone(&self.fingerprint),
        }
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render resolver internals or certificate material; counts and
        // the fingerprint are enough for logs.
        f.debug_struct("Snapshot")
            .field("forward_sites", &self.config.forward.len())
            .field("openstack_sites", &self.config.openstack.len())
            .field("resolvers", &self.resolvers.len())
            .field("firewall_tables", &self.firewall.len())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// The process-wide shared state read on the request path.
pub struct SharedState {
    /// The single atomic configuration snapshot (FIX #3).
    snapshot: Arc<ArcSwap<Snapshot>>,
    /// The shared active-probe registry backing every engine's health gating
    /// (FIX #2). Lives outside the snapshot because it is mutable and shared
    /// across generations.
    probes: Arc<ProbeRegistry>,
}

impl SharedState {
    /// Build the shared state from a loaded [`Config`].
    ///
    /// Constructs the routing table, one load balancer per forward site (all
    /// sharing one [`HealthRegistry`]), the per-SNI certificate table, one
    /// firewall table per openstack site (its `firewall_dir` is required, P16;
    /// `firewall.md` §4) plus one instance resolver.
    ///
    /// A bad `.role` file (`firewall.md` §2) or a missing/unparseable
    /// certificate fails the whole build with context (startup is fail-fast).
    pub fn try_new(config: Config) -> Result<Self, ConfigError> {
        let routing = RoutingTable::try_from(&config)
            .map_err(|err| ConfigError::Validate(err.to_string()))?;

        // The fingerprint is computed from the freshly loaded config before it
        // is moved into the snapshot (P8, reload.rs).
        let fingerprint = crate::reload::config_fingerprint(&config);

        let certs = SniCertMap::from_sites(&config)
            .map_err(|err| ConfigError::Validate(format!("{err:#}")))?;

        // One health registry for every forward engine, with one dispatching
        // probe covering all of them (FIX #2).
        let registry = Arc::new(HealthRegistry::new());
        let probes = Arc::new(ProbeRegistry::new());
        registry.set_health_check(Box::new(crate::lb::SharedProbe(Arc::clone(&probes))));
        probes.sync(&config);

        // Build one firewall table per openstack site; a malformed `.role` file
        // fails the whole build. Every site has a required `firewall_dir` (P16),
        // so there is no "ACL disabled" path.
        let mut firewall: HashMap<String, Arc<FirewallTable>> =
            HashMap::with_capacity(config.openstack.len());
        for site in &config.openstack {
            let table = build_firewall(&site.firewall_dir)?;
            firewall.insert(site.name.clone(), Arc::new(table));
        }

        // One shared resolver per OpenStack environment, so every consumer of
        // an environment (openstack sites and forward instance backends) shares
        // its cache/breaker state (C2/C3).
        let resolvers = build_env_resolvers(&config);

        let config = Arc::new(config);
        let routing = Arc::new(routing);
        let certs = Arc::new(certs);
        let firewall = Arc::new(firewall);
        let resolvers = Arc::new(resolvers);
        let fingerprint = Arc::new(fingerprint);

        // Instance pools read the process snapshot lazily (to reach their env's
        // resolver), so they need the `ArcSwap` handle before the final
        // snapshot exists. Publish a bootstrap snapshot first; no reader can
        // observe it because `SharedState` is not returned until the real
        // snapshot is stored into the same handle.
        let handle = Arc::new(ArcSwap::from_pointee(Snapshot {
            config: Arc::clone(&config),
            routing: Arc::clone(&routing),
            lbs: Arc::new(HashMap::new()),
            certs: Arc::clone(&certs),
            firewall: Arc::clone(&firewall),
            resolvers: Arc::clone(&resolvers),
            registry: Arc::clone(&registry),
            fingerprint: Arc::clone(&fingerprint),
        }));

        let lbs = build_lbs(&config.forward, &registry, Arc::clone(&handle));

        // Commit the real snapshot into the same handle the pools captured.
        handle.store(Arc::new(Snapshot {
            config,
            routing,
            lbs: Arc::new(lbs),
            certs,
            firewall,
            resolvers,
            registry,
            fingerprint,
        }));

        Ok(Self {
            snapshot: handle,
            probes,
        })
    }

    /// The shared [`ArcSwap`] handle (handed to the TLS certificate source so
    /// it reads the same snapshot the proxy reads — FIX #1).
    pub fn handle(&self) -> Arc<ArcSwap<Snapshot>> {
        Arc::clone(&self.snapshot)
    }

    /// Take the current configuration snapshot atomically.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Publish a new configuration snapshot atomically (reload commit).
    pub fn store(&self, snapshot: Snapshot) {
        self.snapshot.store(Arc::new(snapshot));
    }

    /// The shared forward health-probe registry (FIX #2).
    pub fn probes(&self) -> Arc<ProbeRegistry> {
        Arc::clone(&self.probes)
    }

    /// The shared forward [`HealthRegistry`].
    pub fn health_registry(&self) -> Arc<HealthRegistry> {
        Arc::clone(&self.snapshot.load().registry)
    }

    /// The current loaded configuration.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.snapshot.load().config)
    }

    /// The current configuration fingerprint.
    pub fn fingerprint(&self) -> Arc<String> {
        Arc::clone(&self.snapshot.load().fingerprint)
    }

    /// Load the backend pool for forward site `name`, if any.
    pub fn lb(&self, name: &str) -> Option<Arc<ForwardBackends>> {
        self.snapshot.load().lbs.get(name).cloned()
    }

    /// Load the firewall table for openstack site `name`, if any.
    pub fn firewall_for(&self, name: &str) -> Option<Arc<FirewallTable>> {
        self.snapshot.load().firewall.get(name).cloned()
    }

    /// Load the shared resolver for OpenStack environment `env_name`, if any.
    pub fn resolver_for(&self, env_name: &str) -> Option<Arc<dyn Resolver>> {
        self.snapshot.load().resolvers.get(env_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::Path;

    use url::Url;

    use crate::config::{
        AuthConfig, AuthCredentials, Backend, BackendTarget, Config, ForwardSite, LbMode,
        NovaConfig, OpenStackConfig, OpenStackEnv, OpenStackSite,
    };

    fn envs() -> HashMap<String, Arc<OpenStackEnv>> {
        HashMap::from([(
            "prod".to_owned(),
            Arc::new(OpenStackEnv {
                name: "prod".to_owned(),
                config: OpenStackConfig {
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
            }),
        )])
    }

    fn forward_site(name: &str, domain: &str, mode: LbMode) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            openstack_env: None,
            backends: vec![Backend {
                target: BackendTarget::Addr(SocketAddr::from(([127, 0, 0, 1], 18081))),
                weight: 1,
            }],
            lb_mode: mode,
            health_check: None,
        }
    }

    fn openstack_site(name: &str, base_domain: &str, firewall_dir: &Path) -> OpenStackSite {
        OpenStackSite {
            name: name.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            firewall_dir: firewall_dir.to_path_buf(),
            openstack_env: "prod".to_owned(),
        }
    }

    fn state_with(sites: Vec<ForwardSite>) -> SharedState {
        let config = Config {
            forward: sites,
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
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
        assert_eq!(state.snapshot().lbs.len(), 2);
        assert!(state.lb("a").is_some());
        assert!(state.lb("b").is_some());
        assert!(state.lb("missing").is_none());
    }

    #[test]
    fn config_and_routing_snapshots_are_accessible() {
        let state = state_with(vec![forward_site("www", "www.example.com", LbMode::Random)]);
        assert_eq!(state.config().forward.len(), 1);
        assert_eq!(
            state.snapshot().routing.lookup("www.example.com"),
            Some(crate::routing::RouteTarget::Forward(0))
        );
    }

    #[test]
    fn no_forward_sites_yields_empty_lb_map() {
        let state = state_with(Vec::new());
        assert!(state.snapshot().lbs.is_empty());
    }

    #[test]
    fn http_only_sites_yield_empty_cert_map() {
        let state = state_with(vec![forward_site("www", "www.example.com", LbMode::RoundRobin)]);
        assert!(state.snapshot().certs.lookup("www.example.com").is_none());
    }

    #[test]
    fn https_site_populates_cert_map() {
        let state = state_with(vec![https_site("www", "www.example.com")]);
        let cert = state
            .snapshot()
            .certs
            .lookup("www.example.com")
            .expect("https site certificate is registered");
        assert_eq!(cert.site(), "www");
    }

    #[test]
    fn missing_certificate_fails_the_build() {
        // Startup is fail-fast on a bad certificate (FIX #1: the snapshot is the
        // single certificate store, so it must be built correctly).
        let mut site = https_site("www", "www.example.com");
        site.cert_file = Some("/no/such/cert.pem".into());
        site.key_file = Some("/no/such/key.pem".into());
        let config = Config {
            forward: vec![site],
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
        };
        let err = match SharedState::try_new(config) {
            Ok(_) => panic!("a missing certificate must fail the build"),
            Err(err) => err,
        };
        assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
    }

    // ----- P7: firewall / resolvers wiring -----

    #[test]
    fn openstack_site_builds_a_firewall_entry_and_a_resolver() {
        // P16: `firewall_dir` is required, so every openstack site gets a
        // firewall table; the WARN/"ACL disabled" path no longer exists.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80]\n").expect("write role");
        let config = Config {
            forward: Vec::new(),
            openstack: vec![openstack_site("rent", "rent.do.top", dir.path())],
            openstack_envs: envs(),
        };
        let state = SharedState::try_new(config).expect("state builds");
        assert_eq!(state.snapshot().firewall.len(), 1);
        assert!(state.firewall_for("rent").is_some());
        // Resolvers are keyed by ENVIRONMENT name, not site name (C3).
        assert!(state.resolver_for("prod").is_some());
        assert!(state.resolver_for("rent").is_none());
        assert_eq!(state.snapshot().resolvers.len(), 1);
    }

    #[test]
    fn openstack_sites_sharing_an_env_share_one_resolver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            forward: Vec::new(),
            openstack: vec![
                openstack_site("rent-a", "a.do.top", dir.path()),
                openstack_site("rent-b", "b.do.top", dir.path()),
            ],
            openstack_envs: envs(),
        };
        let state = SharedState::try_new(config).expect("state builds");
        assert_eq!(
            state.snapshot().resolvers.len(),
            1,
            "one shared resolver per environment"
        );
        let a = state.resolver_for("prod").expect("prod resolver");
        let b = state.resolver_for("prod").expect("prod resolver");
        assert!(Arc::ptr_eq(&a, &b), "both sites must share the env resolver");
    }

    #[test]
    fn addr_only_forward_site_builds_a_static_pool() {
        let state = state_with(vec![forward_site("www", "www.example.com", LbMode::RoundRobin)]);
        assert!(matches!(
            state.lb("www").expect("pool").as_ref(),
            crate::lb::ForwardBackends::Static(_)
        ));
    }

    #[test]
    fn instance_forward_site_builds_an_instance_pool() {
        let mut site = forward_site("apps", "apps.example.com", LbMode::RoundRobin);
        site.openstack_env = Some("prod".to_owned());
        site.backends = vec![Backend {
            target: BackendTarget::Instance {
                instance: "INS001".to_owned(),
                port: 8080,
            },
            weight: 1,
        }];
        let config = Config {
            forward: vec![site],
            openstack: Vec::new(),
            openstack_envs: envs(),
        };
        let state = SharedState::try_new(config).expect("state builds");
        assert!(matches!(
            state.lb("apps").expect("pool").as_ref(),
            crate::lb::ForwardBackends::Instance(_)
        ));
    }

    #[test]
    fn openstack_site_with_firewall_dir_builds_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80]\n").expect("write role");

        let config = Config {
            forward: Vec::new(),
            openstack: vec![openstack_site("rent", "rent.do.top", dir.path())],
            openstack_envs: envs(),
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
            openstack: vec![openstack_site("rent", "rent.do.top", dir.path())],
            openstack_envs: envs(),
        };
        let err = match SharedState::try_new(config) {
            Ok(_) => panic!("bad role file must fail the build"),
            Err(err) => err,
        };
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    // ----- P8: config fingerprint -----

    #[test]
    fn try_new_computes_a_stable_fingerprint() {
        let config = || Config {
            forward: vec![forward_site("www", "www.example.com", LbMode::RoundRobin)],
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
        };
        let a = SharedState::try_new(config()).expect("state builds");
        let b = SharedState::try_new(config()).expect("state builds");
        assert!(!a.fingerprint().is_empty());
        assert_eq!(a.fingerprint().as_str(), b.fingerprint().as_str());
    }

    #[test]
    fn fingerprint_changes_with_the_config() {
        let a = SharedState::try_new(Config {
            forward: vec![forward_site("www", "www.example.com", LbMode::RoundRobin)],
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
        })
        .expect("state builds");
        let b = SharedState::try_new(Config {
            forward: vec![forward_site("www", "www.example.com", LbMode::Ketama)],
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
        })
        .expect("state builds");
        assert_ne!(a.fingerprint().as_str(), b.fingerprint().as_str());
    }

    // ----- FIX #2: the shared registry is reachable and shared -----

    #[test]
    fn forward_engines_share_the_snapshot_registry() {
        let mut a = forward_site("a", "a.example.com", LbMode::RoundRobin);
        a.backends[0].target = BackendTarget::Addr(SocketAddr::from(([127, 0, 0, 1], 18091)));
        let mut b = forward_site("b", "b.example.com", LbMode::Random);
        b.backends[0].target = BackendTarget::Addr(SocketAddr::from(([127, 0, 0, 1], 18092)));
        let state = state_with(vec![a, b]);

        // Both engines' views contribute to the one snapshot registry.
        assert_eq!(
            state.health_registry().target_count(),
            2,
            "every forward engine must contribute to the shared registry"
        );
    }
}
