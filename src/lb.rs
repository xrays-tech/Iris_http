//! P3 — forward load balancer (`plan/02-contracts.md`, `plan/06-p3-forward.md`).
//!
//! One [`LbEngine`] wraps a Pingora `LoadBalancer<S>` per `forward` site. The
//! selection algorithm is chosen by the type parameter (`RoundRobin` / `Random`
//! / `Consistent`), matching the P0 API probe: `LbMode` is *not* mapped to a
//! runtime enum of algorithms but to the generic parameter of the underlying
//! `LoadBalancer`.
//!
//! # Backend membership vs. health state
//!
//! A site's backend list comes from static YAML, so `update(backends)` must
//! replace the *membership* without discarding the backend's active health
//! state. It does this the same way Pingora's own reload does: the engine owns
//! one [`LoadBalancer`] whose [`Backends`] view is backed by a
//! [`MutableDiscovery`] (an atomically replaceable set — Pingora's
//! `discovery::Static` cannot be mutated after construction). `update` swaps
//! the discovery's set and re-runs `LoadBalancer::update()`, which re-runs
//! discovery against the *same* `Backends`/`HealthRegistry` and rebuilds only
//! the selector. The attached health check (and every backend's observed
//! health) therefore survives a membership change.
//!
//! Active probing is attached once, at startup, through
//! [`health_check_service`]: the probe is set on the engine's shared
//! [`HealthRegistry`] and driven by a [`HealthCheckService`]. Because every
//! freshly built engine keeps its own registry, a `forward` site whose pool /
//! algorithm / health spec is *unchanged* across a config reload must reuse its
//! existing `Arc<LbEngine>` (see `crate::reload`) — otherwise the startup
//! health-check service would keep probing a stale pool and the new engine
//! would select down backends again.
//!
//! Backends with `weight == 0` are **fully excluded** from the pool: they are
//! never added to the Pingora backend set, never selected, and therefore never
//! health-checked (`config.md` §3, `plan/06` §TDD).

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use pingora_load_balancing::discovery::ServiceDiscovery;
use pingora_load_balancing::health_check::{
    HealthCheck as HealthCheckProbe, HttpHealthCheck, TcpHealthCheck,
};
use pingora_load_balancing::selection::{
    BackendIter, BackendSelection, Consistent, Random, RoundRobin,
};
use pingora_load_balancing::{
    Backend as PoolBackend, Backends, HealthCheckService, HealthRegistry, LoadBalancer,
};

use crate::config::{Backend, HealthCheck, HealthKind, LbMode};

/// Upper bound on the number of candidate backends examined per selection.
///
/// Ketama hashing walks the ring linearly; the bound keeps a lookup from
/// degenerating when most candidates are unhealthy.
const SELECT_MAX_ITERATIONS: usize = 100;

/// A replaceable, in-process backend membership for service discovery.
///
/// Pingora's own `discovery::Static` exposes no way to change its set after
/// construction (`set` is crate-private), so this provides the single operation
/// a static YAML reload needs: an atomic membership swap. The [`Backends`] view
/// that owns it keeps its health registry — and thus its attached probe and the
/// observed health of every backend — across a replacement.
struct MutableDiscovery {
    backends: ArcSwap<BTreeSet<PoolBackend>>,
}

impl MutableDiscovery {
    fn new(backends: BTreeSet<PoolBackend>) -> Arc<Self> {
        Arc::new(Self {
            backends: ArcSwap::new(Arc::new(backends)),
        })
    }

    /// Replace the advertised membership.
    fn replace(&self, backends: BTreeSet<PoolBackend>) {
        self.backends.store(Arc::new(backends));
    }
}

/// A [`ServiceDiscovery`] handle that shares one [`MutableDiscovery`] with the
/// engine, so `LoadBalancer::update()` re-reads the set `update(backends)` just
/// installed.
struct SharedDiscovery(Arc<MutableDiscovery>);

#[async_trait]
impl ServiceDiscovery for SharedDiscovery {
    async fn discover(&self) -> pingora::Result<(BTreeSet<PoolBackend>, HashMap<u64, bool>)> {
        // No discovery-time enablement overrides: membership is exactly the set
        // the engine published.
        Ok(((*self.0.backends.load_full()).clone(), HashMap::new()))
    }
}

