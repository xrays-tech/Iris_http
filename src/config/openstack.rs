//! `openstack` (auto-discovery) site configuration: types and parsing.
//!
//! Public types mirror `plan/02-contracts.md` verbatim. Secrets are expanded
//! from `${ENV}` at parse time and never rendered by [`std::fmt::Debug`].

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use url::Url;

use super::{expand_env, is_valid_hostname, site_error, ConfigError};

/// Lower/upper bound for `openstack.cache_ttl_secs` (`config.md` §6).
pub(crate) const MIN_CACHE_TTL_SECS: u64 = 60;
/// Upper bound for `openstack.cache_ttl_secs`: 7 days.
pub(crate) const MAX_CACHE_TTL_SECS: u64 = 7 * 86_400;
/// Lower bound for `openstack.timeout_secs` (`config.md` §4): a zero-second
/// timeout would fail every Nova request instantly (permanent 502).
pub(crate) const MIN_TIMEOUT_SECS: u64 = 1;
/// Upper bound for `openstack.timeout_secs`: 10 minutes.
pub(crate) const MAX_TIMEOUT_SECS: u64 = 600;

/// An `openstack` site: wildcard sub-domain match resolved via Nova.
pub struct OpenStackSite {
    /// Unique site identifier.
    pub name: String,
    /// Root domain; requests of the form `{instance}-{port}.{base_domain}`.
    pub base_domain: String,
    /// Respond on the plain HTTP listener.
    pub http: bool,
    /// Respond on the TLS listener (requires `cert_file` + `key_file`).
    pub https: bool,
    /// PEM certificate path (required when `https`).
    pub cert_file: Option<PathBuf>,
    /// PEM private-key path (required when `https`).
    pub key_file: Option<PathBuf>,
    /// Firewall rule directory (required; ACLs are always enforced).
    pub firewall_dir: PathBuf,
    /// Name of the referenced [`OpenStackEnv`] (file stem under
    /// `<config-dir>/openstack/`); required.
    pub openstack_env: String,
}

impl fmt::Debug for OpenStackSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the environment *name* is rendered; its (redacted) config lives
        // in `Config::openstack_envs`.
        f.debug_struct("OpenStackSite")
            .field("name", &self.name)
            .field("base_domain", &self.base_domain)
            .field("http", &self.http)
            .field("https", &self.https)
            .field("cert_file", &self.cert_file)
            .field("key_file", &self.key_file)
            .field("firewall_dir", &self.firewall_dir)
            .field("openstack_env", &self.openstack_env)
            .finish()
    }
}

/// A standalone OpenStack environment loaded from
/// `<config-dir>/openstack/<name>.yaml`.
///
/// The file stem is the environment name; sites (openstack wildcard sites and
/// forward sites with instance backends) reference it by name.
#[derive(Clone, PartialEq, Eq)]
pub struct OpenStackEnv {
    /// Environment name (the file stem).
    pub name: String,
    /// The control-plane configuration from the environment file.
    pub config: OpenStackConfig,
}

impl OpenStackEnv {
    /// The control-plane configuration (secrets redacted only in `Debug`).
    pub fn config(&self) -> &OpenStackConfig {
        &self.config
    }
}

impl fmt::Debug for OpenStackEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegates to `OpenStackConfig`'s redacting Debug (via `AuthConfig`).
        f.debug_struct("OpenStackEnv")
            .field("name", &self.name)
            .field("config", &self.config)
            .finish()
    }
}

/// OpenStack control-plane configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStackConfig {
    /// Positive-result cache TTL in seconds (default 86400).
    pub cache_ttl_secs: u64,
    /// Per-request Nova timeout in seconds.
    pub timeout_secs: u64,
    /// Keystone authentication credentials.
    pub auth: AuthConfig,
    /// Nova API settings.
    pub nova: NovaConfig,
}

/// Keystone (OpenStack Identity) v3 authentication configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Keystone v3 base URL, e.g. `https://controller:5000/v3`.
    pub api_url: Url,
    /// Credential material for the configured approach.
    pub credentials: AuthCredentials,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `api_url` is non-secret; credentials' Debug redacts the secrets.
        f.debug_struct("AuthConfig")
            .field("api_url", &self.api_url)
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// The Keystone authentication approach and its credentials.
///
/// Exactly one variant's fields are populated, selected by the YAML
/// `auth.approach` discriminator.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthCredentials {
    /// Password (scope: project) authentication.
    Password {
        /// Username.
        username: String,
        /// Password (typically `${ENV}`-sourced).
        password: String,
        /// Project name.
        project_name: String,
        /// Project domain name.
        project_domain_name: String,
        /// User domain name.
        user_domain_name: String,
    },
    /// Application-credential authentication.
    ApplicationCredential {
        /// Application credential id.
        id: String,
        /// Application credential secret.
        secret: String,
    },
}

