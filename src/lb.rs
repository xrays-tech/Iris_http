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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

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

use crate::config::{Backend, BackendTarget, Config, ForwardSite, HealthCheck, HealthKind, LbMode};
use crate::openstack::ResolvedInstance;
use crate::state::Snapshot;

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
// ForwardBackends: static (Pingora LB) or instance-driven pool per site (C3).
// ---------------------------------------------------------------------------

/// A forward site's backend pool: the static Pingora path, or the
/// instance-driven [`InstancePool`] (`02-contracts.md` §ForwardBackends).
pub enum ForwardBackends {
    /// Every backend is an `addr` literal → the existing Pingora engine +
    /// shared health-registry path.
    Static(Arc<LbEngine>),
    /// At least one backend references an OpenStack instance → probe-driven
    /// selection over resolved addresses.
    Instance(Arc<InstancePool>),
}

impl ForwardBackends {
    /// Select the concrete static address for a site (test/reload convenience).
    ///
    /// Only meaningful for a [`ForwardBackends::Static`] pool; an instance pool
    /// has no single static address and yields `None`.
    #[cfg(test)]
    pub(crate) fn select_static(&self, key: Option<IpAddr>) -> Option<SocketAddr> {
        match self {
            Self::Static(lb) => lb.select(key),
            Self::Instance(pool) => match pool.select(key) {
                Some(PoolPick::Static(addr)) => Some(addr),
                _ => None,
            },
        }
    }
}

/// The outcome of selecting from a site's pool.
///
/// [`PoolPick::Static`] is a ready-to-dial address; [`PoolPick::Instance`]
/// carries the instance identity (ketama key) and the configured port, to be
/// resolved to an address at request time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PoolPick {
    /// A literal `addr` backend.
    Static(SocketAddr),
    /// An `instance` backend: resolve `instance` and dial `port`.
    Instance {
        /// Instance name or UUID (also the ketama identity key).
        instance: String,
        /// Target port on the instance.
        port: u16,
    },
}

/// The outcome of one [`InstancePool::pick`]: the selected entry plus, from the
/// same lock scope, the selected instance's cached resolution (OPT-3).
pub(crate) struct PickOutcome {
    /// The selected pool pick (`None` when no healthy, enabled entry exists).
    pub(crate) pick: Option<PoolPick>,
    /// The cached resolution for the selected instance, if any.
    pub(crate) cached: Option<ResolvedInstance>,
}

/// Build the site-level backend pool: [`ForwardBackends::Static`] for an
/// all-`addr` site, [`ForwardBackends::Instance`] once any backend references
/// an instance.
///
/// The instance pool captures the site's `openstack_env` so it resolves through
/// the shared, env-keyed resolver in `snapshot`.
pub fn build_forward_backends(
    site: &ForwardSite,
    registry: &Arc<HealthRegistry>,
    snapshot: Arc<ArcSwap<Snapshot>>,
) -> Arc<ForwardBackends> {
    let has_instance = site
        .backends
        .iter()
        .any(|backend| matches!(backend.target, BackendTarget::Instance { .. }));
    if has_instance {
        Arc::new(ForwardBackends::Instance(Arc::new(InstancePool::new(
            site.openstack_env.as_deref().unwrap_or_default(),
            &site.backends,
            &site.lb_mode,
            snapshot,
        ))))
    } else {
        Arc::new(ForwardBackends::Static(build_lb(
            &site.backends,
            &site.lb_mode,
            registry,
        )))
    }
}

/// The instance-driven backend pool for one forward site (C3).
///
/// Entries are the site's enabled (`weight > 0`) backends. `addr` entries are
/// dialed directly; `instance` entries are resolved (floating preferred, fixed
/// fallback) through the site's env resolver and dialed at the configured port.
/// Selection only ever considers *healthy* entries; [`instance_health_service`]
/// updates the per-entry health bit and caches the latest resolution.
///
/// The ketama ring is keyed on the **instance identity** (name/UUID) — never on
/// the resolved address, which drifts across re-resolutions and would reshuffle
/// every client — so a given client keeps hitting the same backend.
pub struct InstancePool {
    site_env: String,
    mode: LbMode,
    snapshot: Arc<ArcSwap<Snapshot>>,
    state: Mutex<PoolState>,
}

/// Mutable pool state guarded by one short-lived lock (never held across an
/// `await`).
struct PoolState {
    entries: Vec<PoolEntry>,
    /// Round-robin / weighted-round-robin cursor.
    cursor: usize,
    /// Last successfully resolved address per instance identity.
    cached: HashMap<String, ResolvedInstance>,
    /// Generation counter, bumped (under the lock) whenever the entry set or any
    /// entry's health bit changes (OPT-1).
    gen: u64,
    /// Cached ketama ring with the generation it was built at (OPT-1).
    ///
    /// The ring is rebuilt only when `built_gen < gen` (or it is absent); its
    /// input is the healthy/enabled entries' identity + weight, bit-identical
    /// to the ring [`ketama_index`] builds inline.
    ring_cache: Option<(u64, Vec<(u64, usize)>)>,
}

