//! `forward` (basic reverse-proxy) site configuration: types and parsing.
//!
//! Public types mirror `plan/02-contracts.md` verbatim; parsing/validation is
//! pure (env is injected) and reports failures as [`ConfigError`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{expand_env, is_valid_hostname, site_error, ConfigError};

/// A `forward` site: exact-domain match to a weighted backend set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSite {
    /// Unique site identifier (logs/metrics/errors).
    pub name: String,
    /// Exact match domain (lowercase, no port).
    pub domain: String,
    /// Respond on the plain HTTP listener.
    pub http: bool,
    /// Respond on the TLS listener (requires `cert_file` + `key_file`).
    pub https: bool,
    /// PEM certificate path (required when `https`).
    pub cert_file: Option<PathBuf>,
    /// PEM private-key path (required when `https`).
    pub key_file: Option<PathBuf>,
    /// Upstream backends (at least one; `weight == 0` disables an entry).
    pub backends: Vec<Backend>,
    /// Load-balancing algorithm.
    pub lb_mode: LbMode,
    /// Optional active health check.
    pub health_check: Option<HealthCheck>,
}

/// A single forward backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Upstream `IP:port` literal (no hostnames / DNS at load).
    pub addr: SocketAddr,
    /// Relative weight; `0` disables the backend (P3 excludes it).
    pub weight: u32,
}

/// Load-balancing algorithm (`config.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbMode {
    /// Equal-weight round robin.
    RoundRobin,
    /// Weighted round robin.
    WeightedRoundRobin,
    /// Random selection.
    Random,
    /// Consistent hashing on the client IP.
    Ketama,
}

impl LbMode {
    /// Parse the YAML spelling of a load-balancing mode.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "round_robin" => Some(Self::RoundRobin),
            "weighted_round_robin" => Some(Self::WeightedRoundRobin),
            "random" => Some(Self::Random),
            "ketama" => Some(Self::Ketama),
            _ => None,
        }
    }
}

/// Active health-check configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    /// Probe kind.
    pub kind: HealthKind,
    /// HTTP path (required when [`HealthKind::Http`]).
    pub http_path: Option<String>,
    /// Probe interval in seconds.
    pub interval_secs: u64,
    /// Probe timeout in seconds.
    pub timeout_secs: u64,
}

/// Health-check probe kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthKind {
    /// TCP connect probe.
    Tcp,
    /// HTTP probe (2xx/3xx = healthy).
    Http,
}

/// Raw `forward` site as deserialized from YAML (before validation).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawForward {
    pub(crate) name: String,
    pub(crate) mode: String,
    pub(crate) domain: String,
    #[serde(default = "super::default_http")]
    pub(crate) http: bool,
    pub(crate) https: bool,
    #[serde(default)]
    pub(crate) cert_file: Option<String>,
    #[serde(default)]
    pub(crate) key_file: Option<String>,
    pub(crate) backends: Vec<RawBackend>,
    #[serde(default)]
    pub(crate) load_balancer: Option<String>,
    #[serde(default)]
    pub(crate) health_check: Option<RawHealthCheck>,
}

/// Raw backend entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackend {
    pub(crate) addr: String,
    #[serde(default = "super::default_weight")]
    pub(crate) weight: u32,
}

/// Raw health-check entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHealthCheck {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) http_path: Option<String>,
    pub(crate) interval_secs: u64,
    pub(crate) timeout_secs: u64,
}

/// Validate and normalise a raw `forward` site.
pub(crate) fn parse(
    raw: RawForward,
    file: &Path,
    env: &HashMap<String, String>,
) -> Result<ForwardSite, ConfigError> {
    // Defense in depth: dispatch already selected `forward`.
    if raw.mode != "forward" {
        return Err(site_error(
            file,
            format!("site '{}': expected mode 'forward', got '{}'", raw.name, raw.mode),
        ));
    }
    if raw.name.trim().is_empty() {
        return Err(site_error(file, "site name must not be empty"));
    }

    let domain = raw.domain.trim_end_matches('.').to_ascii_lowercase();
    if !is_valid_hostname(&domain) {
        return Err(site_error(
            file,
            format!("site '{}': invalid domain '{}'", raw.name, raw.domain),
        ));
    }

    let cert_file = expand_path(&raw.cert_file, env)?;
    let key_file = expand_path(&raw.key_file, env)?;
    if raw.https {
        match (&cert_file, &key_file) {
            (Some(cert), Some(key)) => {
                if !cert.is_file() {
                    return Err(site_error(
                        file,
                        format!(
                            "site '{}': https=true but cert_file '{}' does not exist",
                            raw.name,
                            cert.display()
                        ),
                    ));
                }
                if !key.is_file() {
                    return Err(site_error(
                        file,
                        format!(
                            "site '{}': https=true but key_file '{}' does not exist",
                            raw.name,
                            key.display()
                        ),
                    ));
                }
            }
            _ => {
                return Err(site_error(
                    file,
                    format!(
                        "site '{}': https=true requires both cert_file and key_file",
                        raw.name
                    ),
                ));
            }
        }
    }

    if raw.backends.is_empty() {
        return Err(site_error(
            file,
            format!("site '{}': backends must not be empty", raw.name),
        ));
    }
    let mut backends = Vec::with_capacity(raw.backends.len());
    for raw_backend in raw.backends {
        let addr: SocketAddr = raw_backend.addr.parse().map_err(|_| {
            site_error(
                file,
                format!(
                    "site '{}': invalid backend addr '{}' (expected an IP:port literal)",
                    raw.name, raw_backend.addr
                ),
            )
        })?;
        if raw_backend.weight > 65_535 {
            return Err(site_error(
                file,
                format!(
                    "site '{}': backend weight {} out of range 0..=65535",
                    raw.name, raw_backend.weight
                ),
            ));
        }
        backends.push(Backend {
            addr,
            weight: raw_backend.weight,
        });
    }

    let lb_mode = match raw.load_balancer.as_deref() {
        None => LbMode::RoundRobin,
        Some(value) => LbMode::parse(value).ok_or_else(|| {
            site_error(
                file,
                format!("site '{}': unknown load_balancer '{}'", raw.name, value),
            )
        })?,
    };

    let health_check = match raw.health_check {
        None => None,
        Some(raw_hc) => Some(parse_health_check(raw_hc, &raw.name, file)?),
    };

    Ok(ForwardSite {
        name: raw.name,
        domain,
        http: raw.http,
        https: raw.https,
        cert_file,
        key_file,
        backends,
        lb_mode,
        health_check,
    })
}

fn parse_health_check(
    raw: RawHealthCheck,
    site: &str,
    file: &Path,
) -> Result<HealthCheck, ConfigError> {
    let kind = match raw.kind.as_str() {
        "tcp" => HealthKind::Tcp,
        "http" => HealthKind::Http,
        other => {
            return Err(site_error(
                file,
                format!("site '{site}': unknown health_check.kind '{other}'"),
            ));
        }
    };
    if kind == HealthKind::Http && raw.http_path.is_none() {
        return Err(site_error(
            file,
            format!("site '{site}': health_check.kind=http requires http_path"),
        ));
    }
    Ok(HealthCheck {
        kind,
        http_path: raw.http_path,
        interval_secs: raw.interval_secs,
        timeout_secs: raw.timeout_secs,
    })
}

/// Expand `${ENV}` tokens in an optional path field.
fn expand_path(
    raw: &Option<String>,
    env: &HashMap<String, String>,
) -> Result<Option<PathBuf>, ConfigError> {
    match raw {
        None => Ok(None),
        Some(value) => Ok(Some(PathBuf::from(expand_env(value, env)?))),
    }
}
