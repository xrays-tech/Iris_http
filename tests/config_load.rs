//! P1 integration tests for `iris_http::config::load_config`.
//!
//! `load_config` is a pure function (env injected), so these tests drive the
//! real parser over tempdirs / committed fixtures with no mocking. OpenStack
//! environments are standalone files under `<config-root>/openstack/`
//! (`prod.yaml` below); sites reference them by name via `openstack_env`. Test
//! code is exempt from the production `unwrap` ban (plan/01 §1.4).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use iris_http::config::{
    load_config, load_openstack_envs, AuthCredentials, BackendTarget, Config, ConfigError,
    HealthKind, LbMode, OpenStackSite,
};

/// A minimal, otherwise-valid `application_credential` environment.
const GOOD_ENV: &str = "timeout_secs: 5\ncache_ttl_secs: 86400\n\
                        auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                        \x20 application_credential_id: a\n  application_credential_secret: b\n\
                        nova: {}\n";

/// Create a tempdir holding `sites/` and `openstack/` with the given files.
fn config_root(
    site_files: &[(&str, &str)],
    env_files: &[(&str, &str)],
) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let sites = dir.path().join("sites");
    let envs = dir.path().join("openstack");
    fs::create_dir(&sites).expect("create sites dir");
    fs::create_dir(&envs).expect("create openstack dir");
    for (name, content) in site_files {
        fs::write(sites.join(name), content).expect("write site file");
    }
    for (name, content) in env_files {
        fs::write(envs.join(name), content).expect("write env file");
    }
    (dir, sites, envs)
}

/// Create a tempdir holding only an `openstack/` dir with the given files.
fn envs_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let envs = dir.path().join("openstack");
    fs::create_dir(&envs).expect("create openstack dir");
    for (name, content) in files {
        fs::write(envs.join(name), content).expect("write env file");
    }
    (dir, envs)
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

/// An openstack site YAML referencing the default `prod` environment.
///
/// `firewall_dir` is required since P16; point it at the committed unit-test
/// firewall directory.
fn openstack_yaml(body_after_env: &str) -> String {
    format!(
        "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
         firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n{body_after_env}"
    )
}

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn load(
    site_files: &[(&str, &str)],
    env_files: &[(&str, &str)],
    env: &HashMap<String, String>,
) -> Result<Config, ConfigError> {
    let (_guard, sites, envs) = config_root(site_files, env_files);
    load_config(&sites, &envs, env)
}

/// `load` with the default `prod` environment.
fn load_ok(files: &[(&str, &str)], env: &HashMap<String, String>) -> Config {
    load(files, &[("prod.yaml", GOOD_ENV)], env).expect("config loads")
}

fn load_err(files: &[(&str, &str)], env: &HashMap<String, String>) -> ConfigError {
    load(files, &[("prod.yaml", GOOD_ENV)], env).expect_err("config must be rejected")
}

fn load_ok_envs(
    site_files: &[(&str, &str)],
    env_files: &[(&str, &str)],
    env: &HashMap<String, String>,
) -> Config {
    load(site_files, env_files, env).expect("config loads")
}

fn load_err_envs(
    site_files: &[(&str, &str)],
    env_files: &[(&str, &str)],
    env: &HashMap<String, String>,
) -> ConfigError {
    load(site_files, env_files, env).expect_err("config must be rejected")
}

// ---------------------------------------------------------------------------
// Directory semantics
// ---------------------------------------------------------------------------

#[test]
fn empty_sites_dir_loads_zero_sites() {
    let (_guard, sites, envs) = config_root(&[], &[]);
    let cfg = load_config(&sites, &envs, &HashMap::new()).expect("empty sites dir is valid");
    assert!(cfg.forward.is_empty());
    assert!(cfg.openstack.is_empty());
    assert!(cfg.openstack_envs.is_empty());
}

#[test]
fn committed_empty_startup_fixture_loads() {
    // The committed empty startup tree has an empty `openstack/` too.
    let dir = tempfile::tempdir().expect("tempdir");
    let empty_envs = dir.path().join("empty-openstack");
    let cfg = load_config(
        Path::new("tests/fixtures/config/sites"),
        &empty_envs,
        &HashMap::new(),
    )
    .expect("startup fixture (empty sites/) loads");
    assert!(cfg.forward.is_empty());
    assert!(cfg.openstack.is_empty());
}