impl fmt::Debug for AuthCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Secrets are always redacted so they can never reach logs.
        match self {
            Self::Password {
                username,
                project_name,
                project_domain_name,
                user_domain_name,
                ..
            } => f
                .debug_struct("AuthCredentials::Password")
                .field("username", username)
                .field("password", &"<redacted>")
                .field("project_name", project_name)
                .field("project_domain_name", project_domain_name)
                .field("user_domain_name", user_domain_name)
                .finish(),
            Self::ApplicationCredential { id, .. } => f
                .debug_struct("AuthCredentials::ApplicationCredential")
                .field("id", id)
                .field("secret", &"<redacted>")
                .finish(),
        }
    }
}

/// Nova API settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovaConfig {
    /// Explicit Nova endpoint; discovered from the catalog when `None`.
    pub api_url: Option<Url>,
    /// Query across all projects (`all_tenants`).
    pub all_tenants: bool,
}

/// Raw `openstack` site as deserialized from YAML (before validation).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOpenStackSite {
    pub(crate) name: String,
    pub(crate) mode: String,
    pub(crate) base_domain: String,
    #[serde(default = "super::default_http")]
    pub(crate) http: bool,
    pub(crate) https: bool,
    #[serde(default)]
    pub(crate) cert_file: Option<String>,
    #[serde(default)]
    pub(crate) key_file: Option<String>,
    #[serde(default)]
    pub(crate) firewall_dir: Option<String>,
    pub(crate) openstack_env: String,
}

/// Raw OpenStack control-plane block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOpenStackConfig {
    #[serde(default = "super::default_cache_ttl")]
    pub(crate) cache_ttl_secs: u64,
    pub(crate) timeout_secs: u64,
    pub(crate) auth: RawAuth,
    pub(crate) nova: RawNova,
}

/// Raw Keystone auth block (both branches represented by optionals).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAuth {
    pub(crate) api_url: Option<String>,
    pub(crate) approach: String,
    #[serde(default)]
    pub(crate) username: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) project_name: Option<String>,
    #[serde(default)]
    pub(crate) project_domain_name: Option<String>,
    #[serde(default)]
    pub(crate) user_domain_name: Option<String>,
    #[serde(default)]
    pub(crate) application_credential_id: Option<String>,
    #[serde(default)]
    pub(crate) application_credential_secret: Option<String>,
}

/// Raw Nova block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNova {
    #[serde(default)]
    pub(crate) api_url: Option<String>,
    #[serde(default = "super::default_true")]
    pub(crate) all_tenants: bool,
}

