//! `net(...)` patterns: a parsed authority, never a URL.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §5.
//! `POTENTIALS.md` P19 records that `http://2130706433/` and
//! `http://127.0.0.1/` are the same destination spelled two ways, and that
//! comparing host strings does not know it. The grammar closes that at the
//! front door: a pattern is a host and an optional port, both normalised at
//! parse time, and the confusing spellings are either normalised to one form
//! or made unspellable.
//!
//! **This half is only worth anything if the destination is the resolved
//! destination.** §5 requires matching to classify what the transport
//! resolved rather than the URL text. Nothing in `ego-core` opens a socket,
//! so the classifier lives here and the transport becomes its caller; until
//! something calls it, a `net` rule parses, canonicalises and gets an ID, and
//! matches no call — because no call carries an endpoint.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::parse::RuleParseError;

/// The host half of an endpoint pattern.
///
/// A name never matches an address literal and an address literal never
/// matches a name. They are different claims: one is about who answers, the
/// other about where the packet goes, and treating them as interchangeable is
/// how a deny on a name is walked past by dialling the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// Any host at all: `net(*)`.
    Any,
    /// An exact name, lowercased and in A-labels.
    Name(String),
    /// `*.github.com`: subdomains, and **not** the apex. Stated because every
    /// implementation that does not state it gets it wrong in one direction
    /// or the other; an operator who wants both writes both.
    Subdomains(String),
    /// An address literal, normalised to its canonical text.
    Ip(IpAddr),
}

/// A canonicalised `net` pattern: a host and an optional port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    host: Host,
    /// `None` means every port. A pattern that named no port and matched only
    /// 443 would be a boundary that moves when the service does.
    port: Option<u16>,
}

impl Endpoint {
    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Parse the text between `net(` and `)`.
    pub fn parse(raw: &str) -> Result<Self, RuleParseError> {
        let pattern = raw.trim();
        if pattern.is_empty() {
            return Err(RuleParseError::EmptyPattern);
        }

        let bad = |why: &str| RuleParseError::BadEndpoint {
            pattern: pattern.to_string(),
            why: why.to_string(),
        };

        // A bare authority and nothing else. Each of these carries a meaning
        // the pattern is not allowed to have, and refusing them is what makes
        // `net(127.0.0.1@evil.com)` — P19's confusion — unspellable rather
        // than merely discouraged.
        if pattern.contains("//") {
            return Err(bad("a scheme is not part of an endpoint"));
        }
        if pattern.contains('@') {
            return Err(bad("userinfo is not part of an endpoint"));
        }
        if pattern.contains('/') {
            return Err(bad("a path is not part of an endpoint"));
        }
        if pattern.contains('?') || pattern.contains('#') {
            return Err(bad("a query is not part of an endpoint"));
        }

        let (host, port) = split_port(pattern).ok_or_else(|| bad("the port is not a number"))?;
        if host.is_empty() {
            return Err(bad("there is no host"));
        }

        Ok(Self {
            host: parse_host(host, &bad)?,
            port,
        })
    }

