//! Configuration parsing and validation (pure-function core).
//!
//! Lands the real types fixed by `plan/02-contracts.md`. [`load_config`] is a
//! pure function: the environment is injected (production collects
//! `std::env::vars` at the call site) so tests drive it directly with no
//! mocking. A single failing site file fails the whole load.

mod forward;
mod openstack;

pub use forward::{Backend, ForwardSite, HealthCheck, HealthKind, LbMode};
pub use openstack::{AuthConfig, AuthCredentials, NovaConfig, OpenStackConfig, OpenStackSite};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The fully loaded and validated site configuration.
pub struct Config {
    /// `forward` sites, in load order.
    pub forward: Vec<ForwardSite>,
    /// `openstack` sites, in load order.
    pub openstack: Vec<OpenStackSite>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `openstack` sites use their redacting Debug; no secret can leak here.
        f.debug_struct("Config")
            .field("forward", &self.forward)
            .field("openstack", &self.openstack)
            .finish()
    }
}

/// Errors raised while loading and validating site configuration.
#[derive(Debug)]
pub enum ConfigError {
    /// I/O failure at the directory level (e.g. missing `sites/`).
    Io(std::io::Error),
    /// A single site file failed (read, YAML syntax, type, unknown field).
    Parse {
        /// The offending file.
        file: PathBuf,
        /// Human-readable failure detail.
        msg: String,
    },
    /// Structural/semantic validation failure.
    Validate(String),
    /// A `${VAR}` reference had no value in the injected environment.
    MissingEnv(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Parse { file, msg } => {
                write!(f, "failed to parse config file '{}': {msg}", file.display())
            }
            Self::Validate(msg) => write!(f, "config validation failed: {msg}"),
            Self::MissingEnv(var) => {
                write!(f, "missing environment variable '{var}' referenced in config")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Parse { .. } | Self::Validate(_) | Self::MissingEnv(_) => None,
        }
    }
}

/// serde default: `http` enabled.
pub(crate) fn default_http() -> bool {
    true
}

/// serde default: `all_tenants` enabled.
pub(crate) fn default_true() -> bool {
    true
}

/// serde default: backend weight of 1.
pub(crate) fn default_weight() -> u32 {
    1
}

/// serde default: one day of positive cache TTL.
pub(crate) fn default_cache_ttl() -> u64 {
    86_400
}

/// Build a per-site [`ConfigError::Validate`] that names the file.
pub(crate) fn site_error(file: &Path, msg: impl Into<String>) -> ConfigError {
    ConfigError::Validate(format!("{}: {}", file.display(), msg.into()))
}

/// Probe struct used to dispatch on the `mode` discriminator before typed parse.
#[derive(serde::Deserialize)]
struct ModeProbe {
    mode: String,
}

/// Load and validate every `*.yaml` site file under `sites_dir`.
///
/// `env` supplies `${VAR}` expansion values (injected for testability). Any
/// single file failure fails the whole load; the directory contents are read in
/// sorted order so results are deterministic.
pub fn load_config(
    sites_dir: &Path,
    env: &HashMap<String, String>,
) -> Result<Config, ConfigError> {
    if !sites_dir.is_dir() {
        return Err(ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "sites directory '{}' does not exist or is not a directory",
                sites_dir.display()
            ),
        )));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(sites_dir).map_err(|err| {
        ConfigError::Io(std::io::Error::new(
            err.kind(),
            format!(
                "failed to read sites directory '{}': {err}",
                sites_dir.display()
            ),
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            ConfigError::Io(std::io::Error::new(
                err.kind(),
                format!("failed to read an entry of '{}': {err}", sites_dir.display()),
            ))
        })?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            files.push(path);
        }
    }
    files.sort();

    let mut forward = Vec::new();
    let mut openstack = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).map_err(|err| ConfigError::Parse {
            file: path.clone(),
            msg: format!("failed to read file: {err}"),
        })?;

        let probe: ModeProbe = serde_yaml::from_str(&text).map_err(|err| ConfigError::Parse {
            file: path.clone(),
            msg: err.to_string(),
        })?;

        match probe.mode.as_str() {
            "forward" => {
                let raw: forward::RawForward =
                    serde_yaml::from_str(&text).map_err(|err| ConfigError::Parse {
                        file: path.clone(),
                        msg: err.to_string(),
                    })?;
                forward.push(forward::parse(raw, path, env)?);
            }
            "openstack" => {
                let raw: openstack::RawOpenStackSite =
                    serde_yaml::from_str(&text).map_err(|err| ConfigError::Parse {
                        file: path.clone(),
                        msg: err.to_string(),
                    })?;
                openstack.push(openstack::parse(raw, path, env)?);
            }
            other => {
                return Err(ConfigError::Parse {
                    file: path.clone(),
                    msg: format!(
                        "unknown mode '{other}' (expected 'forward' or 'openstack')"
                    ),
                });
            }
        }
    }

    validate_global(&forward, &openstack)?;

    Ok(Config { forward, openstack })
}

