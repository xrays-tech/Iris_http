//! P6 integration tests: fixture-driven `build_firewall` + ACL evaluation.
//!
//! The committed fixtures under `tests/fixtures/firewall/` are assembled into
//! temporary directories so a deliberately-invalid file (e.g. `over-limit.role`)
//! can be isolated from the valid ones — the production loader is fail-fast, so
//! all fixtures can never be loaded together successfully.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use iris_http::config::ConfigError;
use iris_http::firewall::{build_firewall, AclReason, AclVerdict, FirewallTable};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/firewall")
}

/// Copy the named fixtures into a fresh temp dir and load them.
fn build_from(names: &[&str]) -> Result<(tempfile::TempDir, FirewallTable), ConfigError> {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in names {
        std::fs::copy(fixtures_dir().join(name), dir.path().join(name)).expect("copy fixture");
    }
    let table = build_firewall(dir.path())?;
    Ok((dir, table))
}

fn ip(s: &str) -> IpAddr {
    s.parse().expect("test IP parses")
}

#[test]
fn valid_allow_permits_open_ports_and_any_non_blacklisted_client() {
    let (_dir, table) = build_from(&["valid-allow.role"]).expect("valid fixture builds");
    assert_eq!(
        table.check("valid-allow", 80, ip("203.0.113.9")),
        AclVerdict::Allow
    );
    assert_eq!(
        table.check("valid-allow", 443, ip("198.51.100.4")),
        AclVerdict::Allow
    );
    assert_eq!(
        table.check("valid-allow", 8080, ip("203.0.113.9")),
        AclVerdict::Deny(AclReason::PortNotAllowed(8080))
    );
}

#[test]
fn deny_all_fixture_denies_every_port() {
    let (_dir, table) = build_from(&["deny-all.role"]).expect("valid fixture builds");
    assert_eq!(
        table.check("deny-all", 80, ip("203.0.113.9")),
        AclVerdict::Deny(AclReason::PortNotAllowed(80))
    );
}

#[test]
fn blacklist_fixture_denies_cidr_plain_and_ipv6() {
    let (_dir, table) = build_from(&["blacklist.role"]).expect("valid fixture builds");
    // Must open the port first for the blacklist branch to be reached.
    assert_eq!(
        table.check("blacklist", 80, ip("203.0.113.42")),
        AclVerdict::Deny(AclReason::Blacklisted(ip("203.0.113.42")))
    );
    assert_eq!(
        table.check("blacklist", 80, ip("10.0.0.5")),
        AclVerdict::Deny(AclReason::Blacklisted(ip("10.0.0.5")))
    );
    assert_eq!(
        table.check("blacklist", 80, ip("2001:db8::1")),
        AclVerdict::Deny(AclReason::Blacklisted(ip("2001:db8::1")))
    );
    assert_eq!(table.check("blacklist", 80, ip("10.0.0.6")), AclVerdict::Allow);
}

#[test]
fn whitelist_fixture_allows_members_and_denies_others() {
    let (_dir, table) = build_from(&["whitelist.role"]).expect("valid fixture builds");
    assert_eq!(
        table.check("whitelist", 8080, ip("10.9.8.7")),
        AclVerdict::Allow
    );
    assert_eq!(table.check("whitelist", 8080, ip("::1")), AclVerdict::Allow);
    assert_eq!(
        table.check("whitelist", 8080, ip("192.0.2.1")),
        AclVerdict::Deny(AclReason::NotWhitelisted(ip("192.0.2.1")))
    );
}

#[test]
fn no_role_is_denied_even_with_other_roles_present() {
    let (_dir, table) = build_from(&["valid-allow.role"]).expect("valid fixture builds");
    assert_eq!(
        table.check("ghost", 80, ip("203.0.113.9")),
        AclVerdict::Deny(AclReason::NoRole)
    );
}

#[test]
fn bad_yaml_fixture_fails_build() {
    let err = build_from(&["bad-yaml.role"]).expect_err("bad yaml must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn bad_type_fixture_fails_build() {
    let err = build_from(&["bad-type.role"]).expect_err("type error must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn bad_cidr_fixture_fails_build() {
    let err = build_from(&["bad-cidr.role"]).expect_err("bad CIDR must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn bad_port_fixture_fails_build() {
    let err = build_from(&["bad-port.role"]).expect_err("port 0 must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn over_limit_fixture_fails_build() {
    let err = build_from(&["over-limit.role"]).expect_err("over-limit must fail");
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn good_and_bad_together_fail_fast() {
    let err = build_from(&["valid-allow.role", "bad-cidr.role"])
        .expect_err("one bad file fails the whole build");
    assert!(matches!(err, ConfigError::Parse { .. }));
}