    /// Does this pattern cover `host`:`port`?
    ///
    /// `host` is the destination the transport resolved, not text a caller
    /// wrote down. It is normalised on the way in by the same code that
    /// normalised the pattern, so the two sides cannot disagree about what
    /// `0177.0.0.1` denotes.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        if self.port.is_some_and(|wanted| wanted != port) {
            return false;
        }

        let bad = |why: &str| RuleParseError::BadEndpoint {
            pattern: host.to_string(),
            why: why.to_string(),
        };
        let Ok(target) = parse_host(host, &bad) else {
            return false;
        };

        match (&self.host, &target) {
            (Host::Any, _) => true,
            (Host::Ip(pattern), Host::Ip(actual)) => pattern == actual,
            (Host::Name(pattern), Host::Name(actual)) => pattern == actual,
            (Host::Subdomains(suffix), Host::Name(actual)) => actual
                .strip_suffix(suffix.as_str())
                .is_some_and(|label| label.ends_with('.') && label.len() > 1),
            _ => false,
        }
    }

    /// The canonical bytes this pattern contributes to a rule's ID.
    pub fn canonical_bytes(&self, out: &mut Vec<u8>) {
        let mut text = |tag: u8, value: &str| {
            out.push(tag);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        };
        match &self.host {
            Host::Any => out.push(0),
            Host::Name(name) => text(1, name),
            Host::Subdomains(suffix) => text(2, suffix),
            Host::Ip(ip) => text(3, &ip.to_string()),
        }
        match self.port {
            Some(port) => {
                out.push(1);
                out.extend_from_slice(&port.to_le_bytes());
            }
            None => out.push(0),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Any => f.write_str("*")?,
            Host::Name(name) => f.write_str(name)?,
            Host::Subdomains(suffix) => write!(f, "*.{suffix}")?,
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]")?,
            Host::Ip(ip) => write!(f, "{ip}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// Split a trailing `:port`, leaving a bracketed IPv6 literal alone — its
/// colons belong to the address.
fn split_port(pattern: &str) -> Option<(&str, Option<u16>)> {
    let after_brackets = match pattern.rfind(']') {
        Some(at) => at + 1,
        None => 0,
    };
    let Some(colon) = pattern[after_brackets..].rfind(':') else {
        return Some((pattern, None));
    };
    let colon = after_brackets + colon;
    // An unbracketed address with several colons is a bare IPv6 literal, not
    // a host with a port.
    if after_brackets == 0 && pattern.matches(':').count() > 1 {
        return Some((pattern, None));
    }
    let port = pattern[colon + 1..].parse::<u16>().ok()?;
    Some((&pattern[..colon], Some(port)))
}

fn parse_host(host: &str, bad: &dyn Fn(&str) -> RuleParseError) -> Result<Host, RuleParseError> {
    if host == "*" {
        return Ok(Host::Any);
    }

    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner
            .parse::<Ipv6Addr>()
            .map(|ip| Host::Ip(IpAddr::V6(ip)))
            .map_err(|_| bad("the bracketed literal is not an IPv6 address"));
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        return Ok(Host::Ip(IpAddr::V6(ip)));
    }
    if let Some(ip) = legacy_ipv4(host) {
        return Ok(Host::Ip(IpAddr::V4(ip)));
    }
    // A host that *looks* like an address and is not one is refused, never
    // demoted to a name. `256.1.1.1` has an octet out of range: falling
    // through would store it as `Host::Name("256.1.1.1")`, a rule that can
    // never match anything, while the operator believes they named an
    // address. That is §2's worst failure mode — a boundary that reads
    // correctly and is not there — reached by a typo rather than by design.
    //
    // It is also what the transport does. WHATWG requires that a host whose
    // last label is numeric parse as IPv4 or the URL be invalid, so `url`
    // refuses `http://256.1.1.1/` outright. Agreeing here is the whole of
    // P19: two subsystems that classify one string differently is the hole.
    if ends_in_a_number(host) {
        return Err(bad(
            "the last label is a number, so this is an address — and it is \
             not a valid one",
        ));
    }

    let (name, subdomains) = match host.strip_prefix("*.") {
        Some(suffix) => (suffix, true),
        None => (host, false),
    };
    if name.contains('*') {
        return Err(bad(
            "`*` in a host is only the leading `*.` of a subdomain pattern",
        ));
    }

    // IDNA to A-labels, then lowercase. An ID that differed by how the
    // operator's editor encoded a character would not be revocable from a
    // config file written on another machine.
    let ascii = idna::domain_to_ascii(name).map_err(|_| bad("the host is not a valid name"))?;
    if ascii.is_empty() {
        return Err(bad("there is no host"));
    }

    Ok(if subdomains {
        Host::Subdomains(ascii)
    } else {
        Host::Name(ascii)
    })
}

/// Would the WHATWG URL parser insist this host be an IPv4 address?
///
/// True when the last label is a number in any of the bases `legacy_ipv4`
/// reads. A trailing dot is the DNS root and is skipped, so `127.0.0.1.` is
/// judged on `1` exactly as `127.0.0.1` is.
///
/// This is the *shape* test only — whether the string is then a valid address
/// is [`legacy_ipv4`]'s answer, and the two are asked in that order so a host
/// can be refused for being a broken address rather than accepted as a name.
fn ends_in_a_number(host: &str) -> bool {
    let mut labels = host.split('.').collect::<Vec<_>>();
    if labels.last() == Some(&"") {
        labels.pop();
    }
    let Some(last) = labels.last() else {
        return false;
    };

    match last.strip_prefix("0x").or_else(|| last.strip_prefix("0X")) {
        // `0x` alone is zero, and hex digits after it are a number.
        Some(hex) => hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()),
    }
}

/// The legacy IPv4 spellings `inet_aton` accepts and Rust's parser does not:
/// `2130706433`, `0177.0.0.1`, `0x7f.0.0.1`, `127.1`.
///
/// Rust rejects all of these, which is the right default for a parser and the
/// wrong one for a *classifier*: the resolver accepts them, so a rule that
/// did not know them would be a boundary walked past by writing the same
/// address in a different base. This is P19, closed by normalising rather
/// than by comparing strings.
fn legacy_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }

    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        values.push(radix_u32(part)?);
    }

    // The last part fills every octet the earlier ones did not name, which is
    // what makes `127.1` the same address as `127.0.0.1`.
    let leading = values.len() - 1;
    let mut address: u32 = 0;
    for (at, value) in values[..leading].iter().enumerate() {
        if *value > u32::from(u8::MAX) {
            return None;
        }
        address |= value << (24 - 8 * at);
    }
    let tail = values[leading];
    let room = 32 - 8 * leading;
    if room < 32 && tail >= 1u32 << room {
        return None;
    }
    address |= tail;

    Some(Ipv4Addr::from(address))
}

/// One `inet_aton` part: `0x` is hexadecimal, a leading `0` is octal, and
/// anything else is decimal.
fn radix_u32(part: &str) -> Option<u32> {
    if part.is_empty() {
        return None;
    }
    if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if part.len() > 1 && part.starts_with('0') {
        return u32::from_str_radix(&part[1..], 8).ok();
    }
    part.parse::<u32>().ok()
}
