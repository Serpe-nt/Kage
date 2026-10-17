//! The SSRF guard (spec §19) as pure functions.
//!
//! The proxy makes the connection, but the guard still runs on the resolved
//! name, because a proxy can reach the operator's own networks. Checking a
//! URL takes three steps, and a redirect repeats all three:
//!
//! 1. [`check_url`] looks at the URL alone: scheme, credentials, port, host
//!    name, literal IP address. It returns the [`Target`] to resolve.
//! 2. The caller resolves the target's host name. This module does no I/O.
//! 3. [`check_resolved`] looks at every resolved address. One denied address
//!    denies the target: the proxy resolves the name again on its side, and
//!    it may pick any of them.
//!
//! [`RedirectChain`] carries the same checks across redirect hops.
//!
//! # The browser path
//!
//! A browser follows redirects by itself, so the hops of a browser
//! navigation cannot be checked one by one from here. Two things keep the
//! guard effective there:
//!
//! - Chromium bypasses the configured proxy for loopback by default, so a
//!   redirect to `localhost` or `127.0.0.1` inside the browser container
//!   would reach the sidecar's own server instead of leaving through the
//!   proxy. The Chromium engine launches every context with the bypass rule
//!   `<-loopback>`, which takes that exception away: loopback names and
//!   addresses go to the proxy like every other host (its integration tests
//!   show a redirect to `127.0.0.1` arriving at the proxy). The Firefox
//!   engine must do the equivalent (M5).
//! - The chain is checked after the navigation with these same functions.
//!   The check lives next to them, in `kage_browser::ssrf::check_chain`,
//!   and not in the sidecar: the rules, the configured exceptions
//!   (`security.allow_ip_targets`) and the resolver are here, and a second
//!   implementation in TypeScript would drift. The sidecar reports the
//!   whole chain (the requested URL, every redirect with its `Location`,
//!   the final URL); `SidecarClient::navigate_checked` walks it and answers
//!   `Navigation::SsrfDenied` with the denied hop, after dropping what the
//!   page captured. The caller reports it as `ssrf_denied`.
//!
//! The check is post hoc: when it denies, the request to the denied host
//! has already left, through the proxy. That is the limit of this design
//! and the reason for the first point; what the check prevents is that
//! anything read from such a host is returned or stored.
//!
//! Intercepting navigation requests to check each hop first is not the
//! first choice: request interception adds an automation fingerprint.
//!
//! The denied ranges are the ones spec §19 names (loopback, link-local,
//! RFC 1918, CGNAT, multicast, IPv6 unique-local, cloud metadata) plus the
//! other ranges that are never a public web server: unspecified, broadcast,
//! reserved, documentation and benchmarking networks, and IPv4 addresses
//! embedded in IPv4-mapped, IPv4-translated, IPv4-compatible, NAT64 and
//! 6to4 IPv6 addresses.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// Redirect hops followed before the chain is cut (spec §9).
pub const DEFAULT_MAX_REDIRECTS: usize = 10;

/// Why an address range is denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeniedRange {
    /// `127.0.0.0/8`, `::1`.
    Loopback,
    /// `169.254.0.0/16`, `fe80::/10`.
    LinkLocal,
    /// RFC 1918 (`10/8`, `172.16/12`, `192.168/16`) and IPv6 site-local.
    Private,
    /// `100.64.0.0/10`, carrier-grade NAT.
    Cgnat,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// `fc00::/7`, IPv6 unique-local.
    UniqueLocal,
    /// A cloud provider's instance metadata or control address.
    CloudMetadata,
    /// `0.0.0.0/8`, `::`.
    Unspecified,
    /// `255.255.255.255`.
    Broadcast,
    /// Reserved, protocol-assignment, tunnelling and discard ranges.
    Reserved,
    /// Documentation networks (`192.0.2.0/24`, `2001:db8::/32`, ...).
    Documentation,
    /// Benchmarking networks (`198.18.0.0/15`, `2001:2::/48`).
    Benchmarking,
}

impl DeniedRange {
    /// The stable `snake_case` name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::LinkLocal => "link_local",
            Self::Private => "private",
            Self::Cgnat => "cgnat",
            Self::Multicast => "multicast",
            Self::UniqueLocal => "unique_local",
            Self::CloudMetadata => "cloud_metadata",
            Self::Unspecified => "unspecified",
            Self::Broadcast => "broadcast",
            Self::Reserved => "reserved",
            Self::Documentation => "documentation",
            Self::Benchmarking => "benchmarking",
        }
    }
}

impl fmt::Display for DeniedRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

const fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> Ipv4Net {
    Ipv4Net::new_assert(Ipv4Addr::new(a, b, c, d), prefix)
}