/// Validate and normalise a raw `openstack` site.
pub(crate) fn parse(
    raw: RawOpenStackSite,
    file: &Path,
    env: &HashMap<String, String>,
) -> Result<OpenStackSite, ConfigError> {
    if raw.mode != "openstack" {
        return Err(site_error(
            file,
            format!(
                "site '{}': expected mode 'openstack', got '{}'",
                raw.name, raw.mode
            ),
        ));
    }
    if raw.name.trim().is_empty() {
        return Err(site_error(file, "site name must not be empty"));
    }

    let base_domain = raw.base_domain.trim_end_matches('.').to_ascii_lowercase();
    if !is_valid_hostname(&base_domain) {
        return Err(site_error(
            file,
            format!(
                "site '{}': invalid base_domain '{}'",
                raw.name, raw.base_domain
            ),
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

    // `firewall_dir` is required (P16): without it an openstack wildcard site
    // would be an unauthenticated arbitrary-port proxy into any visible
    // instance. Missing field or a missing/non-directory path is a
    // `ConfigError::Validate`, so reload keeps the previous configuration.
    let firewall_dir = match expand_path(&raw.firewall_dir, env)? {
        None => {
            return Err(site_error(
                file,
                format!("site '{}': firewall_dir is required", raw.name),
            ));
        }
        Some(dir) => {
            if !dir.is_dir() {
                return Err(site_error(
                    file,
                    format!(
                        "site '{}': firewall_dir '{}' does not exist or is not a directory",
                        raw.name,
                        dir.display()
                    ),
                ));
            }
            dir
        }
    };

    let openstack_env = raw.openstack_env.trim().to_owned();
    if openstack_env.is_empty() {
        return Err(site_error(
            file,
            format!("site '{}': openstack_env is required", raw.name),
        ));
    }

    Ok(OpenStackSite {
        name: raw.name,
        base_domain,
        http: raw.http,
        https: raw.https,
        cert_file,
        key_file,
        firewall_dir,
        openstack_env,
    })
}

/// Validate and normalise a raw environment file's control-plane block.
///
/// `name` names the environment (the file stem) for error messages; `file` is
/// the environment file itself.
pub(crate) fn build_openstack_config(
    os: RawOpenStackConfig,
    name: &str,
    file: &Path,
    env: &HashMap<String, String>,
) -> Result<OpenStackConfig, ConfigError> {
    if !(MIN_CACHE_TTL_SECS..=MAX_CACHE_TTL_SECS).contains(&os.cache_ttl_secs) {
        return Err(site_error(
            file,
            format!(
                "openstack env '{name}': cache_ttl_secs {} out of range {}..={}",
                os.cache_ttl_secs, MIN_CACHE_TTL_SECS, MAX_CACHE_TTL_SECS
            ),
        ));
    }
    if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&os.timeout_secs) {
        return Err(site_error(
            file,
            format!(
                "openstack env '{name}': timeout_secs {} out of range {}..={}",
                os.timeout_secs, MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS
            ),
        ));
    }

    let auth = parse_auth(os.auth, name, file, env)?;

    let api_url = match os.nova.api_url {
        None => None,
        Some(value) => Some(Url::parse(&value).map_err(|err| {
            site_error(
                file,
                format!("openstack env '{name}': invalid nova.api_url '{value}': {err}"),
            )
        })?),
    };

    Ok(OpenStackConfig {
        cache_ttl_secs: os.cache_ttl_secs,
        timeout_secs: os.timeout_secs,
        auth,
        nova: NovaConfig {
            api_url,
            all_tenants: os.nova.all_tenants,
        },
    })
}

/// Load every `*.yaml` environment file under `envs_dir`, keyed by file stem.
///
/// A missing (or non-directory) `envs_dir` is `Ok(empty)`: no environment is
/// configured and any site that references one fails validation. Any single
/// unreadable/unparseable/invalid file fails the whole load, mirroring the
/// site-file semantics.
pub fn load_openstack_envs(
    envs_dir: &Path,
    env: &HashMap<String, String>,
) -> Result<HashMap<String, Arc<OpenStackEnv>>, ConfigError> {
    let mut envs: HashMap<String, Arc<OpenStackEnv>> = HashMap::new();
    if !envs_dir.is_dir() {
        return Ok(envs);
    }

    let entries = std::fs::read_dir(envs_dir).map_err(|err| {
        ConfigError::Io(std::io::Error::new(
            err.kind(),
            format!(
                "failed to read openstack env directory '{}': {err}",
                envs_dir.display()
            ),
        ))
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            ConfigError::Io(std::io::Error::new(
                err.kind(),
                format!(
                    "failed to read an entry of '{}': {err}",
                    envs_dir.display()
                ),
            ))
        })?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            files.push(path);
        }
    }
    files.sort();

    for path in &files {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| ConfigError::Parse {
                file: path.clone(),
                msg: "openstack env file name is not valid UTF-8".to_owned(),
            })?
            .to_owned();
        if envs.contains_key(&name) {
            return Err(ConfigError::Validate(format!(
                "duplicate openstack env name '{name}' (file '{}')",
                path.display()
            )));
        }

        let text = std::fs::read_to_string(path).map_err(|err| ConfigError::Parse {
            file: path.clone(),
            msg: format!("failed to read file: {err}"),
        })?;
        let raw: RawOpenStackConfig =
            serde_yaml::from_str(&text).map_err(|err| ConfigError::Parse {
                file: path.clone(),
                msg: err.to_string(),
            })?;
        let config = build_openstack_config(raw, &name, path, env)?;
        envs.insert(name.clone(), Arc::new(OpenStackEnv { name, config }));
    }

    Ok(envs)
}