/// One enabled pool entry plus its observed health.
struct PoolEntry {
    kind: EntryKind,
    weight: u32,
    healthy: bool,
}

/// The identity of a pool entry: a literal address or an instance + port.
#[derive(Clone)]
enum EntryKind {
    Static(SocketAddr),
    Instance { instance: String, port: u16 },
}

impl EntryKind {
    /// A stable key used to carry health across an [`InstancePool::update`].
    fn health_key(&self) -> String {
        match self {
            Self::Static(addr) => format!("addr:{addr}"),
            Self::Instance { instance, port } => format!("instance:{instance}:{port}"),
        }
    }

    /// The ketama identity: instance name/UUID, or the literal address.
    fn identity(&self) -> String {
        match self {
            Self::Static(addr) => addr.to_string(),
            Self::Instance { instance, .. } => instance.clone(),
        }
    }

    fn to_pick(&self) -> PoolPick {
        match self {
            Self::Static(addr) => PoolPick::Static(*addr),
            Self::Instance { instance, port } => PoolPick::Instance {
                instance: instance.clone(),
                port: *port,
            },
        }
    }
}

/// Ketama virtual nodes per unit of weight.
const KETAMA_REPLICAS: usize = 64;

impl InstancePool {
    /// Build a pool for one site.
    ///
    /// `site_env` is the OpenStack environment name the site references; the
    /// pool resolves its instances through `snapshot.resolvers[site_env]`.
    pub fn new(
        site_env: &str,
        backends: &[Backend],
        mode: &LbMode,
        snapshot: Arc<ArcSwap<Snapshot>>,
    ) -> Self {
        Self {
            site_env: site_env.to_owned(),
            mode: *mode,
            snapshot,
            state: Mutex::new(PoolState {
                entries: build_entries(backends, &HashMap::new()),
                cursor: 0,
                cached: HashMap::new(),
                gen: 0,
                ring_cache: None,
            }),
        }
    }

    /// Select a healthy, enabled entry (`None` when every entry is unhealthy or
    /// disabled). `hash_key` (the client IP) is only used by ketama.
    pub fn select(&self, hash_key: Option<IpAddr>) -> Option<PoolPick> {
        self.pick(hash_key).pick
    }

    /// Select a healthy, enabled entry **and**, in the same lock scope, return
    /// the selected instance's cached resolution (OPT-3).
    ///
    /// This performs exactly the selection [`select`](Self::select) does, but
    /// takes the state lock once so a steady-state instance request needs no
    /// second lock to look up its cached address. `cached` is the pool's last
    /// resolved address for the selected entry when it is an instance and a
    /// probe (or seed) has cached one; a [`PoolPick::Static`] pick or an
    /// uncached instance yields `None`.
    pub(crate) fn pick(&self, hash_key: Option<IpAddr>) -> PickOutcome {
        let mut guard = lock(&self.state);
        // Deref once so `entries` / `cursor` are disjoint field borrows.
        let state = &mut *guard;
        let healthy: Vec<usize> = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.weight > 0 && entry.healthy)
            .map(|(idx, _)| idx)
            .collect();
        if healthy.is_empty() {
            return PickOutcome {
                pick: None,
                cached: None,
            };
        }

        let Some(position) = (match self.mode {
            LbMode::RoundRobin => round_robin_pick(healthy.len(), &mut state.cursor),
            LbMode::WeightedRoundRobin => {
                weighted_pick(&state.entries, &healthy, &mut state.cursor)
            }
            LbMode::Random => random_pick(healthy.len()),
            LbMode::Ketama => {
                // Rebuild the ring only when the cached one is stale (OPT-1);
                // the input is bit-identical to the inline `ketama_index` ring.
                let current_gen = state.gen;
                let stale = state
                    .ring_cache
                    .as_ref()
                    .map(|(built_gen, _)| *built_gen < current_gen)
                    .unwrap_or(true);
                if stale {
                    let nodes: Vec<(String, u32)> = healthy
                        .iter()
                        .map(|&idx| {
                            let entry = &state.entries[idx];
                            (entry.kind.identity(), entry.weight)
                        })
                        .collect();
                    state.ring_cache = Some((current_gen, ring_from_nodes(&nodes)));
                }
                match state.ring_cache.as_ref() {
                    Some((_, ring)) => ketama_lookup(ring, hash_key),
                    None => None,
                }
            }
        }) else {
            return PickOutcome {
                pick: None,
                cached: None,
            };
        };

