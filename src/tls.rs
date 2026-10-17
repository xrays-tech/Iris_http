//! P4 — TLS termination and per-SNI certificate selection (`design.md` §4.2,
//! `plan/07-p4-tls-sni.md`).
//!
//! The proxy terminates TLS on a shared `443` listener and picks the served
//! certificate from the client's SNI. Certificates are held in an
//! [`ArcSwap`]-backed [`SniCertMap`] so a future `reload` (P8) can replace the
//! whole table atomically while in-flight handshakes keep their snapshot.
//!
//! Lookup rules (`plan/02-contracts.md` §`src/tls.rs`):
//! 1. normalise the SNI (lowercase, strip one trailing dot);
//! 2. exact match in [`SniCertMap::exact`] (forward sites, keyed by domain);
//! 3. otherwise the **longest** suffix match against the openstack
//!    `base_domain` list ([`SniCertMap::bases`], keyed by `base_domain`).
//!
//! A miss installs no certificate and the handshake fails at the TLS layer —
//! there is no HTTP status to return (`plan/07` §421 semantics).

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use openssl::pkey::{PKey, Private};
use openssl::x509::X509;
use pingora::listeners::TlsAccept;
use pingora::tls::ext;
use pingora::tls::ssl::{NameType, SslRef};

use crate::config::Config;

/// Normalise a TLS SNI value into a certificate-map key.
///
/// Lowercases and strips exactly one trailing dot (a FQDN-root form). SNI never
/// carries a port, so none is stripped.
pub fn normalize_sni(sni: &str) -> String {
    let lowered = sni.to_ascii_lowercase();
    // Strip exactly one trailing dot (FQDN-root form), mirroring
    // `routing::normalize_host`.
    match lowered.strip_suffix('.') {
        Some(stripped) => stripped.to_owned(),
        None => lowered,
    }
}

/// A parsed certificate/private-key pair together with the site that owns it.
///
/// The owning site name lets the request path detect a *misdirected* request:
/// a TLS connection whose presented certificate belongs to a site other than
/// the one the request `Host` routes to (`proxy.rs` 421 check).
pub struct CertKey {
    cert: X509,
    key: PKey<Private>,
    site: String,
}

impl CertKey {
    /// Load and parse a PEM certificate/key pair.
    ///
    /// `cert_path`/`key_path` are filled in by configuration validation whenever
    /// `https` is enabled, but both are re-checked here so a malformed config
    /// yields a descriptive error rather than a silent miss.
    fn load(cert_path: Option<&Path>, key_path: Option<&Path>, site: &str) -> Result<Self> {
        let cert_path = cert_path.with_context(|| {
            format!("site '{site}': https enabled but cert_file is not configured")
        })?;
        let key_path = key_path.with_context(|| {
            format!("site '{site}': https enabled but key_file is not configured")
        })?;

        let cert_pem = std::fs::read(cert_path)
            .with_context(|| format!("site '{site}': reading cert_file '{}'", cert_path.display()))?;
        let key_pem = std::fs::read(key_path)
            .with_context(|| format!("site '{site}': reading key_file '{}'", key_path.display()))?;

        let cert = X509::from_pem(&cert_pem)
            .with_context(|| format!("site '{site}': parsing cert_file '{}'", cert_path.display()))?;
        let key = PKey::private_key_from_pem(&key_pem)
            .with_context(|| format!("site '{site}': parsing key_file '{}'", key_path.display()))?;

        Ok(Self {
            cert,
            key,
            site: site.to_owned(),
        })
    }

    /// The [`crate::config`] site name that owns this certificate.
    pub fn site(&self) -> &str {
        &self.site
    }
}

impl fmt::Debug for CertKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the private key; the owning site is enough for logs.
        f.debug_struct("CertKey")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

/// The certificate table served by the TLS listener.
///
/// `exact` holds forward-site certificates keyed by (normalised) domain;
/// `bases` holds openstack wildcard certificates keyed by `base_domain`, sorted
/// longest-first so the first suffix match is the most specific one.
pub struct SniCertMap {
    exact: HashMap<String, Arc<CertKey>>,
    bases: Vec<(String, Arc<CertKey>)>,
}

impl SniCertMap {
    /// An empty map (no certificate can be selected; every handshake fails).
    pub fn empty() -> Self {
        Self {
            exact: HashMap::new(),
            bases: Vec::new(),
        }
    }