const fn v6(segments: [u16; 8], prefix: u8) -> Ipv6Net {
    let s = segments;
    Ipv6Net::new_assert(
        Ipv6Addr::new(s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
        prefix,
    )
}

/// Denied IPv4 networks. The first match wins, so single addresses that sit
/// inside a wider denied network come first and keep their precise reason.
const DENIED_V4: [(Ipv4Net, DeniedRange); 21] = [
    // Instance metadata: AWS, GCP, Azure, OpenStack, DigitalOcean, Oracle.
    (v4(169, 254, 169, 254, 32), DeniedRange::CloudMetadata),
    // AWS ECS task metadata and credentials.
    (v4(169, 254, 170, 2, 32), DeniedRange::CloudMetadata),
    // Alibaba Cloud metadata.
    (v4(100, 100, 100, 200, 32), DeniedRange::CloudMetadata),
    // Azure wire server. This one is in public address space.
    (v4(168, 63, 129, 16, 32), DeniedRange::CloudMetadata),
    // Oracle Cloud at Customer metadata.
    (v4(192, 0, 0, 192, 32), DeniedRange::CloudMetadata),
    (v4(255, 255, 255, 255, 32), DeniedRange::Broadcast),
    (v4(0, 0, 0, 0, 8), DeniedRange::Unspecified),
    (v4(10, 0, 0, 0, 8), DeniedRange::Private),
    (v4(100, 64, 0, 0, 10), DeniedRange::Cgnat),
    (v4(127, 0, 0, 0, 8), DeniedRange::Loopback),
    (v4(169, 254, 0, 0, 16), DeniedRange::LinkLocal),
    (v4(172, 16, 0, 0, 12), DeniedRange::Private),
    (v4(192, 0, 0, 0, 24), DeniedRange::Reserved),
    (v4(192, 0, 2, 0, 24), DeniedRange::Documentation),
    (v4(192, 168, 0, 0, 16), DeniedRange::Private),
    (v4(198, 18, 0, 0, 15), DeniedRange::Benchmarking),
    (v4(198, 51, 100, 0, 24), DeniedRange::Documentation),
    (v4(203, 0, 113, 0, 24), DeniedRange::Documentation),
    (v4(224, 0, 0, 0, 4), DeniedRange::Multicast),
    // 6to4 relay anycast, deprecated.
    (v4(192, 88, 99, 0, 24), DeniedRange::Reserved),
    (v4(240, 0, 0, 0, 4), DeniedRange::Reserved),
];

/// Denied IPv6 networks, most specific first.
const DENIED_V6: [(Ipv6Net, DeniedRange); 13] = [
    // AWS IMDS over IPv6.
    (
        v6([0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254], 128),
        DeniedRange::CloudMetadata,
    ),
    (v6([0, 0, 0, 0, 0, 0, 0, 0], 128), DeniedRange::Unspecified),
    (v6([0, 0, 0, 0, 0, 0, 0, 1], 128), DeniedRange::Loopback),
    // Local-use NAT64.
    (
        v6([0x0064, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
        DeniedRange::Reserved,
    ),
    // Discard-only.
    (v6([0x0100, 0, 0, 0, 0, 0, 0, 0], 64), DeniedRange::Reserved),
    (
        v6([0x2001, 0x0002, 0, 0, 0, 0, 0, 0], 48),
        DeniedRange::Benchmarking,
    ),
    (
        v6([0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32),
        DeniedRange::Documentation,
    ),
    (
        v6([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
        DeniedRange::Documentation,
    ),
    // Teredo and the other IETF protocol assignments (ORCHID, AMT, ...).
    (v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 23), DeniedRange::Reserved),
    (
        v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
        DeniedRange::UniqueLocal,
    ),
    (
        v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
        DeniedRange::LinkLocal,
    ),
    // Site-local, deprecated but still routed inside some networks.
    (v6([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10), DeniedRange::Private),
    (v6([0xff00, 0, 0, 0, 0, 0, 0, 0], 8), DeniedRange::Multicast),
];

/// IPv6 prefixes that carry an IPv4 address in their last 32 bits.
const EMBEDS_V4_LOW: [Ipv6Net; 4] = [
    // IPv4-mapped.
    v6([0, 0, 0, 0, 0, 0xffff, 0, 0], 96),
    // IPv4-translated (SIIT, RFC 2765).
    v6([0, 0, 0, 0, 0xffff, 0, 0, 0], 96),
    // IPv4-compatible, deprecated.
    v6([0, 0, 0, 0, 0, 0, 0, 0], 96),
    // Well-known NAT64 prefix.
    v6([0x0064, 0xff9b, 0, 0, 0, 0, 0, 0], 96),
];

/// 6to4: the IPv4 address sits in bits 16 to 48.
const SIX_TO_FOUR: Ipv6Net = v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16);

/// The denied range an address falls in, or `None` for a public address.
pub fn classify_ip(address: IpAddr) -> Option<DeniedRange> {
    match address {
        IpAddr::V4(address) => classify_v4(address),
        IpAddr::V6(address) => classify_v6(address),
    }
}

fn classify_v4(address: Ipv4Addr) -> Option<DeniedRange> {
    DENIED_V4
        .iter()
        .find(|(network, _)| network.contains(&address))
        .map(|(_, range)| *range)
}

fn classify_v6(address: Ipv6Addr) -> Option<DeniedRange> {
    if let Some((_, range)) = DENIED_V6
        .iter()
        .find(|(network, _)| network.contains(&address))
    {
        return Some(*range);
    }
    let octets = address.octets();
    if EMBEDS_V4_LOW
        .iter()
        .any(|network| network.contains(&address))
    {
        return classify_v4(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    if SIX_TO_FOUR.contains(&address) {
        return classify_v4(Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]));
    }
    None
}

/// Guard settings from the `security` section of the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Allow URLs whose host is a literal IP address. The denied ranges
    /// apply either way.
    #[serde(default)]
    pub allow_ip_targets: bool,
}

/// Why the guard refused a URL or a redirect hop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SsrfDenial {
    /// The URL could not be parsed.
    #[error("{0:?} is not a valid URL")]
    InvalidUrl(String),
    /// The scheme is not `http` or `https`.
    #[error("scheme {0:?} is not allowed, only http and https are")]
    Scheme(String),
    /// The URL has no host.
    #[error("the URL has no host")]
    NoHost,
    /// The URL carries a user name or a password.
    #[error("URLs with credentials are not allowed")]
    Credentials,
    /// The port is zero.
    #[error("port 0 is not allowed")]
    Port,
    /// The host is a literal IP address and `security.allow_ip_targets` is off.
    #[error("literal IP address {0} is not allowed as a target")]
    IpLiteral(IpAddr),
    /// The host name is one that never names a public site.
    #[error("host {host:?} is not allowed: {reason}")]
    Name {
        /// The host.
        host: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// An address is in a denied range.
    #[error("{address} is in a denied range ({range})")]
    Address {
        /// The address.
        address: IpAddr,
        /// The range it falls in.
        range: DeniedRange,
    },
    /// The host name resolved to no address.
    #[error("{0:?} did not resolve to any address")]
    Unresolved(String),
    /// More redirect hops than the limit.
    #[error("more than {0} redirects")]
    TooManyRedirects(usize),
}

/// What a checked URL points at: the thing to resolve and connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    host: TargetHost,
    port: u16,
}

/// The host of a [`Target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetHost {
    /// A host name that still has to be resolved.
    Name(String),
    /// A literal address.
    Address(IpAddr),
}

impl Target {
    /// The host.
    pub const fn host(&self) -> &TargetHost {
        &self.host
    }

    /// The port, explicit or the scheme's default.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The name to resolve; `None` when the host is a literal address.
    pub fn name(&self) -> Option<&str> {
        match &self.host {
            TargetHost::Name(name) => Some(name),
            TargetHost::Address(_) => None,
        }
    }
}

/// Host names that never name a public site. Each entry is matched as the
/// whole name or as a suffix after a dot.
const DENIED_NAMES: [(&str, &str); 9] = [
    ("localhost", "it names this machine"),
    ("local", "mDNS names resolve on the local network"),
    ("internal", "reserved for private networks"),
    ("intranet", "reserved for private networks"),
    ("lan", "reserved for private networks"),
    ("home", "reserved for private networks"),
    ("corp", "reserved for private networks"),
    ("home.arpa", "reserved for home networks"),
    ("localdomain", "it names this machine"),
];

/// Step 1: checks everything that can be decided from the URL alone.
///
/// # Errors
///
/// Returns the reason the URL is refused.
pub fn check_url(url: &Url, policy: SsrfPolicy) -> Result<Target, SsrfDenial> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SsrfDenial::Scheme(url.scheme().to_owned()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SsrfDenial::Credentials);
    }
    let port = url.port_or_known_default().unwrap_or(0);
    if port == 0 {
        return Err(SsrfDenial::Port);
    }

    let address = match url.host().ok_or(SsrfDenial::NoHost)? {
        Host::Domain(name) => {
            let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
            check_name(&name)?;
            return Ok(Target {
                host: TargetHost::Name(name),
                port,
            });
        }
        Host::Ipv4(address) => IpAddr::V4(address),
        Host::Ipv6(address) => IpAddr::V6(address),
    };

    // The range comes first: it is the more useful answer, and it holds
    // whatever `allow_ip_targets` says.
    if let Some(range) = classify_ip(address) {
        return Err(SsrfDenial::Address { address, range });
    }
    if !policy.allow_ip_targets {
        return Err(SsrfDenial::IpLiteral(address));
    }
    Ok(Target {
        host: TargetHost::Address(address),
        port,
    })
}