        let entry = match state.entries.get(healthy[position]) {
            Some(entry) => entry,
            None => {
                return PickOutcome {
                    pick: None,
                    cached: None,
                }
            }
        };
        let pick = entry.kind.to_pick();
        let cached = match &pick {
            PoolPick::Instance { instance, .. } => state.cached.get(instance).copied(),
            PoolPick::Static(_) => None,
        };
        PickOutcome {
            pick: Some(pick),
            cached,
        }
    }

    /// Replace the entry set, carrying the observed health of a retained
    /// `instance`/`addr` over (a changed identity starts healthy).
    pub fn update(&self, backends: &[Backend]) {
        let mut state = lock(&self.state);
        let mut old_health: HashMap<String, bool> = HashMap::with_capacity(state.entries.len());
        for entry in &state.entries {
            old_health.insert(entry.kind.health_key(), entry.healthy);
        }
        let entries = build_entries(backends, &old_health);

        // Drop cached resolutions for instances that are no longer present.
        let present: HashSet<String> = entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Instance { instance, .. } => Some(instance.clone()),
                EntryKind::Static(_) => None,
            })
            .collect();
        state.cached.retain(|instance, _| present.contains(instance));

        state.entries = entries;
        state.cursor = 0;
        // A membership swap invalidates the ketama ring (OPT-1).
        state.gen = state.gen.wrapping_add(1);
        state.ring_cache = None;
    }

    /// The latest resolved address for `instance`, if any probe has cached one.
    pub fn resolved(&self, instance: &str) -> Option<ResolvedInstance> {
        lock(&self.state).cached.get(instance).copied()
    }

    /// Seed the resolution cache (test seam for the proxy's cached-address path).
    #[cfg(test)]
    pub(crate) fn seed_resolution(&self, instance: &str, resolved: ResolvedInstance) {
        lock(&self.state).cached.insert(instance.to_owned(), resolved);
    }

    /// Probe every entry once, updating health and the resolution cache.
    ///
    /// A static entry is probed directly. An instance entry is first resolved
    /// through the site's env resolver (a cached resolve) and then probed at
    /// its primary address; a resolution failure marks it unhealthy.
    pub(crate) async fn probe_instances(&self, probe: &(dyn HealthCheckProbe + Send + Sync)) {
        // Snapshot the entry kinds and release the lock before awaiting.
        let kinds: Vec<EntryKind> = {
            let state = lock(&self.state);
            state.entries.iter().map(|entry| entry.kind.clone()).collect()
        };
        let resolver = self.snapshot.load().resolvers.get(&self.site_env).cloned();

        let mut updates: HashMap<String, (bool, Option<(String, ResolvedInstance)>)> =
            HashMap::with_capacity(kinds.len());
        for kind in &kinds {
            match kind {
                EntryKind::Static(addr) => {
                    let healthy = probe_addr(probe, *addr).await;
                    updates.insert(kind.health_key(), (healthy, None));
                }
                EntryKind::Instance { instance, port } => match &resolver {
                    Some(resolver) => match resolver.resolve(instance).await {
                        Ok(resolved) => {
                            let healthy = match resolved.primary() {
                                Some(ip) => probe_addr(probe, SocketAddr::new(ip, *port)).await,
                                None => false,
                            };
                            updates.insert(
                                kind.health_key(),
                                (healthy, Some((instance.clone(), resolved))),
                            );
                        }
                        Err(err) => {
                            tracing::debug!(
                                instance = %instance,
                                error = %err,
                                "forward instance resolution failed; marking unhealthy"
                            );
                            updates.insert(kind.health_key(), (false, None));
                        }
                    },
                    None => {
                        tracing::debug!(
                            instance = %instance,
                            env = %self.site_env,
                            "no resolver for the site's openstack env; marking unhealthy"
                        );
                        updates.insert(kind.health_key(), (false, None));
                    }
                },
            }
        }

        let mut state = lock(&self.state);
        let PoolState {
            entries,
            cached,
            gen,
            ring_cache,
            ..
        } = &mut *state;
        let mut health_flipped = false;
        for entry in entries.iter_mut() {
            if let Some((healthy, resolved)) = updates.get(&entry.kind.health_key()) {
                if entry.healthy != *healthy {
                    health_flipped = true;
                }
                entry.healthy = *healthy;
                if let Some((instance, resolved)) = resolved {
                    cached.insert(instance.clone(), *resolved);
                }
            }
        }
        // A health-bit flip changes the ketama ring's input set (OPT-1).
        if health_flipped {
            *gen = gen.wrapping_add(1);
            *ring_cache = None;
        }
    }
}

/// Build the enabled entry list, carrying health over by identity key.
fn build_entries(backends: &[Backend], old_health: &HashMap<String, bool>) -> Vec<PoolEntry> {
    backends
        .iter()
        .filter(|backend| backend.weight > 0)
        .map(|backend| {
            let kind = match &backend.target {
                BackendTarget::Addr(addr) => EntryKind::Static(*addr),
                BackendTarget::Instance { instance, port } => EntryKind::Instance {
                    instance: instance.clone(),
                    port: *port,
                },
            };
            let healthy = old_health.get(&kind.health_key()).copied().unwrap_or(true);
            PoolEntry {
                kind,
                weight: backend.weight,
                healthy,
            }
        })
        .collect()
}

