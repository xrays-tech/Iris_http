//! Keystone v3 authentication (`discovery.md` §3).
//!
//! [`fetch_token`] performs `POST {auth.api_url}/auth/tokens` with either the
//! `password` or `application_credential` body, parses the `201` response
//! (`X-Subject-Token` header + `token.expires_at`/`token.catalog`) and returns a
//! [`Token`]. The body builders and the response parser are pure so they are
//! unit tested directly; only the HTTP round trip is exercised by the mock
//! integration harness.
//!
//! The 401 "refresh once and retry" behaviour lives one level up (token cache in
//! [`super::cache`]), so [`fetch_token`] itself is a single, side-effect-free
//! authentication attempt.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use super::HttpClient;
use crate::config::{AuthConfig, AuthCredentials};

/// A token is considered stale this many seconds before its real expiry, so the
/// next request refreshes it rather than racing the deadline (`discovery.md` §3).
pub const EARLY_REFRESH_SECS: i64 = 60;

/// A Keystone token together with the Nova endpoint it authorises.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// The `X-Subject-Token` value (secret; redacted by [`fmt::Debug`]).
    pub value: String,
    /// Token expiry (UTC).
    pub expires_at: DateTime<Utc>,
    /// Nova (compute) base URL taken from the service catalog.
    pub nova_base: Url,
}

impl Token {
    /// Whether the token is still usable at `now`, applying the early-refresh
    /// buffer so callers never race the expiry.
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now + chrono::Duration::seconds(EARLY_REFRESH_SECS)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token value is a bearer secret: never render it.
        f.debug_struct("Token")
            .field("value", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("nova_base", &self.nova_base)
            .finish()
    }
}

/// Keystone authentication failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Transport-level failure (connect, TLS, read).
    Http(String),
    /// The request exceeded the configured timeout.
    Timeout,
    /// Non-2xx HTTP status returned by Keystone.
    Status(u16),
    /// The response body/header was malformed.
    Decode(String),
    /// The service catalog contained no usable compute endpoint.
    NoCatalog,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(msg) => write!(f, "keystone request failed: {msg}"),
            Self::Timeout => write!(f, "keystone request timed out"),
            Self::Status(code) => write!(f, "keystone returned HTTP {code}"),
            Self::Decode(msg) => write!(f, "keystone response decode failed: {msg}"),
            Self::NoCatalog => write!(f, "keystone catalog has no compute endpoint"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Build the Keystone `/auth/tokens` endpoint from the configured v3 base URL.
fn auth_endpoint(api_url: &Url) -> Result<Url, AuthError> {
    let mut url = api_url.clone();
    url.path_segments_mut()
        .map_err(|_| AuthError::Http("auth api_url cannot be a base URL".to_owned()))?
        .pop_if_empty()
        .push("auth")
        .push("tokens");
    Ok(url)
}

/// Fetch a fresh token from Keystone.
///
/// `api_url` is the Keystone v3 base (`config.auth.api_url`); call sites pass
/// `(&site.auth, &site.auth.api_url, client)` per `02-contracts.md`.
pub async fn fetch_token(
    cfg: &AuthConfig,
    api_url: &Url,
    client: &HttpClient,
) -> Result<Token, AuthError> {
    let endpoint = auth_endpoint(api_url)?;
    let body = build_auth_body(&cfg.credentials);

    let response = client
        .inner()
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                AuthError::Timeout
            } else {
                AuthError::Http(err.to_string())
            }
        })?;

    let status = response.status().as_u16();
    let subject = response
        .headers()
        .get("X-Subject-Token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .bytes()
        .await
        .map_err(|err| AuthError::Http(err.to_string()))?;

    if status != 201 {
        return Err(AuthError::Status(status));
    }

    let subject = subject.ok_or_else(|| AuthError::Decode("missing X-Subject-Token header".into()))?;
    parse_token_response(&subject, &bytes)
}

// ---------------------------------------------------------------------------
// Request body construction (pure)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PasswordRequest {
    auth: PasswordAuth,
}

#[derive(Serialize)]
struct PasswordAuth {
    identity: PasswordIdentity,
    scope: ProjectScope,
}

