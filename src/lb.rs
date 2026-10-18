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
//! # One shared health registry (FIX #2)
//!
//! Every forward engine contributes to **one** [`HealthRegistry`] owned by
//! `SharedState`, driven by a single [`HealthCheckService`]
//! ([`health_check_service`]). A [`HealthRegistry`] can hold only one probe, so
//! [`ProbeRegistry`] is that probe: it dispatches each target to the site's
//! configured probe (TCP/HTTP, path, timeout) and enforces the site's
//! `interval_secs`. Because the registry is shared and long-lived, a site
//! added or changed by a reload is gated by the already-running probe service
//! with no re-attachment step; a site removed by a reload drops its view
//! (deregistering its targets) when its last `Arc<LbEngine>` is released.
//!
//! Backends with `weight == 0` are **fully excluded** from the pool: they are
//! never added to the Pingora backend set, never selected, and therefore never
//! health-checked (`config.md` §3, `plan/06` §TDD).

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
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

use crate::config::{Backend, BackendTarget, Config, HealthCheck, HealthKind, LbMode};

/// Upper bound on the number of candidate backends examined per selection.
///
/// Ketama hashing walks the ring linearly; the bound keeps a lookup from
/// degenerating when most candidates are unhealthy.
const SELECT_MAX_ITERATIONS: usize = 100;

/// Cadence of the single shared health-check service.
///
/// The service ticks on one fixed cadence; [`ProbeRegistry`] enforces each
/// site's `interval_secs` by skipping targets whose interval has not elapsed,
/// so this is only the resolution (and the minimum effective interval).
pub const HEALTH_CHECK_TICK: Duration = Duration::from_secs(1);

/// A replaceable, in-process backend membership for service discovery.
///
/// Pingora's own `discovery::Static` exposes no way to change its set after
/// construction (`set` is crate-private), so this provides the single operation
/// a static YAML reload needs: an atomic membership swap. The [`Backends`] view
/// that owns it keeps its health registry — and thus the observed health of
/// every backend — across a replacement.
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
/// through [`update`](Self::update)'s discovery swap plus `LoadBalancer::update()`,
/// which is what lets the shared [`HealthRegistry`] outlive a reload.
pub struct LbSlot<S>
where
    S: BackendSelection + 'static,
{
    lb: LoadBalancer<S>,
    discovery: Arc<MutableDiscovery>,
}

impl<S> LbSlot<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn new(backends: &[Backend], registry: Arc<HealthRegistry>) -> Self {
        let discovery = MutableDiscovery::new(pool_set(backends));

        // A shared-registry view: this `LoadBalancer` does not schedule its own
        // probes (`Backends::owns_health_checks` is false); the single
        // `HealthCheckService` drives the shared registry instead. The view owns
        // the registry `Arc`, so it stays alive as long as the engine does.
        let view = Backends::new_with_health_registry(
            Box::new(SharedDiscovery(Arc::clone(&discovery))),
            registry,
        );

        let mut lb = LoadBalancer::<S>::from_backends(view);
        // Static YAML: discovery runs on build and on an explicit `update`, not
        // on a schedule.
        lb.update_frequency = None;

        prime::<S>(&lb);

        Self { lb, discovery }
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
    /// the shared probe and the observed health of the retained backends are
    /// preserved.
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
        let key = key.as_ref().map(IpKey::as_bytes).unwrap_or(&[]);
        match self {
            LbEngine::RR(slot) => slot.select(key),
            LbEngine::Random(slot) => slot.select(key),
            LbEngine::Ketama(slot) => slot.select(key),
        }
    }

    /// Replace the backend pool, preserving the algorithm and health state.
    pub fn update(&self, backends: &[Backend]) {
        match self {
            LbEngine::RR(slot) => slot.update(backends),
            LbEngine::Random(slot) => slot.update(backends),
            LbEngine::Ketama(slot) => slot.update(backends),
        }
    }
}

