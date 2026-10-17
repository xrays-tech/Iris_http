//! Discovery resilience primitives: TTL cache policy, single-flight folding,
//! the circuit breaker and the token cache (`discovery.md` §6).
//!
//! Everything here is plain state (a [`Breaker`] state machine, a
//! [`FlightMap`] keyed by instance, an in-process [`TokenCache`]) so it is unit
//! tested directly with no mocking; only the surrounding resolver performs
//! HTTP, and that boundary is the mock OpenStack server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use moka::future::Cache;
use moka::Expiry;
use url::Url;

use super::auth::{fetch_token, AuthError, Token};
use super::{HttpClient, ResolveError, ResolvedInstance};
use crate::config::AuthConfig;

/// Negative-cache TTL for a definitively absent instance (`discovery.md` §6.1).
pub(crate) const NEGATIVE_TTL: Duration = Duration::from_secs(5);
/// Transient-error cache TTL (network/5xx/timeout) to damp retries.
pub(crate) const TRANSIENT_TTL: Duration = Duration::from_secs(1);
/// Consecutive failures that open the breaker (`discovery.md` §6.3).
pub(crate) const BREAKER_THRESHOLD: u32 = 5;
/// Jitter magnitude in basis points (±10%).
const JITTER_BP: i64 = 1000;
/// Bound on cached instances (process-local, cheap to evict).
const CACHE_CAPACITY: u64 = 10_000;

/// The result stored for an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheEntry {
    /// A frozen positive resolution.
    Positive(ResolvedInstance),
    /// The instance does not exist (short TTL).
    Negative,
    /// The instance exists but exposes no usable address (short TTL).
    NoAddress,
    /// A transient discovery failure (very short TTL).
    Transient,
}

/// A cached entry paired with its per-entry TTL.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stored {
    /// The frozen result.
    pub(crate) entry: CacheEntry,
    /// Remaining lifetime set at insert time.
    pub(crate) ttl: Duration,
}

/// Per-entry expiration: each [`Stored`] carries its own TTL so positive,
/// negative and transient entries coexist in one cache (`discovery.md` §6.1).
struct TtlExpiry;

impl Expiry<String, Stored> for TtlExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Stored,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

/// The discovery cache type (positive/negative/transient share one table).
pub(crate) type DiscoveryCache = Cache<String, Stored>;

/// Build the discovery cache with per-entry TTL.
pub(crate) fn build_cache() -> DiscoveryCache {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .expire_after(TtlExpiry)
        .build()
}

/// Apply ±`basis_points`/10000 jitter to `ttl` (`discovery.md` §6.1). The
/// result never goes below zero.
pub(crate) fn jitter(ttl: Duration, basis_points: i64) -> Duration {
    let ms = ttl.as_millis() as i64;
    let delta = ms.saturating_mul(basis_points) / 10_000;
    Duration::from_millis(ms.saturating_add(delta).max(0) as u64)
}

/// A pseudo-random jitter factor in `[-1000, 1000]` basis points, seeded from
/// the wall clock (no `rand` dependency, no shared RNG state).
pub(crate) fn random_basis_points() -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u64 ^ (d.subsec_nanos() as u64) << 21)
        .unwrap_or(0);
    let mut x = nanos ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x % (2 * JITTER_BP as u64 + 1)) as i64 - JITTER_BP
}

