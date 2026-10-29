//! The Agent Card, built from what this build actually serves.
//!
//! A card is a promise made to a stranger: it is how another agent decides
//! what to send before it sends anything. A literal `capabilities: { streaming:
//! true }` in a file is a promise nothing checks, and the failure mode is not a
//! red test here — it is a caller that opens a stream this build has no route
//! for. So the card is assembled from the same values the router is built from,
//! and `Served` is the single description both read.

use std::collections::HashMap;

use a2a_lf::agent_card::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, HttpAuthSecurityScheme,
    SecurityScheme,
};
use a2a_lf::types::TRANSPORT_PROTOCOL_JSONRPC;
use ego_core::endpoint::Authentication;

use crate::auth::BEARER;

/// The name of the security scheme in the card, referenced by requirements.
const SCHEME_NAME: &str = "bearer";

/// What this build of `ego a2a` serves.
///
/// Every field is something the router's behaviour depends on, which is what
/// keeps the card honest: adding a route without widening this leaves the card
/// silent about it, and widening it without a route fails the test that walks
/// the card against the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The base URL a caller reaches this listener at.
    pub url: String,
    /// Whether `SendStreamingMessage` and `SubscribeToTask` have routes.
    pub streaming: bool,
    /// Whether the push-notification config methods have routes.
    pub push_notifications: bool,
    /// What the endpoint this card is served over actually enforces. Not a
    /// choice this crate makes: it is decided by
    /// [`ego_core::endpoint::Admitted::authentication`] before a socket exists.
    pub authentication: Authentication,
}

/// The bearer scheme, for the endpoints that enforce one.
///
/// A function rather than two branches inlined at the two fields that need it,
/// because a card whose `securitySchemes` and `securityRequirements` disagree
/// is worse than either answer alone.
fn bearer_scheme(authentication: Authentication) -> Option<HashMap<String, SecurityScheme>> {
    match authentication {
        Authentication::PeerIdentity => None,
        Authentication::BearerCredential => Some(HashMap::from([(
            SCHEME_NAME.to_string(),
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: BEARER.to_string(),
                description: Some(
                    "A shared secret the operator sets in EGO_A2A_TOKEN and gives \
                     to the agents they want to admit."
                        .to_string(),
                ),
                bearer_format: None,
            }),
        )])),
    }
}

