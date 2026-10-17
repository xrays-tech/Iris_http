//! P1 integration tests for `iris_http::config::load_config`.
//!
//! `load_config` is a pure function (env injected), so these tests drive the
//! real parser over tempdirs / committed fixtures with no mocking. Test code is
//! exempt from the production `unwrap` ban (plan/01 §1.4).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use iris_http::config::{
    load_config, AuthCredentials, ConfigError, HealthKind, LbMode, OpenStackSite,
};

/// Create a tempdir containing a `sites/` subdirectory with the given files.
fn sites_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let sites = dir.path().join("sites");
    fs::create_dir(&sites).expect("create sites dir");
    for (name, content) in files {
        fs::write(sites.join(name), content).expect("write site file");
    }
    (dir, sites)
}

/// Create a tempdir holding a dummy cert/key pair; returns their paths.
fn cert_pair(dir: &Path, stem: &str) -> (PathBuf, PathBuf) {
    let crt = dir.join(format!("{stem}.crt"));
    let key = dir.join(format!("{stem}.key"));
    fs::write(&crt, "dummy cert\n").expect("write cert");
    fs::write(&key, "dummy key\n").expect("write key");
    (crt, key)
}

/// A forward site YAML with `https` disabled (no cert needed).
fn forward_yaml(body_after_mode: &str) -> String {
    format!("name: site\ndomain: www.example.com\nmode: forward\nhttps: false\n{body_after_mode}")
}

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn load_ok(files: &[(&str, &str)], env: &HashMap<String, String>) -> iris_http::config::Config {
    let (_guard, sites) = sites_dir(files);
    load_config(&sites, env).expect("config loads")
}

fn load_err(files: &[(&str, &str)], env: &HashMap<String, String>) -> ConfigError {
    let (_guard, sites) = sites_dir(files);
    load_config(&sites, env).expect_err("config must be rejected")
}

// ---------------------------------------------------------------------------
// Directory semantics
// ---------------------------------------------------------------------------

#[test]
fn empty_sites_dir_loads_zero_sites() {
    let (_guard, sites) = sites_dir(&[]);
    let cfg = load_config(&sites, &HashMap::new()).expect("empty sites dir is valid");
    assert!(cfg.forward.is_empty());
    assert!(cfg.openstack.is_empty());
}

#[test]
fn committed_empty_startup_fixture_loads() {
    let cfg = load_config(Path::new("tests/fixtures/config/sites"), &HashMap::new())
        .expect("startup fixture (empty sites/) loads");
    assert!(cfg.forward.is_empty());
    assert!(cfg.openstack.is_empty());
}

#[test]
fn missing_sites_dir_is_io_error() {
    let err = load_config(Path::new("/nonexistent/iris/sites"), &HashMap::new())
        .expect_err("missing sites dir must fail");
    assert!(matches!(err, ConfigError::Io(_)), "got {err:?}");
}

#[test]
fn non_yaml_files_are_ignored() {
    let cfg = load_ok(
        &[
            ("README.md", "# not a site\n"),
            ("001.yaml", &forward_yaml("backends:\n  - addr: 127.0.0.1:80\n")),
        ],
        &HashMap::new(),
    );
    assert_eq!(cfg.forward.len(), 1);
}

// ---------------------------------------------------------------------------
// forward: happy path
// ---------------------------------------------------------------------------

