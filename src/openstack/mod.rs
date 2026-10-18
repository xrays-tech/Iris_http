//! P5 — OpenStack instance auto-discovery.
//!
//! [`OpenStackResolver`] resolves an instance name to its upstream
//! `{floating|fixed}` address: cache → single-flight → circuit breaker →
//! Keystone auth → Nova query → address selection (`discovery.md` §2).
//!
//! The network boundary is [`Fetcher`]: production uses [`OpenStackFetcher`]
//! (auth + Nova over an [`HttpClient`]); unit tests inject a hand-rolled fake so
//! the cache/breaker/single-flight logic is exercised directly with no mock
//! crate. Integration tests drive the real fetcher against
//! `scripts/mock_openstack.py`.

pub mod auth;
pub(crate) mod cache;
pub mod nova;

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use url::Url;

use self::auth::{AuthError, Token};
use self::cache::{
    build_cache, random_basis_points, ttl_for, Breaker, CacheEntry, DiscoveryCache, FlightMap,
    FlightRole, LeaderGuard, Stored, TokenCache, BREAKER_THRESHOLD,
};
use self::nova::{
    exact_name_match, is_instance_uuid, query_server_by_id, query_servers, select_address,
    NovaError, NovaServer,
};
use crate::config::{Config, OpenStackConfig};

/// HTTP client shared by all discovery calls (pooled connections).
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Create a client with a default connection pool.
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
        }
    }

    /// Wrap an existing `reqwest::Client` (custom timeouts, proxies, ...).
    pub fn from_reqwest(inner: reqwest::Client) -> Self {
        Self { inner }
    }

    pub(crate) fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

/// The upstream addresses resolved for an instance (`discovery.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedInstance {
    /// Preferred floating address.
    pub floating: Option<IpAddr>,
    /// Fixed fallback address.
    pub fixed: Option<IpAddr>,
}

impl ResolvedInstance {
    /// The address to use first: floating, else fixed.
    pub fn primary(&self) -> Option<IpAddr> {
        self.floating.or(self.fixed)
    }

    /// Whether no address is available (the negative-cache sentinel).
    pub fn is_empty(&self) -> bool {
        self.floating.is_none() && self.fixed.is_none()
    }
}

/// Discovery failures, mapped to outbound HTTP status per `discovery.md` §7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The instance does not exist / no exact-name match (`404`).
    NotFound,
    /// Authentication failed (`502`).
    AuthFailed,
    /// The control plane is unreachable and no last-known-good exists (`502`).
    DiscoveryUnavailable(String),
    /// The instance exists but exposes no usable address (`502`).
    NoAddress,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "instance not found"),
            Self::AuthFailed => write!(f, "openstack authentication failed"),
            Self::DiscoveryUnavailable(msg) => write!(f, "discovery unavailable: {msg}"),
            Self::NoAddress => write!(f, "instance has no usable address"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// The discovery injection seam (`02-contracts.md`). `Send + Sync` so a
/// resolver can live in `Arc<dyn Resolver>` inside `SharedState`.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve `instance` to its upstream address.
    async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError>;
}

/// Classified failure from the network boundary, deciding cache/breaker policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    /// No such instance (negative cache).
    NotFound,
    /// Instance exists but has no address.
    NoAddress,
    /// Credentials were rejected.
    AuthFailed,
    /// Transient control-plane failure (network/5xx/timeout).
    Transient(String),
}

/// The network boundary, injected so the resolver core is testable without a
/// mock crate.
#[async_trait]
pub(crate) trait Fetcher: Send + Sync {
    /// Perform a real Keystone + Nova lookup for `instance`.
    async fn fetch(&self, instance: &str) -> Result<ResolvedInstance, FetchError>;
}

fn map_auth_error(err: AuthError) -> FetchError {
    match &err {
        AuthError::Status(401) | AuthError::Status(403) => FetchError::AuthFailed,
        _ => FetchError::Transient(err.to_string()),
    }
}

fn map_nova_error(err: NovaError) -> FetchError {
    match &err {
        NovaError::NotFound => FetchError::NotFound,
        NovaError::Unauthorized => FetchError::AuthFailed,
        _ => FetchError::Transient(err.to_string()),
    }
}

