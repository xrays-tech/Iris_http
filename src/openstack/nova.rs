//! Nova (OpenStack Compute) API 2.1: response decoding and deterministic
//! address selection (`discovery.md` §4–§5).
//!
//! The decoding and selection functions are pure so the full behaviour matrix
//! (multi-NIC, floating-only, fixed-only, IPv6-only, multiple floating, empty)
//! is unit tested with committed fixtures; only [`query_servers`] touches the
//! network and is exercised via the mock integration harness.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use super::HttpClient;

/// One address entry inside `server.addresses.<network>[]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NovaAddress {
    /// The address literal (IPv4 or IPv6).
    #[serde(default)]
    pub addr: String,
    /// Address version reported by Nova (informational).
    #[serde(default)]
    pub version: u8,
    /// `OS-EXT-IPS:type` — `floating` or `fixed`.
    #[serde(rename = "OS-EXT-IPS:type", default)]
    pub ip_type: Option<String>,
}

/// A Nova server record (only the fields discovery needs).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NovaServer {
    /// Server id (UUID).
    #[serde(default)]
    pub id: String,
    /// Server name; compared byte-for-byte by [`exact_name_match`].
    #[serde(default)]
    pub name: String,
    /// Per-network address lists.
    #[serde(default)]
    pub addresses: HashMap<String, Vec<NovaAddress>>,
}

/// The floating/fixed addresses selected for a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AddressSelection {
    /// Preferred floating address (`None` when the instance has none).
    pub floating: Option<IpAddr>,
    /// Fixed fallback address.
    pub fixed: Option<IpAddr>,
}

impl AddressSelection {
    /// Whether neither a floating nor a fixed address was found.
    pub fn is_empty(&self) -> bool {
        self.floating.is_none() && self.fixed.is_none()
    }
}

/// Failures raised while talking to or decoding Nova.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NovaError {
    /// The response body was not a decodable server envelope.
    Decode(String),
    /// Nova returned `404` (no such server / no match).
    NotFound,
    /// Nova returned `401` (token rejected).
    Unauthorized,
    /// Nova returned `403` (insufficient privileges for `all_tenants`).
    Forbidden,
    /// The request exceeded the configured timeout.
    Timeout,
    /// Transport-level failure.
    Http(String),
    /// Any other non-2xx status.
    Status(u16),
}

impl fmt::Display for NovaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(msg) => write!(f, "nova response decode failed: {msg}"),
            Self::NotFound => write!(f, "nova returned 404"),
            Self::Unauthorized => write!(f, "nova returned 401"),
            Self::Forbidden => write!(f, "nova returned 403"),
            Self::Timeout => write!(f, "nova request timed out"),
            Self::Http(msg) => write!(f, "nova request failed: {msg}"),
            Self::Status(code) => write!(f, "nova returned HTTP {code}"),
        }
    }
}

impl std::error::Error for NovaError {}

/// Whether `s` is a canonical Nova server UUID (`8-4-4-4-12` hex digits,
/// case-insensitive).
///
/// This is the precedence discriminator for [`crate::openstack::Resolver::resolve`]:
/// a UUID-shaped string is always resolved through Nova show-by-id, so an
/// instance *name* that happens to match this shape is treated as a UUID.
pub fn is_instance_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(idx, &byte)| match idx {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

/// Wire envelope: Nova uses either `{"servers": [...]}` (list) or
/// `{"server": {...}}` (single) depending on the endpoint.
#[derive(Deserialize)]
struct ServerEnvelope {
    #[serde(default)]
    servers: Option<Vec<NovaServer>>,
    #[serde(default)]
    server: Option<NovaServer>,
}

/// Decode a Nova server payload, accepting both the list and single envelopes.
pub fn decode_server_list(body: &[u8]) -> Result<Vec<NovaServer>, NovaError> {
    let envelope: ServerEnvelope =
        serde_json::from_slice(body).map_err(|err| NovaError::Decode(err.to_string()))?;
    match (envelope.servers, envelope.server) {
        (Some(list), _) => Ok(list),
        (None, Some(one)) => Ok(vec![one]),
        (None, None) => Err(NovaError::Decode(
            "response contained neither 'servers' nor 'server'".to_owned(),
        )),
    }
}

/// Decode a Nova show-by-id payload (`{"server": {...}}`) into a single server.
///
/// Reuses the same envelope decoding as [`decode_server_list`] but requires the
/// single-server form; a list-only or malformed body is a [`NovaError::Decode`].
pub fn decode_server(body: &[u8]) -> Result<NovaServer, NovaError> {
    let envelope: ServerEnvelope =
        serde_json::from_slice(body).map_err(|err| NovaError::Decode(err.to_string()))?;
    envelope.server.ok_or_else(|| {
        NovaError::Decode("response contained no 'server' object".to_owned())
    })
}


/// Find the server whose `name` equals `name` byte-for-byte.
///
/// Unlike a prefix match this must not treat `web` as matching `web-1`
/// (`discovery.md` §4).
pub fn exact_name_match<'a>(name: &str, servers: &'a [NovaServer]) -> Option<&'a NovaServer> {
    servers.iter().find(|server| server.name == name)
}