fn check_name(name: &str) -> Result<(), SsrfDenial> {
    let denied = |reason| SsrfDenial::Name {
        host: name.to_owned(),
        reason,
    };
    if name.is_empty() {
        return Err(SsrfDenial::NoHost);
    }
    for (suffix, reason) in DENIED_NAMES {
        let is_suffix = name
            .strip_suffix(suffix)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'));
        if is_suffix {
            return Err(denied(reason));
        }
    }
    if !name.contains('.') {
        return Err(denied("a public host name has at least two labels"));
    }
    Ok(())
}

/// Step 3: checks the addresses a target's host name resolved to. A target
/// with a literal address was already checked by [`check_url`] and passes.
///
/// # Errors
///
/// Fails when the name resolved to nothing, or when any address is in a
/// denied range.
pub fn check_resolved(target: &Target, addresses: &[IpAddr]) -> Result<(), SsrfDenial> {
    let TargetHost::Name(name) = &target.host else {
        return Ok(());
    };
    if addresses.is_empty() {
        return Err(SsrfDenial::Unresolved(name.clone()));
    }
    for &address in addresses {
        if let Some(range) = classify_ip(address) {
            return Err(SsrfDenial::Address { address, range });
        }
    }
    Ok(())
}

/// One URL in a redirect chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectHop {
    /// The URL.
    pub url: Url,
    /// Whether this hop left `https` for `http`. Downgrades are followed, as
    /// a browser follows them, and recorded so that a result can show them.
    pub downgrade: bool,
}

/// A redirect hop that the guard refused.
///
/// It ends the chain: the attempt's outcome is `ssrf_denied`, which is not
/// retried, the refused hop goes into the evidence, and nothing fetched on
/// the earlier hops is kept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("redirect hop {hop} to {location:?} denied: {reason}")]
pub struct RedirectDenied {
    /// Which redirect it was: 1 for the first.
    pub hop: usize,
    /// Where it pointed: the `Location` as received, or the resolved URL.
    pub location: String,
    /// Why it was refused.
    pub reason: SsrfDenial,
}

