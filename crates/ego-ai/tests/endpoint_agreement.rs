//! The rule grammar and HTTP transport must agree on endpoint identity, including alternate
//! numeric loopback spellings. Core keeps canonical rule identity stable across dependency
//! updates; HTTP classifies the parsed transport destination. These tests pin their agreement
//! without conflating the two owners.

use ego_core::permit::rule::{Host, Rule};
use url::Url;

/// The host `net(<spelling>)` canonicalises to.
fn as_rule(spelling: &str) -> Host {
    match Rule::parse(&format!("net({spelling})")).expect("a well-formed endpoint") {
        Rule::Net(endpoint) => endpoint.host().clone(),
        other => panic!("`net(...)` did not parse as an endpoint: {other:?}"),
    }
}

/// The host the transport would connect to.
fn as_url(spelling: &str) -> url::Host {
    Url::parse(&format!("http://{spelling}/"))
        .expect("a well-formed authority")
        .host()
        .expect("http:// always has a host")
        .to_owned()
}

/// Every spelling P19 names, and the ones that break the implementations that
/// get this wrong.
///
/// `010.1.1.1` is the one worth staring at: a leading zero is **octal**, so it
/// is `8.1.1.1` and not `10.1.1.1`. An implementation that reads it as decimal
/// admits a private-range address while believing it denied one.
#[test]
fn a_rule_and_the_offline_seam_agree_on_every_spelling_of_one_address() {
    let spellings = [
        // P19's own examples: four spellings of loopback.
        "127.0.0.1",
        "0177.0.0.1",
        "2130706433",
        "0x7f000001",
        "127.1",
        // Octal, where a leading zero changes the address.
        "010.1.1.1",
        // Ordinary dotted quads, so the test is not only about oddities.
        "192.168.0.1",
        "8.8.8.8",
        // Names, including case folding and IDNA to A-labels.
        "github.com",
        "GitHub.COM",
        "münchen.de",
        "xn--mnchen-3ya.de",
    ];

    for spelling in spellings {
        match (as_rule(spelling), as_url(spelling)) {
            (Host::Ip(mine), url::Host::Ipv4(theirs)) => assert_eq!(
                mine,
                std::net::IpAddr::V4(theirs),
                "`{spelling}`: a rule and the transport name different addresses"
            ),
            (Host::Ip(mine), url::Host::Ipv6(theirs)) => assert_eq!(
                mine,
                std::net::IpAddr::V6(theirs),
                "`{spelling}`: a rule and the transport name different addresses"
            ),
            (Host::Name(mine), url::Host::Domain(theirs)) => assert_eq!(
                mine, theirs,
                "`{spelling}`: a rule and the transport name different hosts"
            ),
            (mine, theirs) => panic!(
                "`{spelling}` is an address to one of them and a name to the other: \
                 rule={mine:?}, transport={theirs:?}"
            ),
        }
    }
}

/// The half that would be silently wrong if only the equalities above were
/// checked: an address literal and a name must never be confused for each
/// other, in either direction.
#[test]
fn a_name_is_never_an_address_and_an_address_is_never_a_name() {
    assert!(matches!(as_rule("127.0.0.1"), Host::Ip(_)));
    assert!(matches!(as_url("127.0.0.1"), url::Host::Ipv4(_)));

    assert!(matches!(as_rule("github.com"), Host::Name(_)));
    assert!(matches!(as_url("github.com"), url::Host::Domain(_)));
}

/// Refuse an invalid numeric host instead of storing it as a name. WHATWG rejects
/// `http://256.1.1.1/`; accepting a rule for that spelling would create a boundary that never
/// matches a destination.
#[test]
fn a_host_that_looks_like_a_broken_address_is_refused_by_both_and_named_by_neither() {
    for spelling in [
        "256.1.1.1",   // an octet out of range
        "999.1.1.1",   // the same, further out
        "127.0.0.256", // out of range in the last position
        "1.2.3.4.5",   // too many parts
        "0x1g.1.1.1",  // not hex after all
        "a.1.1.1",     // a name in front, a number at the end
    ] {
        assert!(
            Rule::parse(&format!("net({spelling})")).is_err(),
            "`{spelling}` was accepted as a rule; it is a broken address, not a name"
        );
        assert!(
            Url::parse(&format!("http://{spelling}/")).is_err(),
            "`{spelling}` parsed as a URL; this test's premise has changed"
        );
    }
}

/// The control for the test above. Without it, refusing *every* numeric-
/// looking host would pass just as well — and that would break every real
/// address rule in the grammar.
#[test]
fn a_host_that_looks_like_an_address_and_is_one_is_accepted_by_both() {
    for spelling in ["127.0.0.1", "8.8.8.8", "12.34", "0x7f000001", "2130706433"] {
        assert!(
            matches!(as_rule(spelling), Host::Ip(_)),
            "`{spelling}` is a valid address and the grammar refused it"
        );
        assert!(matches!(as_url(spelling), url::Host::Ipv4(_)));
    }
}

/// P19's other confusion, made unspellable rather than handled.
///
/// `http://127.0.0.1@evil.com/` connects to `evil.com`; the `127.0.0.1` is
/// userinfo. The seam gets this right by reading `Url::host`. The grammar gets
/// it right by refusing the string outright — a `net(...)` pattern is a bare
/// authority, so there is no rule an operator can write that carries the
/// confusion in the first place.
#[test]
fn userinfo_cannot_appear_in_a_rule_and_does_not_fool_the_seam() {
    assert!(
        Rule::parse("net(127.0.0.1@evil.com)").is_err(),
        "a rule may not carry userinfo"
    );
    assert_eq!(
        as_url("127.0.0.1@evil.com"),
        url::Host::Domain("evil.com".to_string()),
        "the destination is evil.com, whatever the string leads with"
    );
}

// The transport and grammar have different representations: loopback transport policy covers
// address ranges, while a rule endpoint denotes a single authority. Keep their observable
// classification aligned through the tests above without adding range syntax to the rule
// grammar.