/// The TTL used for a given entry kind, jittered.
pub(crate) fn ttl_for(entry: CacheEntry, positive_ttl: Duration, basis_points: i64) -> Duration {
    match entry {
        CacheEntry::Positive(_) => jitter(positive_ttl, basis_points),
        CacheEntry::Negative => jitter(NEGATIVE_TTL, basis_points),
        CacheEntry::NoAddress | CacheEntry::Transient => jitter(TRANSIENT_TTL, basis_points),
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Breaker states (`discovery.md` §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakerState {
    /// Normal operation.
    Closed,
    /// Tripped: no requests are sent until the cooldown elapses.
    Open,
    /// Cooldown elapsed; a single probe is allowed.
    HalfOpen,
}

/// A consecutive-failure circuit breaker with an injectable clock so the state
/// machine is unit tested deterministically.
#[derive(Debug)]
pub(crate) struct Breaker {
    threshold: u32,
    cooldown: Duration,
    failures: u32,
    state: BreakerState,
    open_until: Option<Instant>,
}

impl Breaker {
    /// Create a breaker that opens after `threshold` consecutive failures and
    /// stays open for `cooldown` before half-opening.
    pub(crate) fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            failures: 0,
            state: BreakerState::Closed,
            open_until: None,
        }
    }

    /// The current state.
    #[cfg(test)]
    pub(crate) fn state(&self) -> BreakerState {
        self.state
    }

    /// Whether a request may proceed at `now`. Transitions `Open → HalfOpen`
    /// once the cooldown has elapsed.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        match self.state {
            BreakerState::Closed => true,
            BreakerState::HalfOpen => true,
            BreakerState::Open => match self.open_until {
                Some(until) if now >= until => {
                    self.state = BreakerState::HalfOpen;
                    true
                }
                _ => false,
            },
        }
    }

    /// Record a success: close the breaker and reset the failure streak.
    pub(crate) fn on_success(&mut self) {
        self.state = BreakerState::Closed;
        self.failures = 0;
        self.open_until = None;
    }

    /// Record a failure at `now`; open (or re-open) once the threshold is hit.
    pub(crate) fn on_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.state == BreakerState::HalfOpen || self.failures >= self.threshold {
            self.state = BreakerState::Open;
            self.open_until = Some(now + self.cooldown);
        }
    }
}

// ---------------------------------------------------------------------------
// Single-flight
// ---------------------------------------------------------------------------

type FlightResult = Result<ResolvedInstance, ResolveError>;

/// One in-flight resolution shared by all concurrent waiters for a key.
pub(crate) struct Flight {
    tx: tokio::sync::watch::Sender<Option<FlightResult>>,
}

impl Flight {
    fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel::<Option<FlightResult>>(None);
        Self { tx }
    }

    /// Wait until the leader publishes a result and return a clone of it.
    pub(crate) async fn wait(&self) -> FlightResult {
        let mut rx = self.tx.subscribe();
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            if rx.changed().await.is_err() {
                // The leader vanished without publishing; treat as unavailable
                // rather than waiting forever.
                return Err(ResolveError::DiscoveryUnavailable(
                    "single-flight leader disappeared".to_owned(),
                ));
            }
        }
    }
}

/// Whether the caller leads (and must perform) the fetch or follows an
/// existing one.
pub(crate) enum FlightRole {
    /// This caller owns the in-flight fetch.
    Leader(Arc<Flight>),
    /// Another caller is already fetching; wait on this flight.
    Follower(Arc<Flight>),
}

/// Folds concurrent `resolve(key)` calls into a single in-flight fetch
/// (`discovery.md` §6.2).
#[derive(Default)]
pub(crate) struct FlightMap {
    inner: Mutex<HashMap<String, Arc<Flight>>>,
}

impl FlightMap {
    /// Create an empty map.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Flight>>> {
        self.inner.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Join an in-flight fetch for `key`, or become its leader.
    pub(crate) fn join(&self, key: &str) -> FlightRole {
        let mut map = self.lock();
        if let Some(existing) = map.get(key) {
            return FlightRole::Follower(existing.clone());
        }
        let flight = Arc::new(Flight::new());
        map.insert(key.to_owned(), flight.clone());
        FlightRole::Leader(flight)
    }

    /// Publish `result` to all waiters and clear the in-flight slot.
    pub(crate) fn finish(&self, key: &str, result: &FlightResult) {
        let flight = self.lock().remove(key);
        if let Some(flight) = flight {
            let _ = flight.tx.send(Some(result.clone()));
        }
    }
}

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

/// In-process token cache with a 60s early-refresh window and serialised
/// refresh (the async mutex prevents concurrent refresh storms).
pub(crate) struct TokenCache {
    inner: tokio::sync::Mutex<Option<Token>>,
}