impl RedirectDenied {
    /// The evidence string for the block report of the `ssrf_denied` result.
    pub fn evidence(&self) -> String {
        format!("redirect:{}:{} ({})", self.hop, self.location, self.reason)
    }
}

/// Re-checks every hop of a redirect chain (spec §19).
///
/// ```text
/// let mut chain = RedirectChain::start(&url, policy, DEFAULT_MAX_REDIRECTS)?;
/// check_resolved(chain.target(), &resolve(chain.target()))?;
/// // ... send the request; on a 3xx with a Location header:
/// chain.follow(location)?;
/// check_resolved(chain.target(), &resolve(chain.target()))
///     .map_err(|reason| chain.denied(reason))?;
/// ```
///
/// A hop may change host, port, and scheme between `http` and `https`, as a
/// browser allows; a downgrade from `https` to `http` is followed and
/// recorded. Everything [`check_url`] refuses is refused on every hop.
#[derive(Debug, Clone)]
pub struct RedirectChain {
    policy: SsrfPolicy,
    max_redirects: usize,
    hops: Vec<RedirectHop>,
    target: Target,
}

impl RedirectChain {
    /// Starts a chain at the first URL.
    ///
    /// # Errors
    ///
    /// Fails when the first URL itself is refused.
    pub fn start(url: &Url, policy: SsrfPolicy, max_redirects: usize) -> Result<Self, SsrfDenial> {
        let target = check_url(url, policy)?;
        Ok(Self {
            policy,
            max_redirects,
            hops: vec![RedirectHop {
                url: url.clone(),
                downgrade: false,
            }],
            target,
        })
    }

    /// The URL the chain currently points at.
    pub fn current(&self) -> &Url {
        // `hops` always holds at least the first URL.
        &self.hops[self.hops.len() - 1].url
    }

    /// The target of the current URL. Resolve it and pass the addresses to
    /// [`check_resolved`] before connecting.
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// Every URL visited so far, the first one included.
    pub fn hops(&self) -> &[RedirectHop] {
        &self.hops
    }

    /// Redirects followed so far.
    pub fn redirects(&self) -> usize {
        self.hops.len() - 1
    }

    /// Whether any hop so far left `https` for `http`.
    pub fn was_downgraded(&self) -> bool {
        self.hops.iter().any(|hop| hop.downgrade)
    }

    /// Follows a `Location` header, absolute or relative to the current URL.
    /// On success the chain points at the new URL and returns it; on
    /// failure the chain is unchanged.
    ///
    /// # Errors
    ///
    /// Fails, naming the hop, when the limit is reached, the location does
    /// not parse, or the new URL is refused.
    pub fn follow(&mut self, location: &str) -> Result<&Url, RedirectDenied> {
        let denied = |reason| RedirectDenied {
            hop: self.redirects() + 1,
            location: location.trim().to_owned(),
            reason,
        };
        if self.redirects() >= self.max_redirects {
            return Err(denied(SsrfDenial::TooManyRedirects(self.max_redirects)));
        }
        let next = self
            .current()
            .join(location.trim())
            .map_err(|_| denied(SsrfDenial::InvalidUrl(location.to_owned())))?;
        let target = check_url(&next, self.policy).map_err(denied)?;
        let downgrade = self.current().scheme() == "https" && next.scheme() == "http";
        self.target = target;
        self.hops.push(RedirectHop {
            url: next,
            downgrade,
        });
        Ok(self.current())
    }