/// One load-balancer slot for a concrete selection `S`.
///
/// The `LoadBalancer` is built once and never rebuilt: membership changes go
/// through [`mutable`](Self::update)'s discovery swap plus
/// `LoadBalancer::update()`, which is what lets the [`HealthRegistry`] (and the
/// probe attached to it) outlive a reload.
pub struct LbSlot<S>
where
    S: BackendSelection + 'static,
{
    lb: LoadBalancer<S>,
    discovery: Arc<MutableDiscovery>,
    /// The registry the `LoadBalancer`'s `Backends` view contributes targets to
    /// and reads readiness from. Shared so [`health_check_service`] can attach
    /// the probe and drive it without touching the `LoadBalancer`.
    registry: Arc<HealthRegistry>,
    /// The installed health-check spec, if any. Guards the one-shot probe
    /// install (`HealthRegistry::set_health_check` may only be called once).
    health: OnceLock<HealthCheck>,
}

impl<S> LbSlot<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn new(backends: &[Backend]) -> Self {
        let discovery = MutableDiscovery::new(pool_set(backends));
        let registry = Arc::new(HealthRegistry::new());

        // A shared-registry view: this `LoadBalancer` does not schedule its own
        // probes (`Backends::owns_health_checks` is false); the probe attached
        // later to `registry` is driven by the `HealthCheckService` returned
        // from `health_check_service`.
        let view = Backends::new_with_health_registry(
            Box::new(SharedDiscovery(Arc::clone(&discovery))),
            Arc::clone(&registry),
        );

        let mut lb = LoadBalancer::<S>::from_backends(view);
        // Static YAML: discovery runs on build and on an explicit `update`, not
        // on a schedule.
        lb.update_frequency = None;

        prime::<S>(&lb);

        Self {
            lb,
            discovery,
            registry,
            health: OnceLock::new(),
        }
    }

    /// Select a backend for `key`. `key` is only meaningful for consistent
    /// hashing; round-robin and random ignore it (P0 probe).
    fn select(&self, key: &[u8]) -> Option<SocketAddr> {
        self.lb
            .select(key, SELECT_MAX_ITERATIONS)
            .and_then(|backend| backend.addr.as_inet().copied())
    }

    /// Replace the backend pool.
    ///
    /// The member set is swapped in place through the shared discovery and the
    /// selector is rebuilt against the *same* `Backends`/`HealthRegistry`, so
    /// the attached health probe and the observed health of the retained
    /// backends are preserved.
    fn update(&self, backends: &[Backend]) {
        self.discovery.replace(pool_set(backends));
        match poll_once(self.lb.update()) {
            Some(Ok(())) => {}
            Some(Err(err)) => tracing::error!(
                error = %err,
                "forward load balancer membership update failed"
            ),
            None => tracing::warn!(
                "forward load balancer membership update did not complete synchronously"
            ),
        }
    }

    /// Install the health-check probe on this slot's shared registry.
    ///
    /// Installed at most once (a registry accepts a single probe); later calls
    /// are ignored, matching the "attach once at startup" contract.
    fn install_health_check(&self, hc: &HealthCheck) {
        if self.health.set(hc.clone()).is_ok() {
            self.registry.set_health_check(build_probe(hc));
        }
    }

    /// Run the active health-check loop for this slot's registry until
    /// `shutdown`.
    async fn run(&self, shutdown: ShutdownWatch) {
        let mut service = HealthCheckService::new(Arc::clone(&self.registry));
        service.health_check_frequency = self
            .health
            .get()
            .map(|hc| Duration::from_secs(hc.interval_secs.max(1)));
        service.start(shutdown).await;
    }

    /// The shared health registry (test seam: proves `update` keeps it).
    #[cfg(test)]
    pub(crate) fn health_registry(&self) -> Arc<HealthRegistry> {
        Arc::clone(&self.registry)
    }
}

/// The forward load-balancing engine for one site.
///
/// See the module docs for the algorithm-to-type mapping.
pub enum LbEngine {
    /// `round_robin` and `weighted_round_robin` (Pingora weights the backends).
    RR(LbSlot<RoundRobin>),
    /// `random`.
    Random(LbSlot<Random>),
    /// `ketama` (consistent hashing on the client IP key).
    Ketama(LbSlot<Consistent>),
}

