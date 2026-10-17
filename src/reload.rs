//! P8 — dynamic configuration reload, config fingerprint and reload metrics
//! (`reload.md` §1–§6, `plan/11-p8-reload.md`).
//!
//! [`apply_new_config`] implements the *validate-first, keep-old-on-fail* hot
//! swap (`reload.md` §2): every component of the new configuration is built
//! before **any** live state is touched; a build error returns [`ReloadError`]
//! and leaves the running configuration serving unchanged. Once every build
//! succeeds it publishes one new [`Snapshot`] with a single atomic
//! [`SharedState::store`], so a request observes either the whole old
//! configuration or the whole new one — never a half-applied mixture (FIX #3).
//!
//! The forward [`HealthRegistry`](pingora_load_balancing::HealthRegistry) and
//! its dispatching probe are shared across reloads (FIX #2): a site added or
//! changed by a reload is gated by the already-running health-check service.
//!
//! [`ReloadService`] is the [`BackgroundService`] that owns the trigger loop:
//! it selects on a tokio `SIGHUP` stream (`iris reload` sends `SIGHUP`),
//! an optional periodic re-scan when `--watch-config` is enabled, and the
//! process-wide `ShutdownWatch` for graceful termination.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use pingora_prometheus::prometheus::{register_int_counter_vec, IntCounterVec};

use crate::config::{load_config, Config};
use crate::firewall::{build_firewall, FirewallTable};
use crate::lb::{build_lb, LbEngine};
use crate::openstack::{HttpClient, OpenStackResolver, Resolver};
use crate::routing::RoutingTable;
use crate::state::{SharedState, Snapshot};
use crate::tls::SniCertMap;

/// Interval between configuration re-scans when `--watch-config` is enabled.
///
/// A polling re-scan (rather than an event-driven `notify` watcher) keeps the
/// reload service single-threaded and fully testable; the fingerprint check in
/// [`ReloadService::reload_once`] makes a no-op scan cheap.
const WATCH_INTERVAL_SECS: u64 = 2;

/// A configuration reload failed while building the new state.
///
/// The running configuration is untouched: the error is reported to the
/// operator (and to `config_reload_total{ok="false"}`) and the old
/// configuration keeps serving (`reload.md` §2).
#[derive(Debug)]
pub enum ReloadError {
    /// A single component of the new configuration could not be built.
    Build {
        /// The component that failed (e.g. `"routing"`, `"firewall"`).
        component: &'static str,
        /// Human-readable failure detail.
        source: String,
    },
}

impl ReloadError {
    fn build(component: &'static str, source: impl Into<String>) -> Self {
        Self::Build {
            component,
            source: source.into(),
        }
    }
}

impl fmt::Display for ReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build { component, source } => write!(
                f,
                "failed to build '{component}' from the new configuration: {source}"
            ),
        }
    }
}

impl std::error::Error for ReloadError {}

/// A stable fingerprint of a loaded [`Config`]'s contents.
///
/// The fingerprint is a hex-encoded 64-bit hash of the configuration's
/// canonical rendering **including credential material** (FIX #4): the
/// redacting `Debug` rendering covers every non-secret field, and each
/// credential's real secret is hashed separately so a credential-only rotation
/// changes the fingerprint (and is therefore applied by `--watch-config`). Only
/// the hash is exposed (by `/version`), never the secrets.
pub fn config_fingerprint(config: &Config) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use crate::config::AuthCredentials;

    let mut hasher = DefaultHasher::new();
    // `Debug` is deterministic for a fixed field order and renders every
    // non-secret field (secrets are redacted, so they are added explicitly next).
    format!("{config:?}").hash(&mut hasher);
    // The redacted secret fields, hashed in load order.
    for site in &config.openstack {
        site.name.hash(&mut hasher);
        match &site.openstack.auth.credentials {
            AuthCredentials::Password { password, .. } => password.hash(&mut hasher),
            AuthCredentials::ApplicationCredential { secret, .. } => secret.hash(&mut hasher),
        }
    }
    format!("{:016x}", hasher.finish())
}