/// Production fetcher: Keystone token (cached) + Nova name query, with a single
/// 401 refresh-and-retry (`discovery.md` §3).
struct OpenStackFetcher {
    auth: crate::config::AuthConfig,
    keystone: Url,
    nova_override: Option<Url>,
    all_tenants: bool,
    timeout: Duration,
    client: Arc<HttpClient>,
    token: TokenCache,
}

impl OpenStackFetcher {
    /// Map a single Nova server onto its selected upstream address.
    fn pick(server: NovaServer) -> Result<ResolvedInstance, FetchError> {
        let selection = select_address(&server);
        if selection.is_empty() {
            return Err(FetchError::NoAddress);
        }
        Ok(ResolvedInstance {
            floating: selection.floating,
            fixed: selection.fixed,
        })
    }

    fn base(&self, token: &Token) -> Url {
        match &self.nova_override {
            Some(url) => url.clone(),
            None => token.nova_base.clone(),
        }
    }

    /// Look `instance` up on Nova. UUID-shaped input goes to show-by-id
    /// (`GET /servers/{uuid}`); anything else is an exact-name list query.
    async fn query(&self, instance: &str, token: &Token) -> Result<NovaServer, NovaError> {
        if is_instance_uuid(instance) {
            query_server_by_id(
                &self.client,
                &token.value,
                &self.base(token),
                instance,
                self.timeout,
            )
            .await
        } else {
            let servers = query_servers(
                &self.client,
                &token.value,
                &self.base(token),
                instance,
                self.all_tenants,
                self.timeout,
            )
            .await?;
            exact_name_match(instance, &servers)
                .cloned()
                .ok_or(NovaError::NotFound)
        }
    }
}

#[async_trait]
impl Fetcher for OpenStackFetcher {
    async fn fetch(&self, instance: &str) -> Result<ResolvedInstance, FetchError> {
        let token = self
            .token
            .get(&self.auth, &self.keystone, &self.client, self.timeout)
            .await
            .map_err(map_auth_error)?;

        match self.query(instance, &token).await {
            Ok(server) => Self::pick(server),
            Err(NovaError::Unauthorized) => {
                // Refresh once and retry (`discovery.md` §3).
                let token = self
                    .token
                    .force_refresh(&self.auth, &self.keystone, &self.client, self.timeout)
                    .await
                    .map_err(map_auth_error)?;
                match self.query(instance, &token).await {
                    Ok(server) => Self::pick(server),
                    Err(NovaError::Unauthorized) => Err(FetchError::AuthFailed),
                    Err(err) => Err(map_nova_error(err)),
                }
            }
            Err(err) => Err(map_nova_error(err)),
        }
    }
}

/// The OpenStack resolver: cache, single-flight, breaker, last-known-good.
pub struct OpenStackResolver {
    cache_ttl: Duration,
    fetcher: Arc<dyn Fetcher>,
    cache: DiscoveryCache,
    flights: FlightMap,
    breaker: Mutex<Breaker>,
    /// Last-known-good address per instance, keyed by instance name.
    ///
    /// Entry-level storage (a keyed map behind one short-lived lock) so a
    /// positive resolution inserts only its own entry instead of cloning the
    /// whole map, which was O(n) per success at large instance counts. The
    /// fail-open path only ever needs the entry for the instance being resolved.
    last_good: Mutex<HashMap<String, ResolvedInstance>>,
}

impl OpenStackResolver {
    /// Build a resolver for one `openstack` site.
    pub fn new(cfg: &OpenStackConfig, client: Arc<HttpClient>) -> Self {
        let fetcher: Arc<dyn Fetcher> = Arc::new(OpenStackFetcher {
            auth: cfg.auth.clone(),
            keystone: cfg.auth.api_url.clone(),
            nova_override: cfg.nova.api_url.clone(),
            all_tenants: cfg.nova.all_tenants,
            timeout: Duration::from_secs(cfg.timeout_secs),
            client,
            token: TokenCache::new(),
        });
        Self::assemble(cfg, fetcher)
    }

    fn assemble(cfg: &OpenStackConfig, fetcher: Arc<dyn Fetcher>) -> Self {
        let cooldown = Duration::from_secs(cfg.timeout_secs.saturating_mul(3));
        Self {
            cache_ttl: Duration::from_secs(cfg.cache_ttl_secs),
            fetcher,
            cache: build_cache(),
            flights: FlightMap::new(),
            breaker: Mutex::new(Breaker::new(BREAKER_THRESHOLD, cooldown)),
            last_good: Mutex::new(HashMap::new()),
        }
    }

