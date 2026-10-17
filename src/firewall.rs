//! P6 — OpenStack firewall ACL.
//!
//! Parses a directory of `<instance>.role` YAML rule files into a
//! [`FirewallTable`] and evaluates per-request access with a **deny-by-default**
//! decision (see `dev-docs/firewall.md` §2–§5).
//!
//! The module is pure and side-effect free: [`build_firewall`] reads the
//! directory and returns either a complete table or an error. A single bad file
//! fails the whole build (fail-fast, matching the config-loading philosophy);
//! reload (P8) rebuilds the table and swaps it into shared state wholesale.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;

use ipnet::IpNet;
use serde::Deserialize;

use crate::config::ConfigError;

/// Upper bound on entries in a single `whitelist`/`blacklist` (`firewall.md` §5).
pub const MAX_IPS_PER_LIST: usize = 10_000;

/// Per-instance rules: ports open for proxying plus client ACLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRules {
    /// Ports the instance accepts, validated to `1..=65535`, de-duplicated and
    /// sorted. Empty means no port is open.
    pub allowed_ports: Vec<u16>,
    /// Client allow-list. Empty means the whitelist is not enabled.
    pub whitelist: Vec<IpNet>,
    /// Client deny-list. Any match denies the request (highest priority).
    pub blacklist: Vec<IpNet>,
}

/// Immutable firewall snapshot keyed by instance name (case-sensitive).
#[derive(Debug, Default)]
pub struct FirewallTable {
    /// Rules by instance name (the `<instance>.role` file stem).
    pub roles: HashMap<String, RoleRules>,
}

/// Why a request was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclReason {
    /// No `.role` file exists for the target instance (default deny).
    NoRole,
    /// The target port is not in the instance's `allowed_ports`.
    PortNotAllowed(u16),
    /// The client IP matched the blacklist.
    Blacklisted,
    /// A whitelist is configured and the client IP is not in it.
    NotWhitelisted,
}

/// The outcome of an ACL evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclVerdict {
    /// The request may proceed.
    Allow,
    /// The request is forbidden for the given reason.
    Deny(AclReason),
}

/// Raw YAML shape of a `.role` file. Missing lists default to empty.
#[derive(Debug, Deserialize)]
struct RawRole {
    #[serde(default)]
    allowed_ports: Vec<u16>,
    #[serde(default)]
    whitelist: Vec<String>,
    #[serde(default)]
    blacklist: Vec<String>,
}

/// Load and validate every `*.role` file under `dir`.
///
/// Only files whose extension is exactly `role` are read; the file stem is the
/// instance name (case-sensitive). Any single malformed file fails the whole
/// build. Entries are processed in sorted path order for deterministic errors.
pub fn build_firewall(dir: &Path) -> Result<FirewallTable, ConfigError> {
    if !dir.is_dir() {
        return Err(ConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "firewall directory '{}' does not exist or is not a directory",
                dir.display()
            ),
        )));
    }

    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|err| {
        ConfigError::Io(std::io::Error::new(
            err.kind(),
            format!(
                "failed to read firewall directory '{}': {err}",
                dir.display()
            ),
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            ConfigError::Io(std::io::Error::new(
                err.kind(),
                format!("failed to read an entry of '{}': {err}", dir.display()),
            ))
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Hidden files (leading dot) are ignored, as are non-`.role` files.
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("role") {
            continue;
        }
        files.push(path);
    }
    files.sort();

    let mut roles = HashMap::with_capacity(files.len());
    for path in &files {
        // A hidden/empty/dotfile stem cannot name an instance; skip it.
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        roles.insert(stem.to_owned(), parse_role_file(path)?);
    }

    Ok(FirewallTable { roles })
}

/// Parse and validate a single `.role` file.
fn parse_role_file(path: &Path) -> Result<RoleRules, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|err| ConfigError::Parse {
        file: path.to_path_buf(),
        msg: format!("failed to read file: {err}"),
    })?;

    // An empty file means an empty rule set: no ports open, no ACLs.
    if text.trim().is_empty() {
        return Ok(RoleRules {
            allowed_ports: Vec::new(),
            whitelist: Vec::new(),
            blacklist: Vec::new(),
        });
    }

    let raw: RawRole = serde_yaml::from_str(&text).map_err(|err| ConfigError::Parse {
        file: path.to_path_buf(),
        msg: err.to_string(),
    })?;

    let mut allowed_ports = raw.allowed_ports;
    if allowed_ports.contains(&0) {
        return Err(ConfigError::Parse {
            file: path.to_path_buf(),
            msg: "allowed_ports contains invalid port 0 (valid range 1..=65535)".to_owned(),
        });
    }
    allowed_ports.sort_unstable();
    allowed_ports.dedup();

    let whitelist = parse_networks(&raw.whitelist, path, "whitelist")?;
    let blacklist = parse_networks(&raw.blacklist, path, "blacklist")?;

    Ok(RoleRules {
        allowed_ports,
        whitelist,
        blacklist,
    })
}