/// Pick a position within `healthy` for equal-weight round robin.
fn round_robin_pick(len: usize, cursor: &mut usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let position = *cursor % len;
    *cursor = (*cursor + 1) % len;
    Some(position)
}

/// Pick a position within `healthy` for weighted round robin.
fn weighted_pick(entries: &[PoolEntry], healthy: &[usize], cursor: &mut usize) -> Option<usize> {
    let total: usize = healthy
        .iter()
        .map(|&idx| entries[idx].weight as usize)
        .sum();
    if total == 0 {
        return None;
    }
    let mut remaining = *cursor % total;
    *cursor = (*cursor + 1) % total;
    for (position, &idx) in healthy.iter().enumerate() {
        let weight = entries[idx].weight as usize;
        if remaining < weight {
            return Some(position);
        }
        remaining -= weight;
    }
    // Unreachable given `total` accounts for every healthy weight.
    healthy.last().copied()
}

/// Pick a position within `healthy` uniformly at random.
fn random_pick(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(0);
    Some((hasher.finish() as usize) % len)
}

/// Pick a position within `nodes` (identity + weight) by consistent hashing of
/// `key` (the client IP) over a ring of identity-keyed virtual nodes.
///
/// This is the uncached reference used by tests to validate the cached ring
/// (OPT-1); production selection goes through [`InstancePool::pick`].
#[cfg(test)]
fn ketama_index(nodes: &[(String, u32)], key: Option<IpAddr>) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    ketama_lookup(&ring_from_nodes(nodes), key)
}

/// Build the sorted ketama ring from `(identity, weight)` nodes.
///
/// Ring points are `(hash, position)`, where `position` indexes `nodes` — the
/// exact ring [`ketama_index`] and the cached [`PoolState::ring_cache`] share.
fn ring_from_nodes(nodes: &[(String, u32)]) -> Vec<(u64, usize)> {
    let mut ring: Vec<(u64, usize)> = Vec::new();
    for (position, (identity, weight)) in nodes.iter().enumerate() {
        let replicas = (*weight as usize).max(1) * KETAMA_REPLICAS;
        for replica in 0..replicas {
            ring.push((hash_identity(identity, replica as u64), position));
        }
    }
    ring.sort_by(|a, b| a.0.cmp(&b.0));
    ring
}

/// Look up `key` on a prebuilt, sorted ketama `ring`, returning the ring
/// point's position (an index into the node/healthy list).
fn ketama_lookup(ring: &[(u64, usize)], key: Option<IpAddr>) -> Option<usize> {
    if ring.is_empty() {
        return None;
    }
    let digest = hash_key(key);
    let index = match ring.binary_search_by(|point| point.0.cmp(&digest)) {
        Ok(index) => index,
        Err(index) => {
            if index == ring.len() {
                0
            } else {
                index
            }
        }
    };
    ring.get(index).map(|point| point.1)
}

/// Hash an identity + replica index onto the ketama ring.
fn hash_identity(identity: &str, replica: u64) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    replica.hash(&mut hasher);
    hasher.finish()
}