/// Address family classification from `OS-EXT-IPS:type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpKind {
    Floating,
    Fixed,
}

impl IpKind {
    fn from_type(raw: Option<&str>) -> Self {
        match raw {
            Some("floating") => IpKind::Floating,
            _ => IpKind::Fixed,
        }
    }
}

/// Flatten a server's addresses into a deterministic order: networks sorted by
/// name, entries in their wire order. This makes "first floating wins" stable.
fn ordered_addresses(server: &NovaServer) -> Vec<(IpAddr, IpKind)> {
    let mut networks: Vec<&String> = server.addresses.keys().collect();
    networks.sort();

    let mut out = Vec::new();
    for network in networks {
        if let Some(entries) = server.addresses.get(network) {
            for entry in entries {
                if let Ok(ip) = IpAddr::from_str(entry.addr.trim()) {
                    out.push((ip, IpKind::from_type(entry.ip_type.as_deref())));
                }
            }
        }
    }
    out
}

/// Select the upstream addresses for a server (`discovery.md` §5):
/// IPv4 is preferred globally; only when no IPv4 exists are IPv6 addresses
/// considered. Within a family, `floating` wins over `fixed`, and the first
/// matching entry in deterministic order is used.
pub fn select_address(server: &NovaServer) -> AddressSelection {
    let entries = ordered_addresses(server);

    let floating_v4 = first_of(&entries, true, IpKind::Floating);
    let fixed_v4 = first_of(&entries, true, IpKind::Fixed);
    if floating_v4.is_some() || fixed_v4.is_some() {
        return AddressSelection {
            floating: floating_v4,
            fixed: fixed_v4,
        };
    }

    AddressSelection {
        floating: first_of(&entries, false, IpKind::Floating),
        fixed: first_of(&entries, false, IpKind::Fixed),
    }
}

fn first_of(entries: &[(IpAddr, IpKind)], ipv4: bool, kind: IpKind) -> Option<IpAddr> {
    entries
        .iter()
        .find(|(ip, k)| ip.is_ipv4() == ipv4 && *k == kind)
        .map(|(ip, _)| *ip)
}

/// Map a Nova HTTP status onto the [`NovaError`] taxonomy (2xx = `Ok`).
fn check_status(status: u16) -> Result<(), NovaError> {
    match status {
        401 => Err(NovaError::Unauthorized),
        403 => Err(NovaError::Forbidden),
        404 => Err(NovaError::NotFound),
        code if !(200..300).contains(&code) => Err(NovaError::Status(code)),
        _ => Ok(()),
    }
}

/// `GET url` with the standard Nova headers, mapping transport failures and
/// non-2xx statuses; returns the raw body bytes.
async fn get_json(
    client: &HttpClient,
    url: Url,
    token: &str,
    timeout: Duration,
) -> Result<Vec<u8>, NovaError> {
    let response = client
        .inner()
        .get(url)
        .header("X-Auth-Token", token)
        .header("OpenStack-API-Version", "compute 2.1")
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                NovaError::Timeout
            } else {
                NovaError::Http(err.to_string())
            }
        })?;

    check_status(response.status().as_u16())?;

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| NovaError::Http(err.to_string()))
}

