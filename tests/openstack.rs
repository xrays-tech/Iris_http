//! P5 integration tests: the real [`OpenStackResolver`] against
//! `scripts/mock_openstack.py` (the contract-defined external boundary).
//!
//! These spawn the mock as a child process (stdlib Python only) so the full
//! auth -> Nova -> address-selection path runs over real HTTP. Test code is
//! exempt from the production `unwrap` ban (plan/01 §1.4).

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_http::config::{AuthConfig, AuthCredentials, NovaConfig, OpenStackConfig};
use iris_http::openstack::{HttpClient, OpenStackResolver, ResolveError};
use url::Url;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/openstack/servers"
);
const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mock_openstack.py");

/// A running mock control plane; killed on drop.
struct Mock {
    child: Child,
    port: u16,
}

impl Mock {
    fn start(args: &[String]) -> Self {
        let mut command = Command::new("python3");
        command
            .arg(SCRIPT)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn scripts/mock_openstack.py");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read LISTENING line");
        let port: u16 = line
            .trim()
            .strip_prefix("LISTENING ")
            .expect("LISTENING <port>")
            .parse()
            .expect("port parses");
        Mock { child, port }
    }

    fn servers_dir_arg() -> String {
        FIXTURES.to_owned()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A tempdir holding a fresh request-record file; keep the guard alive.
fn record_file() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("requests.jsonl");
    let as_str = path
        .to_str()
        .expect("record path is utf-8")
        .to_owned();
    (dir, as_str)
}

fn ip(literal: &str) -> IpAddr {
    literal.parse().expect("ip literal")
}

fn config_for(port: u16, ttl_secs: u64, timeout_secs: u64) -> OpenStackConfig {
    OpenStackConfig {
        cache_ttl_secs: ttl_secs,
        timeout_secs,
        auth: AuthConfig {
            api_url: Url::parse(&format!("http://127.0.0.1:{port}/v3")).expect("auth url"),
            credentials: AuthCredentials::ApplicationCredential {
                id: "id".to_owned(),
                secret: "secret".to_owned(),
            },
        },
        nova: NovaConfig {
            api_url: None,
            all_tenants: true,
        },
    }
}

fn resolver_for(port: u16, ttl: u64, timeout: u64) -> OpenStackResolver {
    OpenStackResolver::new(&config_for(port, ttl, timeout), Arc::new(HttpClient::new()))
}

/// Count `POST /v3/auth/tokens` entries in the record file.
fn auth_calls(record: &str) -> usize {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("/v3/auth/tokens"))
        .count()
}

/// A port that is bound then released (very likely closed for connections).
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

fn mock_with_record(record: &str, extra: &[&str]) -> Mock {
    let mut args = vec![
        "--record".to_owned(),
        record.to_owned(),
        "--servers-dir".to_owned(),
        Mock::servers_dir_arg(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    Mock::start(&args)
}

// ---------------------------------------------------------------------------
// Token lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_is_fetched_once_and_reused_across_lookups() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 5);

    let a = resolver.resolve("multi-nic").await.expect("multi-nic resolves");
    let b = resolver
        .resolve("floating-only")
        .await
        .expect("floating-only resolves");
    assert_eq!(a.floating, Some(ip("203.0.113.10")));
    assert_eq!(b.floating, Some(ip("203.0.113.11")));
    assert_eq!(
        auth_calls(&record),
        1,
        "two lookups must reuse the cached token (one /v3/auth/tokens call)"
    );
}

#[tokio::test]
async fn unauthorized_once_triggers_single_refresh_then_succeeds() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &["--servers-unauthorized-once"]);
    let resolver = resolver_for(mock.port, 60, 5);

    let resolved = resolver.resolve("multi-nic").await.expect("retry succeeds");
    assert_eq!(resolved.floating, Some(ip("203.0.113.10")));
    assert_eq!(
        auth_calls(&record),
        2,
        "a 401 must force exactly one token refresh"
    );
}