/// Parse and validate one CIDR/IP list, enforcing the per-list entry cap.
fn parse_networks(
    entries: &[String],
    file: &Path,
    field: &str,
) -> Result<Vec<IpNet>, ConfigError> {
    if entries.len() > MAX_IPS_PER_LIST {
        return Err(ConfigError::Parse {
            file: file.to_path_buf(),
            msg: format!(
                "{field} has {} entries, exceeding the limit of {MAX_IPS_PER_LIST}",
                entries.len()
            ),
        });
    }

    let mut nets = Vec::with_capacity(entries.len());
    for entry in entries {
        let net = parse_network(entry).ok_or_else(|| ConfigError::Parse {
            file: file.to_path_buf(),
            msg: format!("invalid IP/CIDR in {field}: '{entry}'"),
        })?;
        nets.push(net);
    }
    Ok(nets)
}

/// Parse a single network entry: a true CIDR, or a plain IP normalised to
/// `/32` (IPv4) or `/128` (IPv6).
fn parse_network(entry: &str) -> Option<IpNet> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<IpNet>()
        .ok()
        .or_else(|| trimmed.parse::<IpAddr>().ok().map(IpNet::from))
}

impl FirewallTable {
    /// Evaluate the ACL for `instance` on `port` from `client`.
    ///
    /// Order matters and mirrors `firewall.md` §3: (1) no role denies, (2) port
    /// must be open, (3) blacklist wins, (4) a non-empty whitelist must contain
    /// the client, (5) otherwise allow.
    pub fn check(&self, instance: &str, port: u16, client: IpAddr) -> AclVerdict {
        let Some(rules) = self.roles.get(instance) else {
            return AclVerdict::Deny(AclReason::NoRole);
        };
        if !rules.allowed_ports.contains(&port) {
            return AclVerdict::Deny(AclReason::PortNotAllowed(port));
        }
        if rules.blacklist.iter().any(|net| net.contains(&client)) {
            return AclVerdict::Deny(AclReason::Blacklisted);
        }
        if !rules.whitelist.is_empty() && !rules.whitelist.iter().any(|net| net.contains(&client))
        {
            return AclVerdict::Deny(AclReason::NotWhitelisted);
        }
        AclVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP parses")
    }

    fn net(s: &str) -> IpNet {
        s.parse().expect("test CIDR parses")
    }

    fn rules(ports: &[u16], whitelist: &[&str], blacklist: &[&str]) -> RoleRules {
        RoleRules {
            allowed_ports: ports.to_vec(),
            whitelist: whitelist
                .iter()
                .map(|s| parse_network(s).expect("test CIDR/IP parses"))
                .collect(),
            blacklist: blacklist
                .iter()
                .map(|s| parse_network(s).expect("test CIDR/IP parses"))
                .collect(),
        }
    }

    fn table(instance: &str, rules: RoleRules) -> FirewallTable {
        let mut roles = HashMap::new();
        roles.insert(instance.to_owned(), rules);
        FirewallTable { roles }
    }

    // ----- decision order (plan/09 table-driven list) -----

    #[test]
    fn no_role_denies() {
        let table = FirewallTable::default();
        assert_eq!(
            table.check("missing", 80, ip("203.0.113.1")),
            AclVerdict::Deny(AclReason::NoRole)
        );
    }

    #[test]
    fn port_not_allowed_denies() {
        let table = table("INS", rules(&[80, 443], &[], &[]));
        assert_eq!(
            table.check("INS", 8080, ip("203.0.113.1")),
            AclVerdict::Deny(AclReason::PortNotAllowed(8080))
        );
    }

    #[test]
    fn empty_allowed_ports_denies_every_port() {
        let table = table("INS", rules(&[], &[], &[]));
        assert_eq!(
            table.check("INS", 80, ip("203.0.113.1")),
            AclVerdict::Deny(AclReason::PortNotAllowed(80))
        );
        assert_eq!(
            table.check("INS", 65535, ip("203.0.113.1")),
            AclVerdict::Deny(AclReason::PortNotAllowed(65535))
        );
    }

    #[test]
    fn empty_whitelist_allows_non_blacklisted_client() {
        let table = table("INS", rules(&[80], &[], &[]));
        assert_eq!(table.check("INS", 80, ip("203.0.113.1")), AclVerdict::Allow);
    }

    #[test]
    fn whitelist_non_empty_allows_member() {
        let table = table("INS", rules(&[80], &["10.0.0.0/8"], &[]));
        assert_eq!(table.check("INS", 80, ip("10.1.2.3")), AclVerdict::Allow);
    }