/// Apply a freshly loaded configuration to `state`.
///
/// # Semantics
///
/// 1. **Build phase** — the routing table, per-site firewall tables, per-SNI
///    certificate map, the forward load-balancer map and the openstack resolver
///    map are all constructed first. A failure at any step returns
///    [`ReloadError`] and **no live state is modified**.
/// 2. **Commit phase** — one [`Snapshot`] is published with a single atomic
///    `store` (FIX #3). OpenStack resolvers whose `openstack` config is
///    unchanged and forward engines whose pool/algorithm/health spec is
///    unchanged are *reused* (same `Arc`), so the resolver cache survives and
///    the shared health probe keeps gating both (`reload.md` §3).
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError> {
    // ---- build phase: nothing below touches live state ----
    let routing = RoutingTable::try_from(&cfg)
        .map_err(|err| ReloadError::build("routing", err.to_string()))?;

    let fingerprint = config_fingerprint(&cfg);

    let mut firewall: HashMap<String, Arc<FirewallTable>> =
        HashMap::with_capacity(cfg.openstack.len());
    for site in &cfg.openstack {
        if let Some(dir) = &site.firewall_dir {
            let table = build_firewall(dir).map_err(|err| {
                ReloadError::build("firewall", format!("site '{}': {err}", site.name))
            })?;
            firewall.insert(site.name.clone(), Arc::new(table));
        }
    }

    let certs = SniCertMap::from_sites(&cfg)
        .map_err(|err| ReloadError::build("certificates", format!("{err:#}")))?;

    // The live snapshot, used to decide what can be reused below and to carry
    // the shared health registry forward. Held for the whole build phase.
    let old = state.snapshot();

    // Forward load balancers: reuse the live engine for a forward site whose
    // *pool-relevant* configuration (backends, algorithm, health check) is
    // unchanged. The engine shares the process-wide health registry, so even a
    // rebuilt engine is gated by the already-running probe; reuse additionally
    // keeps the observed health history across the reload.
    let mut lbs: HashMap<String, Arc<LbEngine>> = HashMap::with_capacity(cfg.forward.len());
    for site in &cfg.forward {
        let unchanged = old.config.forward.iter().any(|old_site| {
            old_site.name == site.name
                && old_site.backends == site.backends
                && old_site.lb_mode == site.lb_mode
                && old_site.health_check == site.health_check
        });
        let engine = if unchanged {
            match old.lbs.get(&site.name) {
                Some(existing) => Arc::clone(existing),
                None => build_lb(&site.backends, &site.lb_mode, &old.registry),
            }
        } else {
            build_lb(&site.backends, &site.lb_mode, &old.registry)
        };
        lbs.insert(site.name.clone(), engine);
    }

    // Resolvers: reuse the live resolver for an openstack site whose
    // control-plane configuration is byte-for-byte unchanged.
    let client = Arc::new(HttpClient::new());
    let mut resolvers: HashMap<String, Arc<dyn Resolver>> =
        HashMap::with_capacity(cfg.openstack.len());
    for site in &cfg.openstack {
        let unchanged = old
            .config
            .openstack
            .iter()
            .any(|old_site| old_site.name == site.name && old_site.openstack == site.openstack);
        let resolver: Arc<dyn Resolver> = if unchanged {
            match old.resolvers.get(&site.name) {
                Some(existing) => Arc::clone(existing),
                None => Arc::new(OpenStackResolver::new(&site.openstack, Arc::clone(&client))),
            }
        } else {
            Arc::new(OpenStackResolver::new(&site.openstack, Arc::clone(&client)))
        };
        resolvers.insert(site.name.clone(), resolver);
    }

    // Re-point the shared probe at the new site/health specs. Targets of removed
    // or health-check-less sites stop being probed (FIX #8); the engine views
    // deregister their membership when their last `Arc<LbEngine>` is released.
    state.probes().sync(&cfg);

    // ---- commit phase: every build above succeeded ----
    let snapshot = Snapshot {
        config: Arc::new(cfg),
        routing: Arc::new(routing),
        lbs: Arc::new(lbs),
        certs: Arc::new(certs),
        firewall: Arc::new(firewall),
        resolvers: Arc::new(resolvers),
        registry: Arc::clone(&old.registry),
        fingerprint: Arc::new(fingerprint),
    };
    state.store(snapshot);
    Ok(())
}