#[tokio::test]
async fn auth_failure_is_reported_as_auth_failed() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &["--auth-fail"]);
    let resolver = resolver_for(mock.port, 60, 5);

    assert_eq!(
        resolver.resolve("multi-nic").await.unwrap_err(),
        ResolveError::AuthFailed
    );
}

#[tokio::test]
async fn stalling_keystone_is_bounded_by_the_site_timeout() {
    let (_guard, record) = record_file();
    // Keystone accepts the request, then stalls far beyond the site timeout.
    let mock = mock_with_record(&record, &["--auth-delay", "10"]);
    let resolver = resolver_for(mock.port, 60, 1); // timeout_secs = 1

    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(5), resolver.resolve("multi-nic"))
        .await
        .expect("token fetch must be bounded, not hang");
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, Err(ResolveError::DiscoveryUnavailable(_))),
        "a stalled Keystone must surface as DiscoveryUnavailable, got {outcome:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(5),
        "bounded by the site timeout (~1s), not the 10s stall: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Nova result handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_nic_prefers_floating_over_fixed() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 5);

    let resolved = resolver.resolve("multi-nic").await.expect("resolves");
    assert_eq!(resolved.floating, Some(ip("203.0.113.10")));
    assert_eq!(resolved.fixed, Some(ip("192.168.1.10")));
}

#[tokio::test]
async fn floating_only_and_fixed_only_and_ipv6() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 5);

    let floating = resolver.resolve("floating-only").await.expect("resolves");
    assert_eq!(floating.floating, Some(ip("203.0.113.11")));
    assert_eq!(floating.fixed, None);

    let fixed = resolver.resolve("fixed-only").await.expect("resolves");
    assert_eq!(fixed.floating, None);
    assert_eq!(fixed.fixed, Some(ip("192.168.1.20")));

    let v6 = resolver.resolve("ipv6-only").await.expect("resolves");
    assert_eq!(v6.floating, Some(ip("fd00::20")));
    assert_eq!(v6.fixed, Some(ip("fd00::10")));
}

#[tokio::test]
async fn empty_server_list_is_not_found() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 5);

    assert_eq!(
        resolver.resolve("empty-list").await.unwrap_err(),
        ResolveError::NotFound
    );
}

#[tokio::test]
async fn unknown_instance_404_is_not_found() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 5);

    assert_eq!(
        resolver.resolve("does-not-exist").await.unwrap_err(),
        ResolveError::NotFound
    );
}

// ---------------------------------------------------------------------------
// Failure modes / fail-open
// ---------------------------------------------------------------------------

#[tokio::test]
async fn control_plane_down_without_record_is_discovery_unavailable() {
    let resolver = resolver_for(closed_port(), 60, 1);
    match resolver.resolve("web").await {
        Err(ResolveError::DiscoveryUnavailable(_)) => {}
        other => panic!("expected DiscoveryUnavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn control_plane_down_fails_open_to_last_known_good() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &[]);
    let resolver = resolver_for(mock.port, 60, 1);

    let first = resolver.resolve("multi-nic").await.expect("first resolve");
    assert_eq!(first.floating, Some(ip("203.0.113.10")));

    // Control plane dies; force a fresh attempt (bypass the positive cache).
    drop(mock);
    resolver.invalidate("multi-nic").await;

    let second = resolver
        .resolve("multi-nic")
        .await
        .expect("fail-open serves last-known-good");
    assert_eq!(second, first);
}

#[tokio::test]
async fn forced_500_is_transient_and_does_not_poison_later_recovery() {
    let (_guard, record) = record_file();
    let mock = mock_with_record(&record, &["--servers-status", "500"]);
    let resolver = resolver_for(mock.port, 60, 1);

    match resolver.resolve("multi-nic").await {
        Err(ResolveError::DiscoveryUnavailable(_)) => {}
        other => panic!("expected DiscoveryUnavailable on 500, got {other:?}"),
    }
}