impl LbEngine {
    /// Pick a backend. `hash_key` (the client IP) is only used by ketama;
    /// round-robin and random ignore it.
    pub fn select(&self, hash_key: Option<IpAddr>) -> Option<SocketAddr> {
        let key = hash_key.map(ip_key_bytes);
        let key = key.as_deref().unwrap_or(&[]);
        match self {
            LbEngine::RR(slot) => slot.select(key),
            LbEngine::Random(slot) => slot.select(key),
            LbEngine::Ketama(slot) => slot.select(key),
        }
    }

    /// Replace the backend pool, preserving the algorithm and health check.
    ///
    /// Membership is refreshed against the engine's existing `Backends`, so an
    /// attached health-check probe (and the observed health of every retained
    /// backend) keeps working across the update.
    pub fn update(&self, backends: &[Backend]) {
        match self {
            LbEngine::RR(slot) => slot.update(backends),
            LbEngine::Random(slot) => slot.update(backends),
            LbEngine::Ketama(slot) => slot.update(backends),
        }
    }

    fn install_health_check(&self, hc: &HealthCheck) {
        match self {
            LbEngine::RR(slot) => slot.install_health_check(hc),
            LbEngine::Random(slot) => slot.install_health_check(hc),
            LbEngine::Ketama(slot) => slot.install_health_check(hc),
        }
    }

    async fn run(&self, shutdown: ShutdownWatch) {
        match self {
            LbEngine::RR(slot) => slot.run(shutdown).await,
            LbEngine::Random(slot) => slot.run(shutdown).await,
            LbEngine::Ketama(slot) => slot.run(shutdown).await,
        }
    }

    /// The shared health registry of this engine (test seam).
    #[cfg(test)]
    pub(crate) fn health_registry(&self) -> Arc<HealthRegistry> {
        match self {
            LbEngine::RR(slot) => slot.health_registry(),
            LbEngine::Random(slot) => slot.health_registry(),
            LbEngine::Ketama(slot) => slot.health_registry(),
        }
    }
}

/// Build an engine for `backends` under `mode`.
///
/// `weight == 0` backends are dropped here (and by [`enabled_backends`]).
pub fn build_lb(backends: &[Backend], mode: &LbMode) -> Arc<LbEngine> {
    let engine = match mode {
        LbMode::RoundRobin | LbMode::WeightedRoundRobin => LbEngine::RR(LbSlot::new(backends)),
        LbMode::Random => LbEngine::Random(LbSlot::new(backends)),
        LbMode::Ketama => LbEngine::Ketama(LbSlot::new(backends)),
    };
    Arc::new(engine)
}

/// The config backends that participate in selection (`weight > 0`).
pub fn enabled_backends(backends: &[Backend]) -> Vec<Backend> {
    backends.iter().filter(|b| b.weight > 0).cloned().collect()
}

/// Attach an active health check to `lb` and return the background service
/// that runs it.
///
/// The probe is installed once on the engine's shared health registry; failed
/// backends are excluded from [`LbEngine::select`] and recover automatically
/// once they pass again. Because the probe lives on the engine's own registry,
/// the reload path must reuse an unchanged site's `Arc<LbEngine>` (keeping this
/// service pointed at the same pool) — see `crate::reload::apply_new_config`.
pub fn health_check_service(lb: Arc<LbEngine>, hc: &HealthCheck) -> impl BackgroundService {
    lb.install_health_check(hc);
    LbHealthService { engine: lb }
}

/// Background service wrapper around an [`LbEngine`]'s active probing.
struct LbHealthService {
    engine: Arc<LbEngine>,
}

#[async_trait]
impl BackgroundService for LbHealthService {
    async fn start(&self, shutdown: ShutdownWatch) {
        self.engine.run(shutdown).await;
    }
}

/// Pure health-transition state machine (proxy-side observation helper).
///
/// Pingora owns the real probes; this mirrors a threshold-of-one transition so
/// the "observed a change -> became unhealthy/healthy" decision is unit
/// testable without a live backend (`plan/06` §TDD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthState {
    healthy: bool,
}

impl HealthState {
    /// New state with the given initial health.
    pub fn new(healthy: bool) -> Self {
        Self { healthy }
    }

    /// Whether the state currently considers the backend healthy.
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    /// Apply one observation and return the resulting state.
    pub fn next(self, observed: bool) -> Self {
        Self { healthy: observed }
    }
}