// ---------------------------------------------------------------------------
// Reload metrics (`reload.md` §6).
//
// Registered lazily into the process-wide prometheus default registry, which
// `pingora_prometheus::prometheus::gather()` (served by `/metrics`) reads from.
// `OnceLock` guarantees a single registration; the register error path is only
// reachable if a caller registers the same name twice.
// ---------------------------------------------------------------------------

static CONFIG_RELOAD: OnceLock<IntCounterVec> = OnceLock::new();
static FIREWALL_DENIED: OnceLock<IntCounterVec> = OnceLock::new();

/// Register the reload/firewall counters into the default prometheus registry.
///
/// Idempotent: a second call is a no-op. Must run before `/metrics` is scraped
/// so the counters appear in the exposition.
pub fn init_metrics() {
    if CONFIG_RELOAD.get().is_none() {
        match register_int_counter_vec!(
            "config_reload_total",
            "Configuration reload attempts by outcome",
            &["ok"]
        ) {
            Ok(counter) => {
                // Materialise both label series at 0 so the counter is present
                // in the exposition before the first reload.
                let _ = counter.with_label_values(&["true"]);
                let _ = counter.with_label_values(&["false"]);
                let _ = CONFIG_RELOAD.set(counter);
            }
            Err(err) => tracing::error!(error = %err, "failed to register config_reload_total"),
        }
    }
    if FIREWALL_DENIED.get().is_none() {
        match register_int_counter_vec!(
            "firewall_denied_total",
            "OpenStack firewall ACL denials by reason",
            &["reason"]
        ) {
            Ok(counter) => {
                for reason in [
                    "no_role",
                    "port_not_allowed",
                    "blacklisted",
                    "not_whitelisted",
                    "no_table",
                    "no_client",
                ] {
                    let _ = counter.with_label_values(&[reason]);
                }
                let _ = FIREWALL_DENIED.set(counter);
            }
            Err(err) => tracing::error!(error = %err, "failed to register firewall_denied_total"),
        }
    }
}

/// Record one configuration reload attempt (`config_reload_total{ok=...}`).
pub fn record_config_reload(ok: bool) {
    if let Some(counter) = CONFIG_RELOAD.get() {
        counter
            .with_label_values(&[if ok { "true" } else { "false" }])
            .inc();
    }
}

/// Record one firewall denial (`firewall_denied_total{reason=...}`).
pub fn record_firewall_denied(reason: &str) {
    if let Some(counter) = FIREWALL_DENIED.get() {
        counter.with_label_values(&[reason]).inc();
    }
}

// ---------------------------------------------------------------------------
// ReloadService
// ---------------------------------------------------------------------------

/// Background service that reloads the configuration on `SIGHUP` (and, when
/// `--watch-config` is set, on a periodic re-scan).
pub struct ReloadService {
    state: Arc<SharedState>,
    sites_dir: PathBuf,
    env: HashMap<String, String>,
    watch_config: bool,
}

impl ReloadService {
    /// Build the service for a running process.
    pub fn new(
        state: Arc<SharedState>,
        sites_dir: PathBuf,
        env: HashMap<String, String>,
        watch_config: bool,
    ) -> Self {
        Self {
            state,
            sites_dir,
            env,
            watch_config,
        }
    }

    /// Load, validate and (on success) apply the on-disk configuration.
    ///
    /// Returns `true` when the running configuration was replaced. `force`
    /// distinguishes an explicit reload (always attempted) from a
    /// `--watch-config` re-scan (skipped when the fingerprint is unchanged).
    /// A load or build failure is logged, counted as
    /// `config_reload_total{ok="false"}`, and leaves the old configuration
    /// serving.
    pub fn reload_once(&self, force: bool) -> bool {
        let cfg = match load_config(&self.sites_dir, &self.env) {
            Ok(cfg) => cfg,
            Err(err) => {
                record_config_reload(false);
                tracing::error!(
                    error = %err,
                    "configuration reload rejected; keeping the previous configuration"
                );
                return false;
            }
        };

        let fingerprint = config_fingerprint(&cfg);
        let current = self.state.fingerprint();
        if !force && fingerprint.as_str() == current.as_str() {
            return false;
        }

        match apply_new_config(&self.state, cfg) {
            Ok(()) => {
                record_config_reload(true);
                let applied = self.state.fingerprint();
                tracing::info!(
                    fingerprint = %applied.as_str(),
                    previous = %current.as_str(),
                    "configuration reloaded"
                );
                true
            }
            Err(err) => {
                record_config_reload(false);
                tracing::error!(
                    error = %err,
                    "configuration reload failed; keeping the previous configuration"
                );
                false
            }
        }
    }
}