    /// Turns a refusal of the current hop into a [`RedirectDenied`]. For the
    /// address check, which the caller runs after resolving the hop's name.
    pub fn denied(&self, reason: SsrfDenial) -> RedirectDenied {
        RedirectDenied {
            hop: self.redirects(),
            location: self.current().to_string(),
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use url::Url;

    use super::{
        DEFAULT_MAX_REDIRECTS, DeniedRange, RedirectChain, SsrfDenial, SsrfPolicy, TargetHost,
        check_resolved, check_url, classify_ip,
    };

    const STRICT: SsrfPolicy = SsrfPolicy {
        allow_ip_targets: false,
    };
    const IPS_ALLOWED: SsrfPolicy = SsrfPolicy {
        allow_ip_targets: true,
    };

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn range_of(text: &str) -> Option<DeniedRange> {
        classify_ip(ip(text))
    }

    /// Every range spec §19 names, each at its first address, its last
    /// address and one in between.
    #[test]
    fn every_range_named_by_the_spec_is_denied() {
        let cases = [
            // loopback
            ("127.0.0.0", DeniedRange::Loopback),
            ("127.0.0.1", DeniedRange::Loopback),
            ("127.255.255.255", DeniedRange::Loopback),
            ("::1", DeniedRange::Loopback),
            // link-local
            ("169.254.0.0", DeniedRange::LinkLocal),
            ("169.254.10.20", DeniedRange::LinkLocal),
            ("169.254.255.255", DeniedRange::LinkLocal),
            ("fe80::", DeniedRange::LinkLocal),
            ("fe80::1ff:fe23:4567:890a", DeniedRange::LinkLocal),
            (
                "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                DeniedRange::LinkLocal,
            ),
            // RFC 1918
            ("10.0.0.0", DeniedRange::Private),
            ("10.20.30.40", DeniedRange::Private),
            ("10.255.255.255", DeniedRange::Private),
            ("172.16.0.0", DeniedRange::Private),
            ("172.20.1.1", DeniedRange::Private),
            ("172.31.255.255", DeniedRange::Private),
            ("192.168.0.0", DeniedRange::Private),
            ("192.168.1.1", DeniedRange::Private),
            ("192.168.255.255", DeniedRange::Private),
            // CGNAT
            ("100.64.0.0", DeniedRange::Cgnat),
            ("100.100.0.1", DeniedRange::Cgnat),
            ("100.127.255.255", DeniedRange::Cgnat),
            // multicast
            ("224.0.0.0", DeniedRange::Multicast),
            ("224.0.0.251", DeniedRange::Multicast),
            ("239.255.255.255", DeniedRange::Multicast),
            ("ff00::", DeniedRange::Multicast),
            ("ff02::1", DeniedRange::Multicast),
            (
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                DeniedRange::Multicast,
            ),
            // IPv6 ULA
            ("fc00::", DeniedRange::UniqueLocal),
            ("fd12:3456:789a::1", DeniedRange::UniqueLocal),
            (
                "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                DeniedRange::UniqueLocal,
            ),
            // cloud metadata
            ("169.254.169.254", DeniedRange::CloudMetadata),
            ("169.254.170.2", DeniedRange::CloudMetadata),
            ("100.100.100.200", DeniedRange::CloudMetadata),
            ("168.63.129.16", DeniedRange::CloudMetadata),
            ("192.0.0.192", DeniedRange::CloudMetadata),
            ("fd00:ec2::254", DeniedRange::CloudMetadata),
        ];
        for (address, expected) in cases {
            assert_eq!(range_of(address), Some(expected), "{address}");
        }
    }

    #[test]
    fn other_non_public_ranges_are_denied_too() {
        let cases = [
            ("0.0.0.0", DeniedRange::Unspecified),
            ("0.255.255.255", DeniedRange::Unspecified),
            ("::", DeniedRange::Unspecified),
            ("255.255.255.255", DeniedRange::Broadcast),
            ("240.0.0.1", DeniedRange::Reserved),
            ("255.255.255.254", DeniedRange::Reserved),
            ("192.0.0.1", DeniedRange::Reserved),
            ("192.88.99.1", DeniedRange::Reserved),
            ("192.0.2.1", DeniedRange::Documentation),
            ("198.51.100.7", DeniedRange::Documentation),
            ("203.0.113.9", DeniedRange::Documentation),
            ("2001:db8::1", DeniedRange::Documentation),
            ("3fff:0abc::1", DeniedRange::Documentation),
            ("198.18.0.1", DeniedRange::Benchmarking),
            ("198.19.255.255", DeniedRange::Benchmarking),
            ("2001:2::1", DeniedRange::Benchmarking),
            ("fec0::1", DeniedRange::Private),
            ("2001::1", DeniedRange::Reserved),
            ("2001:1ff::1", DeniedRange::Reserved),
            ("100::1", DeniedRange::Reserved),
            ("64:ff9b:1::a00:1", DeniedRange::Reserved),
        ];
        for (address, expected) in cases {
            assert_eq!(range_of(address), Some(expected), "{address}");
        }
    }

    #[test]
    fn addresses_just_outside_each_range_are_public() {
        for address in [
            "9.255.255.255",
            "11.0.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "223.255.255.255",
            "168.63.129.15",
            "168.63.129.17",
            "198.17.255.255",
            "198.20.0.0",
            "192.0.1.0",
            "192.0.3.0",
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4001:81b::200e",
            "2001:200::1",
            "2001:4860:4860::8888",
            "fbff:ffff::1",
            "fe00::1",
        ] {
            assert_eq!(range_of(address), None, "{address}");
        }
    }

    #[test]
    fn ipv4_embedded_in_ipv6_is_judged_by_the_ipv4_address() {
        let cases = [
            // IPv4-mapped
            ("::ffff:127.0.0.1", Some(DeniedRange::Loopback)),
            ("::ffff:10.0.0.1", Some(DeniedRange::Private)),
            ("::ffff:169.254.169.254", Some(DeniedRange::CloudMetadata)),
            ("::ffff:93.184.216.34", None),
            // IPv4-compatible
            ("::10.0.0.1", Some(DeniedRange::Private)),
            ("::93.184.216.34", None),
            // IPv4-translated (SIIT, RFC 2765): security review G6
            ("::ffff:0:7f00:1", Some(DeniedRange::Loopback)),
            ("::ffff:0:a00:1", Some(DeniedRange::Private)),
            ("::ffff:0:a9fe:a9fe", Some(DeniedRange::CloudMetadata)),
            ("::ffff:0:5db8:d822", None),
            // NAT64 well-known prefix
            ("64:ff9b::7f00:1", Some(DeniedRange::Loopback)),
            ("64:ff9b::c0a8:101", Some(DeniedRange::Private)),
            ("64:ff9b::5db8:d822", None),
            // 6to4
            ("2002:7f00:1::1", Some(DeniedRange::Loopback)),
            ("2002:a00:1::1", Some(DeniedRange::Private)),
            ("2002:a9fe:a9fe::1", Some(DeniedRange::CloudMetadata)),
            ("2002:5db8:d822::1", None),
        ];
        for (address, expected) in cases {
            assert_eq!(range_of(address), expected, "{address}");
        }
    }

    #[test]
    fn range_names_are_stable() {
        let all = [
            (DeniedRange::Loopback, "loopback"),
            (DeniedRange::LinkLocal, "link_local"),
            (DeniedRange::Private, "private"),
            (DeniedRange::Cgnat, "cgnat"),
            (DeniedRange::Multicast, "multicast"),
            (DeniedRange::UniqueLocal, "unique_local"),
            (DeniedRange::CloudMetadata, "cloud_metadata"),
            (DeniedRange::Unspecified, "unspecified"),
            (DeniedRange::Broadcast, "broadcast"),
            (DeniedRange::Reserved, "reserved"),
            (DeniedRange::Documentation, "documentation"),
            (DeniedRange::Benchmarking, "benchmarking"),
        ];
        for (range, name) in all {
            assert_eq!(range.as_str(), name);
            assert_eq!(range.to_string(), name);
            assert_eq!(
                serde_json::to_string(&range).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn a_public_name_passes_and_reports_what_to_resolve() {
        let target = check_url(&url("https://Example.ORG/listing?page=2"), STRICT).unwrap();

        assert_eq!(target.host(), &TargetHost::Name("example.org".to_owned()));
        assert_eq!(target.name(), Some("example.org"));
        assert_eq!(target.port(), 443);

        assert_eq!(
            check_url(&url("http://example.org/"), STRICT)
                .unwrap()
                .port(),
            80
        );
        assert_eq!(
            check_url(&url("http://example.org:8080/"), STRICT)
                .unwrap()
                .port(),
            8080
        );
        assert_eq!(
            check_url(&url("https://example.org:8443/"), STRICT)
                .unwrap()
                .port(),
            8443
        );
        // A fully qualified name with a trailing dot is the same name.
        assert_eq!(
            check_url(&url("https://example.org./"), STRICT)
                .unwrap()
                .name(),
            Some("example.org")
        );
    }

    #[test]
    fn only_http_and_https_are_allowed() {
        for text in [
            "ftp://example.org/file",
            "file:///etc/passwd",
            "gopher://example.org/",
            "javascript:alert(1)",
            "data:text/html,hi",
            "ws://example.org/socket",
            "about:blank",
        ] {
            let scheme = text.split(':').next().unwrap().to_owned();
            assert_eq!(
                check_url(&url(text), STRICT),
                Err(SsrfDenial::Scheme(scheme)),
                "{text}"
            );
        }
    }

    #[test]
    fn credentials_and_port_zero_are_refused() {
        assert_eq!(
            check_url(&url("https://user:pass@example.org/"), STRICT),
            Err(SsrfDenial::Credentials)
        );
        assert_eq!(
            check_url(&url("https://user@example.org/"), STRICT),
            Err(SsrfDenial::Credentials)
        );
        assert_eq!(
            check_url(&url("http://example.org:0/"), STRICT),
            Err(SsrfDenial::Port)
        );
    }

    #[test]
    fn literal_ip_targets_need_the_configuration_flag() {
        let public = url("http://93.184.216.34/");
        assert_eq!(
            check_url(&public, STRICT),
            Err(SsrfDenial::IpLiteral(ip("93.184.216.34")))
        );
        let target = check_url(&public, IPS_ALLOWED).unwrap();
        assert_eq!(target.host(), &TargetHost::Address(ip("93.184.216.34")));
        assert_eq!(target.name(), None);
        // A literal target needs no resolution; the second step passes.
        assert_eq!(check_resolved(&target, &[]), Ok(()));

        let v6 = url("https://[2606:4700:4700::1111]/");
        assert!(matches!(
            check_url(&v6, STRICT),
            Err(SsrfDenial::IpLiteral(_))
        ));
        assert!(check_url(&v6, IPS_ALLOWED).is_ok());
    }

    #[test]
    fn the_flag_never_opens_a_denied_range() {
        for policy in [STRICT, IPS_ALLOWED] {
            for (text, range) in [
                ("http://127.0.0.1/", DeniedRange::Loopback),
                ("http://10.0.0.5:8080/", DeniedRange::Private),
                (
                    "http://169.254.169.254/latest/meta-data/",
                    DeniedRange::CloudMetadata,
                ),
                ("http://[::1]/", DeniedRange::Loopback),
                ("http://[fd00:ec2::254]/", DeniedRange::CloudMetadata),
                ("http://[::ffff:192.168.1.1]/", DeniedRange::Private),
            ] {
                assert!(
                    matches!(check_url(&url(text), policy), Err(SsrfDenial::Address { range: found, .. }) if found == range),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn disguised_ipv4_literals_are_seen_through() {
        // The URL standard reads all of these as 127.0.0.1.
        for text in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://0x7f.1/",
        ] {
            assert_eq!(
                check_url(&url(text), IPS_ALLOWED),
                Err(SsrfDenial::Address {
                    address: ip("127.0.0.1"),
                    range: DeniedRange::Loopback
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn names_that_never_name_a_public_site_are_refused_before_dns() {
        for host in [
            "localhost",
            "LOCALHOST",
            "app.localhost",
            "printer.local",
            "metadata.google.internal",
            "wiki.intranet",
            "nas.lan",
            "router.home",
            "git.corp",
            "tv.home.arpa",
            "localhost.localdomain",
            // Single labels: compose service names and the like.
            "postgres",
            "browser-chromium",
        ] {
            let result = check_url(&url(&format!("http://{host}/")), STRICT);
            assert!(
                matches!(result, Err(SsrfDenial::Name { .. })),
                "{host}: {result:?}"
            );
        }

        // Suffixes only count at a label boundary.
        for host in [
            "notlocalhost.example",
            "mylan.example",
            "corp.example",
            "local.example.org",
        ] {
            assert!(
                check_url(&url(&format!("http://{host}/")), STRICT).is_ok(),
                "{host}"
            );
        }
    }

    #[test]
    fn one_denied_address_among_the_resolved_ones_denies_the_target() {
        let target = check_url(&url("https://example.org/"), STRICT).unwrap();

        assert_eq!(check_resolved(&target, &[ip("93.184.216.34")]), Ok(()));
        assert_eq!(
            check_resolved(&target, &[ip("93.184.216.34"), ip("2606:4700:4700::1111")]),
            Ok(())
        );
        assert_eq!(
            check_resolved(&target, &[ip("93.184.216.34"), ip("10.0.0.7")]),
            Err(SsrfDenial::Address {
                address: ip("10.0.0.7"),
                range: DeniedRange::Private
            })
        );
        assert_eq!(
            check_resolved(&target, &[ip("::1"), ip("93.184.216.34")]),
            Err(SsrfDenial::Address {
                address: ip("::1"),
                range: DeniedRange::Loopback
            })
        );
        assert_eq!(
            check_resolved(&target, &[]),
            Err(SsrfDenial::Unresolved("example.org".to_owned()))
        );
    }

    fn chain(start: &str) -> RedirectChain {
        RedirectChain::start(&url(start), STRICT, DEFAULT_MAX_REDIRECTS).unwrap()
    }

    #[test]
    fn a_chain_starts_with_the_same_check_as_a_single_url() {
        assert_eq!(
            RedirectChain::start(&url("http://127.0.0.1/"), STRICT, 10).unwrap_err(),
            SsrfDenial::Address {
                address: ip("127.0.0.1"),
                range: DeniedRange::Loopback
            }
        );

        let chain = chain("https://example.org/a");
        assert_eq!(chain.current().as_str(), "https://example.org/a");
        assert_eq!(chain.target().name(), Some("example.org"));
        assert_eq!(chain.redirects(), 0);
        assert_eq!(chain.hops().len(), 1);
    }

    #[test]
    fn hops_may_be_relative_cross_host_cross_port_and_cross_scheme() {
        let mut chain = chain("https://example.org/a/b?x=1");

        assert_eq!(
            chain.follow("/login").unwrap().as_str(),
            "https://example.org/login"
        );
        assert_eq!(
            chain.follow("next?step=2").unwrap().as_str(),
            "https://example.org/next?step=2"
        );
        assert_eq!(
            chain.follow("//cdn.example.net/x").unwrap().as_str(),
            "https://cdn.example.net/x"
        );
        assert_eq!(chain.target().name(), Some("cdn.example.net"));
        // A downgrade to http is followed, as a browser follows it.
        assert_eq!(
            chain.follow("http://example.com:8080/y").unwrap().as_str(),
            "http://example.com:8080/y"
        );
        assert_eq!(chain.target().port(), 8080);
        assert_eq!(
            chain
                .follow(" https://example.org/final ")
                .unwrap()
                .as_str(),
            "https://example.org/final"
        );

        assert_eq!(chain.redirects(), 5);
        let visited: Vec<&str> = chain.hops().iter().map(|hop| hop.url.as_str()).collect();
        assert_eq!(
            visited,
            [
                "https://example.org/a/b?x=1",
                "https://example.org/login",
                "https://example.org/next?step=2",
                "https://cdn.example.net/x",
                "http://example.com:8080/y",
                "https://example.org/final",
            ]
        );
    }

    #[test]
    fn every_hop_is_checked_again() {
        let denied_hops = [
            (
                "http://169.254.169.254/latest/meta-data/",
                "cloud metadata literal",
            ),
            ("http://10.0.0.1/admin", "private literal"),
            ("http://[::1]:8080/", "loopback literal"),
            ("http://93.184.216.34/", "public literal without the flag"),
            ("http://localhost:5432/", "local name"),
            ("http://postgres:5432/", "single-label name"),
            ("http://metadata.google.internal/", "internal name"),
            ("ftp://example.org/file", "scheme"),
            ("file:///etc/passwd", "scheme"),
            ("javascript:alert(1)", "scheme"),
            ("https://user:pw@example.org/", "credentials"),
            ("http://example.org:0/", "port zero"),
        ];
        for (location, what) in denied_hops {
            let mut chain = chain("https://example.org/start");

            assert!(chain.follow(location).is_err(), "{what}: {location}");
            // A refused hop leaves the chain where it was.
            assert_eq!(
                chain.current().as_str(),
                "https://example.org/start",
                "{what}"
            );
            assert_eq!(chain.target().name(), Some("example.org"), "{what}");
            assert_eq!(chain.redirects(), 0, "{what}");
        }
    }

    #[test]
    fn a_hop_to_a_name_must_pass_the_address_check_again() {
        let mut chain = chain("https://example.org/start");
        check_resolved(chain.target(), &[ip("93.184.216.34")]).unwrap();

        // The first host was fine; the redirect points at a name that
        // resolves into the operator's network.
        chain.follow("https://innocent.example.net/").unwrap();
        assert_eq!(
            check_resolved(chain.target(), &[ip("192.168.1.10")]),
            Err(SsrfDenial::Address {
                address: ip("192.168.1.10"),
                range: DeniedRange::Private
            })
        );

        // The same name is judged on every hop it appears in: a second
        // resolution that turned private is caught (DNS rebinding).
        let mut rebinding = self::chain("https://rebind.example.net/a");
        check_resolved(rebinding.target(), &[ip("93.184.216.34")]).unwrap();
        rebinding.follow("/b").unwrap();
        assert!(check_resolved(rebinding.target(), &[ip("127.0.0.1")]).is_err());
    }

    #[test]
    fn the_hop_limit_cuts_the_chain() {
        let mut chain = RedirectChain::start(&url("https://example.org/0"), STRICT, 3).unwrap();

        for n in 1..=3 {
            chain.follow(&format!("/{n}")).unwrap();
        }
        let cut = chain.follow("/4").unwrap_err();
        assert_eq!(cut.reason, SsrfDenial::TooManyRedirects(3));
        assert_eq!(cut.hop, 4);
        assert_eq!(chain.current().as_str(), "https://example.org/3");
        assert_eq!(chain.redirects(), 3);

        let mut none = RedirectChain::start(&url("https://example.org/"), STRICT, 0).unwrap();
        assert_eq!(
            none.follow("/a").unwrap_err().reason,
            SsrfDenial::TooManyRedirects(0)
        );
    }

    #[test]
    fn a_downgrade_to_http_is_followed_and_recorded() {
        let mut chain = chain("https://example.org/a");
        assert!(!chain.was_downgraded());

        chain.follow("https://example.org/b").unwrap();
        chain.follow("http://example.org/c").unwrap();
        chain.follow("http://example.org/d").unwrap();
        chain.follow("https://example.org/e").unwrap();

        let downgrades: Vec<bool> = chain.hops().iter().map(|hop| hop.downgrade).collect();
        // Only the hop that left https is marked, not the http hops after
        // it and not the upgrade back.
        assert_eq!(downgrades, [false, false, true, false, false]);
        assert!(chain.was_downgraded());
        assert_eq!(chain.hops()[2].url.as_str(), "http://example.org/c");

        // A chain that starts on http has nothing to downgrade from.
        let mut plain = self::chain("http://example.org/a");
        plain.follow("http://example.org/b").unwrap();
        assert!(!plain.was_downgraded());
    }

    #[test]
    fn a_denied_hop_names_itself_for_the_evidence() {
        let mut chain = chain("https://example.org/start");
        chain.follow("/step").unwrap();

        // Refused from the URL alone.
        let denied = chain
            .follow(" http://169.254.169.254/latest/meta-data/ ")
            .unwrap_err();
        assert_eq!(denied.hop, 2);
        assert_eq!(denied.location, "http://169.254.169.254/latest/meta-data/");
        assert!(matches!(
            denied.reason,
            SsrfDenial::Address {
                range: DeniedRange::CloudMetadata,
                ..
            }
        ));
        assert_eq!(
            denied.evidence(),
            "redirect:2:http://169.254.169.254/latest/meta-data/ (169.254.169.254 is in a denied range (cloud_metadata))"
        );
        assert!(denied.to_string().starts_with("redirect hop 2 to "));

        // Refused after resolving the hop's name.
        chain.follow("https://innocent.example.net/x").unwrap();
        let reason = check_resolved(chain.target(), &[ip("10.1.2.3")]).unwrap_err();
        let denied = chain.denied(reason);
        assert_eq!(denied.hop, 2);
        assert_eq!(denied.location, "https://innocent.example.net/x");
        assert_eq!(
            denied.evidence(),
            "redirect:2:https://innocent.example.net/x (10.1.2.3 is in a denied range (private))"
        );
    }

    #[test]
    fn an_unparseable_location_is_refused() {
        let mut chain = chain("https://example.org/");

        assert_eq!(
            chain.follow("http://[not-an-address]/").unwrap_err().reason,
            SsrfDenial::InvalidUrl("http://[not-an-address]/".to_owned())
        );
    }

    #[test]
    fn literal_hops_follow_the_flag_but_never_into_a_denied_range() {
        let mut chain = RedirectChain::start(
            &url("https://example.org/"),
            IPS_ALLOWED,
            DEFAULT_MAX_REDIRECTS,
        )
        .unwrap();

        chain.follow("http://93.184.216.34:8080/").unwrap();
        assert_eq!(
            chain.target().host(),
            &TargetHost::Address(ip("93.184.216.34"))
        );
        assert!(matches!(
            chain.follow("http://192.168.0.1/").unwrap_err().reason,
            SsrfDenial::Address {
                range: DeniedRange::Private,
                ..
            }
        ));
    }

    #[test]
    fn denials_explain_themselves() {
        let message = |denial: SsrfDenial| denial.to_string();

        assert!(message(SsrfDenial::Scheme("ftp".to_owned())).contains("ftp"));
        assert!(message(SsrfDenial::IpLiteral(ip("1.2.3.4"))).contains("1.2.3.4"));
        assert!(
            message(SsrfDenial::Address {
                address: ip("10.0.0.1"),
                range: DeniedRange::Private
            })
            .contains("10.0.0.1 is in a denied range (private)")
        );
        assert!(message(SsrfDenial::TooManyRedirects(10)).contains("10"));
        assert!(message(SsrfDenial::NoHost).contains("no host"));
        assert!(message(SsrfDenial::Unresolved("a.example".to_owned())).contains("a.example"));
        assert_eq!(SsrfPolicy::default(), STRICT);
    }
}
