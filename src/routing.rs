//! Routing table and host matching (pure functions).
//!
//! Lands the real types fixed by `plan/02-contracts.md` and the matching rules
//! of `config.md` §8. Everything here is side-effect-free: a [`RoutingTable`] is
//! built once from a loaded [`Config`] and then answers per-request lookups.
//!
//! Matching order (`config.md` §8) is *exact first, then wildcard*:
//! 1. [`RoutingTable::lookup`] consults the exact-domain map (forward sites).
//! 2. Otherwise it scans the openstack `{instance}-{port}.{base_domain}`
//!    patterns; the first match wins (patterns cannot overlap — P1 guarantees
//!    that `base_domain`s are mutually non-nested).
//! 3. No match yields `None`, which callers turn into a 404.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::config::Config;

/// Which configured site an entry refers to.
///
/// The index is into the matching `Vec` of [`Config`]. The exact map is built
/// from `forward` sites only (`config.md` §8: an openstack `base_domain` never
/// participates in exact matching), but the enum mirrors the pinned contract so
/// openstack entries can be represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteKind {
    /// Index into [`Config::forward`].
    Forward(usize),
    /// Index into [`Config::openstack`].
    OpenStack(usize),
}

/// A resolved routing decision for a request host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteTarget {
    /// Forward to the `forward` site at the given [`Config::forward`] index.
    Forward(usize),
    /// Route to a specific instance `{instance}:{port}` of the `openstack` site
    /// at the given [`Config::openstack`] index.
    OpenStack {
        /// Index into [`Config::openstack`].
        site_idx: usize,
        /// Instance name extracted from the host prefix.
        instance: String,
        /// Target port extracted from the host prefix (`1..=65535`).
        port: u16,
    },
}

/// A compiled wildcard matcher for one openstack site's
/// `{instance}-{port}.{base_domain}` host form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStackPattern {
    /// Index into [`Config::openstack`].
    pub site_idx: usize,
    /// The site's root domain (already normalised: lowercase, no trailing dot).
    pub base_domain: String,
}

impl OpenStackPattern {
    /// Match a pre-normalised host against this pattern.
    ///
    /// Equivalent to `config.md` §8's regular expression
    /// `^([A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?)-([0-9]{1,5})\.{base}$`.
    fn matches(&self, host: &str) -> Option<(String, u16)> {
        parse_instance_host(host, &self.base_domain)
    }
}

/// The compiled routing index: exact domains plus openstack wildcard patterns.
pub struct RoutingTable {
    exact: HashMap<String, SiteKind>,
    openstack_patterns: Vec<OpenStackPattern>,
}

/// Errors raised while building a [`RoutingTable`].
///
/// P1 global validation already guarantees that forward domains and openstack
/// base domains are unique, so a well-formed [`Config`] never produces one of
/// these; the variants exist purely for defensive reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    /// A forward domain was configured more than once.
    DuplicateForwardDomain(String),
    /// An openstack `base_domain` was configured more than once.
    DuplicateBaseDomain(String),
}

impl fmt::Display for RoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateForwardDomain(domain) => {
                write!(f, "duplicate forward domain '{domain}' in routing table")
            }
            Self::DuplicateBaseDomain(base) => {
                write!(f, "duplicate openstack base_domain '{base}' in routing table")
            }
        }
    }
}

impl std::error::Error for RoutingError {}

impl RoutingTable {
    /// Resolve `host` to a [`RouteTarget`].
    ///
    /// `host` MUST already be normalised with [`normalize_host`] (lowercased,
    /// trailing dot stripped, default port removed). Matching order follows
    /// `config.md` §8: exact domains win, then the first matching openstack
    /// wildcard pattern; otherwise `None` (the caller returns 404).
    pub fn lookup(&self, host: &str) -> Option<RouteTarget> {
        match self.exact.get(host) {
            Some(SiteKind::Forward(idx)) => return Some(RouteTarget::Forward(*idx)),
            // An openstack `base_domain` never enters the exact table
            // (`config.md` §8); if one somehow did it is not routable on its
            // own, so fall through to wildcard matching.
            Some(SiteKind::OpenStack(_)) => {}
            None => {}
        }

        for pattern in &self.openstack_patterns {
            if let Some((instance, port)) = pattern.matches(host) {
                return Some(RouteTarget::OpenStack {
                    site_idx: pattern.site_idx,
                    instance,
                    port,
                });
            }
        }

        None
    }