#[async_trait]
impl BackgroundService for ReloadService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut hangup = hangup::install();
        let mut watch = if self.watch_config {
            let mut interval = tokio::time::interval(Duration::from_secs(WATCH_INTERVAL_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            Some(interval)
        } else {
            None
        };

        tracing::info!(
            sites_dir = %self.sites_dir.display(),
            watch_config = self.watch_config,
            "reload service started (SIGHUP triggers a hot reload)"
        );

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    tracing::info!("reload service shutting down");
                    break;
                }
                _ = hangup::wait(&mut hangup) => {
                    tracing::info!("received SIGHUP; reloading configuration");
                    self.reload_once(true);
                }
                _ = watch_tick(&mut watch) => {
                    self.reload_once(false);
                }
            }
        }
    }
}

/// Await the next `--watch-config` tick, or pend forever when disabled.
async fn watch_tick(watch: &mut Option<tokio::time::Interval>) {
    match watch {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Platform-specific `SIGHUP` handling.
///
/// Pingora's `run_forever` only installs `SIGINT`/`SIGTERM`/`SIGQUIT` handlers,
/// so `SIGHUP` is ours to own (P0 probe: `iris reload` left the process with
/// wait status 129 before P8).
mod hangup {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    /// The installed `SIGHUP` stream, or `None` when installation failed.
    pub type Stream = Option<Signal>;

    /// Install the `SIGHUP` handler (best-effort).
    pub fn install() -> Stream {
        match signal(SignalKind::hangup()) {
            Ok(stream) => Some(stream),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to install SIGHUP handler; 'iris reload' is disabled"
                );
                None
            }
        }
    }

    /// Await the next `SIGHUP` (or pend forever when installation failed).
    pub async fn wait(stream: &mut Stream) {
        match stream {
            Some(stream) => {
                stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::path::Path;

    use url::Url;

    use crate::config::{
        AuthConfig, AuthCredentials, Backend, ForwardSite, HealthCheck, HealthKind, LbMode,
        NovaConfig, OpenStackConfig, OpenStackSite,
    };

    fn forward_site(name: &str, domain: &str, port: u16) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            backends: vec![Backend {
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
                weight: 1,
            }],
            lb_mode: LbMode::RoundRobin,
            health_check: None,
        }
    }

    fn openstack_site(name: &str, base_domain: &str, cache_ttl_secs: u64) -> OpenStackSite {
        OpenStackSite {
            name: name.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            firewall_dir: None,
            openstack: OpenStackConfig {
                cache_ttl_secs,
                timeout_secs: 5,
                auth: AuthConfig {
                    api_url: Url::parse("http://127.0.0.1:9/v3").expect("valid test URL"),
                    credentials: AuthCredentials::ApplicationCredential {
                        id: "id".to_owned(),
                        secret: "secret".to_owned(),
                    },
                },
                nova: NovaConfig {
                    api_url: None,
                    all_tenants: true,
                },
            },
        }
    }

    fn config_a() -> Config {
        Config {
            forward: vec![forward_site("www", "www.example.com", 18081)],
            openstack: vec![openstack_site("rent", "rent.do.top", 86_400)],
        }
    }

    fn config_b() -> Config {
        Config {
            forward: vec![
                forward_site("www", "www.example.com", 18082),
                forward_site("api", "api.example.com", 18083),
            ],
            openstack: vec![openstack_site("rent", "rent.do.top", 43_200)],
        }
    }

    fn own_state(config: Config) -> SharedState {
        SharedState::try_new(config).expect("shared state builds")
    }

    // ----- fingerprint -----

    #[test]
    fn fingerprint_is_stable_and_content_sensitive() {
        let one = config_fingerprint(&config_a());
        let two = config_fingerprint(&config_a());
        assert_eq!(one, two, "identical configs fingerprint identically");

        let changed = config_fingerprint(&config_b());
        assert_ne!(one, changed, "a changed config fingerprints differently");

        // A single field change is enough.
        let mut tweaked = config_a();
        tweaked.forward[0].backends[0].addr = SocketAddr::from(([127, 0, 0, 1], 19999));
        assert_ne!(one, config_fingerprint(&tweaked));
    }

    #[test]
    fn credential_rotation_changes_the_fingerprint_but_not_debug() {
        let a = config_a();
        let mut b = config_a();
        match &mut b.openstack[0].openstack.auth.credentials {
            AuthCredentials::ApplicationCredential { secret, .. } => {
                *secret = "rotated-secret".to_owned();
            }
            other => panic!("expected an application credential, got {other:?}"),
        }

        // The redacting Debug cannot see the secret...
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "Debug must redact the secret so the two renderings are identical"
        );
        // ...but the fingerprint must, so --watch-config applies a rotation.
        assert_ne!(
            config_fingerprint(&a),
            config_fingerprint(&b),
            "a credential-only rotation must change the fingerprint"
        );
        assert_eq!(config_fingerprint(&a), config_fingerprint(&config_a()));
    }

    // ----- apply_new_config: success -----

    #[test]
    fn apply_rebuilds_and_swaps_every_field() {
        let state = own_state(config_a());
        let before_fp = state.fingerprint();

        apply_new_config(&state, config_b()).expect("valid config applies");

        // config + fingerprint
        assert_eq!(state.config().forward.len(), 2);
        assert_ne!(state.fingerprint().as_str(), before_fp.as_str());

        // routing table sees the new site
        assert!(state.snapshot().routing.lookup("api.example.com").is_some());

        // lbs map rebuilt and selects the new backend
        assert!(state.lb("api").is_some());
        assert_eq!(
            state.lb("www").expect("www lb").select(None),
            Some(SocketAddr::from(([127, 0, 0, 1], 18082)))
        );

        // resolvers map rebuilt for the changed openstack site
        assert!(state.resolver_for("rent").is_some());
    }

    #[test]
    fn apply_is_idempotent() {
        let state = own_state(config_a());
        apply_new_config(&state, config_b()).expect("first apply");
        let fp = state.fingerprint();
        let weighted = state.lb("www").expect("www lb").select(None);

        apply_new_config(&state, config_b()).expect("second apply");
        assert_eq!(state.fingerprint().as_str(), fp.as_str());
        assert_eq!(
            state.lb("www").expect("www lb").select(None),
            weighted,
            "re-applying the same config keeps the same pool"
        );
    }

    // ----- apply_new_config: failure keeps old -----

    #[test]
    fn build_failure_leaves_every_field_untouched() {
        let state = own_state(config_a());
        let snapshot_before = state.snapshot();

        // An https forward site whose certificate cannot be loaded fails the
        // certificate build (validate-first: the whole reload is rejected).
        let mut bad = config_b();
        bad.forward.push(ForwardSite {
            name: "tls".to_owned(),
            domain: "tls.example.com".to_owned(),
            http: true,
            https: true,
            cert_file: Some("/no/such/cert.pem".into()),
            key_file: Some("/no/such/key.pem".into()),
            backends: vec![Backend {
                addr: SocketAddr::from(([127, 0, 0, 1], 18084)),
                weight: 1,
            }],
            lb_mode: LbMode::RoundRobin,
            health_check: None,
        });

        let err = apply_new_config(&state, bad).expect_err("bad cert must reject the reload");
        assert!(matches!(err, ReloadError::Build { component, .. } if component == "certificates"));

        // One atomic snapshot: a rejected reload leaves the exact same snapshot
        // published (FIX #3).
        assert!(
            Arc::ptr_eq(&snapshot_before, &state.snapshot()),
            "a rejected reload must not publish a new snapshot"
        );

        // Sanity: the old pool still answers.
        assert_eq!(
            state.lb("www").expect("www lb").select(None),
            Some(SocketAddr::from(([127, 0, 0, 1], 18081)))
        );
    }

    #[test]
    fn bad_firewall_role_rejects_the_reload() {
        let state = own_state(config_a());

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("web.role"), "allowed_ports: [80\n").expect("write role");

        let mut bad = config_a();
        bad.openstack[0].firewall_dir = Some(dir.path().to_path_buf());

        let err = apply_new_config(&state, bad).expect_err("bad role must reject the reload");
        assert!(matches!(err, ReloadError::Build { component, .. } if component == "firewall"));
        assert_eq!(state.config().openstack[0].firewall_dir, None);
    }

    // ----- resolver cache retention -----

    #[test]
    fn resolver_is_reused_when_openstack_config_is_unchanged() {
        let state = own_state(config_a());
        let existing = state.resolver_for("rent").expect("resolver present");

        // Same openstack config, different forward lane.
        let mut same_openstack = config_a();
        same_openstack.forward.push(forward_site("api", "api.example.com", 18083));
        apply_new_config(&state, same_openstack).expect("apply");

        let after = state.resolver_for("rent").expect("resolver present");
        assert!(
            Arc::ptr_eq(&existing, &after),
            "an unchanged openstack site must keep its resolver (cache retained)"
        );
    }

    #[test]
    fn resolver_is_rebuilt_when_openstack_config_changes() {
        let state = own_state(config_a());
        let existing = state.resolver_for("rent").expect("resolver present");

        // Changes cache_ttl_secs -> the openstack config differs.
        let mut changed = config_a();
        changed.openstack[0].openstack.cache_ttl_secs = 43_200;
        apply_new_config(&state, changed).expect("apply");

        let after = state.resolver_for("rent").expect("resolver present");
        assert!(
            !Arc::ptr_eq(&existing, &after),
            "a changed openstack site must rebuild its resolver"
        );
    }

    // ----- forward LB reuse across a reload -----

    #[test]
    fn unchanged_forward_site_reuses_its_lb_engine() {
        let state = own_state(config_a());
        let before = state.lb("www").expect("www lb");

        // Only the openstack site changes; www's pool / algorithm / health spec
        // are equal, so its engine (and attached health probe) must be kept.
        let mut next = config_a();
        next.openstack[0].openstack.cache_ttl_secs = 43_200;
        apply_new_config(&state, next).expect("apply");

        let after = state.lb("www").expect("www lb");
        assert!(
            Arc::ptr_eq(&before, &after),
            "an unchanged forward site must reuse its engine so its health probe survives"
        );
    }

    #[test]
    fn changed_forward_backends_rebuild_the_lb_engine() {
        let state = own_state(config_a());
        let before = state.lb("www").expect("www lb");

        let mut changed = config_a();
        changed.forward[0].backends[0].addr = SocketAddr::from(([127, 0, 0, 1], 18099));
        apply_new_config(&state, changed).expect("apply");

        let after = state.lb("www").expect("www lb");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a changed backend set must rebuild the engine"
        );
        assert_eq!(
            after.select(None),
            Some(SocketAddr::from(([127, 0, 0, 1], 18099)))
        );
    }

    #[test]
    fn changed_lb_mode_or_health_check_rebuilds_the_engine() {
        // Algorithm change.
        let state = own_state(config_a());
        let before = state.lb("www").expect("www lb");
        let mut mode_changed = config_a();
        mode_changed.forward[0].lb_mode = LbMode::Random;
        apply_new_config(&state, mode_changed).expect("apply");
        assert!(
            !Arc::ptr_eq(&before, &state.lb("www").expect("www lb")),
            "a changed algorithm must rebuild the engine"
        );

        // Health-check change.
        let state = own_state(config_a());
        let before = state.lb("www").expect("www lb");
        let mut health_changed = config_a();
        health_changed.forward[0].health_check = Some(HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 5,
            timeout_secs: 2,
        });
        apply_new_config(&state, health_changed).expect("apply");
        assert!(
            !Arc::ptr_eq(&before, &state.lb("www").expect("www lb")),
            "a changed health check must rebuild the engine"
        );
    }

    fn health_checked_site(name: &str, port: u16) -> ForwardSite {
        let mut site = forward_site(name, &format!("{name}.example.com"), port);
        site.health_check = Some(HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 5,
            timeout_secs: 2,
        });
        site
    }

    #[test]
    fn reload_adds_a_health_checked_site_to_the_shared_registry() {
        let state = own_state(config_a());
        assert_eq!(state.probes().slot_count(), 0, "no health checks configured yet");

        let mut next = config_a();
        next.forward.push(health_checked_site("api", 18083));
        apply_new_config(&state, next).expect("apply");

        // The new site's single backend is probed by the shared registry and
        // its freshly built engine contributes to that same registry (FIX #2),
        // so the already-running health-check service gates it with no
        // re-attachment.
        assert_eq!(state.probes().slot_count(), 1);
        assert_eq!(
            state.health_registry().target_count(),
            2,
            "the new engine must contribute to the shared registry"
        );

        // Removing the site stops probing it (FIX #8).
        apply_new_config(&state, config_a()).expect("apply");
        assert_eq!(state.probes().slot_count(), 0, "a removed site stops being probed");
        assert!(state.lb("api").is_none());
    }

    #[test]
    fn reload_changing_a_health_interval_keeps_gating_the_site() {
        let mut initial = config_a();
        initial.forward[0].health_check = Some(HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 5,
            timeout_secs: 2,
        });
        let state = own_state(initial);
        assert_eq!(state.probes().slot_count(), 1);

        let mut changed = config_a();
        changed.forward[0].health_check = Some(HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 1,
            timeout_secs: 2,
        });
        apply_new_config(&state, changed).expect("apply");

        assert_eq!(
            state.probes().slot_count(),
            1,
            "the site stays probed after an interval change"
        );
    }

    // ----- ReloadService.load_config / reload_once -----

    fn write_forward_site(dir: &Path, port: u16) {
        let body = format!(
            "name: www\nmode: forward\ndomain: www.example.com\nhttp: true\nhttps: false\nbackends:\n  - addr: 127.0.0.1:{port}\n"
        );
        std::fs::write(dir.join("001-forward.yaml"), body).expect("write site");
    }

    fn service_for(sites_dir: &Path) -> ReloadService {
        let env = HashMap::new();
        let cfg = load_config(sites_dir, &env).expect("initial config loads");
        let state = Arc::new(own_state(cfg));
        ReloadService::new(state, sites_dir.to_path_buf(), env, false)
    }

    #[test]
    fn reload_once_applies_a_changed_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sites = dir.path().join("sites");
        std::fs::create_dir(&sites).expect("mkdir");
        write_forward_site(&sites, 18081);

        let service = service_for(&sites);
        let before = service.state.fingerprint();

        write_forward_site(&sites, 18082);
        assert!(service.reload_once(true), "a changed config reloads");
        assert_ne!(service.state.fingerprint().as_str(), before.as_str());
        assert_eq!(
            service.state.lb("www").expect("www lb").select(None),
            Some(SocketAddr::from(([127, 0, 0, 1], 18082)))
        );
    }

    #[test]
    fn reload_once_keeps_old_config_on_invalid_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sites = dir.path().join("sites");
        std::fs::create_dir(&sites).expect("mkdir");
        write_forward_site(&sites, 18081);

        let service = service_for(&sites);
        let before = service.state.fingerprint();

        std::fs::write(sites.join("002-bad.yaml"), ": : not yaml\n").expect("write bad");
        assert!(!service.reload_once(true), "an invalid config must not reload");
        assert_eq!(
            service.state.fingerprint().as_str(),
            before.as_str(),
            "the fingerprint must be unchanged after a rejected reload"
        );
        assert_eq!(
            service.state.lb("www").expect("www lb").select(None),
            Some(SocketAddr::from(([127, 0, 0, 1], 18081)))
        );
    }

    #[test]
    fn watch_rescan_skips_an_unchanged_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sites = dir.path().join("sites");
        std::fs::create_dir(&sites).expect("mkdir");
        write_forward_site(&sites, 18081);

        let service = service_for(&sites);
        assert!(
            !service.reload_once(false),
            "an unchanged config is a no-op for a watch re-scan"
        );
    }

    #[tokio::test]
    async fn service_stops_on_shutdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sites = dir.path().join("sites");
        std::fs::create_dir(&sites).expect("mkdir");
        write_forward_site(&sites, 18081);

        let service = service_for(&sites);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            service.start(rx).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).expect("signal shutdown");

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("reload service stops promptly")
            .expect("reload task joins");
    }
}