#[test]
fn missing_sites_dir_is_io_error() {
    let err = load_config(
        Path::new("/nonexistent/iris/sites"),
        Path::new("/nonexistent/iris/openstack"),
        &HashMap::new(),
    )
    .expect_err("missing sites dir must fail");
    assert!(matches!(err, ConfigError::Io(_)), "got {err:?}");
}

#[test]
fn missing_openstack_dir_is_empty_envs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sites = dir.path().join("sites");
    fs::create_dir(&sites).expect("create sites dir");
    fs::write(
        sites.join("a.yaml"),
        forward_yaml("backends:\n  - addr: 127.0.0.1:80\n"),
    )
    .expect("write site");
    // No `openstack/` directory at all.
    let cfg = load_config(&sites, &dir.path().join("openstack"), &HashMap::new())
        .expect("a missing env dir means no environments");
    assert!(cfg.openstack_envs.is_empty());
    assert_eq!(cfg.forward.len(), 1);
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
// load_openstack_envs
// ---------------------------------------------------------------------------

#[test]
fn load_envs_missing_dir_is_empty() {
    let envs = load_openstack_envs(Path::new("/nonexistent/iris/openstack"), &HashMap::new())
        .expect("missing env dir is Ok(empty)");
    assert!(envs.is_empty());
}

#[test]
fn load_envs_parses_valid_file_keyed_by_stem() {
    let (_guard, envs) = envs_dir(&[("prod.yaml", GOOD_ENV)]);
    let loaded = load_openstack_envs(&envs, &HashMap::new()).expect("valid env loads");
    assert_eq!(loaded.len(), 1);
    let prod = loaded.get("prod").expect("prod env");
    assert_eq!(prod.name, "prod");
    assert_eq!(prod.config().timeout_secs, 5);
    assert_eq!(prod.config().cache_ttl_secs, 86_400);
}