/// The card for `served`.
///
/// `streaming` and `push_notifications` are written as `Some(false)` rather
/// than left out. The A2A card treats an absent capability as unsupported, so
/// both spellings are correct on the wire — but a caller reading the JSON to
/// decide what ego can do is better served by a stated `false` than by having
/// to know that silence means no.
pub fn card(served: &Served) -> AgentCard {
    AgentCard {
        name: "ego".to_string(),
        description: "A durable, auditable coding agent. Every run is an event \
                      log, and every tool call crosses an admission boundary."
            .to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_interfaces: vec![AgentInterface::new(
            served.url.clone(),
            TRANSPORT_PROTOCOL_JSONRPC,
        )],
        capabilities: AgentCapabilities {
            streaming: Some(served.streaming),
            push_notifications: Some(served.push_notifications),
            extensions: None,
            // ego serves one card. There is no authenticated extended card to
            // fetch, so `GetExtendedAgentCard` is not a route and this is
            // `false` rather than absent.
            extended_agent_card: Some(false),
        },
        // Text in, text out. Widening this is a claim about what the *run* can
        // accept, not about the transport, so it belongs to the milestone that
        // maps A2A parts onto ego's prompt content.
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        // Skills are a directory of what an agent will do, and ego's answer
        // depends on the workspace it was pointed at rather than on the build.
        // An invented skill list would be the same broken promise as an
        // invented capability.
        skills: Vec::new(),
        provider: Some(AgentProvider {
            organization: "ego".to_string(),
            url: "https://github.com/sstraus/ego".to_string(),
        }),
        documentation_url: None,
        icon_url: None,
        // The card declares the scheme this endpoint actually enforces, which
        // is a property of the transport it is served over and not of A2A.
        // `docs/architecture.md`
        // authenticates a local endpoint by the operating system, and a card
        // demanding a bearer token there would send a caller to configure a
        // secret this listener will never read — an instruction that is simply
        // false. A2A has no scheme meaning "the transport already answered", so
        // the honest spelling is to declare none.
        security_schemes: bearer_scheme(served.authentication),
        // A requirement with no scopes: presenting the token is the whole of
        // the authorization decision, because this listener has one caller
        // class. Anything finer would be a claim ego does not enforce.
        security_requirements: bearer_scheme(served.authentication)
            .map(|_| vec![HashMap::from([(SCHEME_NAME.to_string(), Vec::new())])]),
        signatures: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served() -> Served {
        Served {
            url: "http://127.0.0.1:8080/".to_string(),
            streaming: false,
            push_notifications: false,
            authentication: Authentication::BearerCredential,
        }
    }

    /// The card is served over a local endpoint too, and there it must not send
    /// a caller to configure a secret this listener will never read.
    #[test]
    fn a_local_endpoint_declares_no_scheme_because_the_transport_already_answered() {
        let json = serde_json::to_value(card(&Served {
            authentication: Authentication::PeerIdentity,
            ..served()
        }))
        .expect("a card serialises");
        assert!(
            json.get("securitySchemes")
                .is_none_or(serde_json::Value::is_null),
            "a local endpoint advertised a security scheme: {json}"
        );
        assert!(
            json.get("securityRequirements")
                .is_none_or(serde_json::Value::is_null),
            "a local endpoint advertised a security requirement: {json}"
        );
    }

    /// The two fields are written from one source, so a card cannot demand a
    /// scheme it does not declare or declare one it does not demand.
    #[test]
    fn the_declared_scheme_and_the_stated_requirement_agree_for_every_kind() {
        for authentication in [
            Authentication::PeerIdentity,
            Authentication::BearerCredential,
        ] {
            let card = card(&Served {
                authentication,
                ..served()
            });
            assert_eq!(
                card.security_schemes.is_some(),
                card.security_requirements.is_some(),
                "{authentication:?} produced a card that disagrees with itself"
            );
        }
    }

    /// The card must survive a round trip through the wire format, because the
    /// thing under test is what a stranger parses, not what we constructed.
    #[test]
    fn the_card_round_trips_through_its_own_wire_format() {
        let card = card(&served());
        let json = serde_json::to_string(&card).expect("a card serialises");
        let back: AgentCard = serde_json::from_str(&json).expect("a card parses");
        assert_eq!(back, card);
    }

    #[test]
    fn the_card_states_this_slice_supports_neither_streaming_nor_push() {
        let json = serde_json::to_value(card(&served())).expect("a card serialises");
        assert_eq!(json["capabilities"]["streaming"], false);
        assert_eq!(json["capabilities"]["pushNotifications"], false);
        assert_eq!(json["capabilities"]["extendedAgentCard"], false);
    }

    /// A card that declared streaming while `Served` said otherwise would tell
    /// a caller to open a stream that has no route.
    #[test]
    fn the_declared_capabilities_are_the_ones_the_build_was_described_with() {
        for (streaming, push) in [(false, false), (true, false), (false, true), (true, true)] {
            let json = serde_json::to_value(card(&Served {
                streaming,
                push_notifications: push,
                ..served()
            }))
            .expect("a card serialises");
            assert_eq!(json["capabilities"]["streaming"], streaming);
            assert_eq!(json["capabilities"]["pushNotifications"], push);
        }
    }

    #[test]
    fn the_card_names_the_jsonrpc_binding_at_the_url_the_listener_serves() {
        let json = serde_json::to_value(card(&served())).expect("a card serialises");
        let interface = &json["supportedInterfaces"][0];
        assert_eq!(interface["url"], "http://127.0.0.1:8080/");
        assert_eq!(interface["protocolBinding"], TRANSPORT_PROTOCOL_JSONRPC);
        assert_eq!(interface["protocolVersion"], a2a_lf::VERSION);
    }

    /// The scheme a caller reads must be the one the listener enforces; the
    /// two used to be able to drift because `BEARER` was spelled twice.
    #[test]
    fn the_declared_security_scheme_is_the_one_the_listener_checks() {
        let json = serde_json::to_value(card(&served())).expect("a card serialises");
        assert_eq!(
            json["securitySchemes"][SCHEME_NAME]["httpAuthSecurityScheme"]["scheme"],
            BEARER
        );
        assert_eq!(
            json["securityRequirements"][0][SCHEME_NAME],
            serde_json::json!([])
        );
    }

    /// The version is the build's, so a card cannot claim to be a release it
    /// is not.
    #[test]
    fn the_card_reports_this_builds_version() {
        assert_eq!(card(&served()).version, env!("CARGO_PKG_VERSION"));
    }
}