    #[test]
    fn whitelist_non_empty_denies_non_member() {
        let table = table("INS", rules(&[80], &["10.0.0.0/8"], &[]));
        assert_eq!(
            table.check("INS", 80, ip("192.168.1.1")),
            AclVerdict::Deny(AclReason::NotWhitelisted)
        );
    }

    #[test]
    fn blacklisted_client_denied() {
        let table = table("INS", rules(&[80], &[], &["203.0.113.0/24"]));
        assert_eq!(
            table.check("INS", 80, ip("203.0.113.7")),
            AclVerdict::Deny(AclReason::Blacklisted)
        );
    }

    #[test]
    fn blacklist_beats_whitelist() {
        let table = table(
            "INS",
            rules(&[80], &["10.0.0.0/8"], &["10.1.0.0/16"]),
        );
        assert_eq!(
            table.check("INS", 80, ip("10.1.2.3")),
            AclVerdict::Deny(AclReason::Blacklisted)
        );
    }

    #[test]
    fn port_check_precedes_blacklist() {
        let table = table("INS", rules(&[80], &[], &["203.0.113.0/24"]));
        assert_eq!(
            table.check("INS", 8080, ip("203.0.113.7")),
            AclVerdict::Deny(AclReason::PortNotAllowed(8080))
        );
    }

    #[test]
    fn blacklisted_boundary_network_and_broadcast() {
        let table = table("INS", rules(&[80], &[], &["192.168.1.0/24"]));
        assert_eq!(
            table.check("INS", 80, ip("192.168.1.0")),
            AclVerdict::Deny(AclReason::Blacklisted)
        );
        assert_eq!(
            table.check("INS", 80, ip("192.168.1.255")),
            AclVerdict::Deny(AclReason::Blacklisted)
        );
        assert_eq!(table.check("INS", 80, ip("192.168.2.0")), AclVerdict::Allow);
    }

    // ----- CIDR / IPv6 boundaries -----

    #[test]
    fn ipv4_cidr10_boundaries() {
        let table = table("INS", rules(&[80], &["10.0.0.0/8"], &[]));
        assert_eq!(table.check("INS", 80, ip("10.0.0.0")), AclVerdict::Allow);
        assert_eq!(
            table.check("INS", 80, ip("10.255.255.255")),
            AclVerdict::Allow
        );
        assert_eq!(
            table.check("INS", 80, ip("11.0.0.0")),
            AclVerdict::Deny(AclReason::NotWhitelisted)
        );
    }

    #[test]
    fn ipv4_cidr172_16_boundaries() {
        let table = table("INS", rules(&[80], &["172.16.0.0/12"], &[]));
        assert_eq!(table.check("INS", 80, ip("172.16.0.0")), AclVerdict::Allow);
        assert_eq!(
            table.check("INS", 80, ip("172.31.255.255")),
            AclVerdict::Allow
        );
        assert_eq!(
            table.check("INS", 80, ip("172.32.0.0")),
            AclVerdict::Deny(AclReason::NotWhitelisted)
        );
        assert_eq!(
            table.check("INS", 80, ip("172.15.255.255")),
            AclVerdict::Deny(AclReason::NotWhitelisted)
        );
    }

    #[test]
    fn ipv6_host_route_boundaries() {
        let table = table("INS", rules(&[80], &["::1/128"], &[]));
        assert_eq!(table.check("INS", 80, ip("::1")), AclVerdict::Allow);
        assert_eq!(
            table.check("INS", 80, ip("::2")),
            AclVerdict::Deny(AclReason::NotWhitelisted)
        );
    }

    // ----- plain IP -> /32 / /128 normalisation -----

    #[test]
    fn plain_ipv4_normalises_to_slash32() {
        assert_eq!(parse_network("203.0.113.1"), Some(net("203.0.113.1/32")));
        let table = table("INS", rules(&[80], &[], &["203.0.113.1"]));
        assert_eq!(
            table.check("INS", 80, ip("203.0.113.1")),
            AclVerdict::Deny(AclReason::Blacklisted)
        );
        assert_eq!(table.check("INS", 80, ip("203.0.113.2")), AclVerdict::Allow);
    }

    #[test]
    fn plain_ipv6_normalises_to_slash128() {
        assert_eq!(parse_network("2001:db8::1"), Some(net("2001:db8::1/128")));
    }

    #[test]
    fn invalid_network_entries_rejected() {
        assert_eq!(parse_network(""), None);
        assert_eq!(parse_network("   "), None);
        assert_eq!(parse_network("not-an-ip"), None);
        assert_eq!(parse_network("10.0.0.0/33"), None);
        assert_eq!(parse_network("300.1.1.1"), None);
        assert_eq!(parse_network("::1/129"), None);
    }

    // ----- parsing / validation via real files -----