#[test]
fn load_envs_bad_yaml_is_parse() {
    let (_guard, envs) = envs_dir(&[("prod.yaml", "auth: [unclosed\n")]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("bad yaml rejected");
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn load_envs_invalid_api_url_is_validate() {
    let bad = GOOD_ENV.replace("https://keystone.example.com:5000/v3", "ftp://nope/v3");
    let (_guard, envs) = envs_dir(&[("prod.yaml", &bad)]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("bad url rejected");
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn load_envs_timeout_out_of_range_is_validate() {
    let bad = GOOD_ENV.replace("timeout_secs: 5", "timeout_secs: 0");
    let (_guard, envs) = envs_dir(&[("prod.yaml", &bad)]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("timeout 0 rejected");
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn load_envs_cache_ttl_out_of_range_is_validate() {
    let bad = GOOD_ENV.replace("cache_ttl_secs: 86400", "cache_ttl_secs: 59");
    let (_guard, envs) = envs_dir(&[("prod.yaml", &bad)]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("ttl 59 rejected");
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn load_envs_missing_credentials_is_validate() {
    let bad = "timeout_secs: 5\n\
               auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: u\n\
               \x20 password: p\n  project_name: pr\n  project_domain_name: Default\n\
               nova: {}\n";
    let (_guard, envs) = envs_dir(&[("prod.yaml", bad)]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("missing user_domain rejected");
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn load_envs_single_bad_file_fails_whole_load() {
    let (_guard, envs) = envs_dir(&[("prod.yaml", GOOD_ENV), ("broken.yaml", ": : not yaml\n")]);
    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("one bad env fails all");
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn load_envs_expands_env_and_reports_missing() {
    let with_token = "timeout_secs: 5\n\
                      auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                      \x20 application_credential_id: a\n  application_credential_secret: ${OS_SECRET}\n\
                      nova: {}\n";
    let (_guard, envs) = envs_dir(&[("prod.yaml", with_token)]);
    let loaded =
        load_openstack_envs(&envs, &env(&[("OS_SECRET", "topsecret")])).expect("expands");
    match &loaded.get("prod").expect("prod env").config().auth.credentials {
        AuthCredentials::ApplicationCredential { secret, .. } => assert_eq!(secret, "topsecret"),
        other => panic!("expected ApplicationCredential, got {other:?}"),
    }

    let err = load_openstack_envs(&envs, &HashMap::new()).expect_err("missing env var");
    match err {
        ConfigError::MissingEnv(var) => assert_eq!(var, "OS_SECRET"),
        other => panic!("expected MissingEnv, got {other:?}"),
    }
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
    assert_eq!(f.openstack_env, None);
    assert_eq!(f.backends.len(), 2);
    assert_eq!(f.backends[0].weight, 3);
    assert_eq!(f.backends[1].weight, 1, "weight defaults to 1");
    assert_eq!(
        f.backends[0].target,
        BackendTarget::Addr("10.0.0.5:8080".parse().expect("addr"))
    );
    let hc = f.health_check.as_ref().expect("health_check present");
    assert_eq!(hc.kind, HealthKind::Http);
    assert_eq!(hc.http_path.as_deref(), Some("/healthz"));
    assert_eq!(hc.interval_secs, 5);
    assert_eq!(hc.timeout_secs, 2);
}

#[test]
fn forward_instance_backend_parses_with_env() {
    let yaml = "name: apps\nmode: forward\ndomain: apps.example.com\nhttps: false\n\
                openstack_env: prod\n\
                backends:\n  - instance: INS001\n    port: 8080\n    weight: 2\n  - addr: 10.0.0.6:8080\n";
    let cfg = load_ok(&[("a.yaml", yaml)], &HashMap::new());
    let f = &cfg.forward[0];
    assert_eq!(f.openstack_env.as_deref(), Some("prod"));
    assert_eq!(
        f.backends[0].target,
        BackendTarget::Instance {
            instance: "INS001".to_owned(),
            port: 8080,
        }
    );
    assert_eq!(f.backends[0].weight, 2);
    assert_eq!(
        f.backends[1].target,
        BackendTarget::Addr("10.0.0.6:8080".parse().expect("addr"))
    );
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
fn forward_backend_addr_and_instance_both_rejected() {
    let yaml = forward_yaml(
        "openstack_env: prod\nbackends:\n  - addr: 10.0.0.5:8080\n    instance: INS001\n    port: 80\n",
    );
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_backend_neither_addr_nor_instance_rejected() {
    let yaml = forward_yaml("backends:\n  - weight: 1\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_instance_backend_without_port_rejected() {
    let yaml = forward_yaml("openstack_env: prod\nbackends:\n  - instance: INS001\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_addr_backend_with_port_field_rejected() {
    let yaml = forward_yaml("backends:\n  - addr: 10.0.0.5:8080\n    port: 8080\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_empty_instance_rejected() {
    let yaml = forward_yaml("openstack_env: prod\nbackends:\n  - instance: '   '\n    port: 80\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_instance_backend_without_openstack_env_rejected() {
    let yaml = forward_yaml("backends:\n  - instance: INS001\n    port: 80\n");
    let err = load_err(&[("a.yaml", &yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_openstack_env_without_instance_backend_is_allowed() {
    let yaml = forward_yaml("openstack_env: prod\nbackends:\n  - addr: 10.0.0.9:8080\n");
    let cfg = load_ok(&[("a.yaml", &yaml)], &HashMap::new());
    assert_eq!(cfg.forward[0].openstack_env.as_deref(), Some("prod"));
}

#[test]
fn forward_instance_backend_unknown_env_rejected() {
    let yaml = forward_yaml("openstack_env: ghost\nbackends:\n  - instance: INS001\n    port: 80\n");
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
    let env_yaml = "timeout_secs: 5\ncache_ttl_secs: 86400\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: proxy\n  password: ${OS_PASSWORD}\n\
                    \x20 project_name: admin\n  project_domain_name: Default\n  user_domain_name: Default\n\
                    nova:\n  api_url: https://controller.example.com:8774/v2.1\n";
    let site_yaml = format!(
        "name: rent\nmode: openstack\nbase_domain: RENT.DO.TOP.\nhttps: true\n\
         cert_file: {crt}\nkey_file: {key}\nfirewall_dir: {fw}\n\
         openstack_env: prod\n",
        crt = crt.display(),
        key = key.display(),
        fw = fw.display(),
    );
    let cfg = load_ok_envs(
        &[("o.yaml", &site_yaml)],
        &[("prod.yaml", env_yaml)],
        &env(&[("OS_PASSWORD", "s3cret")]),
    );

    assert_eq!(cfg.openstack.len(), 1);
    let o = &cfg.openstack[0];
    assert_eq!(o.name, "rent");
    assert_eq!(o.openstack_env, "prod");
    assert_eq!(o.base_domain, "rent.do.top");
    assert!(o.https);
    assert_eq!(o.firewall_dir, fw);

    let prod = cfg.openstack_envs.get("prod").expect("prod env");
    assert_eq!(prod.config.timeout_secs, 5);
    assert_eq!(prod.config.cache_ttl_secs, 86400);
    assert!(prod.config.nova.all_tenants, "all_tenants defaults to true");
    assert_eq!(
        prod.config.nova.api_url.as_ref().map(|u| u.as_str()),
        Some("https://controller.example.com:8774/v2.1")
    );
    assert_eq!(
        prod.config.auth.api_url.as_str(),
        "https://keystone.example.com:5000/v3"
    );
    match &prod.config.auth.credentials {
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
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                    \x20 application_credential_id: abc123\n  application_credential_secret: ${OS_SECRET}\n\
                    nova:\n  all_tenants: false\n";
    let cfg = load_ok_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &env(&[("OS_SECRET", "topsecret")]),
    );
    let prod = cfg.openstack_envs.get("prod").expect("prod env");
    match &prod.config.auth.credentials {
        AuthCredentials::ApplicationCredential { id, secret } => {
            assert_eq!(id, "abc123");
            assert_eq!(secret, "topsecret");
        }
        other => panic!("expected ApplicationCredential, got {other:?}"),
    }
    assert!(!prod.config.nova.all_tenants);
}

#[test]
fn openstack_cache_ttl_defaults_to_one_day() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                    \x20 application_credential_id: a\n  application_credential_secret: b\n\
                    nova: {}\n";
    let cfg = load_ok_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert_eq!(
        cfg.openstack_envs.get("prod").expect("prod env").config.cache_ttl_secs,
        86_400
    );
}

// ---------------------------------------------------------------------------
// openstack: rejections
// ---------------------------------------------------------------------------

#[test]
fn openstack_env_field_is_required() {
    // The old inline `openstack:` block is gone; a missing `openstack_env` is a
    // parse error.
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn openstack_inline_block_is_rejected() {
    // An old-format site that still embeds `openstack:` must fail to parse
    // (`deny_unknown_fields`) rather than silently ignoring the block.
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                openstack:\n  timeout_secs: 5\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
}

#[test]
fn openstack_unknown_env_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: ghost\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_cache_ttl_below_min_rejected() {
    let env_yaml = GOOD_ENV.replace("cache_ttl_secs: 86400", "cache_ttl_secs: 59");
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", &env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_cache_ttl_above_max_rejected() {
    let env_yaml = GOOD_ENV.replace("cache_ttl_secs: 86400", "cache_ttl_secs: 604801");
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", &env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_cache_ttl_bounds_inclusive() {
    for ttl in [60u64, 604_800u64] {
        let env_yaml = GOOD_ENV.replace("cache_ttl_secs: 86400", &format!("cache_ttl_secs: {ttl}"));
        let cfg = load_ok_envs(
            &[("o.yaml", &openstack_yaml(""))],
            &[("prod.yaml", &env_yaml)],
            &HashMap::new(),
        );
        assert_eq!(
            cfg.openstack_envs.get("prod").expect("prod env").config.cache_ttl_secs,
            ttl
        );
    }
}

#[test]
fn openstack_invalid_base_domain_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: https://rent.do.top\nhttps: false\n\
                firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_password_branch_missing_field_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: proxy\n  password: p\n\
                    \x20 project_name: admin\n  project_domain_name: Default\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_approach_mixing_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: proxy\n  password: p\n\
                    \x20 project_name: admin\n  project_domain_name: Default\n  user_domain_name: Default\n\
                    \x20 application_credential_id: a\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_unknown_approach_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: token\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_missing_firewall_dir_rejected() {
    // `firewall_dir` is required (P16): a site that omits it is rejected so an
    // openstack wildcard site can never become an ACL-free arbitrary-port proxy.
    let yaml =
        "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\nopenstack_env: prod\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_nonexistent_firewall_dir_rejected() {
    let yaml = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
                firewall_dir: /no/such/firewall/dir\nopenstack_env: prod\n";
    let err = load_err(&[("o.yaml", yaml)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_invalid_nova_api_url_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                    \x20 application_credential_id: a\n  application_credential_secret: b\n\
                    nova:\n  api_url: not-a-url\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_missing_auth_api_url_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  approach: application_credential\n\
                    \x20 application_credential_id: a\n  application_credential_secret: b\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_invalid_auth_api_url_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: not-a-url\n  approach: application_credential\n\
                    \x20 application_credential_id: a\n  application_credential_secret: b\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn openstack_non_http_auth_api_url_rejected() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: ftp://keystone.example.com/v3\n  approach: application_credential\n\
                    \x20 application_credential_id: a\n  application_credential_secret: b\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
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
    let a = "name: rent-a\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
             firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
    let b = "name: rent-b\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
             firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
    let err = load_err(&[("a.yaml", a), ("b.yaml", b)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn overlapping_base_domains_rejected() {
    let parent = "name: p\nmode: openstack\nbase_domain: example.com\nhttps: false\n\
                  firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
    let child = "name: c\nmode: openstack\nbase_domain: a.example.com\nhttps: false\n\
                 firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
    let err = load_err(&[("p.yaml", parent), ("c.yaml", child)], &HashMap::new());
    assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
}

#[test]
fn forward_domain_overlaps_base_domain_rejected() {
    let fwd = "name: f\nmode: forward\ndomain: www.rent.do.top\nhttps: false\nbackends:\n  - addr: 10.0.0.1:80\n";
    let os = "name: rent\nmode: openstack\nbase_domain: rent.do.top\nhttps: false\n\
              firewall_dir: tests/fixtures/openstack-firewall\nopenstack_env: prod\n";
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
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: u\n  password: ${ABSENT_VAR}\n\
                    \x20 project_name: p\n  project_domain_name: Default\n  user_domain_name: Default\n\
                    nova: {}\n";
    let err = load_err_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
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
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: password\n  username: u\n  password: hunter2\n\
                    \x20 project_name: p\n  project_domain_name: Default\n  user_domain_name: Default\n\
                    nova: {}\n";
    let cfg = load_ok_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    // `Config` masks `openstack_envs` to its length, so the secret cannot appear
    // there at all.
    let rendered = format!("{cfg:?}");
    assert!(!rendered.contains("hunter2"), "secret leaked: {rendered}");

    // The environment that carries the secret must redact it in its own Debug.
    let prod = cfg.openstack_envs.get("prod").expect("prod env");
    let env_rendered = format!("{prod:?}");
    assert!(
        !env_rendered.contains("hunter2"),
        "secret leaked: {env_rendered}"
    );
    assert!(
        env_rendered.contains("redacted"),
        "expected redaction marker: {env_rendered}"
    );
}

#[test]
fn debug_redacts_application_credential_secret() {
    let env_yaml = "timeout_secs: 5\n\
                    auth:\n  api_url: https://keystone.example.com:5000/v3\n  approach: application_credential\n\
                    \x20 application_credential_id: id1\n  application_credential_secret: supersecret\n\
                    nova: {}\n";
    let cfg = load_ok_envs(
        &[("o.yaml", &openstack_yaml(""))],
        &[("prod.yaml", env_yaml)],
        &HashMap::new(),
    );
    let rendered = format!("{cfg:?}");
    assert!(!rendered.contains("supersecret"), "secret leaked: {rendered}");

    // Debugging the openstack site, the environment, or its auth directly must
    // not leak either.
    let site: &OpenStackSite = &cfg.openstack[0];
    assert!(!format!("{site:?}").contains("supersecret"));
    let prod = cfg.openstack_envs.get("prod").expect("prod env");
    assert!(!format!("{prod:?}").contains("supersecret"));
    assert!(!format!("{:?}", prod.config.auth).contains("supersecret"));
}

// ---------------------------------------------------------------------------
// committed positive fixtures
// ---------------------------------------------------------------------------

#[test]
fn committed_fixture_directory_loads_sites_and_envs() {
    let cfg = load_config(
        Path::new("tests/fixtures/sites"),
        Path::new("tests/fixtures/openstack"),
        &env(&[("OS_PASSWORD", "s3cret")]),
    )
    .expect("committed fixtures load");
    assert_eq!(cfg.forward.len(), 2, "001 forward + 003 forward-instance");
    assert_eq!(cfg.openstack.len(), 1);
    assert_eq!(cfg.forward[0].domain, "www.example.com");
    assert_eq!(cfg.openstack[0].base_domain, "rent.do.top");
    assert_eq!(cfg.openstack[0].openstack_env, "prod");
    assert!(cfg.openstack_envs.contains_key("prod"));
}