/// Build an engine for `backends` under `mode`, contributing to `registry`.
///
/// `weight == 0` backends are dropped here (and by [`enabled_backends`]).
pub fn build_lb(
    backends: &[Backend],
    mode: &LbMode,
    registry: &Arc<HealthRegistry>,
) -> Arc<LbEngine> {
    let engine = match mode {
        LbMode::RoundRobin | LbMode::WeightedRoundRobin => {
            LbEngine::RR(LbSlot::new(backends, Arc::clone(registry)))
        }
        LbMode::Random => LbEngine::Random(LbSlot::new(backends, Arc::clone(registry))),
        LbMode::Ketama => LbEngine::Ketama(LbSlot::new(backends, Arc::clone(registry))),
    };
    Arc::new(engine)
}

/// The config backends that participate in selection (`weight > 0`).
pub fn enabled_backends(backends: &[Backend]) -> Vec<Backend> {
    backends.iter().filter(|b| b.weight > 0).cloned().collect()
}

/// Build the background service that drives the shared forward health registry.
///
/// One service covers every forward site, so sites added or changed by a reload
/// are gated without a per-site service (FIX #2). The registry must have a probe
/// configured before this service starts; [`ProbeRegistry`] is that probe.
pub fn health_check_service(registry: Arc<HealthRegistry>) -> impl BackgroundService {
    let mut service = HealthCheckService::new(registry);
    service.health_check_frequency = Some(HEALTH_CHECK_TICK);
    service
}

// ---------------------------------------------------------------------------
// ProbeRegistry: the single dispatching probe behind the shared registry.
// ---------------------------------------------------------------------------

/// The process-wide active-probe dispatcher shared by every forward engine.
///
/// A [`HealthRegistry`] holds a single probe, so this implements
/// [`HealthCheckProbe`] and routes each target to a per-site probe built from
/// the site's [`HealthCheck`] spec, enforcing each site's `interval_secs`
/// against the shared service's fixed tick. [`sync`](Self::sync) rebuilds the
/// target table from a [`Config`] on startup and after every successful reload;
/// a target with no spec is left healthy (the site configured no health check).
pub struct ProbeRegistry {
    slots: Mutex<HashMap<u64, Arc<ProbeSlot>>>,
}

/// One target's probe plus its interval bookkeeping.
struct ProbeSlot {
    spec: HealthCheck,
    probe: Box<dyn HealthCheckProbe + Send + Sync>,
    timing: Mutex<ProbeTiming>,
}

/// The last observed result and when it was taken (guards the per-site interval).
struct ProbeTiming {
    last: Option<(bool, Instant)>,
}

impl ProbeSlot {
    fn new(spec: HealthCheck) -> Self {
        let probe = build_probe(&spec);
        Self {
            spec,
            probe,
            timing: Mutex::new(ProbeTiming { last: None }),
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(self.spec.interval_secs.max(1))
    }

    /// Whether `interval_secs` has elapsed since the last observation (or the
    /// target has never been observed).
    fn is_due(&self) -> bool {
        let timing = lock(&self.timing);
        match timing.last {
            None => true,
            Some((_, at)) => at.elapsed() >= self.interval(),
        }
    }

    /// The last observed health, if the target was ever checked.
    fn last_health(&self) -> Option<bool> {
        lock(&self.timing).last.map(|(healthy, _)| healthy)
    }

    fn record(&self, healthy: bool) {
        lock(&self.timing).last = Some((healthy, Instant::now()));
    }
}

impl ProbeRegistry {
    /// Create an empty probe registry.
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Rebuild the target table from `config`.
    ///
    /// Only `forward` sites with a `health_check` contribute; a target whose
    /// spec is unchanged keeps its previous slot (and observed health). Targets
    /// absent from `config` are dropped, so a site/health-check removed by a
    /// reload stops being probed (FIX #8).
    pub fn sync(&self, config: &Config) {
        let mut slots = lock(&self.slots);
        let mut next: HashMap<u64, Arc<ProbeSlot>> = HashMap::with_capacity(slots.len());
        for site in &config.forward {
            let Some(hc) = site.health_check.as_ref() else {
                continue;
            };
            for backend in enabled_backends(&site.backends) {
                let Some(pool) = pool_backend(&backend) else {
                    continue;
                };
                let key = backend_key(&pool);
                let slot = match slots.get(&key) {
                    Some(existing) if existing.spec == *hc => Arc::clone(existing),
                    _ => Arc::new(ProbeSlot::new(hc.clone())),
                };
                next.insert(key, slot);
            }
        }
        *slots = next;
    }