    /// Build the certificate table from a validated [`Config`].
    ///
    /// Only `https`-enabled sites contribute: forward sites enter `exact`
    /// (keyed by domain); openstack sites enter `bases` (keyed by `base_domain`,
    /// requiring an operator-provided `*.base_domain` wildcard certificate).
    /// A missing or unparseable certificate/key file is an error.
    pub fn from_sites(config: &Config) -> Result<Self> {
        let mut exact = HashMap::with_capacity(config.forward.len());
        for site in &config.forward {
            if !site.https {
                continue;
            }
            let cert = CertKey::load(
                site.cert_file.as_deref(),
                site.key_file.as_deref(),
                &site.name,
            )
            .with_context(|| format!("forward site '{}'", site.name))?;
            exact.insert(normalize_sni(&site.domain), Arc::new(cert));
        }

        let mut bases: Vec<(String, Arc<CertKey>)> = Vec::with_capacity(config.openstack.len());
        for site in &config.openstack {
            if !site.https {
                continue;
            }
            let cert = CertKey::load(
                site.cert_file.as_deref(),
                site.key_file.as_deref(),
                &site.name,
            )
            .with_context(|| format!("openstack site '{}'", site.name))?;
            bases.push((normalize_sni(&site.base_domain), Arc::new(cert)));
        }

        // Longest base_domain first; lexicographic tie-break keeps the order
        // deterministic regardless of config load order.
        bases.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

        Ok(Self { exact, bases })
    }

    /// Select the certificate for `sni`, or `None` when no site matches.
    ///
    /// Exact (forward) match wins; otherwise the longest `base_domain` that is
    /// a proper label-suffix of `sni` (openstack wildcard). The bare
    /// `base_domain` itself does **not** match — `*.base_domain` does not cover
    /// the apex, and a bare base is not a routable instance host.
    pub fn lookup(&self, sni: &str) -> Option<Arc<CertKey>> {
        let name = normalize_sni(sni);
        if name.is_empty() {
            return None;
        }
        if let Some(cert) = self.exact.get(&name) {
            return Some(Arc::clone(cert));
        }
        for (base, cert) in &self.bases {
            if is_label_suffix(&name, base) {
                return Some(Arc::clone(cert));
            }
        }
        None
    }
}

impl fmt::Debug for SniCertMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render only the domain keys and owning sites; never certificate or
        // key material.
        let exact: Vec<&str> = self.exact.keys().map(String::as_str).collect();
        let bases: Vec<&str> = self.bases.iter().map(|(base, _)| base.as_str()).collect();
        f.debug_struct("SniCertMap")
            .field("exact", &exact)
            .field("bases", &bases)
            .finish()
    }
}

/// Whether `name` ends with `.base` at a label boundary (`a.base`, not `abase`).
fn is_label_suffix(name: &str, base: &str) -> bool {
    if name.len() <= base.len() || !name.ends_with(base) {
        return false;
    }
    name.as_bytes()[name.len() - base.len() - 1] == b'.'
}

/// The SNI a TLS connection was established with, attached to the connection
/// digest by [`SniCertSource::handshake_complete_callback`].
///
/// The HTTP layer reads it from `Session::digest().ssl_digest.extension` to
/// detect misdirected requests (`proxy.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSni(pub String);

impl TlsSni {
    /// The SNI as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The Pingora [`TlsAccept`] callback that selects a certificate per SNI.
///
/// The certificate table is an [`ArcSwap`] snapshot so lookups on the handshake
/// path are lock-free and a reload swaps the table atomically.
pub struct SniCertSource {
    certs: ArcSwap<SniCertMap>,
}

impl SniCertSource {
    /// Build the source from a validated [`Config`].
    pub fn from_sites(config: &Config) -> Result<Self> {
        Ok(Self {
            certs: ArcSwap::from_pointee(SniCertMap::from_sites(config)?),
        })
    }

    /// Build the source from an existing map (used by tests / reload wiring).
    pub fn new(map: SniCertMap) -> Self {
        Self {
            certs: ArcSwap::from_pointee(map),
        }
    }

