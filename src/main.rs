//! Iris HTTP binary entry point.
//!
//! P3 assembles the minimal forward-lane server (design.md §4.1): load the site
//! configuration, compile the routing table and per-site load balancers into a
//! [`SharedState`], register the [`IrisProxy`] on a shared HTTP listener, and
//! run until a signal-driven shutdown. TLS/SNI (P4), OpenStack (P5/P7) and
//! reload (P8) arrive in later stages.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::http_proxy_service;
use pingora::server::configuration::{Opt, ServerConf};
use pingora::server::Server;
use pingora::services::background::background_service;

use iris_http::config::load_config;
use iris_http::lb::health_check_service;
use iris_http::logging;
use iris_http::proxy::IrisProxy;
use iris_http::state::SharedState;
use iris_http::tls::SniCertSource;
use iris_http::{reload, Cli, Command, RunArgs};

/// Default address of the shared HTTP listener (design.md §4.1 / D1).
const HTTP_LISTEN_ADDR: &str = "0.0.0.0:80";
/// Environment override for the HTTP listener (test/runtime port selection).
///
/// The production default is fixed at `0.0.0.0:80`; `config.md` §2 keeps the
/// port out of the site files, so tests pick an ephemeral port here instead.
const HTTP_LISTEN_ADDR_ENV: &str = "IRIS_HTTP_LISTEN_ADDR";
/// Default address of the shared TLS (HTTPS) listener (design.md §4.1 / D1).
const TLS_LISTEN_ADDR: &str = "0.0.0.0:443";
/// Environment override for the TLS listener (test/runtime port selection).
///
/// Mirrors [`HTTP_LISTEN_ADDR_ENV`] so e2e tests can bind an unprivileged port
/// without root rather than the production default `0.0.0.0:443`.
const TLS_LISTEN_ADDR_ENV: &str = "IRIS_HTTP_LISTEN_TLS_ADDR";
/// Time to let in-flight requests drain on graceful shutdown (design.md §4.1:
/// `grace_period_seconds`; Pingora otherwise defaults to 5 minutes).
const GRACE_PERIOD_SECONDS: u64 = 5;
/// Hard cap on runtime shutdown after the grace period (design.md §4.1:
/// `graceful_shutdown_timeout_seconds`).
const GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS: u64 = 10;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => match run(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("iris run failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Reload(args) => match reload(&args.pidfile) {
            Ok(pid) => {
                eprintln!("sent SIGHUP to pid {pid}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("iris reload failed: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Resolve the HTTP listen address, honouring [`HTTP_LISTEN_ADDR_ENV`].
fn http_listen_addr() -> String {
    std::env::var(HTTP_LISTEN_ADDR_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| HTTP_LISTEN_ADDR.to_owned())
}

/// Resolve the TLS listen address, honouring [`TLS_LISTEN_ADDR_ENV`].
fn tls_listen_addr() -> String {
    std::env::var(TLS_LISTEN_ADDR_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TLS_LISTEN_ADDR.to_owned())
}

/// Start the proxy server. Diverges: `run_forever` only returns by exiting the
/// process (graceful shutdown returns exit code 0 — see P0 smoke test).
fn run(args: RunArgs) -> anyhow::Result<()> {
    logging::init(true).map_err(|e| anyhow::anyhow!("failed to initialise logging: {e}"))?;

    // Design `reload.md` §6 + `config.md` §1: `--config-dir` is the configuration
    // *root* and site files live in its `sites/` subdirectory. A missing root (or
    // missing `sites/`) is a startup failure, not a silent fallback.
    let sites_dir = args.config_dir.join("sites");
    if !args.config_dir.is_dir() {
        anyhow::bail!(
            "config directory '{}' does not exist or is not a directory (--config-dir)",
            args.config_dir.display()
        );
    }
    if !sites_dir.is_dir() {
        anyhow::bail!(
            "sites directory '{}' does not exist or is not a directory (expected '<config-dir>/sites')",
            sites_dir.display()
        );
    }

    let env: HashMap<String, String> = std::env::vars().collect();
    let config = load_config(&sites_dir, &env)
        .map_err(|err| anyhow::anyhow!("failed to load configuration: {err}"))?;

    // Build the per-SNI certificate source before `config` is moved into the
    // shared state. A missing/unparseable certificate is a startup failure.
    let cert_source = SniCertSource::from_sites(&config)
        .map_err(|err| anyhow::anyhow!("failed to build TLS certificate map: {err:#}"))?;

    // Compile the routing table, per-site load balancers, firewall tables and
    // instance resolvers into the shared snapshots read on the request path.
    // A bad `.role` file fails startup here with context (`firewall.md` §2).
    let shared = Arc::new(
        SharedState::try_new(config)
            .map_err(|err| anyhow::anyhow!("failed to build shared state: {err}"))?,
    );

    if let Some(pidfile) = &args.pidfile {
        write_pidfile(pidfile)?;
    }

    let listen_addr = http_listen_addr();
    let tls_addr = tls_listen_addr();
    let openstack_sites = shared.config.load().openstack.len();
    let resolvers = shared.resolvers.load().len();
    let firewalled_sites = shared.firewall.load().len();
    tracing::info!(
        config_dir = %args.config_dir.display(),
        sites_dir = %sites_dir.display(),
        forward_sites = shared.config.load().forward.len(),
        openstack_sites,
        resolvers,
        firewalled_sites,
        watch_config = args.watch_config,
        version = env!("CARGO_PKG_VERSION"),
        "starting iris"
    );

    let mut server = build_server()?;
    server.bootstrap();

    let app = IrisProxy {
        shared: Arc::clone(&shared),
    };
    let mut proxy_service = http_proxy_service(&server.configuration, app);
    proxy_service.add_tcp(&listen_addr);

    // TLS termination: per-SNI certificate selection + H2/H1 ALPN on the same
    // proxy service as the plain listener (design.md §4.1 / D1).
    let mut tls_settings = TlsSettings::with_callbacks(Box::new(cert_source))
        .map_err(|err| anyhow::anyhow!("failed to build TLS settings: {err}"))?;
    tls_settings.enable_h2();
    proxy_service.add_tls_with_settings(&tls_addr, None, tls_settings);

    server.add_service(proxy_service);

    register_health_checks(&mut server, &shared);

    // NOTE: the Prometheus service (`pingora_prometheus::prometheus_http_service()`)
    // is confirmed to assemble in `probe_tests::service_assembly_compiles`; it is
    // not bound here because P3 has no metrics to expose yet. Wiring lands with
    // the metrics in a later stage (P7/P8).

    tracing::info!(http = %listen_addr, https = %tls_addr, "iris TLS lane ready; awaiting requests");

    // Never returns; graceful SIGTERM shutdown exits the process with code 0.
    server.run_forever()
}

/// Register one background health-check service per forward site that
/// configures `health_check` (`design.md` §4.5).
fn register_health_checks(server: &mut Server, shared: &Arc<SharedState>) {
    let config = shared.config.load();
    for site in &config.forward {
        let Some(hc) = site.health_check.as_ref() else {
            continue;
        };
        let Some(lb) = shared.lb(&site.name) else {
            tracing::warn!(site = %site.name, "health_check configured but no load balancer found");
            continue;
        };
        let name = format!("health check {}", site.name);
        server.add_service(background_service(
            &name,
            health_check_service(lb, hc),
        ));
    }
}

/// Build the [`Server`] with the [`ServerConf`] required by design.md §4.1
/// (`version`, `grace_period_seconds`, `graceful_shutdown_timeout_seconds`).
fn build_server() -> anyhow::Result<Server> {
    let mut conf = ServerConf::new().context("failed to build default ServerConf")?;
    conf.version = 1;
    conf.grace_period_seconds = Some(GRACE_PERIOD_SECONDS);
    conf.graceful_shutdown_timeout_seconds = Some(GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS);
    Ok(Server::new_with_opt_and_conf(None::<Opt>, conf))
}

/// Persist the current process id to `path`.
fn write_pidfile(path: &Path) -> anyhow::Result<()> {
    std::fs::write(path, format!("{}\n", std::process::id()))
        .with_context(|| format!("failed to write pidfile '{}'", path.display()))
}

#[cfg(test)]
mod probe_tests {
    //! API probe smoke tests (plan/03 §API 探针冒烟). These compile-check the
    //! real assembly path; they do not bind sockets (binding happens at
    //! `run_forever`).

    use super::*;

    use pingora::server::ShutdownWatch;
    use pingora::services::background::BackgroundService;

    use iris_http::config::Config;

    fn test_shared() -> Arc<SharedState> {
        let config = Config {
            forward: Vec::new(),
            openstack: Vec::new(),
        };
        Arc::new(SharedState::try_new(config).expect("empty shared state builds"))
    }

    struct NoopBackground;

    #[async_trait::async_trait]
    impl BackgroundService for NoopBackground {
        async fn start(&self, mut shutdown: ShutdownWatch) {
            let _ = shutdown.changed().await;
        }
    }

    /// Probe #2 (listener wiring) + #4 (background/prometheus assembly):
    /// `http_proxy_service(&conf, app)` -> `svc.add_tcp(..)` -> `server.add_service`;
    /// `background_service(name, task)`; `prometheus_http_service()`.
    #[test]
    fn service_assembly_compiles() {
        let conf = Arc::new(ServerConf::new().expect("default ServerConf"));

        let app = IrisProxy {
            shared: test_shared(),
        };
        let mut proxy = http_proxy_service(&conf, app);
        proxy.add_tcp("127.0.0.1:0");

        let mut metrics = pingora_prometheus::prometheus_http_service();
        metrics.add_tcp("127.0.0.1:0");

        let _background = background_service("noop", NoopBackground);

        // Keep the objects alive so the assembly is not optimised away.
        assert!(proxy.app_logic().is_some());
        assert!(metrics.app_logic().is_some());
    }

    /// P4 TLS assembly probe: `SniCertSource::from_sites` -> `TlsSettings::with_callbacks`
    /// -> `enable_h2()` -> `svc.add_tls_with_settings(addr, None, settings)` on the
    /// same proxy service as `add_tcp`. Compile-checks the real wiring without
    /// binding a socket (binding happens at `run_forever`).
    #[test]
    fn tls_service_assembly_compiles() {
        let conf = Arc::new(ServerConf::new().expect("default ServerConf"));

        let app = IrisProxy {
            shared: test_shared(),
        };
        let mut proxy = http_proxy_service(&conf, app);
        proxy.add_tcp("127.0.0.1:0");

        let source = SniCertSource::from_sites(&Config {
            forward: Vec::new(),
            openstack: Vec::new(),
        })
        .expect("empty certificate map builds");
        let mut tls_settings = TlsSettings::with_callbacks(Box::new(source))
            .expect("TLS settings with callbacks build");
        tls_settings.enable_h2();
        proxy.add_tls_with_settings("127.0.0.1:0", None, tls_settings);

        assert!(proxy.app_logic().is_some());
    }

    /// Probe #4 / design.md §4.1: `ServerConf` carries the shutdown timing that
    /// P3 relies on for a prompt, graceful SIGTERM exit.
    #[test]
    fn server_conf_sets_version_and_shutdown_timings() {
        let server = build_server().expect("server builds");
        assert_eq!(server.configuration.version, 1);
        assert_eq!(
            server.configuration.grace_period_seconds,
            Some(GRACE_PERIOD_SECONDS)
        );
        assert_eq!(
            server.configuration.graceful_shutdown_timeout_seconds,
            Some(GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS)
        );
    }

    #[test]
    fn http_listen_addr_defaults_when_env_absent() {
        // `http_listen_addr` must never panic regardless of the ambient env.
        let addr = http_listen_addr();
        assert!(!addr.trim().is_empty());
    }

    #[test]
    fn tls_listen_addr_defaults_when_env_absent() {
        // `tls_listen_addr` must never panic regardless of the ambient env.
        let addr = tls_listen_addr();
        assert!(!addr.trim().is_empty());
    }
}