    /// Number of probed targets (test seam).
    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        lock(&self.slots).len()
    }
}

impl Default for ProbeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The boxed [`HealthCheckProbe`] handed to the shared registry; delegates to a
/// shared [`ProbeRegistry`].
pub struct SharedProbe(pub Arc<ProbeRegistry>);

#[async_trait]
impl HealthCheckProbe for SharedProbe {
    async fn check(&self, target: &PoolBackend) -> pingora::Result<()> {
        let key = backend_key(target);
        let slot = {
            let slots = lock(&self.0.slots);
            slots.get(&key).cloned()
        };
        let Some(slot) = slot else {
            // No health check configured for this target: leave it healthy.
            return Ok(());
        };

        if !slot.is_due() {
            // Repeat the last result so an intermediate registry pass cannot
            // flip a not-yet-rechecked target back to healthy.
            return match slot.last_health() {
                Some(false) => Err(pingora::Error::explain(
                    pingora::ErrorType::InternalError,
                    "cached unhealthy: health-check interval has not elapsed",
                )),
                _ => Ok(()),
            };
        }

        let result = slot.probe.check(target).await;
        slot.record(result.is_ok());
        result
    }

    fn health_threshold(&self, _success: bool) -> usize {
        // The built-in TCP/HTTP probes use a threshold of one; the dispatcher
        // aggregates targets that all do.
        1
    }
}

/// Lock a mutex, recovering from poisoning (the guarded data is plain state).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stable key for a pool backend (mirrors the registry's own identity: addr +
/// weight; `ext` is ignored by `Backend`'s `Hash`).
fn backend_key(backend: &PoolBackend) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    backend.hash(&mut hasher);
    hasher.finish()
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
///
/// An [`BackendTarget::Instance`] entry has no address to probe/select yet, so
/// it is skipped here: its pool is owned by the instance router (C3). Until
/// then an instance-only site yields an empty pool (`select` returns `None`).
fn pool_backend(backend: &Backend) -> Option<PoolBackend> {
    match &backend.target {
        BackendTarget::Addr(addr) => {
            PoolBackend::new_with_weight(&addr.to_string(), backend.weight as usize).ok()
        }
        BackendTarget::Instance { .. } => None,
    }
}