#[test]
fn valid_forward_site_parses_all_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (crt, key) = cert_pair(dir.path(), "site");
    let yaml = format!(
        "name: www-example\nmode: forward\ndomain: WWW.Example.COM.\nhttp: false\nhttps: true\n\
         cert_file: {crt}\nkey_file: {key}\n\
         backends:\n  - addr: 10.0.0.5:8080\n    weight: 3\n  - addr: 10.0.0.6:8080\n\
         load_balancer: weighted_round_robin\n\
         health_check:\n  kind: http\n  http_path: /healthz\n  interval_secs: 5\n  timeout_secs: 2\n",
        crt = crt.display(),
        key = key.display(),
    );
    let cfg = load_ok(&[("a.yaml", &yaml)], &HashMap::new());

    assert_eq!(cfg.forward.len(), 1);
    let f = &cfg.forward[0];
    assert_eq!(f.name, "www-example");
    // domain is normalised: lowercased, trailing dot stripped.
    assert_eq!(f.domain, "www.example.com");
    assert!(!f.http);
    assert!(f.https);
    assert_eq!(f.cert_file.as_deref(), Some(crt.as_path()));
    assert_eq!(f.key_file.as_deref(), Some(key.as_path()));
    assert_eq!(f.lb_mode, LbMode::WeightedRoundRobin);
    assert_eq!(f.backends.len(), 2);
    assert_eq!(f.backends[0].weight, 3);
    assert_eq!(f.backends[1].weight, 1, "weight defaults to 1");
    assert_eq!(f.backends[0].addr.to_string(), "10.0.0.5:8080");
    let hc = f.health_check.as_ref().expect("health_check present");
    assert_eq!(hc.kind, HealthKind::Http);
    assert_eq!(hc.http_path.as_deref(), Some("/healthz"));
    assert_eq!(hc.interval_secs, 5);
    assert_eq!(hc.timeout_secs, 2);
}

#[test]
fn forward_defaults_http_true_and_lb_round_robin() {
    let cfg = load_ok(
        &[(
            "a.yaml",
            "name: s\nmode: forward\ndomain: a.example.com\nhttps: false\nbackends:\n  - addr: 127.0.0.1:80\n",
        )],
        &HashMap::new(),
    );
    let f = &cfg.forward[0];
    assert!(f.http, "http defaults to true");
    assert_eq!(f.lb_mode, LbMode::RoundRobin, "lb default is round_robin");
    assert!(f.health_check.is_none());
}

// ---------------------------------------------------------------------------
// forward: rejections
// ---------------------------------------------------------------------------

