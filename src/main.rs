//! Iris HTTP binary entry point.
//!
//! P0 assembles a minimal Pingora server skeleton (design.md §4.1): a shared
//! `0.0.0.0:80` HTTP listener. Routing, TLS, load balancing, metrics and
//! OpenStack discovery arrive in later stages; the placeholder [`ScaffoldProxy`]
//! therefore rejects every request with 503 rather than pretending to proxy.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;
use async_trait::async_trait;
use clap::Parser;
use pingora::proxy::{http_proxy_service, ProxyHttp, Session};
use pingora::server::configuration::{Opt, ServerConf};
use pingora::server::Server;
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result as PingoraResult};

use iris_http::logging;
use iris_http::{reload, Cli, Command, RunArgs};

/// Address of the shared HTTP listener (design.md §4.1 / D1).
const HTTP_LISTEN_ADDR: &str = "0.0.0.0:80";
/// Time to let in-flight requests drain on graceful shutdown (design.md §4.1:
/// `grace_period_seconds`; Pingora otherwise defaults to 5 minutes).
const GRACE_PERIOD_SECONDS: u64 = 5;
/// Hard cap on runtime shutdown after the grace period (design.md §4.1:
/// `graceful_shutdown_timeout_seconds`).
const GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS: u64 = 10;

/// Placeholder `ProxyHttp` for the P0 skeleton.
///
/// It intentionally implements no routing: `upstream_peer` fails with 503 until
/// P3 wires the real forward/OpenStack logic.
struct ScaffoldProxy;

#[async_trait]
impl ProxyHttp for ScaffoldProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        Err(Error::explain(
            ErrorType::HTTPStatus(503),
            "iris P0 scaffold: no sites configured",
        ))
    }
}

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

/// Start the proxy server. Diverges: `run_forever` only returns by exiting the
/// process (graceful shutdown returns exit code 0 — see P0 smoke test).
fn run(args: RunArgs) -> anyhow::Result<()> {
    logging::init(true).map_err(|e| anyhow::anyhow!("failed to initialise logging: {e}"))?;

    // Design `reload.md` §6: an explicit (or default) config dir must exist;
    // a missing one is a startup failure, not a silent fallback. P0 has no
    // loader yet, so existence is the only check available; P1 replaces this.
    if !args.config_dir.is_dir() {
        anyhow::bail!(
            "config directory '{}' does not exist or is not a directory (--config-dir)",
            args.config_dir.display()
        );
    }

    if let Some(pidfile) = &args.pidfile {
        write_pidfile(pidfile)?;
    }

    tracing::info!(
        config_dir = %args.config_dir.display(),
        watch_config = args.watch_config,
        version = env!("CARGO_PKG_VERSION"),
        "starting iris"
    );

    let mut server = build_server()?;
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, ScaffoldProxy);
    proxy_service.add_tcp(HTTP_LISTEN_ADDR);
    server.add_service(proxy_service);

    // NOTE: the Prometheus service (`pingora_prometheus::prometheus_http_service()`)
    // is confirmed to assemble in `probe_tests::service_assembly_compiles`; it is
    // not bound here because P0 has no metrics to expose yet and the design's
    // default port (9090) is commonly occupied during local development. Wiring
    // lands with the metrics in a later stage (P7/P8).

    tracing::info!(
        http = HTTP_LISTEN_ADDR,
        "iris skeleton ready; awaiting requests"
    );

    // Never returns; graceful SIGTERM shutdown exits the process with code 0.
    server.run_forever()
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
    use std::sync::Arc;

    use pingora::server::ShutdownWatch;
    use pingora::services::background::{background_service, BackgroundService};

    struct NoopBackground;

    #[async_trait]
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

        let mut proxy = http_proxy_service(&conf, ScaffoldProxy);
        proxy.add_tcp("127.0.0.1:0");

        let mut metrics = pingora_prometheus::prometheus_http_service();
        metrics.add_tcp("127.0.0.1:0");

        let _background = background_service("noop", NoopBackground);

        // Keep the objects alive so the assembly is not optimised away.
        assert!(proxy.app_logic().is_some());
        assert!(metrics.app_logic().is_some());
    }

    /// Probe #4 / design.md §4.1: `ServerConf` carries the shutdown timing that
    /// P0 relies on for a prompt, graceful SIGTERM exit.
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
}