impl TokenCache {
    /// Create an empty token cache.
    pub(crate) fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
        }
    }

    /// Return a valid token, fetching a fresh one when missing or near expiry.
    pub(crate) async fn get(
        &self,
        auth: &AuthConfig,
        api_url: &Url,
        client: &HttpClient,
    ) -> Result<Token, AuthError> {
        let mut guard = self.inner.lock().await;
        if let Some(token) = guard.as_ref() {
            if token.is_valid_at(Utc::now()) {
                return Ok(token.clone());
            }
        }
        let token = fetch_token(auth, api_url, client).await?;
        *guard = Some(token.clone());
        Ok(token)
    }

    /// Force a refresh (used once after a Nova 401, `discovery.md` §3).
    pub(crate) async fn force_refresh(
        &self,
        auth: &AuthConfig,
        api_url: &Url,
        client: &HttpClient,
    ) -> Result<Token, AuthError> {
        let mut guard = self.inner.lock().await;
        let token = fetch_token(auth, api_url, client).await?;
        *guard = Some(token.clone());
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn inst(v4: [u8; 4]) -> ResolvedInstance {
        ResolvedInstance {
            floating: Some(IpAddr::V4(Ipv4Addr::from(v4))),
            fixed: None,
        }
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let base = Duration::from_secs(100);
        assert_eq!(jitter(base, 0), base);
        assert_eq!(jitter(base, 1000), Duration::from_secs(110));
        assert_eq!(jitter(base, -1000), Duration::from_secs(90));
        assert_eq!(jitter(base, 500), Duration::from_millis(105_000));
    }

    #[test]
    fn jitter_never_underflows() {
        assert_eq!(jitter(Duration::from_secs(1), -10_000), Duration::ZERO);
    }

    #[test]
    fn random_basis_points_is_in_range() {
        for _ in 0..1000 {
            let bp = random_basis_points();
            assert!((-JITTER_BP..=JITTER_BP).contains(&bp), "out of range: {bp}");
        }
    }

    #[test]
    fn ttl_for_selects_per_kind_ttl() {
        let positive = Duration::from_secs(86_400);
        assert_eq!(
            ttl_for(CacheEntry::Positive(inst([10, 0, 0, 1])), positive, 0),
            positive
        );
        assert_eq!(
            ttl_for(CacheEntry::Negative, positive, 0),
            NEGATIVE_TTL
        );
        assert_eq!(
            ttl_for(CacheEntry::NoAddress, positive, 0),
            TRANSIENT_TTL
        );
        assert_eq!(
            ttl_for(CacheEntry::Transient, positive, 0),
            TRANSIENT_TTL
        );
    }

    #[test]
    fn breaker_opens_after_threshold_and_half_opens_after_cooldown() {
        let cooldown = Duration::from_secs(15);
        let mut breaker = Breaker::new(5, cooldown);
        let start = Instant::now();

        for i in 0..4 {
            assert!(breaker.allow(start), "closed breaker allows ({i})");
            breaker.on_failure(start);
            assert_eq!(breaker.state(), BreakerState::Closed, "still closed at {i}");
        }
        // Fifth failure trips it.
        assert!(breaker.allow(start));
        breaker.on_failure(start);
        assert_eq!(breaker.state(), BreakerState::Open);

        // Open: no requests until cooldown elapses.
        assert!(!breaker.allow(start + Duration::from_secs(14)));
        assert_eq!(breaker.state(), BreakerState::Open);

        // Cooldown elapsed → half-open probe allowed.
        assert!(breaker.allow(start + cooldown));
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn breaker_half_open_success_closes() {
        let mut breaker = Breaker::new(1, Duration::from_secs(1));
        let start = Instant::now();
        breaker.on_failure(start);
        assert_eq!(breaker.state(), BreakerState::Open);

        assert!(breaker.allow(start + Duration::from_secs(1)));
        assert_eq!(breaker.state(), BreakerState::HalfOpen);

        breaker.on_success();
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert!(breaker.allow(start));
    }

    #[test]
    fn breaker_half_open_failure_reopens() {
        let cooldown = Duration::from_secs(10);
        let mut breaker = Breaker::new(1, cooldown);
        let start = Instant::now();
        breaker.on_failure(start);
        let probe = start + cooldown;
        assert!(breaker.allow(probe));
        assert_eq!(breaker.state(), BreakerState::HalfOpen);

        breaker.on_failure(probe);
        assert_eq!(breaker.state(), BreakerState::Open);
        assert!(!breaker.allow(probe + Duration::from_secs(9)));
        assert!(breaker.allow(probe + cooldown));
    }
}