    /// Build a routing table from a validated [`Config`].
    ///
    /// `config.md` §6 (enforced by P1) already guarantees that forward domains
    /// and openstack `base_domain`s are unique and mutually non-overlapping, so
    /// no conflict can occur here. Duplicates are nevertheless skipped
    /// defensively (first occurrence wins) so a malformed `Config` cannot
    /// produce an ambiguous table.
    pub fn try_from(cfg: &Config) -> Result<Self, RoutingError> {
        let mut exact: HashMap<String, SiteKind> = HashMap::with_capacity(cfg.forward.len());
        for (idx, site) in cfg.forward.iter().enumerate() {
            exact
                .entry(site.domain.clone())
                .or_insert(SiteKind::Forward(idx));
        }

        let mut seen_bases: HashSet<&str> = HashSet::with_capacity(cfg.openstack.len());
        let mut openstack_patterns = Vec::with_capacity(cfg.openstack.len());
        for (site_idx, site) in cfg.openstack.iter().enumerate() {
            if !seen_bases.insert(site.base_domain.as_str()) {
                continue;
            }
            openstack_patterns.push(OpenStackPattern {
                site_idx,
                base_domain: site.base_domain.clone(),
            });
        }

        Ok(Self {
            exact,
            openstack_patterns,
        })
    }
}

/// Normalise a request `Host` header / TLS SNI value (`config.md` §8).
///
/// Steps, in order (chosen so that a default port *and* a trailing dot are both
/// stripped, e.g. `Example.COM.:443` -> `example.com`):
/// 1. ASCII-lowercase.
/// 2. Strip a default port `:80` or `:443` (non-default ports are kept).
/// 3. Strip a single trailing `.`.
///
/// Empty input (or input that normalises to nothing) yields an empty string.
pub fn normalize_host(host: &str) -> String {
    let mut h = host.to_ascii_lowercase();
    if h.ends_with(":80") {
        h.truncate(h.len() - 3);
    } else if h.ends_with(":443") {
        h.truncate(h.len() - 4);
    }
    if h.ends_with('.') {
        h.pop();
    }
    h
}

/// Parse a normalised `Host` of the form `{prefix}.{base_domain}` into
/// `(instance, port)`.
///
/// The host must carry the `{instance}-{port}` prefix *and* the base domain
/// (e.g. `INS001-80.rent.do.top` with `base_domain = rent.do.top`). The prefix
/// must match `[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?` and is split at its
/// **last** `-` so instance names containing `-` work (`my-app1-8080` ->
/// `("my-app1", 8080)`). The port must be `1..=65535`. The base-domain suffix is
/// matched case-insensitively; the returned instance preserves the host's
/// original casing.
///
/// Returns `None` for any malformed input: an absent/mismatched base domain, an
/// empty instance, a missing separator, or a port that is `0`, `> 65535`,
/// non-numeric, or longer than five digits.
pub fn parse_instance_host(host: &str, base_domain: &str) -> Option<(String, u16)> {
    if host.is_empty() || base_domain.is_empty() || host.len() <= base_domain.len() {
        return None;
    }

    // `host.len() - base_domain.len()` is the byte offset where the base domain
    // begins. `str::get` keeps this panic-free even for non-boundary offsets.
    let split_at = host.len() - base_domain.len();
    let suffix = host.get(split_at..)?;
    if !suffix.eq_ignore_ascii_case(base_domain) {
        return None;
    }

    let prefix = host.get(..split_at)?.strip_suffix('.')?;
    if prefix.is_empty() {
        return None;
    }

    let (instance, port_str) = prefix.rsplit_once('-')?;
    if !is_valid_instance_label(instance) {
        return None;
    }

    let port = parse_port(port_str)?;
    Some((instance.to_owned(), port))
}

/// `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$` — an instance label that may
/// contain dots, underscores, and hyphens but must start and end alphanumeric.
fn is_valid_instance_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(&first), Some(&last))
            if first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parse a `[0-9]{1,5}` port and range-check it to `1..=65535`.