/// Query Nova for servers matching `instance` (`GET /servers?name=...`).
///
/// `all_tenants=true` is appended when configured, so an admin principal can
/// find instances across projects (`discovery.md` §4).
pub async fn query_servers(
    client: &HttpClient,
    token: &str,
    nova_base: &Url,
    instance: &str,
    all_tenants: bool,
    timeout: Duration,
) -> Result<Vec<NovaServer>, NovaError> {
    let mut url = nova_base.clone();
    url.path_segments_mut()
        .map_err(|_| NovaError::Http("nova base URL cannot be a base URL".to_owned()))?
        .pop_if_empty()
        .push("servers");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("name", instance);
        if all_tenants {
            query.append_pair("all_tenants", "true");
        }
    }

    let bytes = get_json(client, url, token, timeout).await?;
    decode_server_list(&bytes)
}

/// Query Nova for one server by id (`GET /servers/{uuid}`, show-by-id).
///
/// Used for UUID-shaped instances; a missing id is [`NovaError::NotFound`].
pub(crate) async fn query_server_by_id(
    client: &HttpClient,
    token: &str,
    nova_base: &Url,
    uuid: &str,
    timeout: Duration,
) -> Result<NovaServer, NovaError> {
    let mut url = nova_base.clone();
    url.path_segments_mut()
        .map_err(|_| NovaError::Http("nova base URL cannot be a base URL".to_owned()))?
        .pop_if_empty()
        .push("servers")
        .push(uuid);

    let bytes = get_json(client, url, token, timeout).await?;
    decode_server(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<NovaServer> {
        let body = match name {
            "multi-nic" => include_str!("../../tests/fixtures/openstack/servers/multi-nic.json"),
            "floating-only" => {
                include_str!("../../tests/fixtures/openstack/servers/floating-only.json")
            }
            "fixed-only" => {
                include_str!("../../tests/fixtures/openstack/servers/fixed-only.json")
            }
            "ipv6-only" => include_str!("../../tests/fixtures/openstack/servers/ipv6-only.json"),
            "multiple-floating" => {
                include_str!("../../tests/fixtures/openstack/servers/multiple-floating.json")
            }
            "empty-addresses" => {
                include_str!("../../tests/fixtures/openstack/servers/empty-addresses.json")
            }
            "single-envelope" => {
                include_str!("../../tests/fixtures/openstack/servers/single-envelope.json")
            }
            "empty-list" => include_str!("../../tests/fixtures/openstack/servers/empty-list.json"),
            other => panic!("unknown fixture {other}"),
        };
        decode_server_list(body.as_bytes()).expect("fixture decodes")
    }

    fn first(name: &str) -> NovaServer {
        fixture(name).into_iter().next().expect("fixture has a server")
    }

    #[test]
    fn decodes_list_envelope() {
        let servers = fixture("multi-nic");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "multi-nic");
    }

    #[test]
    fn decodes_single_envelope() {
        let servers = fixture("single-envelope");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "single-envelope");
    }

    #[test]
    fn decodes_empty_list() {
        assert!(fixture("empty-list").is_empty());
    }

    #[test]
    fn decode_rejects_invalid_json() {
        assert!(matches!(
            decode_server_list(b"{not json").unwrap_err(),
            NovaError::Decode(_)
        ));
    }

    #[test]
    fn decode_rejects_missing_envelope() {
        assert!(matches!(
            decode_server_list(br#"{"other": 1}"#).unwrap_err(),
            NovaError::Decode(_)
        ));
    }

    #[test]
    fn is_instance_uuid_accepts_canonical_shapes() {
        assert!(is_instance_uuid("11111111-1111-1111-1111-111111111111"));
        assert!(is_instance_uuid("abcdef01-2345-6789-abcd-ef0123456789"));
        assert!(is_instance_uuid("ABCDEF01-2345-6789-ABCD-EF0123456789"));
    }

    #[test]
    fn is_instance_uuid_rejects_non_uuids() {
        for bad in [
            "",
            "web",
            "web-1",
            "11111111-1111-1111-1111-11111111111",   // one hex short
            "11111111-1111-1111-1111-1111111111111", // one hex long
            "11111111111111111111111111111111",      // no separators
            "11111111_1111_1111_1111_111111111111",  // wrong separator
            "g1111111-1111-1111-1111-111111111111",  // non-hex digit
            "11111111-1111-1111-1111-11111111111z",  // non-hex tail
            "deadbeef",
        ] {
            assert!(!is_instance_uuid(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn decode_server_reads_single_envelope() {
        let body = include_bytes!(
            "../../tests/fixtures/openstack/servers/11111111-1111-1111-1111-111111111111.json"
        );
        let server = decode_server(body).expect("single envelope decodes");
        assert_eq!(server.id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(server.name, "multi-nic");
        let selection = select_address(&server);
        assert_eq!(selection.floating, Some(addr("203.0.113.10")));
        assert_eq!(selection.fixed, Some(addr("192.168.1.10")));
    }

    #[test]
    fn decode_server_rejects_malformed_and_list_only() {
        assert!(matches!(
            decode_server(b"{not json").unwrap_err(),
            NovaError::Decode(_)
        ));
        assert!(matches!(decode_server(b"").unwrap_err(), NovaError::Decode(_)));
        assert!(matches!(
            decode_server(br#"{"servers": []}"#).unwrap_err(),
            NovaError::Decode(_)
        ));
    }

    #[test]
    fn exact_name_match_is_not_prefix() {
        let servers = vec![
            NovaServer {
                id: "1".into(),
                name: "web-1".into(),
                addresses: HashMap::new(),
            },
            NovaServer {
                id: "2".into(),
                name: "web".into(),
                addresses: HashMap::new(),
            },
        ];
        assert_eq!(exact_name_match("web", &servers).unwrap().id, "2");
        assert_eq!(exact_name_match("web-1", &servers).unwrap().id, "1");
        assert!(exact_name_match("we", &servers).is_none());
        assert!(exact_name_match("web-10", &servers).is_none());
    }

    #[test]
    fn exact_name_match_handles_duplicate_names_first_wins() {
        let servers = vec![
            NovaServer {
                id: "a".into(),
                name: "dup".into(),
                addresses: HashMap::new(),
            },
            NovaServer {
                id: "b".into(),
                name: "dup".into(),
                addresses: HashMap::new(),
            },
        ];
        assert_eq!(exact_name_match("dup", &servers).unwrap().id, "a");
    }

    fn addr(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn select_address_matrix() {
        // (fixture, expected floating, expected fixed)
        let cases = [
            (
                "multi-nic",
                Some(addr("203.0.113.10")),
                Some(addr("192.168.1.10")),
            ),
            ("floating-only", Some(addr("203.0.113.11")), None),
            ("fixed-only", None, Some(addr("192.168.1.20"))),
            (
                "ipv6-only",
                Some(addr("fd00::20")),
                Some(addr("fd00::10")),
            ),
            (
                "multiple-floating",
                Some(addr("203.0.113.20")), // lexicographically first network (net-a)
                None,
            ),
            ("empty-addresses", None, None),
        ];
        for (name, floating, fixed) in cases {
            let selection = select_address(&first(name));
            assert_eq!(selection.floating, floating, "fixture {name} floating");
            assert_eq!(selection.fixed, fixed, "fixture {name} fixed");
            assert_eq!(
                selection.is_empty(),
                floating.is_none() && fixed.is_none(),
                "fixture {name} is_empty"
            );
        }
    }

    #[test]
    fn ipv4_preferred_over_ipv6_when_mixed() {
        let body = br#"{"servers":[{"name":"s","addresses":{"net":[
            {"version":6,"addr":"fd00::1","OS-EXT-IPS:type":"floating"},
            {"version":4,"addr":"10.0.0.1","OS-EXT-IPS:type":"fixed"}]}}]}"#;
        let server = decode_server_list(body).unwrap().remove(0);
        let selection = select_address(&server);
        assert_eq!(selection.fixed, Some(addr("10.0.0.1")));
        assert_eq!(selection.floating, None, "IPv6 ignored while IPv4 exists");
    }

    #[test]
    fn invalid_address_literals_are_skipped() {
        let body = br#"{"servers":[{"name":"s","addresses":{"net":[
            {"version":4,"addr":"not-an-ip","OS-EXT-IPS:type":"floating"}]}}]}"#;
        let server = decode_server_list(body).unwrap().remove(0);
        assert!(select_address(&server).is_empty());
    }

    #[test]
    fn missing_type_is_treated_as_fixed() {
        let body = br#"{"servers":[{"name":"s","addresses":{"net":[
            {"version":4,"addr":"10.0.0.9"}]}}]}"#;
        let server = decode_server_list(body).unwrap().remove(0);
        let selection = select_address(&server);
        assert_eq!(selection.fixed, Some(addr("10.0.0.9")));
        assert_eq!(selection.floating, None);
    }
}