/// A ketama select key built inline from a client address.
///
/// Holds the address octets on the stack (no heap) so a per-request selection
/// allocates nothing; [`as_bytes`](Self::as_bytes) yields exactly the octets the
/// previous `Vec::from(octets)` produced — 4 for IPv4, 16 for IPv6 (including
/// IPv4-mapped IPv6, whose mapping is preserved unchanged).
enum IpKey {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl IpKey {
    /// The raw octets, borrowed from the inline buffer.
    fn as_bytes(&self) -> &[u8] {
        match self {
            IpKey::V4(bytes) => bytes,
            IpKey::V6(bytes) => bytes,
        }
    }
}

/// The ketama hash key for a client address: the raw IP octets, inline.
fn ip_key_bytes(ip: IpAddr) -> IpKey {
    match ip {
        IpAddr::V4(v4) => IpKey::V4(v4.octets()),
        IpAddr::V6(v6) => IpKey::V6(v6.octets()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::config::{ForwardSite, OpenStackSite};

    fn backend(port: u16, weight: u32) -> Backend {
        Backend {
            target: BackendTarget::Addr(SocketAddr::from(([127, 0, 0, 1], port))),
            weight,
        }
    }

    fn instance_backend(instance: &str, port: u16, weight: u32) -> Backend {
        Backend {
            target: BackendTarget::Instance {
                instance: instance.to_owned(),
                port,
            },
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

    /// An engine with its own throwaway registry (selection-only tests).
    fn engine(backends: &[Backend], mode: &LbMode) -> Arc<LbEngine> {
        build_lb(backends, mode, &Arc::new(HealthRegistry::new()))
    }

    fn forward_site(name: &str, backends: Vec<Backend>, health_check: Option<HealthCheck>) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: format!("{name}.example.com"),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            openstack_env: None,
            backends,
            lb_mode: LbMode::RoundRobin,
            health_check,
        }
    }

    fn config(forward: Vec<ForwardSite>) -> Config {
        Config {
            forward,
            openstack: Vec::<OpenStackSite>::new(),
            openstack_envs: HashMap::new(),
        }
    }

    // ----- LbMode -> selection mapping -----

    #[test]
    fn lb_mode_maps_to_engine_variant() {
        let backends = [backend(1, 1)];
        assert!(matches!(
            *engine(&backends, &LbMode::RoundRobin),
            LbEngine::RR(_)
        ));
        assert!(matches!(
            *engine(&backends, &LbMode::WeightedRoundRobin),
            LbEngine::RR(_)
        ));
        assert!(matches!(
            *engine(&backends, &LbMode::Random),
            LbEngine::Random(_)
        ));
        assert!(matches!(
            *engine(&backends, &LbMode::Ketama),
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
        assert!(!enabled
            .iter()
            .any(|b| b.target == BackendTarget::Addr(addr(2))));
    }

    #[test]
    fn mixed_addr_and_instance_site_yields_an_addr_only_pool() {
        // VERIFY-2: an instance entry is skipped, leaving only the addr peer.
        let backends = [backend(1, 1), instance_backend("INS001", 8080, 1)];
        let lb = engine(&backends, &LbMode::RoundRobin);
        for _ in 0..6 {
            assert_eq!(lb.select(None), Some(addr(1)));
        }
    }

    #[test]
    fn instance_only_site_yields_an_empty_pool_that_selects_none() {
        // VERIFY-2: with no addr entry the transition pool is empty (C3 adds
        // the InstancePool), so selection fails hard rather than panicking.
        let backends = [instance_backend("INS001", 8080, 1)];
        let lb = engine(&backends, &LbMode::RoundRobin);
        assert_eq!(lb.select(None), None);
    }

    #[test]
    fn weight_zero_backend_is_never_selected() {
        let backends = [backend(1, 1), backend(2, 0)];
        let lb = engine(&backends, &LbMode::RoundRobin);
        for _ in 0..10 {
            let selected = lb.select(None);
            assert_eq!(selected, Some(addr(1)));
        }
    }

    #[test]
    fn empty_pool_selects_none() {
        let lb = engine(&[], &LbMode::RoundRobin);
        assert_eq!(lb.select(None), None);
    }

    // ----- selection semantics -----

    #[test]
    fn round_robin_alternates() {
        let backends = [backend(1, 1), backend(2, 1)];
        let lb = engine(&backends, &LbMode::RoundRobin);
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
        let lb = engine(&backends, &LbMode::WeightedRoundRobin);
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
        let lb = engine(&backends, &LbMode::Random);
        for _ in 0..50 {
            let selected = lb.select(None);
            assert!(selected == Some(addr(1)) || selected == Some(addr(2)));
        }
    }

    #[test]
    fn ketama_is_stable_for_a_given_client() {
        let backends = [backend(1, 1), backend(2, 1)];
        let lb = engine(&backends, &LbMode::Ketama);
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
        let lb = engine(&[backend(1, 1)], &LbMode::RoundRobin);
        assert_eq!(lb.select(None), Some(addr(1)));
        lb.update(&[backend(9, 1)]);
        assert_eq!(lb.select(None), Some(addr(9)));
    }

    #[test]
    fn update_drops_weight_zero_backends() {
        let lb = engine(&[backend(1, 1)], &LbMode::RoundRobin);
        lb.update(&[backend(1, 0), backend(2, 1)]);
        for _ in 0..4 {
            assert_eq!(lb.select(None), Some(addr(2)));
        }
    }

    // ----- shared registry (FIX #2) -----

    #[test]
    fn fresh_engine_registers_into_the_shared_registry() {
        let registry = Arc::new(HealthRegistry::new());
        assert_eq!(registry.target_count(), 0);

        let _lb = build_lb(&[backend(1, 1), backend(2, 1)], &LbMode::RoundRobin, &registry);

        assert_eq!(
            registry.target_count(),
            2,
            "a freshly built engine must contribute its targets to the shared registry"
        );
    }

    #[test]
    fn update_preserves_the_health_registry() {
        let registry = Arc::new(HealthRegistry::new());
        let lb = build_lb(
            &[backend(1, 1), backend(2, 1)],
            &LbMode::RoundRobin,
            &registry,
        );

        assert_eq!(
            registry.target_count(),
            2,
            "the shared registry must see the primed pool"
        );

        lb.update(&[backend(9, 1)]);

        assert_eq!(
            registry.target_count(),
            1,
            "the membership refresh must reach the same shared registry"
        );
        assert_eq!(lb.select(None), Some(addr(9)));
    }

    // ----- ProbeRegistry -----

    #[test]
    fn probe_registry_tracks_only_health_checked_sites() {
        let probes = ProbeRegistry::new();
        probes.sync(&config(vec![
            forward_site("plain", vec![backend(1, 1)], None),
            forward_site(
                "gated",
                vec![backend(2, 1), backend(3, 0)],
                Some(tcp_health()),
            ),
        ]));
        // Only `gated`'s single enabled backend is probed.
        assert_eq!(probes.slot_count(), 1);
    }

    #[test]
    fn probe_registry_sync_is_idempotent_and_drops_removed_sites() {
        let probes = ProbeRegistry::new();
        let gated = || forward_site("gated", vec![backend(2, 1)], Some(tcp_health()));

        probes.sync(&config(vec![gated()]));
        let key_len = probes.slot_count();
        assert_eq!(key_len, 1);

        // Re-syncing the same spec must not add slots.
        probes.sync(&config(vec![gated()]));
        assert_eq!(probes.slot_count(), 1);

        // Removing the site (or its health check) drops its probe (FIX #8).
        probes.sync(&config(vec![forward_site("gated", vec![backend(2, 1)], None)]));
        assert_eq!(probes.slot_count(), 0);
    }

    #[test]
    fn probe_registry_sync_skips_instance_backends() {
        // A health-checked site whose only enabled backend is an instance has
        // no addressable target yet, so it contributes no probe slot.
        let probes = ProbeRegistry::new();
        probes.sync(&config(vec![forward_site(
            "instanced",
            vec![instance_backend("INS001", 8080, 1)],
            Some(tcp_health()),
        )]));
        assert_eq!(probes.slot_count(), 0);
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
        assert_eq!(ip_key_bytes(IpAddr::from([10, 0, 0, 1])).as_bytes(), &[10, 0, 0, 1]);
        assert_eq!(ip_key_bytes(IpAddr::from([0u16; 8])).as_bytes().len(), 16);
        // An IPv4-mapped IPv6 address keeps its 16-byte form (unchanged mapping).
        assert_eq!(
            ip_key_bytes(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 10, 0, 0, 1]))
                .as_bytes()
                .len(),
            16
        );
    }
}