fn parse_port(port_str: &str) -> Option<u16> {
    let bytes = port_str.as_bytes();
    if bytes.is_empty() || bytes.len() > 5 || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ForwardSite, LbMode, OpenStackSite};
    use std::collections::HashMap;

    // ----- fixtures -----

    fn forward(domain: &str) -> ForwardSite {
        ForwardSite {
            name: domain.to_owned(),
            domain: domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            openstack_env: None,
            backends: Vec::new(),
            lb_mode: LbMode::RoundRobin,
            health_check: None,
        }
    }

    fn openstack(base_domain: &str) -> OpenStackSite {
        OpenStackSite {
            name: base_domain.to_owned(),
            base_domain: base_domain.to_owned(),
            http: true,
            https: false,
            cert_file: None,
            key_file: None,
            firewall_dir: None,
            openstack_env: "prod".to_owned(),
        }
    }

    fn table(forward_domains: &[&str], base_domains: &[&str]) -> RoutingTable {
        let cfg = Config {
            forward: forward_domains.iter().map(|d| forward(d)).collect(),
            openstack: base_domains.iter().map(|b| openstack(b)).collect(),
            openstack_envs: HashMap::new(),
        };
        RoutingTable::try_from(&cfg).expect("routing table builds")
    }

    fn os(site_idx: usize, instance: &str, port: u16) -> RouteTarget {
        RouteTarget::OpenStack {
            site_idx,
            instance: instance.to_owned(),
            port,
        }
    }

    // ----- normalize_host (plan/05) -----

    #[test]
    fn normalize_host_lowercases() {
        assert_eq!(normalize_host("WWW.Example.COM"), "www.example.com");
    }

    #[test]
    fn normalize_host_strips_single_trailing_dot() {
        assert_eq!(normalize_host("example.com."), "example.com");
        // Exactly one trailing dot is removed.
        assert_eq!(normalize_host("example.com.."), "example.com.");
    }

    #[test]
    fn normalize_host_strips_default_ports_only() {
        assert_eq!(normalize_host("example.com:80"), "example.com");
        assert_eq!(normalize_host("example.com:443"), "example.com");
        assert_eq!(normalize_host("example.com:8080"), "example.com:8080");
        assert_eq!(normalize_host("example.com:8443"), "example.com:8443");
        assert_eq!(normalize_host("example.com:8000"), "example.com:8000");
        // A label that merely ends in the digits `80`/`443` is untouched.
        assert_eq!(normalize_host("example.com:180"), "example.com:180");
        assert_eq!(normalize_host("example.com:1443"), "example.com:1443");
    }

    #[test]
    fn normalize_host_empty_is_empty() {
        assert_eq!(normalize_host(""), "");
        assert_eq!(normalize_host("."), "");
        assert_eq!(normalize_host(":80"), "");
        assert_eq!(normalize_host(":443"), "");
    }

    #[test]
    fn normalize_host_port_then_trailing_dot() {
        assert_eq!(normalize_host("Example.COM.:443"), "example.com");
        assert_eq!(normalize_host("example.com.:80"), "example.com");
    }

    // ----- parse_instance_host (plan/05) -----

    #[test]
    fn parse_instance_host_basic() {
        assert_eq!(
            parse_instance_host("ins001-80.rent.do.top", "rent.do.top"),
            Some(("ins001".to_owned(), 80))
        );
        assert_eq!(
            parse_instance_host("INS001-80.rent.do.top", "rent.do.top"),
            Some(("INS001".to_owned(), 80))
        );
    }

    #[test]
    fn parse_instance_host_splits_at_last_dash() {
        assert_eq!(
            parse_instance_host("my-app1-8080.rent.do.top", "rent.do.top"),
            Some(("my-app1".to_owned(), 8080))
        );
        assert_eq!(
            parse_instance_host("a-b-c-80.rent.do.top", "rent.do.top"),
            Some(("a-b-c".to_owned(), 80))
        );
    }

    #[test]
    fn parse_instance_host_rejects_bad_ports() {
        for host in [
            "ins-0.rent.do.top",      // zero
            "ins-65536.rent.do.top",  // overflow
            "ins-99999.rent.do.top",  // overflow
            "ins-abc.rent.do.top",    // non-numeric
            "ins-.rent.do.top",       // port missing
            "ins.rent.do.top",        // no separator at all
            "ins-00000.rent.do.top",  // parses to zero
            "ins-123456.rent.do.top", // more than five digits
        ] {
            assert_eq!(
                parse_instance_host(host, "rent.do.top"),
                None,
                "host={host}"
            );
        }
    }

    #[test]
    fn parse_instance_host_rejects_empty_instance() {
        assert_eq!(parse_instance_host("-80.rent.do.top", "rent.do.top"), None);
        assert_eq!(parse_instance_host("-.rent.do.top", "rent.do.top"), None);
    }

    #[test]
    fn parse_instance_host_matches_base_case_insensitively() {
        assert_eq!(
            parse_instance_host("INS001-80.RENT.DO.TOP", "rent.do.top"),
            Some(("INS001".to_owned(), 80))
        );
    }

    #[test]
    fn parse_instance_host_requires_full_host() {
        // The base_domain must be part of the host (`{prefix}.{base_domain}`).
        assert_eq!(parse_instance_host("INS001-80", "rent.do.top"), None);
        assert_eq!(
            parse_instance_host("INS001-80.other.com", "rent.do.top"),
            None
        );
        // A bare base_domain has no `{instance}-{port}` prefix.
        assert_eq!(parse_instance_host("rent.do.top", "rent.do.top"), None);
    }

    #[test]
    fn parse_instance_host_enforces_label_legality() {
        // Instance may contain dots/underscores per the §8 regex.
        assert_eq!(
            parse_instance_host("a_b.c-8080.rent.do.top", "rent.do.top"),
            Some(("a_b.c".to_owned(), 8080))
        );
        // Leading/trailing punctuation in the instance is rejected.
        assert_eq!(parse_instance_host("-x-80.rent.do.top", "rent.do.top"), None);
        assert_eq!(parse_instance_host("x--80.rent.do.top", "rent.do.top"), None);
    }

    // ----- lookup (plan/05) -----

    #[test]
    fn lookup_exact_forward_hit() {
        let t = table(&["www.example.com"], &[]);
        assert_eq!(t.lookup("www.example.com"), Some(RouteTarget::Forward(0)));
    }

    #[test]
    fn lookup_openstack_wildcard_hit() {
        let t = table(&[], &["rent.do.top"]);
        assert_eq!(t.lookup("ins001-80.rent.do.top"), Some(os(0, "ins001", 80)));
    }

    #[test]
    fn lookup_misses_return_none() {
        let t = table(&["www.example.com"], &["rent.do.top"]);
        assert_eq!(t.lookup("nope.example.com"), None);
        assert_eq!(t.lookup(""), None);
        // The bare base_domain is not routable (404 per config.md §8).
        assert_eq!(t.lookup("rent.do.top"), None);
        // An instance form under the wrong base is not routable.
        assert_eq!(t.lookup("ins001-80.example.com"), None);
    }

    #[test]
    fn lookup_exact_takes_priority_over_wildcard() {
        // `www.rent.do.top` is an exact forward site under the `rent.do.top` base.
        let t = table(&["www.rent.do.top"], &["rent.do.top"]);
        assert_eq!(t.lookup("www.rent.do.top"), Some(RouteTarget::Forward(0)));
        // A genuine instance form still hits the wildcard.
        assert_eq!(t.lookup("app-80.rent.do.top"), Some(os(0, "app", 80)));
    }

    #[test]
    fn lookup_exact_wins_when_host_also_matches_wildcard() {
        // Constructed directly (P1 would reject this overlap): an exact domain
        // that is itself a `{instance}-{port}` form must prefer the exact site.
        let t = table(&["app-80.rent.do.top"], &["rent.do.top"]);
        assert_eq!(
            t.lookup("app-80.rent.do.top"),
            Some(RouteTarget::Forward(0))
        );
    }

    #[test]
    fn lookup_table_driven_mixed() {
        let t = table(
            &["www.example.com", "api.example.com"],
            &["rent.do.top", "cloud.example.net"],
        );
        let cases: Vec<(&str, Option<RouteTarget>)> = vec![
            ("www.example.com", Some(RouteTarget::Forward(0))),
            ("api.example.com", Some(RouteTarget::Forward(1))),
            ("ins001-80.rent.do.top", Some(os(0, "ins001", 80))),
            ("my-app1-8080.rent.do.top", Some(os(0, "my-app1", 8080))),
            ("vm-443.cloud.example.net", Some(os(1, "vm", 443))),
            ("rent.do.top", None),
            ("cloud.example.net", None),
            ("other.example.com", None),
        ];
        for (host, expected) in cases {
            assert_eq!(t.lookup(host), expected, "host={host}");
        }
    }

    #[test]
    fn lookup_exact_openstack_entry_is_not_directly_routable() {
        // Defensive: an openstack SiteKind in the exact map is not routable on
        // its own; it falls through to wildcard matching.
        let t = RoutingTable {
            exact: HashMap::from([("rent.do.top".to_owned(), SiteKind::OpenStack(0))]),
            openstack_patterns: vec![OpenStackPattern {
                site_idx: 0,
                base_domain: "rent.do.top".to_owned(),
            }],
        };
        assert_eq!(t.lookup("rent.do.top"), None);
        assert_eq!(t.lookup("ins-80.rent.do.top"), Some(os(0, "ins", 80)));
    }

    // ----- try_from -----

    #[test]
    fn try_from_dedupes_duplicate_forward_domains_first_wins() {
        let cfg = Config {
            forward: vec![forward("a.example.com"), forward("a.example.com")],
            openstack: Vec::new(),
            openstack_envs: HashMap::new(),
        };
        let t = RoutingTable::try_from(&cfg).expect("builds");
        assert_eq!(t.lookup("a.example.com"), Some(RouteTarget::Forward(0)));
    }

    #[test]
    fn try_from_dedupes_duplicate_base_domains_first_wins() {
        let cfg = Config {
            forward: Vec::new(),
            openstack: vec![openstack("rent.do.top"), openstack("rent.do.top")],
            openstack_envs: HashMap::new(),
        };
        let t = RoutingTable::try_from(&cfg).expect("builds");
        assert_eq!(t.lookup("ins-80.rent.do.top"), Some(os(0, "ins", 80)));
    }

    #[test]
    fn routing_error_displays_detail() {
        let err = RoutingError::DuplicateForwardDomain("a.example.com".to_owned());
        assert!(err.to_string().contains("a.example.com"));
    }
}