    /// Look up the certificate currently mapped to `sni`.
    pub fn lookup(&self, sni: &str) -> Option<Arc<CertKey>> {
        self.certs.load().lookup(sni)
    }
}

#[async_trait]
impl TlsAccept for SniCertSource {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let sni = ssl
            .servername(NameType::HOST_NAME)
            .unwrap_or_default()
            .to_owned();
        let map = self.certs.load();
        let Some(cert) = map.lookup(&sni) else {
            // No certificate for this SNI: install nothing so the handshake
            // aborts (there is no HTTP status to return at the TLS layer).
            tracing::debug!(sni = %sni, "no certificate configured for SNI; aborting handshake");
            return;
        };
        if let Err(err) = ext::ssl_use_certificate(ssl, &cert.cert) {
            tracing::warn!(sni = %sni, error = %err, "failed to install certificate for SNI");
            return;
        }
        if let Err(err) = ext::ssl_use_private_key(ssl, &cert.key) {
            tracing::warn!(sni = %sni, error = %err, "failed to install private key for SNI");
        }
    }

    async fn handshake_complete_callback(&self, ssl: &SslRef) -> Option<Arc<dyn Any + Send + Sync>> {
        // Re-read the SNI (still available after the handshake) and expose it to
        // the HTTP layer via the connection digest extension.
        let sni = ssl.servername(NameType::HOST_NAME)?;
        Some(Arc::new(TlsSni(sni.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::path::PathBuf;

    use url::Url;

    use crate::config::{
        AuthConfig, AuthCredentials, Backend, ForwardSite, LbMode, NovaConfig, OpenStackConfig,
        OpenStackSite,
    };

    const WWW_CERT: &str = "tests/fixtures/certs/www.example.com.crt";
    const WWW_KEY: &str = "tests/fixtures/certs/www.example.com.key";
    const RENT_CERT: &str = "tests/fixtures/certs/rent.do.top.crt";
    const RENT_KEY: &str = "tests/fixtures/certs/rent.do.top.key";

    fn forward_site(name: &str, domain: &str, https: bool) -> ForwardSite {
        ForwardSite {
            name: name.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https,
            cert_file: https.then(|| PathBuf::from(WWW_CERT)),
            key_file: https.then(|| PathBuf::from(WWW_KEY)),
            backends: vec![Backend {
                addr: SocketAddr::from(([127, 0, 0, 1], 18081)),
                weight: 1,
            }],
            lb_mode: LbMode::RoundRobin,
            health_check: None,
        }
    }

    fn openstack_site(name: &str, base_domain: &str, https: bool) -> OpenStackSite {
        OpenStackSite {
            name: name.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https,
            cert_file: https.then(|| PathBuf::from(RENT_CERT)),
            key_file: https.then(|| PathBuf::from(RENT_KEY)),
            firewall_dir: None,
            openstack: OpenStackConfig {
                cache_ttl_secs: 86_400,
                timeout_secs: 5,
                auth: AuthConfig {
                    api_url: Url::parse("https://controller.example.com:5000/v3")
                        .expect("valid test URL"),
                    credentials: AuthCredentials::ApplicationCredential {
                        id: "test-id".to_owned(),
                        secret: "test-secret".to_owned(),
                    },
                },
                nova: NovaConfig {
                    api_url: None,
                    all_tenants: true,
                },
            },
        }
    }

    /// A map built directly from a config with no parsing (fixtures only).
    fn map(forward: Vec<ForwardSite>, openstack: Vec<OpenStackSite>) -> SniCertMap {
        let config = Config { forward, openstack };
        SniCertMap::from_sites(&config).expect("certificate map builds")
    }

    // ----- lookup -----

    #[test]
    fn lookup_exact_match() {
        let m = map(vec![forward_site("www", "www.example.com", true)], vec![]);
        let cert = m.lookup("www.example.com").expect("exact hit");
        assert_eq!(cert.site(), "www");
    }

    #[test]
    fn lookup_base_suffix_match() {
        let m = map(vec![], vec![openstack_site("rent", "rent.do.top", true)]);
        let cert = m.lookup("INS001-80.rent.do.top").expect("base hit");
        assert_eq!(cert.site(), "rent");
    }

    #[test]
    fn lookup_miss_returns_none() {
        let m = map(
            vec![forward_site("www", "www.example.com", true)],
            vec![openstack_site("rent", "rent.do.top", true)],
        );
        assert!(m.lookup("unknown.example.org").is_none());
        assert!(m.lookup("").is_none());
        // The bare base_domain is not covered by `*.base_domain`.
        assert!(m.lookup("rent.do.top").is_none());
        // A lookalike suffix must not match at a non-label boundary.
        assert!(m.lookup("notrent.do.top").is_none());
        // Suffix under the wrong base.
        assert!(m.lookup("ins-80.example.com").is_none());
    }

    #[test]
    fn lookup_normalises_case_and_trailing_dot() {
        let m = map(vec![forward_site("www", "www.example.com", true)], vec![]);
        assert!(m.lookup("WWW.Example.COM").is_some());
        assert!(m.lookup("www.example.com.").is_some());
        assert!(m.lookup("WWW.Example.COM.").is_some());
    }

    #[test]
    fn lookup_prefers_longest_base() {
        // Overlapping bases are impossible through config validation, but the
        // map's ordering guarantee must still pick the most specific suffix.
        let m = map(
            vec![],
            vec![
                openstack_site("rent", "rent.do.top", true),
                openstack_site("sub", "a.rent.do.top", true),
            ],
        );
        // `x.a.rent.do.top` ends with both bases; the longer one must win.
        let cert = m.lookup("x.a.rent.do.top").expect("hit");
        assert_eq!(cert.site(), "sub");
        // The shorter base still serves its own subdomains.
        let cert = m.lookup("b.rent.do.top").expect("hit");
        assert_eq!(cert.site(), "rent");
    }

    #[test]
    fn lookup_exact_wins_over_base() {
        let m = map(
            vec![forward_site("exact", "www.rent.do.top", true)],
            vec![openstack_site("rent", "rent.do.top", true)],
        );
        // `www.rent.do.top` is both an exact domain and a wildcard subdomain.
        let cert = m.lookup("www.rent.do.top").expect("hit");
        assert_eq!(cert.site(), "exact");
    }

    // ----- from_sites -----

    #[test]
    fn from_sites_mixed_config_only_https_sites() {
        let m = map(
            vec![
                forward_site("www", "www.example.com", true),
                forward_site("plain", "plain.example.com", false),
            ],
            vec![
                openstack_site("rent", "rent.do.top", true),
                openstack_site("off", "off.example.net", false),
            ],
        );
        assert_eq!(m.exact.len(), 1, "only the https forward site");
        assert_eq!(m.bases.len(), 1, "only the https openstack site");
        assert!(m.lookup("www.example.com").is_some());
        assert!(m.lookup("plain.example.com").is_none(), "https-off excluded");
        assert!(m.lookup("INS001-80.rent.do.top").is_some());
        assert!(m.lookup("x-80.off.example.net").is_none(), "https-off excluded");
    }

    #[test]
    fn from_sites_empty_config_is_empty_map() {
        let config = Config {
            forward: Vec::new(),
            openstack: Vec::new(),
        };
        let m = SniCertMap::from_sites(&config).expect("empty config builds");
        assert!(m.exact.is_empty());
        assert!(m.bases.is_empty());
    }

    #[test]
    fn from_sites_missing_cert_file_is_error() {
        let mut site = forward_site("www", "www.example.com", true);
        site.cert_file = Some(PathBuf::from("/no/such/cert.crt"));
        let config = Config {
            forward: vec![site],
            openstack: Vec::new(),
        };
        let err = SniCertMap::from_sites(&config).expect_err("missing cert must error");
        assert!(format!("{err:#}").contains("/no/such/cert.crt"));
    }

    #[test]
    fn from_sites_unparseable_cert_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("bad.crt");
        let key = dir.path().join("bad.key");
        std::fs::write(&cert, "not a certificate\n").expect("write");
        std::fs::write(&key, "not a key\n").expect("write");

        let mut site = forward_site("www", "www.example.com", true);
        site.cert_file = Some(cert.clone());
        site.key_file = Some(key);
        let config = Config {
            forward: vec![site],
            openstack: Vec::new(),
        };
        let err = SniCertMap::from_sites(&config).expect_err("garbage cert must error");
        assert!(format!("{err:#}").contains("bad.crt"));
    }

    #[test]
    fn from_sites_duplicate_domain_last_wins() {
        // Config validation rejects duplicate domains; the map must still be
        // deterministic (later insert overwrites).
        let m = map(
            vec![
                forward_site("first", "www.example.com", true),
                forward_site("second", "www.example.com", true),
            ],
            vec![],
        );
        assert_eq!(m.lookup("www.example.com").expect("hit").site(), "second");
    }

    // ----- normalize_sni -----

    #[test]
    fn normalize_sni_lowercases_and_strips_one_dot() {
        assert_eq!(normalize_sni("WWW.Example.COM"), "www.example.com");
        assert_eq!(normalize_sni("example.com."), "example.com");
        assert_eq!(normalize_sni("example.com.."), "example.com.");
        assert_eq!(normalize_sni(""), "");
    }
}