fn parse_auth(
    raw: RawAuth,
    site: &str,
    file: &Path,
    env: &HashMap<String, String>,
) -> Result<AuthConfig, ConfigError> {
    // `auth.api_url` (Keystone v3 base) is required; missing or unparseable fails.
    let api_url_raw = require(raw.api_url, "api_url", site, file)?;
    let api_url = Url::parse(&api_url_raw).map_err(|err| {
        site_error(
            file,
            format!("site '{site}': invalid auth.api_url '{api_url_raw}': {err}"),
        )
    })?;
    if !matches!(api_url.scheme(), "http" | "https") {
        return Err(site_error(
            file,
            format!(
                "site '{site}': auth.api_url must be an http(s) URL, got scheme '{}'",
                api_url.scheme()
            ),
        ));
    }

    let credentials = match raw.approach.as_str() {
        "password" => {
            if raw.application_credential_id.is_some() || raw.application_credential_secret.is_some()
            {
                return Err(site_error(
                    file,
                    format!(
                        "site '{site}': auth.approach=password must not set application_credential_* fields"
                    ),
                ));
            }
            let username = require(raw.username, "username", site, file)?;
            let password = require(raw.password, "password", site, file)?;
            let project_name = require(raw.project_name, "project_name", site, file)?;
            let project_domain_name =
                require(raw.project_domain_name, "project_domain_name", site, file)?;
            let user_domain_name = require(raw.user_domain_name, "user_domain_name", site, file)?;
            AuthCredentials::Password {
                username,
                password: expand_env(&password, env)?,
                project_name,
                project_domain_name,
                user_domain_name,
            }
        }
        "application_credential" => {
            if raw.username.is_some()
                || raw.password.is_some()
                || raw.project_name.is_some()
                || raw.project_domain_name.is_some()
                || raw.user_domain_name.is_some()
            {
                return Err(site_error(
                    file,
                    format!(
                        "site '{site}': auth.approach=application_credential must not set password fields"
                    ),
                ));
            }
            let id = require(
                raw.application_credential_id,
                "application_credential_id",
                site,
                file,
            )?;
            let secret = require(
                raw.application_credential_secret,
                "application_credential_secret",
                site,
                file,
            )?;
            AuthCredentials::ApplicationCredential {
                id,
                secret: expand_env(&secret, env)?,
            }
        }
        other => {
            return Err(site_error(
                file,
                format!("site '{site}': unknown auth.approach '{other}'"),
            ));
        }
    };

    Ok(AuthConfig {
        api_url,
        credentials,
    })
}

fn require(
    value: Option<String>,
    field: &str,
    site: &str,
    file: &Path,
) -> Result<String, ConfigError> {
    value.ok_or_else(|| site_error(file, format!("site '{site}': auth.{field} is required")))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, otherwise-valid raw environment block with the given timeout.
    fn raw(timeout_secs: u64) -> RawOpenStackConfig {
        RawOpenStackConfig {
            cache_ttl_secs: 86_400,
            timeout_secs,
            auth: RawAuth {
                api_url: Some("https://controller.example.com:5000/v3".to_owned()),
                approach: "application_credential".to_owned(),
                username: None,
                password: None,
                project_name: None,
                project_domain_name: None,
                user_domain_name: None,
                application_credential_id: Some("id".to_owned()),
                application_credential_secret: Some("secret".to_owned()),
            },
            nova: RawNova {
                api_url: None,
                all_tenants: true,
            },
        }
    }

    #[test]
    fn timeout_secs_out_of_range_is_rejected() {
        let file = Path::new("openstack/prod.yaml");
        let env = HashMap::new();

        // A zero timeout fails every Nova request instantly; reject it.
        let err =
            build_openstack_config(raw(0), "prod", file, &env).expect_err("timeout 0 must be rejected");
        assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");

        let err = build_openstack_config(raw(601), "prod", file, &env)
            .expect_err("timeout 601 must be rejected");
        assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
    }

    #[test]
    fn timeout_secs_accepts_the_inclusive_bounds() {
        let file = Path::new("openstack/prod.yaml");
        let env = HashMap::new();

        assert!(build_openstack_config(raw(MIN_TIMEOUT_SECS), "prod", file, &env).is_ok());
        assert!(build_openstack_config(raw(MAX_TIMEOUT_SECS), "prod", file, &env).is_ok());
    }
}