#[derive(Serialize)]
struct PasswordIdentity {
    methods: [&'static str; 1],
    password: PasswordCredential,
}

#[derive(Serialize)]
struct PasswordCredential {
    user: UserRef,
}

#[derive(Serialize)]
struct UserRef {
    name: String,
    domain: DomainRef,
    password: String,
}

#[derive(Serialize)]
struct ProjectScope {
    project: ProjectRef,
}

#[derive(Serialize)]
struct ProjectRef {
    name: String,
    domain: DomainRef,
}

#[derive(Serialize)]
struct DomainRef {
    name: String,
}

#[derive(Serialize)]
struct AppCredRequest {
    auth: AppCredAuth,
}

#[derive(Serialize)]
struct AppCredAuth {
    identity: AppCredIdentity,
}

#[derive(Serialize)]
struct AppCredIdentity {
    methods: [&'static str; 1],
    application_credential: AppCredRef,
}

#[derive(Serialize)]
struct AppCredRef {
    id: String,
    secret: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AuthRequest {
    Password(PasswordRequest),
    ApplicationCredential(AppCredRequest),
}

/// Build the JSON request body for the configured credential approach
/// (`discovery.md` §3).
fn build_auth_body(credentials: &AuthCredentials) -> AuthRequest {
    match credentials {
        AuthCredentials::Password {
            username,
            password,
            project_name,
            project_domain_name,
            user_domain_name,
        } => AuthRequest::Password(PasswordRequest {
            auth: PasswordAuth {
                identity: PasswordIdentity {
                    methods: ["password"],
                    password: PasswordCredential {
                        user: UserRef {
                            name: username.clone(),
                            domain: DomainRef {
                                name: user_domain_name.clone(),
                            },
                            password: password.clone(),
                        },
                    },
                },
                scope: ProjectScope {
                    project: ProjectRef {
                        name: project_name.clone(),
                        domain: DomainRef {
                            name: project_domain_name.clone(),
                        },
                    },
                },
            },
        }),
        AuthCredentials::ApplicationCredential { id, secret } => {
            AuthRequest::ApplicationCredential(AppCredRequest {
                auth: AppCredAuth {
                    identity: AppCredIdentity {
                        methods: ["application_credential"],
                        application_credential: AppCredRef {
                            id: id.clone(),
                            secret: secret.clone(),
                        },
                    },
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Response parsing (pure)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthResponse {
    token: TokenBody,
}

#[derive(Deserialize)]
struct TokenBody {
    #[serde(default)]
    expires_at: String,
    #[serde(default)]
    catalog: Vec<CatalogService>,
}

#[derive(Deserialize)]
struct CatalogService {
    #[serde(rename = "type", default)]
    service_type: String,
    #[serde(default)]
    endpoints: Vec<CatalogEndpoint>,
}

#[derive(Deserialize)]
struct CatalogEndpoint {
    #[serde(default)]
    interface: String,
    #[serde(default)]
    url: String,
}

/// Select the Nova base URL from a Keystone catalog: the `compute` service's
/// `public` endpoint, falling back to its first parseable endpoint
/// (`discovery.md` §3; no `region` field exists in the frozen config contract,
/// so the first `compute` service is used).
fn nova_base_from_catalog(catalog: &[CatalogService]) -> Option<Url> {
    for service in catalog {
        if service.service_type != "compute" {
            continue;
        }
        if let Some(endpoint) = service.endpoints.iter().find(|e| e.interface == "public") {
            if let Ok(url) = Url::parse(&endpoint.url) {
                return Some(url);
            }
        }
        for endpoint in &service.endpoints {
            if let Ok(url) = Url::parse(&endpoint.url) {
                return Some(url);
            }
        }
    }
    None
}

/// Parse a successful Keystone response into a [`Token`].
fn parse_token_response(subject_token: &str, body: &[u8]) -> Result<Token, AuthError> {
    let parsed: AuthResponse =
        serde_json::from_slice(body).map_err(|err| AuthError::Decode(err.to_string()))?;

    let expires_at = DateTime::parse_from_rfc3339(parsed.token.expires_at.trim())
        .map_err(|err| AuthError::Decode(format!("invalid token.expires_at: {err}")))?
        .with_timezone(&Utc);

    let nova_base = nova_base_from_catalog(&parsed.token.catalog).ok_or(AuthError::NoCatalog)?;

    Ok(Token {
        value: subject_token.to_owned(),
        expires_at,
        nova_base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password_credentials() -> AuthCredentials {
        AuthCredentials::Password {
            username: "proxy".into(),
            password: "s3cret".into(),
            project_name: "admin".into(),
            project_domain_name: "Default".into(),
            user_domain_name: "Users".into(),
        }
    }

    #[test]
    fn auth_endpoint_appends_auth_tokens() {
        let base = Url::parse("https://controller:5000/v3").unwrap();
        assert_eq!(
            auth_endpoint(&base).unwrap().as_str(),
            "https://controller:5000/v3/auth/tokens"
        );
        let trailing = Url::parse("https://controller:5000/v3/").unwrap();
        assert_eq!(
            auth_endpoint(&trailing).unwrap().as_str(),
            "https://controller:5000/v3/auth/tokens"
        );
    }

    #[test]
    fn password_body_matches_keystone_shape() {
        let value = serde_json::to_value(build_auth_body(&password_credentials())).unwrap();
        assert_eq!(value["auth"]["identity"]["methods"][0], "password");
        assert_eq!(
            value["auth"]["identity"]["password"]["user"]["name"],
            "proxy"
        );
        assert_eq!(
            value["auth"]["identity"]["password"]["user"]["password"],
            "s3cret"
        );
        assert_eq!(
            value["auth"]["identity"]["password"]["user"]["domain"]["name"],
            "Users"
        );
        assert_eq!(value["auth"]["scope"]["project"]["name"], "admin");
        assert_eq!(
            value["auth"]["scope"]["project"]["domain"]["name"],
            "Default"
        );
        assert!(
            value["auth"].get("application_credential").is_none(),
            "password body must not carry application_credential"
        );
    }

    #[test]
    fn application_credential_body_matches_keystone_shape() {
        let creds = AuthCredentials::ApplicationCredential {
            id: "abc".into(),
            secret: "topsecret".into(),
        };
        let value = serde_json::to_value(build_auth_body(&creds)).unwrap();
        assert_eq!(value["auth"]["identity"]["methods"][0], "application_credential");
        assert_eq!(
            value["auth"]["identity"]["application_credential"]["id"],
            "abc"
        );
        assert_eq!(
            value["auth"]["identity"]["application_credential"]["secret"],
            "topsecret"
        );
        assert!(
            value["auth"].get("scope").is_none(),
            "application_credential body must not scope a project"
        );
    }

    fn token_body(expires_at: &str, catalog_type: &str, endpoint: &str) -> Vec<u8> {
        format!(
            r#"{{"token":{{"expires_at":"{expires_at}",
               "catalog":[{{"type":"{catalog_type}","name":"nova",
               "endpoints":[{{"interface":"public","region":"RegionOne","url":"{endpoint}"}}]}}]}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parse_token_response_extracts_expiry_and_catalog() {
        let body = token_body("2030-01-02T03:04:05Z", "compute", "http://nova:8774/v2.1");
        let token = parse_token_response("tok-123", &body).unwrap();
        assert_eq!(token.value, "tok-123");
        assert_eq!(
            token.expires_at,
            DateTime::parse_from_rfc3339("2030-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(token.nova_base.as_str(), "http://nova:8774/v2.1");
    }

    #[test]
    fn parse_token_response_accepts_offset_timestamps() {
        let body = token_body("2030-01-02T03:04:05+00:00", "compute", "http://nova/v2.1");
        assert!(parse_token_response("t", &body).is_ok());
    }

    #[test]
    fn parse_token_response_without_compute_catalog_is_no_catalog() {
        let body = token_body("2030-01-02T03:04:05Z", "identity", "http://keystone/v3");
        assert_eq!(
            parse_token_response("t", &body).unwrap_err(),
            AuthError::NoCatalog
        );
    }

    #[test]
    fn parse_token_response_rejects_bad_expiry() {
        let body = token_body("not-a-date", "compute", "http://nova/v2.1");
        assert!(matches!(
            parse_token_response("t", &body).unwrap_err(),
            AuthError::Decode(_)
        ));
    }

    #[test]
    fn catalog_prefers_public_compute_endpoint() {
        let body = br#"{"token":{"expires_at":"2030-01-02T03:04:05Z","catalog":[
            {"type":"compute","endpoints":[
                {"interface":"internal","url":"http://internal/v2.1"},
                {"interface":"public","url":"http://public/v2.1"}]}]}}"#;
        let token = parse_token_response("t", body).unwrap();
        assert_eq!(token.nova_base.as_str(), "http://public/v2.1");
    }

    #[test]
    fn token_validity_honours_early_refresh_window() {
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let make = |secs: i64| Token {
            value: "t".into(),
            expires_at: now + chrono::Duration::seconds(secs),
            nova_base: Url::parse("http://nova/v2.1").unwrap(),
        };
        assert!(make(3600).is_valid_at(now));
        assert!(!make(30).is_valid_at(now), "inside 60s buffer -> stale");
        assert!(!make(60).is_valid_at(now), "exactly 60s -> stale");
        assert!(make(61).is_valid_at(now));
        assert!(!make(-1).is_valid_at(now));
    }

    #[test]
    fn token_debug_redacts_value() {
        let token = Token {
            value: "super-secret-token".into(),
            expires_at: Utc::now(),
            nova_base: Url::parse("http://nova/v2.1").unwrap(),
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-token"));
        assert!(rendered.contains("redacted"));
    }
}