/// Build a concrete Pingora probe from the site's [`HealthCheck`] spec.
fn build_probe(hc: &HealthCheck) -> Box<dyn HealthCheckProbe + Send + Sync + 'static> {
    let timeout = Duration::from_secs(hc.timeout_secs.max(1));
    match hc.kind {
        HealthKind::Tcp => {
            let mut probe = TcpHealthCheck::new();
            probe.peer_template.options.connection_timeout = Some(timeout);
            probe
        }
        HealthKind::Http => {
            let mut probe = HttpHealthCheck::new("", false);
            probe.peer_template.options.connection_timeout = Some(timeout);
            probe.peer_template.options.read_timeout = Some(timeout);
            if let Some(path) = hc.http_path.as_deref() {
                if let Err(err) = probe.req.set_raw_path(path.as_bytes()) {
                    tracing::warn!(
                        error = %err,
                        path,
                        "invalid health-check path; keeping the default '/'"
                    );
                }
            }
            Box::new(probe)
        }
    }
}

/// The Pingora pool for `backends` (weight-0 entries excluded).
fn pool_set(backends: &[Backend]) -> BTreeSet<PoolBackend> {
    enabled_backends(backends)
        .iter()
        .filter_map(pool_backend)
        .collect()
}

/// Run the initial discovery pass to populate the selector.
///
/// The mutable discovery future completes on the first poll, so a single poll
/// is sufficient and cannot deadlock.
fn prime<S>(lb: &LoadBalancer<S>)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    match poll_once(lb.update()) {
        Some(Ok(())) => {}
        Some(Err(err)) => {
            tracing::error!(error = %err, "forward load balancer initial discovery failed");
        }
        None => {
            tracing::warn!("forward load balancer initial discovery did not complete synchronously");
        }
    }
}

/// Poll a future exactly once with a no-op waker.
fn poll_once<F: Future>(fut: F) -> Option<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// Convert a config backend into a Pingora pool backend.
fn pool_backend(backend: &Backend) -> Option<PoolBackend> {
    PoolBackend::new_with_weight(&backend.addr.to_string(), backend.weight as usize).ok()
}