    /// Construct a resolver around an injected fetcher (unit tests).
    #[cfg(test)]
    fn with_fetcher(cfg: &OpenStackConfig, fetcher: Arc<dyn Fetcher>) -> Self {
        Self::assemble(cfg, fetcher)
    }

    /// Resolve `instance` (`discovery.md` §2): cache hit, else single-flight
    /// fetch gated by the breaker, then cache + last-known-good update.
    pub async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError> {
        if let Some(stored) = self.cache.get(instance).await {
            match stored.entry {
                CacheEntry::Positive(resolved) => return Ok(resolved),
                CacheEntry::Negative => return Err(ResolveError::NotFound),
                CacheEntry::NoAddress => return Err(ResolveError::NoAddress),
                CacheEntry::Transient => {
                    // Anti-avalanche window: report the cached failure without
                    // hitting the control plane again (`discovery.md` §6.1).
                    return Err(ResolveError::DiscoveryUnavailable(
                        "transient discovery failure (cached)".to_owned(),
                    ));
                }
            }
        }

        match self.flights.join(instance) {
            FlightRole::Follower(flight) => flight.wait().await,
            FlightRole::Leader(_flight) => {
                // RAII guard: publishes the result and clears the in-flight
                // slot on the happy path; if this future is dropped mid-`await`
                // the guard's `Drop` publishes a cancellation error so parked
                // followers unblock and the key is never poisoned (FIX #5).
                let guard = LeaderGuard::new(&self.flights, instance);
                let result = self.attempt(instance).await;
                guard.finish(&result);
                result
            }
        }
    }

    /// Drop any cached entry for `instance` (used by reload / tests to force a
    /// fresh attempt).
    pub async fn invalidate(&self, instance: &str) {
        self.cache.invalidate(instance).await;
    }

    async fn attempt(&self, instance: &str) -> Result<ResolvedInstance, ResolveError> {
        let now = Instant::now();
        let allowed = self
            .breaker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .allow(now);
        if !allowed {
            return self.fail_open(instance, "circuit breaker open");
        }

        match self.fetcher.fetch(instance).await {
            Ok(resolved) => {
                self.on_success();
                self.store_positive(instance, resolved).await;
                Ok(resolved)
            }
            Err(FetchError::NotFound) => {
                self.store_entry(instance, CacheEntry::Negative).await;
                Err(ResolveError::NotFound)
            }
            Err(FetchError::NoAddress) => {
                self.store_entry(instance, CacheEntry::NoAddress).await;
                Err(ResolveError::NoAddress)
            }
            Err(FetchError::AuthFailed) => Err(ResolveError::AuthFailed),
            Err(FetchError::Transient(msg)) => {
                self.breaker
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .on_failure(now);
                // Fail-open: prefer last-known-good over a 5xx
                // (`discovery.md` §6.4).
                match self.last_good_of(instance) {
                    Some(resolved) => Ok(resolved),
                    None => {
                        self.store_entry(instance, CacheEntry::Transient).await;
                        Err(ResolveError::DiscoveryUnavailable(msg))
                    }
                }
            }
        }
    }

    fn fail_open(
        &self,
        instance: &str,
        reason: &str,
    ) -> Result<ResolvedInstance, ResolveError> {
        match self.last_good_of(instance) {
            Some(resolved) => Ok(resolved),
            None => Err(ResolveError::DiscoveryUnavailable(reason.to_owned())),
        }
    }

    fn on_success(&self) {
        self.breaker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .on_success();
    }

    async fn store_positive(&self, instance: &str, resolved: ResolvedInstance) {
        self.store_entry(instance, CacheEntry::Positive(resolved)).await;
        // Per-instance insert: no whole-map copy. The lock is taken and released
        // synchronously (never held across `.await`).
        self.last_good
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(instance.to_owned(), resolved);
    }

    async fn store_entry(&self, instance: &str, entry: CacheEntry) {
        let ttl = ttl_for(entry, self.cache_ttl, random_basis_points());
        self.cache
            .insert(instance.to_owned(), Stored { entry, ttl })
            .await;
    }

    fn last_good_of(&self, instance: &str) -> Option<ResolvedInstance> {
        self.last_good
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(instance)
            .copied()
    }
}