/// Global cross-site validation (`config.md` §6).
fn validate_global(
    forward: &[ForwardSite],
    openstack: &[OpenStackSite],
) -> Result<(), ConfigError> {
    // 1. Unique site names.
    let mut names: HashSet<&str> = HashSet::with_capacity(forward.len() + openstack.len());
    for site in forward {
        if !names.insert(site.name.as_str()) {
            return Err(ConfigError::Validate(format!(
                "duplicate site name '{}'",
                site.name
            )));
        }
    }
    for site in openstack {
        if !names.insert(site.name.as_str()) {
            return Err(ConfigError::Validate(format!(
                "duplicate site name '{}'",
                site.name
            )));
        }
    }

    // 2. Unique forward domains / openstack base domains.
    let mut domains: HashSet<&str> = HashSet::with_capacity(forward.len());
    for site in forward {
        if !domains.insert(site.domain.as_str()) {
            return Err(ConfigError::Validate(format!(
                "duplicate forward domain '{}'",
                site.domain
            )));
        }
    }
    let mut bases: HashSet<&str> = HashSet::with_capacity(openstack.len());
    for site in openstack {
        if !bases.insert(site.base_domain.as_str()) {
            return Err(ConfigError::Validate(format!(
                "duplicate base_domain '{}'",
                site.base_domain
            )));
        }
    }

    // 3. No base_domain may be a subdomain of another (wildcard ambiguity).
    for (i, a) in openstack.iter().enumerate() {
        for b in openstack.iter().skip(i + 1) {
            let (a, b) = (a.base_domain.as_str(), b.base_domain.as_str());
            if a != b && (is_subdomain(a, b) || is_subdomain(b, a)) {
                return Err(ConfigError::Validate(format!(
                    "base_domain '{a}' and '{b}' overlap (one is a subdomain of the other)"
                )));
            }
        }
    }

    // 4. A forward domain may not overlap any openstack base_domain.
    for site in forward {
        for os in openstack {
            let (domain, base) = (site.domain.as_str(), os.base_domain.as_str());
            if domain == base || is_subdomain(domain, base) || is_subdomain(base, domain) {
                return Err(ConfigError::Validate(format!(
                    "forward domain '{domain}' overlaps openstack base_domain '{base}'"
                )));
            }
        }
    }

    Ok(())
}

/// Whether `child` is a strict subdomain of `parent` (`a.x` under `x`).
///
/// Both inputs are expected to be normalised (lowercase, no trailing dot).
fn is_subdomain(child: &str, parent: &str) -> bool {
    if child.len() <= parent.len() || !child.ends_with(parent) {
        return false;
    }
    child.as_bytes()[child.len() - parent.len() - 1] == b'.'
}

/// Strict hostname check: dot-separated labels of `[A-Za-z0-9-]`, no
/// scheme/path/port, no wildcard, at least two labels (`config.md` §6).
fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in labels {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
    }
    true
}

/// Expand `${NAME}` tokens from `env`; an unknown name is
/// [`ConfigError::MissingEnv`]. Malformed/unclosed tokens are left verbatim.
fn expand_env(input: &str, env: &HashMap<String, String>) -> Result<String, ConfigError> {
    if !input.contains("${") {
        return Ok(input.to_owned());
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(rel_end) => {
                let name = &after[..rel_end];
                if name.is_empty() {
                    out.push_str("${}");
                } else if let Some(value) = env.get(name) {
                    out.push_str(value);
                } else {
                    return Err(ConfigError::MissingEnv(name.to_owned()));
                }
                rest = &after[rel_end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_hostname_accepts_domains() {
        assert!(is_valid_hostname("www.example.com"));
        assert!(is_valid_hostname("rent.do.top"));
        assert!(is_valid_hostname("a-b.c-d.example"));
        assert!(is_valid_hostname("xn--bcher-kva.example"));
        assert!(is_valid_hostname("localhost.localdomain"));
    }

    #[test]
    fn is_valid_hostname_rejects_bad_input() {
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname("single"));
        assert!(!is_valid_hostname("https://example.com"));
        assert!(!is_valid_hostname("example.com:443"));
        assert!(!is_valid_hostname("example.com/path"));
        assert!(!is_valid_hostname("*.example.com"));
        assert!(!is_valid_hostname("a..example.com"));
        assert!(!is_valid_hostname("-a.example.com"));
        assert!(!is_valid_hostname("a-.example.com"));
        assert!(!is_valid_hostname("exa mple.com"));
    }

    #[test]
    fn is_subdomain_requires_label_boundary() {
        assert!(is_subdomain("a.example.com", "example.com"));
        assert!(is_subdomain("a.b.example.com", "b.example.com"));
        assert!(!is_subdomain("example.com", "example.com"));
        assert!(!is_subdomain("notexample.com", "example.com"));
        assert!(!is_subdomain("example.com", "a.example.com"));
    }

    #[test]
    fn expand_env_passes_through_without_tokens() {
        let env = HashMap::new();
        assert_eq!(expand_env("plain", &env).unwrap(), "plain");
        assert_eq!(expand_env("a/b/c.key", &env).unwrap(), "a/b/c.key");
    }

    #[test]
    fn expand_env_resolves_tokens() {
        let env = HashMap::from([("A".to_string(), "1".to_string())]);
        assert_eq!(expand_env("x${A}y", &env).unwrap(), "x1y");
        assert_eq!(expand_env("${A}${A}", &env).unwrap(), "11");
    }

    #[test]
    fn expand_env_reports_missing_variable() {
        let env = HashMap::new();
        match expand_env("${NOPE}", &env) {
            Err(ConfigError::MissingEnv(var)) => assert_eq!(var, "NOPE"),
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    #[test]
    fn expand_env_leaves_unclosed_token_verbatim() {
        let env = HashMap::new();
        assert_eq!(expand_env("a${B", &env).unwrap(), "a${B");
        assert_eq!(expand_env("${}", &env).unwrap(), "${}");
    }
}