    fn build_in(dir: &Path, files: &[(&str, &str)]) -> Result<FirewallTable, ConfigError> {
        for (name, body) in files {
            std::fs::write(dir.join(name), body).expect("write test file");
        }
        build_firewall(dir)
    }

    #[test]
    fn build_missing_directory_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(matches!(
            build_firewall(&missing),
            Err(ConfigError::Io(_))
        ));
    }

    #[test]
    fn build_empty_directory_yields_empty_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_firewall(dir.path()).expect("empty dir builds");
        assert!(table.roles.is_empty());
    }

    #[test]
    fn build_parses_ports_and_dedups_sorts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_in(
            dir.path(),
            &[("INS.role", "allowed_ports: [443, 80, 443, 80]\n")],
        )
        .expect("valid role builds");
        assert_eq!(table.roles["INS"].allowed_ports, vec![80, 443]);
    }

    #[test]
    fn build_ignores_non_role_and_hidden_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_in(
            dir.path(),
            &[
                ("INS.role", "allowed_ports: [80]\n"),
                ("notes.txt", "allowed_ports: [1]\n"),
                ("other.yaml", "allowed_ports: [1]\n"),
                (".hidden.role", "allowed_ports: [1]\n"),
            ],
        )
        .expect("only INS.role is read");
        assert_eq!(table.roles.len(), 1);
        assert!(table.roles.contains_key("INS"));
    }

    #[test]
    fn instance_lookup_is_case_sensitive() {
        // Only one file is used so the test is independent of the host
        // filesystem's case sensitivity; lookups are still exact-match.
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_in(dir.path(), &[("INS.role", "allowed_ports: [80]\n")])
            .expect("role builds");
        assert!(table.roles.contains_key("INS"));
        assert!(!table.roles.contains_key("ins"));
        assert_eq!(table.check("INS", 80, ip("1.2.3.4")), AclVerdict::Allow);
        assert_eq!(
            table.check("ins", 80, ip("1.2.3.4")),
            AclVerdict::Deny(AclReason::NoRole)
        );
    }

    #[test]
    fn build_defaults_missing_lists_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_in(dir.path(), &[("INS.role", "allowed_ports: [80]\n")])
            .expect("missing lists default");
        let rules = &table.roles["INS"];
        assert!(rules.whitelist.is_empty());
        assert!(rules.blacklist.is_empty());
    }

    #[test]
    fn build_empty_file_is_deny_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = build_in(dir.path(), &[("INS.role", "\n  \n")]).expect("empty role builds");
        assert!(table.roles["INS"].allowed_ports.is_empty());
        assert_eq!(
            table.check("INS", 80, ip("1.2.3.4")),
            AclVerdict::Deny(AclReason::PortNotAllowed(80))
        );
    }

    #[test]
    fn build_bad_yaml_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = build_in(dir.path(), &[("INS.role", "allowed_ports: [80\n")])
            .expect_err("bad yaml fails");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn build_type_error_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = build_in(dir.path(), &[("INS.role", "allowed_ports:\n  - eighty\n")])
            .expect_err("type error fails");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn build_port_out_of_range_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 70000 overflows u16 -> deserialisation type error.
        assert!(matches!(
            build_in(dir.path(), &[("INS.role", "allowed_ports: [70000]\n")]),
            Err(ConfigError::Parse { .. })
        ));
        // 0 is in range for u16 but semantically invalid.
        assert!(matches!(
            build_in(dir.path(), &[("INS.role", "allowed_ports: [0]\n")]),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn build_invalid_cidr_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = build_in(
            dir.path(),
            &[("INS.role", "allowed_ports: [80]\nblacklist:\n  - 10.0.0.0/33\n")],
        )
        .expect_err("invalid CIDR fails");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn build_over_limit_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut body = String::from("allowed_ports: [80]\nblacklist:\n");
        for i in 0..=MAX_IPS_PER_LIST {
            body.push_str(&format!("  - 10.0.{}.{}\n", i / 256 % 256, i % 256));
        }
        let err = build_in(dir.path(), &[("INS.role", &body)]).expect_err("over-limit fails");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn build_at_limit_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut body = String::from("allowed_ports: [80]\nblacklist:\n");
        for i in 0..MAX_IPS_PER_LIST {
            body.push_str(&format!("  - 10.0.{}.{}\n", i / 256 % 256, i % 256));
        }
        let table = build_in(dir.path(), &[("INS.role", &body)]).expect("at-limit builds");
        assert_eq!(table.roles["INS"].blacklist.len(), MAX_IPS_PER_LIST);
    }

    #[test]
    fn build_single_bad_file_fails_whole_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = build_in(
            dir.path(),
            &[
                ("GOOD.role", "allowed_ports: [80]\n"),
                ("BAD.role", "blacklist:\n  - not-an-ip\n"),
            ],
        )
        .expect_err("one bad file fails the build");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }
}