/// The ketama hash key for a client address: the raw IP octets.
fn ip_key_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn backend(port: u16, weight: u32) -> Backend {
        Backend {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            weight,
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn tcp_health() -> HealthCheck {
        HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 5,
            timeout_secs: 2,
        }
    }

    // ----- LbMode -> selection mapping -----

    #[test]
    fn lb_mode_maps_to_engine_variant() {
        let backends = [backend(1, 1)];
        assert!(matches!(
            *build_lb(&backends, &LbMode::RoundRobin),
            LbEngine::RR(_)
        ));
        assert!(matches!(
            *build_lb(&backends, &LbMode::WeightedRoundRobin),
            LbEngine::RR(_)
        ));
        assert!(matches!(
            *build_lb(&backends, &LbMode::Random),
            LbEngine::Random(_)
        ));
        assert!(matches!(
            *build_lb(&backends, &LbMode::Ketama),
            LbEngine::Ketama(_)
        ));
    }

    // ----- backend construction / weight handling -----

    #[test]
    fn enabled_backends_excludes_weight_zero() {
        let backends = [backend(1, 1), backend(2, 0), backend(3, 5)];
        let enabled = enabled_backends(&backends);
        assert_eq!(enabled.len(), 2);
        assert!(enabled.iter().all(|b| b.weight > 0));
        assert!(!enabled.iter().any(|b| b.addr == addr(2)));
    }

    #[test]
    fn weight_zero_backend_is_never_selected() {
        let backends = [backend(1, 1), backend(2, 0)];
        let lb = build_lb(&backends, &LbMode::RoundRobin);
        for _ in 0..10 {
            let selected = lb.select(None);
            assert_eq!(selected, Some(addr(1)));
        }
    }

    #[test]
    fn empty_pool_selects_none() {
        let lb = build_lb(&[], &LbMode::RoundRobin);
        assert_eq!(lb.select(None), None);
    }

    // ----- selection semantics -----

    #[test]
    fn round_robin_alternates() {
        let backends = [backend(1, 1), backend(2, 1)];
        let lb = build_lb(&backends, &LbMode::RoundRobin);
        let seen: Vec<Option<SocketAddr>> = (0..4).map(|_| lb.select(None)).collect();
        assert_eq!(
            seen,
            vec![Some(addr(1)), Some(addr(2)), Some(addr(1)), Some(addr(2))]
        );
    }

    #[test]
    fn weighted_round_robin_honours_weights() {
        // weight 3 : 1 -> three picks of the heavy backend, then one of the light.
        let backends = [backend(1, 3), backend(2, 1)];
        let lb = build_lb(&backends, &LbMode::WeightedRoundRobin);
        let mut counts: HashMap<SocketAddr, usize> = HashMap::new();
        for _ in 0..8 {
            if let Some(selected) = lb.select(None) {
                *counts.entry(selected).or_default() += 1;
            }
        }
        assert_eq!(counts.get(&addr(1)), Some(&6));
        assert_eq!(counts.get(&addr(2)), Some(&2));
    }

    #[test]
    fn random_selects_a_configured_backend() {
        let backends = [backend(1, 1), backend(2, 1)];
        let lb = build_lb(&backends, &LbMode::Random);
        for _ in 0..50 {
            let selected = lb.select(None);
            assert!(selected == Some(addr(1)) || selected == Some(addr(2)));
        }
    }

    #[test]
    fn ketama_is_stable_for_a_given_client() {
        let backends = [backend(1, 1), backend(2, 1)];
        let lb = build_lb(&backends, &LbMode::Ketama);
        let client = IpAddr::from([203, 0, 113, 7]);
        let first = lb.select(Some(client));
        assert!(first.is_some());
        for _ in 0..10 {
            assert_eq!(lb.select(Some(client)), first, "ketama must be sticky");
        }
    }

    // ----- update -----

    #[test]
    fn update_swaps_the_backend_pool() {
        let lb = build_lb(&[backend(1, 1)], &LbMode::RoundRobin);
        assert_eq!(lb.select(None), Some(addr(1)));
        lb.update(&[backend(9, 1)]);
        assert_eq!(lb.select(None), Some(addr(9)));
    }

    #[test]
    fn update_drops_weight_zero_backends() {
        let lb = build_lb(&[backend(1, 1)], &LbMode::RoundRobin);
        lb.update(&[backend(1, 0), backend(2, 1)]);
        for _ in 0..4 {
            assert_eq!(lb.select(None), Some(addr(2)));
        }
    }

    #[test]
    fn update_preserves_the_health_registry() {
        let lb = build_lb(&[backend(1, 1), backend(2, 1)], &LbMode::RoundRobin);
        lb.install_health_check(&tcp_health());

        let registry = lb.health_registry();
        assert_eq!(
            registry.target_count(),
            2,
            "the probe registry must see the primed pool"
        );

        lb.update(&[backend(9, 1)]);

        let after = lb.health_registry();
        assert!(
            Arc::ptr_eq(&registry, &after),
            "update must keep the same health registry, so the attached probe survives"
        );
        assert_eq!(
            after.target_count(),
            1,
            "the membership refresh must reach the shared registry"
        );
        assert_eq!(lb.select(None), Some(addr(9)));
    }

    #[test]
    fn install_health_check_is_idempotent() {
        // A registry accepts a single probe; a second install must be a no-op
        // rather than a panic.
        let lb = build_lb(&[backend(1, 1)], &LbMode::RoundRobin);
        let hc = tcp_health();
        lb.install_health_check(&hc);
        lb.install_health_check(&hc);
        assert_eq!(lb.select(None), Some(addr(1)));
    }

    // ----- pure health state machine -----

    #[test]
    fn health_state_flips_to_observed_value() {
        let healthy = HealthState::new(true);
        assert!(healthy.is_healthy());
        let unhealthy = healthy.next(false);
        assert!(!unhealthy.is_healthy());
        // Idempotent while the observation does not change.
        assert_eq!(unhealthy.next(false), unhealthy);
        assert!(unhealthy.next(true).is_healthy());
    }

    // ----- health check probe construction -----

    #[test]
    fn build_probe_accepts_tcp_and_http_specs() {
        let tcp = HealthCheck {
            kind: HealthKind::Tcp,
            http_path: None,
            interval_secs: 5,
            timeout_secs: 2,
        };
        let http = HealthCheck {
            kind: HealthKind::Http,
            http_path: Some("/healthz".to_owned()),
            interval_secs: 5,
            timeout_secs: 2,
        };
        // Both must construct without panicking.
        let _ = build_probe(&tcp);
        let _ = build_probe(&http);
    }

    #[test]
    fn ip_key_bytes_uses_address_octets() {
        assert_eq!(ip_key_bytes(IpAddr::from([10, 0, 0, 1])), vec![10, 0, 0, 1]);
        assert_eq!(ip_key_bytes(IpAddr::from([0u16; 8])).len(), 16);
    }
}