#[test]
fn forward_empty_backends_rejected() {
    let yaml = forward_yaml("backends: []\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_missing_backends_rejected() {
    let yaml = "name: s\nmode: forward\ndomain: a.example.com\nhttps: false\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn forward_hostname_backend_addr_rejected() {
    let yaml = forward_yaml("backends:\n  - addr: backend.example.com:8080\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_addr_without_port_rejected() {
    let yaml = forward_yaml("backends:\n  - addr: 10.0.0.5\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_weight_zero_is_accepted_but_disabled() {
    let yaml = forward_yaml("backends:\n  - addr: 10.0.0.9:8080\n    weight: 0\n");
    let cfg = load_ok(&[("a.yaml", &yaml)], &HashMap::new());
    assert_eq!(cfg.forward[0].backends[0].weight, 0);
}

#[test]
fn forward_weight_above_max_rejected() {
    let yaml = forward_yaml("backends:\n  - addr: 10.0.0.9:8080\n    weight: 65536\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_weight_max_accepted() {
    let yaml = forward_yaml("backends:\n  - addr: 10.0.0.9:8080\n    weight: 65535\n");
    let cfg = load_ok(&[("a.yaml", &yaml)], &HashMap::new());
    assert_eq!(cfg.forward[0].backends[0].weight, 65535);
}

#[test]
fn forward_unknown_lb_mode_rejected() {
    let yaml = forward_yaml("load_balancer: least_conn\nbackends:\n  - addr: 127.0.0.1:80\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn all_lb_modes_parse() {
    for (raw, expected) in [
        ("round_robin", LbMode::RoundRobin),
        ("weighted_round_robin", LbMode::WeightedRoundRobin),
        ("random", LbMode::Random),
        ("ketama", LbMode::Ketama),
    ] {
        let yaml = forward_yaml(&format!(
            "load_balancer: {raw}\nbackends:\n  - addr: 127.0.0.1:80\n"
        ));
        let cfg = load_ok(&[("a.yaml", &yaml)], &HashMap::new());
        assert_eq!(cfg.forward[0].lb_mode, expected);
    }
}

#[test]
fn forward_https_true_without_cert_rejected() {
    let yaml = "name: s\nmode: forward\ndomain: a.example.com\nhttps: true\n\
                backends:\n  - addr: 10.0.0.1:80\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_https_true_with_missing_cert_file_rejected() {
    let yaml = "name: s\nmode: forward\ndomain: a.example.com\nhttps: true\n\
                cert_file: /no/such/cert.crt\nkey_file: /no/such/key.key\n\
                backends:\n  - addr: 10.0.0.1:80\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_domain_with_scheme_rejected() {
    let yaml = "name: s\nmode: forward\ndomain: https://a.example.com\nhttps: false\n\
                backends:\n  - addr: 10.0.0.1:80\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_wildcard_domain_rejected() {
    let yaml = "name: s\nmode: forward\ndomain: '*.example.com'\nhttps: false\n\
                backends:\n  - addr: 10.0.0.1:80\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// openstack: happy path
// ---------------------------------------------------------------------------

#[test]
fn valid_openstack_password_site_expands_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (crt, key) = cert_pair(dir.path(), "rent");
    let fw = dir.path().join("firewall");
    fs::create_dir(&fw).expect("firewall dir");
    let yaml = format!(
        "name: rent\nmode: openstack\nbase_domain: RENT.DO.TOP.\nhttps: true\n\
         cert_file: {crt}\nkey_file: {key}\nfirewall_dir: {fw}\n\
         openstack:\n  timeout_secs: 5\n  cache_ttl_secs: 86400\n\
         \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: password\n    username: proxy\n    password: ${{OS_PASSWORD}}\n\
         \x20   project_name: admin\n    project_domain_name: Default\n    user_domain_name: Default\n\
         \x20 nova:\n    api_url: https://controller.example.com:8774/v2.1\n",
        crt = crt.display(),
        key = key.display(),
        fw = fw.display(),
    );
    let cfg = load_ok(&[("o.yaml", &yaml)], &env(&[("OS_PASSWORD", "s3cret")]));

    assert_eq!(cfg.openstack.len(), 1);
    let o = &cfg.openstack[0];
    assert_eq!(o.name, "rent");
    assert_eq!(o.base_domain, "rent.do.top");
    assert!(o.https);
    assert_eq!(o.firewall_dir.as_deref(), Some(fw.as_path()));
    assert_eq!(o.openstack.timeout_secs, 5);
    assert_eq!(o.openstack.cache_ttl_secs, 86400);
    assert!(o.openstack.nova.all_tenants, "all_tenants defaults to true");
    assert_eq!(
        o.openstack.nova.api_url.as_ref().map(|u| u.as_str()),
        Some("https://controller.example.com:8774/v2.1")
    );
    assert_eq!(
        o.openstack.auth.api_url.as_str(),
        "https://keystone.example.com:5000/v3"
    );
    match &o.openstack.auth.credentials {
        AuthCredentials::Password {
            username, password, ..
        } => {
            assert_eq!(username, "proxy");
            assert_eq!(password, "s3cret", "env var must be expanded");
        }
        other => panic!("expected Password auth, got {other:?}"),
    }
}

#[test]
fn openstack_application_credential_parses_and_expands_secret() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: abc123\n    application_credential_secret: ${OS_SECRET}\n\
                \x20 nova:\n    all_tenants: false\n";
    let cfg = load_ok(&[("o.yaml", yaml)], &env(&[("OS_SECRET", "topsecret")]));
    match &cfg.openstack[0].openstack.auth.credentials {
        AuthCredentials::ApplicationCredential { id, secret } => {
            assert_eq!(id, "abc123");
            assert_eq!(secret, "topsecret");
        }
        other => panic!("expected ApplicationCredential, got {other:?}"),
    }
    assert!(!cfg.openstack[0].openstack.nova.all_tenants);
}

#[test]
fn openstack_cache_ttl_defaults_to_one_day() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let cfg = load_ok(&[("o.yaml", yaml)], &HashMap::new());
    assert_eq!(cfg.openstack[0].openstack.cache_ttl_secs, 86_400);
}

// ---------------------------------------------------------------------------
// openstack: rejections
// ---------------------------------------------------------------------------

#[test]
fn openstack_cache_ttl_below_min_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n  cache_ttl_secs: 59\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_cache_ttl_above_max_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n  cache_ttl_secs: 604801\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_cache_ttl_bounds_inclusive() {
    for ttl in [60u64, 604_800u64] {
        let yaml = format!(
            "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
             openstack:\n  timeout_secs: 5\n  cache_ttl_secs: {ttl}\n\
             \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
             \x20   application_credential_id: a\n    application_credential_secret: b\n\
             \x20 nova: {{}}\n"
        );
        let cfg = load_ok(&[("o.yaml", &yaml)], &HashMap::new());
        assert_eq!(cfg.openstack[0].openstack.cache_ttl_secs, ttl);
    }
}

#[test]
fn openstack_invalid_base_domain_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: https://rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_password_branch_missing_field_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: password\n    username: proxy\n    password: p\n\
                \x20   project_name: admin\n    project_domain_name: Default\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_approach_mixing_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: password\n    username: proxy\n    password: p\n\
                \x20   project_name: admin\n    project_domain_name: Default\n    user_domain_name: Default\n\
                \x20   application_credential_id: a\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_unknown_approach_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: token\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_missing_firewall_dir_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                firewall_dir: /no/such/firewall/dir\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_invalid_nova_api_url_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova:\n    api_url: not-a-url\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_missing_auth_api_url_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_invalid_auth_api_url_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: not-a-url\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_non_http_auth_api_url_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: ftp://keystone.example.com/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// global validation
// ---------------------------------------------------------------------------

#[test]
fn duplicate_name_rejected() {
    let a = "name: dup\nmode: forward\ndomain: a.example.com\nhttps: false\nbackends:\n  - addr: 10.0.0.1:80\n";
    let b = "name: dup\nmode: forward\ndomain: b.example.com\nhttps: false\nbackends:\n  - addr: 10.0.0.2:80\n";
    let err = load_err(&[("a.yaml", a), ("b.yaml", b)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn duplicate_domain_rejected() {
    let a = "name: a\nmode: forward\ndomain: dup.example.com\nhttps: false\nbackends:\n  - addr: 10.0.0.1:80\n";
    let b = "name: b\nmode: forward\ndomain: dup.example.com\nhttps: false\nbackends:\n  - addr: 10.0.0.2:80\n";
    let err = load_err(&[("a.yaml", a), ("b.yaml", b)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn duplicate_base_domain_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: a\n    application_credential_secret: b\n\
                \x20 nova: {}\n";
    let err = load_err(&[("a.yaml", yaml), ("b.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn overlapping_base_domains_rejected() {
    let parent = "name: p\nmode: openstack\nbase_domain: example.com\nhttps: false\n\
                  openstack:\n  timeout_secs: 5\n\
                  \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                  \x20   application_credential_id: a\n    application_credential_secret: b\n\
                  \x20 nova: {}\n";
    let child = "name: c\nmode: openstack\nbase_domain: a.example.com\nhttps: false\n\
                 openstack:\n  timeout_secs: 5\n\
                 \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                 \x20   application_credential_id: a\n    application_credential_secret: b\n\
                 \x20 nova: {}\n";
    let err = load_err(&[("p.yaml", parent), ("c.yaml", child)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_domain_overlaps_base_domain_rejected() {
    let fwd = "name: f\nmode: forward\ndomain: www.rent.do.top\nhttps: false\nbackends:\n  - addr: 10.0.0.1:80\n";
    let os = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
              openstack:\n  timeout_secs: 5\n\
              \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
              \x20   application_credential_id: a\n    application_credential_secret: b\n\
              \x20 nova: {}\n";
    let err = load_err(&[("f.yaml", fwd), ("o.yaml", os)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn single_bad_file_fails_whole_load() {
    let good = "name: good\nmode: forward\ndomain: good.example.com\nhttps: false\nbackends:\n  - addr: 10.0.0.1:80\n";
    let bad = "name: bad\nmode: forward\ndomain: bad.example.com\nhttps: false\nbackends: []\n";
    let err = load_err(&[("a.yaml", good), ("b.yaml", bad)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// env expansion
// ---------------------------------------------------------------------------

#[test]
fn missing_env_variable_reports_missing_env() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: password\n    username: u\n    password: ${ABSENT_VAR}\n\
                \x20   project_name: p\n    project_domain_name: Default\n    user_domain_name: Default\n\
                \x20 nova: {}\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    match err {
        ConfigError::MissingEnv(var) => assert_eq!(var, "ABSENT_VAR"),
        other => panic!("expected MissingEnv, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// parse robustness
// ---------------------------------------------------------------------------

#[test]
fn yaml_syntax_error_is_parse_with_file_path() {
    let err = load_err(&[("broken.yaml", "name: [unclosed\n")], &HashMap::new());
    match err {
        ConfigError::Parse { file, .. } => assert!(file.ends_with("broken.yaml")),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn unknown_field_is_parse_with_file_path() {
    let yaml = format!("{}bogus_field: 1\n", forward_yaml(""));
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    match err {
        ConfigError::Parse { file, msg } => {
            assert!(file.ends_with("a.yaml"));
            assert!(msg.contains("bogus_field"), "msg was: {msg}");
        }
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn wrong_field_type_is_parse() {
    let yaml = "name: s\nmode: forward\ndomain: a.example.com\nhttps: \"yes please\"\nbackends: []\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn missing_mode_field_is_parse() {
    let yaml = "name: s\ndomain: a.example.com\nhttps: false\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn unknown_mode_is_parse() {
    let yaml = "name: s\nmode: reverse\n";
    let err = load_err(&[("a.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// redacted Debug
// ---------------------------------------------------------------------------

#[test]
fn debug_redacts_password() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: password\n    username: u\n    password: hunter2\n\
                \x20   project_name: p\n    project_domain_name: Default\n    user_domain_name: Default\n\
                \x20 nova: {}\n";
    let cfg = load_ok(&[("o.yaml", yaml)], &HashMap::new());
    let rendered = format!("{cfg:?}");
    assert!(!rendered.contains("hunter2"), "secret leaked: {rendered}");
    assert!(rendered.contains("redacted"), "expected redaction marker: {rendered}");
}

#[test]
fn debug_redacts_application_credential_secret() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n\
                \x20 auth:\n    api_url: https://keystone.example.com:5000/v3\n    approach: application_credential\n\
                \x20   application_credential_id: id1\n    application_credential_secret: supersecret\n\
                \x20 nova: {}\n";
    let cfg = load_ok(&[("o.yaml", yaml)], &HashMap::new());
    let rendered = format!("{cfg:?}");
    assert!(!rendered.contains("supersecret"), "secret leaked: {rendered}");

    // Debugging the nested site / openstack site directly must not leak either.
    let site: &OpenStackSite = &cfg.openstack[0];
    assert!(!format!("{site:?}").contains("supersecret"));
    assert!(!format!("{:?}", site.openstack.auth).contains("supersecret"));
}

// ---------------------------------------------------------------------------
// committed positive fixtures
// ---------------------------------------------------------------------------

#[test]
fn committed_fixture_directory_loads_one_forward_and_one_openstack() {
    let cfg = load_config(
        Path::new("tests/fixtures/sites"),
        &env(&[("OS_PASSWORD", "s3cret")]),
    )
    .expect("committed fixtures load");
    assert_eq!(cfg.forward.len(), 1);
    assert_eq!(cfg.openstack.len(), 1);
    assert_eq!(cfg.forward[0].domain, "www.example.com");
    assert_eq!(cfg.openstack[0].base_domain, "rent.do.top");
}