#[async_trait]
impl Resolver for OpenStackResolver {
    async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError> {
        OpenStackResolver::resolve(self, instance).await
    }
}

/// Build one [`Resolver`] per openstack site, keyed by site name (P7).
///
/// All resolvers share a single [`HttpClient`] so Keystone/Nova connections are
/// pooled across every site. The returned map is the value stored in
/// [`crate::state::SharedState::resolvers`] and indexed by the routing layer's
/// `site_idx` → site name.
pub fn build_resolvers(config: &Config) -> HashMap<String, Arc<dyn Resolver>> {
    let client = Arc::new(HttpClient::new());
    config
        .openstack
        .iter()
        .filter_map(|site| {
            let env = config.openstack_envs.get(&site.openstack_env)?;
            let resolver: Arc<dyn Resolver> =
                Arc::new(OpenStackResolver::new(env.config(), Arc::clone(&client)));
            Some((site.name.clone(), resolver))
        })
        .collect()
}

/// Build one shared [`Resolver`] per OpenStack environment, keyed by
/// environment name (C2 — the env-keyed successor to [`build_resolvers`]).
///
/// Every consumer of an environment (openstack wildcard sites and forward
/// sites with instance backends) shares the same resolver instance, and thus
/// the same cache / single-flight / circuit-breaker / last-known-good state.
/// All resolvers share a single [`HttpClient`] so Keystone/Nova connections are
/// pooled across every environment.
pub fn build_env_resolvers(config: &Config) -> HashMap<String, Arc<dyn Resolver>> {
    let client = Arc::new(HttpClient::new());
    config
        .openstack_envs
        .iter()
        .map(|(name, env)| {
            let resolver: Arc<dyn Resolver> =
                Arc::new(OpenStackResolver::new(env.config(), Arc::clone(&client)));
            (name.clone(), resolver)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    enum Reply {
        Ok(ResolvedInstance),
        NotFound,
        NoAddress,
        AuthFailed,
        Transient,
        /// Never completes (a hung control plane) — used to cancel a leader.
        Hang,
    }

    struct FakeFetcher {
        calls: AtomicUsize,
        reply: Mutex<Reply>,
        delay: Duration,
    }

    impl FakeFetcher {
        fn new(reply: Reply, delay: Duration) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                reply: Mutex::new(reply),
                delay,
            }
        }

        fn set_reply(&self, reply: Reply) {
            *self.reply.lock().unwrap_or_else(|p| p.into_inner()) = reply;
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Fetcher for FakeFetcher {
        async fn fetch(&self, _instance: &str) -> Result<ResolvedInstance, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let reply = self.reply.lock().unwrap_or_else(|p| p.into_inner()).clone();
            match reply {
                Reply::Ok(resolved) => Ok(resolved),
                Reply::NotFound => Err(FetchError::NotFound),
                Reply::NoAddress => Err(FetchError::NoAddress),
                Reply::AuthFailed => Err(FetchError::AuthFailed),
                Reply::Transient => Err(FetchError::Transient("boom".to_owned())),
                Reply::Hang => std::future::pending::<Result<ResolvedInstance, FetchError>>().await,
            }
        }
    }

    fn ipv4(octets: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(octets))
    }

    fn inst(octets: [u8; 4]) -> ResolvedInstance {
        ResolvedInstance {
            floating: Some(ipv4(octets)),
            fixed: None,
        }
    }

    fn test_cfg() -> OpenStackConfig {
        OpenStackConfig {
            cache_ttl_secs: 60,
            timeout_secs: 5,
            auth: crate::config::AuthConfig {
                api_url: Url::parse("http://127.0.0.1:9/v3").unwrap(),
                credentials: crate::config::AuthCredentials::ApplicationCredential {
                    id: "id".to_owned(),
                    secret: "secret".to_owned(),
                },
            },
            nova: crate::config::NovaConfig {
                api_url: None,
                all_tenants: true,
            },
        }
    }

    fn resolver_with(fetcher: Arc<FakeFetcher>) -> OpenStackResolver {
        OpenStackResolver::with_fetcher(&test_cfg(), fetcher)
    }

    #[tokio::test]
    async fn positive_result_is_cached_after_first_fetch() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Ok(inst([10, 0, 0, 1])), Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());

        assert_eq!(resolver.resolve("web").await.unwrap().floating, Some(ipv4([10, 0, 0, 1])));
        assert_eq!(resolver.resolve("web").await.unwrap().floating, Some(ipv4([10, 0, 0, 1])));
        assert_eq!(fetcher.calls(), 1, "second resolve must be a cache hit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_flight_folds_concurrent_resolves() {
        let fetcher = Arc::new(FakeFetcher::new(
            Reply::Ok(inst([10, 0, 0, 7])),
            Duration::from_millis(150),
        ));
        let resolver = Arc::new(resolver_with(fetcher.clone()));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let resolver = resolver.clone();
            handles.push(tokio::spawn(async move { resolver.resolve("web").await }));
        }
        for handle in handles {
            let resolved = handle.await.unwrap().unwrap();
            assert_eq!(resolved.floating, Some(ipv4([10, 0, 0, 7])));
        }
        assert_eq!(fetcher.calls(), 1, "concurrent resolves fold into one fetch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_leader_does_not_poison_the_single_flight_key() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Hang, Duration::ZERO));
        let resolver = Arc::new(resolver_with(fetcher.clone()));

        // The leader enters the flight and hangs on the never-returning fetch.
        let leader = {
            let resolver = resolver.clone();
            tokio::spawn(async move { resolver.resolve("web").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A concurrent lookup parks on that flight instead of leading.
        let follower = {
            let resolver = resolver.clone();
            tokio::spawn(async move { resolver.resolve("web").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fetcher.calls(), 1, "follower must park on the leader's flight");

        // Cancel the leader mid-`await`: the RAII guard must publish a failure.
        leader.abort();
        let _ = leader.await;

        let follower_outcome = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .expect("follower must unblock after leader cancellation")
            .expect("follower task joins");
        assert!(
            matches!(follower_outcome, Err(ResolveError::DiscoveryUnavailable(_))),
            "cancelled leader must publish a failure, got {follower_outcome:?}"
        );

        // The key must be usable again: a fresh resolve is not poisoned.
        fetcher.set_reply(Reply::Ok(inst([10, 0, 0, 5])));
        let recovered = tokio::time::timeout(Duration::from_secs(2), resolver.resolve("web"))
            .await
            .expect("key must not be poisoned")
            .expect("recovers after cancellation");
        assert_eq!(recovered.floating, Some(ipv4([10, 0, 0, 5])));
    }

    #[tokio::test]
    async fn negative_result_is_negatively_cached() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::NotFound, Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());

        assert_eq!(resolver.resolve("ghost").await.unwrap_err(), ResolveError::NotFound);
        // Upstream would now answer, but the negative cache short-circuits.
        fetcher.set_reply(Reply::Ok(inst([10, 0, 0, 2])));
        assert_eq!(resolver.resolve("ghost").await.unwrap_err(), ResolveError::NotFound);
        assert_eq!(fetcher.calls(), 1);
    }

    #[tokio::test]
    async fn transient_failure_is_cached_and_reported_unavailable() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Transient, Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());

        assert!(matches!(
            resolver.resolve("web").await.unwrap_err(),
            ResolveError::DiscoveryUnavailable(_)
        ));
        assert!(matches!(
            resolver.resolve("web").await.unwrap_err(),
            ResolveError::DiscoveryUnavailable(_)
        ));
        assert_eq!(fetcher.calls(), 1, "transient cache damps retries");
    }

    #[tokio::test]
    async fn breaker_opens_after_five_failures_and_stops_calling_upstream() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Transient, Duration::ZERO));
        let resolver = OpenStackResolver::with_fetcher(&test_cfg(), fetcher.clone());

        for _ in 0..5 {
            resolver.invalidate("web").await;
            assert!(matches!(
                resolver.resolve("web").await.unwrap_err(),
                ResolveError::DiscoveryUnavailable(_)
            ));
        }
        assert_eq!(fetcher.calls(), 5);

        resolver.invalidate("web").await;
        assert!(matches!(
            resolver.resolve("web").await.unwrap_err(),
            ResolveError::DiscoveryUnavailable(_)
        ));
        assert_eq!(fetcher.calls(), 5, "open breaker must not hit the upstream");
    }

    #[tokio::test]
    async fn transient_failure_fails_open_to_last_known_good() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Ok(inst([10, 0, 0, 9])), Duration::ZERO));
        let resolver = OpenStackResolver::with_fetcher(&test_cfg(), fetcher.clone());

        assert_eq!(resolver.resolve("web").await.unwrap().floating, Some(ipv4([10, 0, 0, 9])));

        resolver.invalidate("web").await;
        fetcher.set_reply(Reply::Transient);
        assert_eq!(
            resolver.resolve("web").await.unwrap().floating,
            Some(ipv4([10, 0, 0, 9])),
            "a control-plane blip serves the last-known-good address"
        );
    }

    #[tokio::test]
    async fn last_known_good_is_isolated_per_instance() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::Ok(inst([10, 0, 0, 1])), Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());

        // Two instances, each with its own positive address.
        assert_eq!(
            resolver.resolve("a").await.unwrap().floating,
            Some(ipv4([10, 0, 0, 1]))
        );
        fetcher.set_reply(Reply::Ok(inst([10, 0, 0, 2])));
        assert_eq!(
            resolver.resolve("b").await.unwrap().floating,
            Some(ipv4([10, 0, 0, 2]))
        );

        // A later failure on `a` serves `a`'s OWN last-known-good, not `b`'s:
        // the per-instance store must not have been disturbed by `b`'s success.
        resolver.invalidate("a").await;
        fetcher.set_reply(Reply::Transient);
        assert_eq!(
            resolver.resolve("a").await.unwrap().floating,
            Some(ipv4([10, 0, 0, 1])),
            "instance a must keep its own last-known-good"
        );

        // ...and `b` still holds its own entry, undisturbed.
        assert_eq!(
            resolver.last_good_of("b").map(|resolved| resolved.floating),
            Some(Some(ipv4([10, 0, 0, 2]))),
            "instance b's last-known-good must be untouched"
        );
        resolver.invalidate("b").await;
        assert_eq!(
            resolver.resolve("b").await.unwrap().floating,
            Some(ipv4([10, 0, 0, 2])),
            "instance b must keep its own last-known-good"
        );
    }

    #[tokio::test]
    async fn no_address_is_reported() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::NoAddress, Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());
        assert_eq!(resolver.resolve("web").await.unwrap_err(), ResolveError::NoAddress);
    }

    #[tokio::test]
    async fn auth_failure_is_reported() {
        let fetcher = Arc::new(FakeFetcher::new(Reply::AuthFailed, Duration::ZERO));
        let resolver = resolver_with(fetcher.clone());
        assert_eq!(resolver.resolve("web").await.unwrap_err(), ResolveError::AuthFailed);
    }

    #[test]
    fn primary_prefers_floating_then_fixed() {
        let both = ResolvedInstance {
            floating: Some(ipv4([1, 1, 1, 1])),
            fixed: Some(ipv4([2, 2, 2, 2])),
        };
        assert_eq!(both.primary(), Some(ipv4([1, 1, 1, 1])));
        let fixed_only = ResolvedInstance {
            floating: None,
            fixed: Some(ipv4([2, 2, 2, 2])),
        };
        assert_eq!(fixed_only.primary(), Some(ipv4([2, 2, 2, 2])));
        assert!(ResolvedInstance { floating: None, fixed: None }.is_empty());
    }

    #[test]
    fn auth_error_mapping_classifies_failures() {
        assert_eq!(map_auth_error(AuthError::Status(401)), FetchError::AuthFailed);
        assert_eq!(map_auth_error(AuthError::Status(403)), FetchError::AuthFailed);
        assert!(matches!(
            map_auth_error(AuthError::Status(500)),
            FetchError::Transient(_)
        ));
        assert!(matches!(map_auth_error(AuthError::Timeout), FetchError::Transient(_)));
        assert!(matches!(
            map_auth_error(AuthError::Http("down".to_owned())),
            FetchError::Transient(_)
        ));
    }

    #[test]
    fn nova_error_mapping_classifies_failures() {
        assert_eq!(map_nova_error(NovaError::NotFound), FetchError::NotFound);
        assert_eq!(map_nova_error(NovaError::Unauthorized), FetchError::AuthFailed);
        assert!(matches!(
            map_nova_error(NovaError::Forbidden),
            FetchError::Transient(_)
        ));
        assert!(matches!(
            map_nova_error(NovaError::Timeout),
            FetchError::Transient(_)
        ));
        assert!(matches!(
            map_nova_error(NovaError::Status(500)),
            FetchError::Transient(_)
        ));
    }
}