/// Hash the client key for a ketama lookup.
fn hash_key(key: Option<IpAddr>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    match key {
        Some(IpAddr::V4(addr)) => addr.octets().hash(&mut hasher),
        Some(IpAddr::V6(addr)) => addr.octets().hash(&mut hasher),
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// Run one health probe against a bare `addr`.
async fn probe_addr(probe: &(dyn HealthCheckProbe + Send + Sync), addr: SocketAddr) -> bool {
    match PoolBackend::new_with_weight(&addr.to_string(), 1) {
        Ok(backend) => probe.check(&backend).await.is_ok(),
        Err(err) => {
            tracing::debug!(error = %err, %addr, "invalid health-check target");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Instance health service (C3): the probe loop for Instance pools only.
// ---------------------------------------------------------------------------

/// Build the background service that probes [`ForwardBackends::Instance`] pools.
///
/// The static [`health_check_service`] gates every `addr` backend through the
/// shared [`HealthRegistry`]; this service complements it by resolving and
/// probing the instance pools, which the registry never sees
/// ([`ProbeRegistry::sync`] skips them). A site with no `health_check` gets no
/// probe loop at all: its enabled entries stay selectable and a failure
/// surfaces as a request-time `502`.
pub fn instance_health_service(snapshot: Arc<ArcSwap<Snapshot>>) -> impl BackgroundService {
    InstanceHealthService {
        snapshot,
        last: Mutex::new(HashMap::new()),
    }
}

/// The [`BackgroundService`] that drives instance-pool probes.
struct InstanceHealthService {
    snapshot: Arc<ArcSwap<Snapshot>>,
    /// Last probe time per site, enforcing each site's `interval_secs`.
    last: Mutex<HashMap<String, Instant>>,
}

impl InstanceHealthService {
    /// Probe every due instance-pool site once.
    async fn tick(&self) {
        let snapshot = self.snapshot.load_full();
        for site in &snapshot.config.forward {
            let Some(health_check) = site.health_check.as_ref() else {
                continue;
            };
            let Some(ForwardBackends::Instance(pool)) =
                snapshot.lbs.get(&site.name).map(Arc::as_ref)
            else {
                continue;
            };
            if !self.due(&site.name, health_check.interval_secs) {
                continue;
            }
            let probe = build_probe(health_check);
            pool.probe_instances(probe.as_ref()).await;
        }
    }

    /// Whether `site` is due for a probe under its `interval_secs`.
    fn due(&self, site: &str, interval_secs: u64) -> bool {
        let mut last = lock(&self.last);
        let now = Instant::now();
        let interval = Duration::from_secs(interval_secs.max(1));
        match last.get(site) {
            Some(at) if now.duration_since(*at) < interval => false,
            _ => {
                last.insert(site.to_owned(), now);
                true
            }
        }
    }
}

#[async_trait]
impl BackgroundService for InstanceHealthService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut ticker = tokio::time::interval(HEALTH_CHECK_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => self.tick().await,
            }
        }
    }
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
            // A site whose pool is `ForwardBackends::Instance` is probed by
            // `instance_health_service`, not the shared registry: probing here
            // too would double-probe and split the health state (C3 contract).
            if site
                .backends
                .iter()
                .any(|backend| matches!(backend.target, BackendTarget::Instance { .. }))
            {
                continue;
            }
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

    // =======================================================================
    // C3 — ForwardBackends / InstancePool / instance health service
    // =======================================================================

    use std::net::TcpListener;

    use crate::openstack::{ResolveError, Resolver};
    use crate::state::SharedState;

    fn ip4(octets: [u8; 4]) -> IpAddr {
        IpAddr::from(octets)
    }

    /// A fake resolver — the sanctioned external-boundary stub for the injected
    /// [`Resolver`] seam.
    struct FakeResolver {
        reply: Result<ResolvedInstance, ResolveError>,
    }

    #[async_trait]
    impl Resolver for FakeResolver {
        async fn resolve(&self, _instance: &str) -> Result<ResolvedInstance, ResolveError> {
            self.reply.clone()
        }
    }

    fn fake_ok(floating: Option<[u8; 4]>, fixed: Option<[u8; 4]>) -> Arc<dyn Resolver> {
        Arc::new(FakeResolver {
            reply: Ok(ResolvedInstance {
                floating: floating.map(ip4),
                fixed: fixed.map(ip4),
            }),
        })
    }

    fn fake_err(err: ResolveError) -> Arc<dyn Resolver> {
        Arc::new(FakeResolver { reply: Err(err) })
    }

    fn instance_site(
        name: &str,
        backends: Vec<Backend>,
        mode: LbMode,
        health_check: Option<HealthCheck>,
    ) -> ForwardSite {
        let mut site = forward_site(name, backends, health_check);
        site.openstack_env = Some("prod".to_owned());
        site.lb_mode = mode;
        site
    }

    fn state_for(forward: Vec<ForwardSite>) -> SharedState {
        SharedState::try_new(config(forward)).expect("state builds")
    }

    fn pool_for(state: &SharedState, site: &str) -> Arc<InstancePool> {
        match state.lb(site).expect("pool present").as_ref() {
            ForwardBackends::Instance(pool) => Arc::clone(pool),
            ForwardBackends::Static(_) => panic!("expected an instance pool"),
        }
    }

    fn inject_resolver(state: &SharedState, env: &str, resolver: Arc<dyn Resolver>) {
        let mut snapshot = state.snapshot().shallow();
        let mut map = (*snapshot.resolvers).clone();
        map.insert(env.to_owned(), resolver);
        snapshot.resolvers = Arc::new(map);
        state.store(snapshot);
    }

    fn listening_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        (listener, port)
    }

    fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    }

    fn instance_of(pick: Option<PoolPick>) -> Option<String> {
        match pick {
            Some(PoolPick::Instance { instance, .. }) => Some(instance),
            _ => None,
        }
    }

    // ----- site pool assembly -----

    #[test]
    fn addr_only_site_yields_a_static_pool() {
        let state = state_for(vec![forward_site("www", vec![backend(1, 1)], None)]);
        assert!(matches!(
            state.lb("www").expect("pool").as_ref(),
            ForwardBackends::Static(_)
        ));
    }

    #[test]
    fn instance_site_yields_an_instance_pool() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("INS001", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        assert!(matches!(
            state.lb("apps").expect("pool").as_ref(),
            ForwardBackends::Instance(_)
        ));
    }

    #[test]
    fn mixed_addr_and_instance_site_yields_an_instance_pool() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![backend(1, 1), instance_backend("INS001", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        assert!(matches!(
            state.lb("apps").expect("pool").as_ref(),
            ForwardBackends::Instance(_)
        ));
    }

    // ----- selection: weights, round robin, weight-0, ketama -----

    #[test]
    fn instance_pool_weighted_round_robin_honours_weights() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", 8080, 3),
                instance_backend("beta", 8080, 1),
            ],
            LbMode::WeightedRoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for _ in 0..8 {
            if let Some(instance) = instance_of(pool.select(None)) {
                *counts.entry(instance).or_default() += 1;
            }
        }
        assert_eq!(counts.get("alpha"), Some(&6));
        assert_eq!(counts.get("beta"), Some(&2));
    }

    #[test]
    fn instance_pool_round_robin_alternates() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", 8080, 1),
                instance_backend("beta", 8080, 1),
            ],
            LbMode::RoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        let seen: Vec<Option<String>> = (0..4).map(|_| instance_of(pool.select(None))).collect();
        assert_eq!(
            seen,
            vec![
                Some("alpha".to_owned()),
                Some("beta".to_owned()),
                Some("alpha".to_owned()),
                Some("beta".to_owned())
            ]
        );
    }

    #[test]
    fn instance_pool_excludes_weight_zero_entries() {
        // A weight-0 instance alongside an enabled addr yields only the addr.
        let state = state_for(vec![instance_site(
            "apps",
            vec![backend(7, 1), instance_backend("alpha", 8080, 0)],
            LbMode::RoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        for _ in 0..4 {
            assert_eq!(pool.select(None), Some(PoolPick::Static(addr(7))));
        }
    }

    #[test]
    fn instance_pool_with_no_enabled_entries_selects_none() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("alpha", 8080, 0)],
            LbMode::RoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        assert_eq!(pool.select(None), None);
    }

    #[test]
    fn ketama_index_is_keyed_on_identity_not_address() {
        let nodes = vec![("alpha".to_owned(), 1u32), ("beta".to_owned(), 1u32)];
        let key = Some(ip4([203, 0, 113, 7]));
        let first = ketama_index(&nodes, key);
        assert!(first.is_some());
        for _ in 0..10 {
            assert_eq!(ketama_index(&nodes, key), first, "ketama must be sticky");
        }
        // Both identity-keyed nodes are reachable across a spread of clients.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1024u32 {
            let client = Some(ip4([10, 0, (i >> 8) as u8, i as u8]));
            if let Some(index) = ketama_index(&nodes, client) {
                seen.insert(index);
            }
        }
        assert_eq!(seen.len(), 2, "both instances must be reachable");
    }

    #[test]
    fn instance_pool_ketama_is_stable_per_client() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", 8080, 1),
                instance_backend("beta", 8080, 1),
            ],
            LbMode::Ketama,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        let client = Some(ip4([203, 0, 113, 7]));
        let first = pool.select(client);
        assert!(first.is_some());
        for _ in 0..10 {
            assert_eq!(pool.select(client), first, "ketama must be sticky per client");
        }
    }

    // ----- OPT-1: cached ketama ring -----

    /// The cached ring (rebuilt only on a membership/health change) must select
    /// exactly what the uncached [`ketama_index`] reference selects for the same
    /// node set.
    #[test]
    fn ketama_distribution_matches_previous_algorithm() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", 8080, 3),
                instance_backend("beta", 8080, 1),
                instance_backend("gamma", 8080, 2),
            ],
            LbMode::Ketama,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        // Reference ring input: healthy, enabled entries in order (identity+weight).
        let nodes = vec![
            ("alpha".to_owned(), 3u32),
            ("beta".to_owned(), 1u32),
            ("gamma".to_owned(), 2u32),
        ];
        for i in 0..2000u32 {
            let client = ip4([10, (i >> 16) as u8, (i >> 8) as u8, i as u8]);
            let expected = ketama_index(&nodes, Some(client)).map(|pos| nodes[pos].0.clone());
            assert_eq!(
                instance_of(pool.select(Some(client))),
                expected,
                "cached ring must match the uncached algorithm for client {client}"
            );
        }
    }

    /// A ketama ring built once must be rebuilt only when the pool changes: a
    /// health flip (via a probe) must be reflected, and the post-flip
    /// distribution must equal a fresh ring over the surviving node.
    #[tokio::test]
    async fn ketama_ring_is_rebuilt_only_when_pool_changes() {
        let (_listener, up) = listening_port();
        let down = closed_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", up, 1),
                instance_backend("beta", down, 1),
            ],
            LbMode::Ketama,
            Some(tcp_health()),
        )]);
        // Both instances resolve to the same host; only alpha's port is live.
        inject_resolver(&state, "prod", fake_ok(Some([127, 0, 0, 1]), None));
        let pool = pool_for(&state, "apps");

        let both = vec![("alpha".to_owned(), 1u32), ("beta".to_owned(), 1u32)];
        let mut changed = false;
        for i in 0..512u32 {
            let client = ip4([10, 0, (i >> 8) as u8, i as u8]);
            let expected = ketama_index(&both, Some(client)).map(|pos| both[pos].0.clone());
            assert_eq!(
                instance_of(pool.select(Some(client))),
                expected,
                "steady-state ring must match the full node set"
            );
            // Record whether this key actually distinguishes the two rings.
            if expected.as_deref() == Some("beta") {
                changed = true;
            }
        }
        assert!(changed, "the two-node ring must reach beta for some client");

        // The probe flips beta unhealthy: the ring must be rebuilt over alpha.
        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        let only_alpha = vec![("alpha".to_owned(), 1u32)];
        for i in 0..512u32 {
            let client = ip4([10, 0, (i >> 8) as u8, i as u8]);
            let expected =
                ketama_index(&only_alpha, Some(client)).map(|pos| only_alpha[pos].0.clone());
            let actual = instance_of(pool.select(Some(client)));
            assert_eq!(
                actual, expected,
                "post-flip ring must exclude the unhealthy node"
            );
            assert_eq!(actual.as_deref(), Some("alpha"));
        }
    }

    /// `update` (an entry-set change) must invalidate the cached ring.
    #[test]
    fn ketama_ring_tracks_entry_set_update() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                instance_backend("alpha", 8080, 1),
                instance_backend("beta", 8080, 1),
            ],
            LbMode::Ketama,
            None,
        )]);
        let pool = pool_for(&state, "apps");

        pool.update(&[
            instance_backend("alpha", 8080, 1),
            instance_backend("gamma", 8080, 1),
        ]);
        let after = vec![("alpha".to_owned(), 1u32), ("gamma".to_owned(), 1u32)];
        for i in 0..512u32 {
            let client = ip4([10, 0, (i >> 8) as u8, i as u8]);
            let expected = ketama_index(&after, Some(client)).map(|pos| after[pos].0.clone());
            assert_eq!(instance_of(pool.select(Some(client))), expected);
        }
    }

    // ----- OPT-3: single-lock pick -----

    /// `pick` performs the same selection as `select` **and** returns the
    /// selected instance's cached resolution from the same lock scope.
    #[test]
    fn pick_returns_cached_resolution_without_second_lock() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("alpha", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");

        // No cached resolution yet: still selects, `cached` is `None`.
        let first = pool.pick(None);
        assert_eq!(
            first.pick,
            Some(PoolPick::Instance {
                instance: "alpha".to_owned(),
                port: 8080,
            })
        );
        assert_eq!(first.cached, None);
        // `select` delegates to `pick` (same pick).
        assert_eq!(pool.select(None), first.pick.clone());

        // A seeded resolution is returned in the same call.
        let seeded = ResolvedInstance {
            floating: Some(ip4([203, 0, 113, 10])),
            fixed: None,
        };
        pool.seed_resolution("alpha", seeded);
        let second = pool.pick(None);
        assert_eq!(second.pick, first.pick);
        assert_eq!(second.cached, Some(seeded));
    }

    /// A static entry carries no cached resolution.
    #[test]
    fn pick_static_entry_has_no_cached_resolution() {
        let state = state_for(vec![instance_site(
            "seed",
            vec![instance_backend("seed", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        let backends = [backend(7, 1)];
        let pool = InstancePool::new("prod", &backends, &LbMode::RoundRobin, state.handle());
        let outcome = pool.pick(None);
        assert_eq!(outcome.pick, Some(PoolPick::Static(addr(7))));
        assert_eq!(outcome.cached, None);
    }

    /// After an entry-set change, `pick` reflects the new entries and never
    /// leaks a removed instance's cached resolution.
    #[test]
    fn pick_reflects_updated_entries() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("alpha", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        let pool = pool_for(&state, "apps");
        let seeded = ResolvedInstance {
            floating: Some(ip4([203, 0, 113, 10])),
            fixed: None,
        };
        pool.seed_resolution("alpha", seeded);
        assert_eq!(pool.pick(None).cached, Some(seeded));

        pool.update(&[instance_backend("beta", 8080, 1)]);
        let after = pool.pick(None);
        assert_eq!(
            after.pick,
            Some(PoolPick::Instance {
                instance: "beta".to_owned(),
                port: 8080,
            })
        );
        assert_eq!(after.cached, None, "the removed instance's cache must not leak");
    }

    // ----- health probing + resolution cache -----

    #[tokio::test]
    async fn instance_pool_probe_marks_resolved_healthy_and_caches_address() {
        let (_listener, port) = listening_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("INS001", port, 1)],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]);
        inject_resolver(&state, "prod", fake_ok(Some([127, 0, 0, 1]), None));
        let pool = pool_for(&state, "apps");

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        assert!(pool.select(None).is_some(), "a reachable instance stays selectable");
        assert_eq!(
            pool.resolved("INS001").map(|resolved| resolved.floating),
            Some(Some(ip4([127, 0, 0, 1])))
        );
    }

    #[tokio::test]
    async fn instance_pool_probe_marks_unreachable_unhealthy() {
        let port = closed_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("INS001", port, 1)],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]);
        inject_resolver(&state, "prod", fake_ok(Some([127, 0, 0, 1]), None));
        let pool = pool_for(&state, "apps");

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        assert_eq!(pool.select(None), None, "an unreachable instance is not selectable");
    }

    #[tokio::test]
    async fn instance_pool_resolve_failure_marks_unhealthy() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("ghost", 8080, 1)],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]);
        inject_resolver(&state, "prod", fake_err(ResolveError::NotFound));
        let pool = pool_for(&state, "apps");

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        assert_eq!(pool.select(None), None);
        assert!(pool.resolved("ghost").is_none());
    }

    #[tokio::test]
    async fn instance_pool_probes_static_entries_directly() {
        let (_listener, up) = listening_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![
                backend(up, 1),
                instance_backend("ghost", 8080, 1),
            ],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]);
        inject_resolver(&state, "prod", fake_err(ResolveError::NotFound));
        let pool = pool_for(&state, "apps");

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        // The reachable static entry is healthy; the unresolvable instance is
        // not, so only the static entry is selected.
        assert_eq!(pool.select(None), Some(PoolPick::Static(addr(up))));
    }

    #[tokio::test]
    async fn instance_pool_all_unhealthy_selects_none() {
        let down_a = closed_port();
        let down_b = closed_port();
        let state = state_for(vec![instance_site(
            "seed",
            vec![instance_backend("seed", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        let backends = [backend(down_a, 1), backend(down_b, 1)];
        let pool = InstancePool::new("prod", &backends, &LbMode::RoundRobin, state.handle());

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;

        assert_eq!(pool.select(None), None);
    }

    // ----- update: health carry-over -----

    #[tokio::test]
    async fn instance_pool_update_preserves_health_by_identity() {
        let (_up, up) = listening_port();
        let down = closed_port();
        let state = state_for(vec![instance_site(
            "seed",
            vec![instance_backend("seed", 8080, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        let backends = [backend(up, 1), backend(down, 1)];
        let pool = InstancePool::new("prod", &backends, &LbMode::RoundRobin, state.handle());

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;
        assert_eq!(pool.select(None), Some(PoolPick::Static(addr(up))));

        // Re-applying the same membership must keep the unhealthy entry's bit.
        pool.update(&backends);
        assert_eq!(
            pool.select(None),
            Some(PoolPick::Static(addr(up))),
            "update must carry the observed health over by identity"
        );

        // A brand-new identity starts healthy.
        pool.update(&[backend(up, 1), backend(7, 1)]);
        let picks: std::collections::HashSet<Option<PoolPick>> =
            (0..4).map(|_| pool.select(None)).collect();
        assert!(picks.contains(&Some(PoolPick::Static(addr(7)))));
    }

    #[tokio::test]
    async fn instance_pool_update_drops_removed_cached_resolution() {
        let (_listener, port) = listening_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("alpha", port, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        inject_resolver(&state, "prod", fake_ok(Some([127, 0, 0, 1]), None));
        let pool = pool_for(&state, "apps");

        let probe = build_probe(&tcp_health());
        pool.probe_instances(probe.as_ref()).await;
        assert!(pool.resolved("alpha").is_some());

        pool.update(&[instance_backend("beta", port, 1)]);
        assert!(
            pool.resolved("alpha").is_none(),
            "a removed instance's cached resolution must be dropped"
        );
        assert!(instance_of(pool.select(None)).as_deref() == Some("beta"));
    }

    // ----- instance health service -----

    #[tokio::test]
    async fn instance_health_service_skips_sites_without_health_check() {
        let (_listener, port) = listening_port();
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("INS001", port, 1)],
            LbMode::RoundRobin,
            None,
        )]);
        // Even an unresolvable instance stays selectable: no probe loop exists.
        inject_resolver(&state, "prod", fake_err(ResolveError::NotFound));
        let pool = pool_for(&state, "apps");

        let service = InstanceHealthService {
            snapshot: state.handle(),
            last: Mutex::new(HashMap::new()),
        };
        service.tick().await;

        assert!(pool.resolved("INS001").is_none(), "no health_check must not probe");
        assert!(pool.select(None).is_some(), "enabled entries stay selectable");
    }

    #[tokio::test]
    async fn instance_health_service_marks_unresolvable_unhealthy() {
        let state = state_for(vec![instance_site(
            "apps",
            vec![instance_backend("INS001", 8080, 1)],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]);
        inject_resolver(&state, "prod", fake_err(ResolveError::NotFound));
        let pool = pool_for(&state, "apps");

        let service = InstanceHealthService {
            snapshot: state.handle(),
            last: Mutex::new(HashMap::new()),
        };
        service.tick().await;

        assert_eq!(pool.select(None), None);
    }

    #[test]
    fn probe_registry_sync_skips_mixed_instance_sites() {
        // A mixed site's pool is `ForwardBackends::Instance`, so the shared
        // registry must not probe its static entry (no double probing).
        let probes = ProbeRegistry::new();
        probes.sync(&config(vec![instance_site(
            "mixed",
            vec![backend(2, 1), instance_backend("INS001", 8080, 1)],
            LbMode::RoundRobin,
            Some(tcp_health()),
        )]));
        assert_eq!(probes.slot_count(), 0);
    }
}
