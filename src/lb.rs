//! P3 — forward load balancer (`plan/02-contracts.md`, `plan/06-p3-forward.md`).
//!
//! One [`LbEngine`] wraps a Pingora `LoadBalancer<S>` per `forward` site. The
//! selection algorithm is chosen by the type parameter (`RoundRobin` / `Random`
//! / `Consistent`), matching the P0 API probe: `LbMode` is *not* mapped to a
//! runtime enum of algorithms but to the generic parameter of the underlying
//! `LoadBalancer`.
//!
//! Hot updates (`update`) rebuild a fresh `LoadBalancer` from the new backend
//! set and atomically swap the interior [`arc_swap::ArcSwap`]. This is the
//! correct static-config behaviour: Pingora's own `LoadBalancer::update()`
//! re-runs the *service discovery* pass, which is unsuitable for a static YAML
//! backend list.
//!
//! Backends with `weight == 0` are **fully excluded** from the pool: they are
//! never added to the Pingora backend set, never selected, and therefore never
//! health-checked (`config.md` §3, `plan/06` §TDD).

use std::collections::BTreeSet;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use pingora_load_balancing::discovery::Static;
use pingora_load_balancing::health_check::{
    HealthCheck as HealthCheckProbe, HttpHealthCheck, TcpHealthCheck,
};
use pingora_load_balancing::selection::{BackendIter, BackendSelection, Consistent, Random, RoundRobin};
use pingora_load_balancing::{Backend as PoolBackend, Backends, LoadBalancer};

use crate::config::{Backend, HealthCheck, HealthKind, LbMode};

/// Upper bound on the number of candidate backends examined per selection.
///
/// Ketama hashing walks the ring linearly; the bound keeps a lookup from
/// degenerating when most candidates are unhealthy.
const SELECT_MAX_ITERATIONS: usize = 100;

/// A `LoadBalancer<S>` plus the configuration needed to rebuild it.
///
/// Kept private to the engine: it deliberately holds the *config* backends
/// (all of them, including disabled ones) so `install_health_check` can rebuild
/// without re-reading `Config`.
pub struct LbInner<S>
where
    S: BackendSelection + 'static,
{
    lb: LoadBalancer<S>,
    /// The config backends the pool was built from (weight > 0 already
    /// filtered by the builder; retained verbatim for rebuilds).
    backends: Vec<Backend>,
    /// The health-check spec, if one is installed. Preserved across rebuilds so
    /// a config reload keeps the same active probing.
    health: Option<HealthCheck>,
}

/// One interior-swappable load-balancer slot for a concrete selection `S`.
pub struct LbSlot<S>
where
    S: BackendSelection + 'static,
{
    inner: ArcSwap<LbInner<S>>,
}

impl<S> LbSlot<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn new(backends: &[Backend], health: Option<HealthCheck>) -> Self {
        Self {
            inner: ArcSwap::from_pointee(build_inner::<S>(backends, health)),
        }
    }

    /// Select a backend for `key`. `key` is only meaningful for consistent
    /// hashing; round-robin and random ignore it (P0 probe).
    fn select(&self, key: &[u8]) -> Option<SocketAddr> {
        let inner = self.inner.load_full();
        inner
            .lb
            .select(key, SELECT_MAX_ITERATIONS)
            .and_then(|backend| backend.addr.as_inet().copied())
    }

    /// Rebuild the pool from `backends`, preserving the installed health check.
    fn update(&self, backends: &[Backend]) {
        let health = self.inner.load_full().health.clone();
        self.inner
            .store(Arc::new(build_inner::<S>(backends, health)));
    }

    /// Install (or replace) the health-check probe, rebuilding the pool.
    fn install_health_check(&self, hc: &HealthCheck) {
        let backends = self.inner.load_full().backends.clone();
        self.inner
            .store(Arc::new(build_inner::<S>(&backends, Some(hc.clone()))));
    }

    /// Run the pool as a background service (drives discovery once and the
    /// active health checks on their interval).
    async fn run(&self, shutdown: ShutdownWatch) {
        let inner = self.inner.load_full();
        inner.lb.run(shutdown, None).await;
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
    /// Rebuilds a fresh `LoadBalancer` and swaps it in atomically; in-flight
    /// selections keep using the snapshot they already loaded.
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
}

/// Build an engine for `backends` under `mode`.
///
/// `weight == 0` backends are dropped here (and by [`enabled_backends`]).
pub fn build_lb(backends: &[Backend], mode: &LbMode) -> Arc<LbEngine> {
    let engine = match mode {
        LbMode::RoundRobin | LbMode::WeightedRoundRobin => {
            LbEngine::RR(LbSlot::new(backends, None))
        }
        LbMode::Random => LbEngine::Random(LbSlot::new(backends, None)),
        LbMode::Ketama => LbEngine::Ketama(LbSlot::new(backends, None)),
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
/// The service runs the pool's discovery once and then probes every backend on
/// `hc.interval_secs`; failed backends are excluded from [`LbEngine::select`]
/// and recover automatically once they pass again.
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

/// Build a fresh [`LbInner`] from `backends` (weight-0 entries excluded).
fn build_inner<S>(backends: &[Backend], health: Option<HealthCheck>) -> LbInner<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let enabled = enabled_backends(backends);
    let pool: BTreeSet<PoolBackend> = enabled.iter().filter_map(pool_backend).collect();

    let mut view = Backends::new(Static::new(pool));
    if let Some(hc) = health.as_ref() {
        view.set_health_check(build_probe(hc));
    }

    let mut lb = LoadBalancer::<S>::from_backends(view);
    lb.health_check_frequency = health
        .as_ref()
        .map(|hc| Duration::from_secs(hc.interval_secs.max(1)));
    // The backend list is static YAML, so discovery runs exactly once (at
    // `prime`) rather than on a schedule.
    lb.update_frequency = None;

    prime::<S>(&lb);

    LbInner {
        lb,
        backends: enabled,
        health,
    }
}

/// Run the initial discovery pass to populate the selector.
///
/// The static discovery future completes on the first poll, so a single poll is
/// sufficient and cannot deadlock.
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
