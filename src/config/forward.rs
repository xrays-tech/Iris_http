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
    /// OpenStack environment referenced by this site's instance backends.
    ///
    /// Required when any backend is [`BackendTarget::Instance`]; must name an
    /// entry of [`crate::config::Config::openstack_envs`]. `None` is valid only
    /// for an all-`addr` backend set.
    pub openstack_env: Option<String>,
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
    /// Where the backend lives: a literal address or a discoverable instance.
    pub target: BackendTarget,
    /// Relative weight; `0` disables the backend (P3 excludes it).
    pub weight: u32,
}

/// The target of a forward backend: exactly one of an address or an instance.
///
/// `addr` and `instance` are mutually exclusive at the YAML entry level
/// (`config.md` §3); an instance target carries its explicit `port` and is
/// resolved through the site's referenced OpenStack environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendTarget {
    /// Upstream `IP:port` literal (no hostnames / DNS at load).
    Addr(SocketAddr),
    /// An OpenStack instance (name or `8-4-4-4-12` UUID) plus its target port.
    Instance {
        /// Instance name or UUID.
        instance: String,
        /// Target port on the instance.
        port: u16,
    },
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
    #[serde(default)]
    pub(crate) openstack_env: Option<String>,
    pub(crate) backends: Vec<RawBackend>,
    #[serde(default)]
    pub(crate) load_balancer: Option<String>,
    #[serde(default)]
    pub(crate) health_check: Option<RawHealthCheck>,
}

/// Raw backend entry: exactly one of `addr` or `instance`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackend {
    #[serde(default)]
    pub(crate) addr: Option<String>,
    #[serde(default)]
    pub(crate) instance: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
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
    let mut has_instance = false;
    for raw_backend in raw.backends {
        let target = match (raw_backend.addr, raw_backend.instance) {
            (Some(_), Some(_)) => {
                return Err(site_error(
                    file,
                    format!(
                        "site '{}': backend must set exactly one of addr or instance, not both",
                        raw.name
                    ),
                ));
            }
            (None, None) => {
                return Err(site_error(
                    file,
                    format!(
                        "site '{}': backend must set either addr or instance",
                        raw.name
                    ),
                ));
            }
            (Some(addr), None) => {
                if raw_backend.port.is_some() {
                    return Err(site_error(
                        file,
                        format!(
                            "site '{}': an addr backend must not set the port field (use addr: IP:port)",
                            raw.name
                        ),
                    ));
                }
                let addr: SocketAddr = addr.parse().map_err(|_| {
                    site_error(
                        file,
                        format!(
                            "site '{}': invalid backend addr '{}' (expected an IP:port literal)",
                            raw.name, addr
                        ),
                    )
                })?;
                BackendTarget::Addr(addr)
            }
            (None, Some(instance)) => {
                let instance = instance.trim().to_owned();
                if instance.is_empty() {
                    return Err(site_error(
                        file,
                        format!("site '{}': backend instance must not be empty", raw.name),
                    ));
                }
                let port = raw_backend.port.ok_or_else(|| {
                    site_error(
                        file,
                        format!(
                            "site '{}': an instance backend requires the port field",
                            raw.name
                        ),
                    )
                })?;
                has_instance = true;
                BackendTarget::Instance { instance, port }
            }
        };
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
            target,
            weight: raw_backend.weight,
        });
    }

    let openstack_env = match raw.openstack_env {
        None => None,
        Some(name) => {
            let name = name.trim().to_owned();
            if name.is_empty() {
                return Err(site_error(
                    file,
                    format!("site '{}': openstack_env must not be empty", raw.name),
                ));
            }
            Some(name)
        }
    };
    if has_instance && openstack_env.is_none() {
        return Err(site_error(
            file,
            format!(
                "site '{}': an instance backend requires openstack_env",
                raw.name
            ),
        ));
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
        openstack_env,
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
