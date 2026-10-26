//! Who may reach this process, decided before a socket exists.
//!
//! Every surface ego served before this one — `ego acp`, `ego mcp-server` —
//! speaks over stdio to a parent that already had the right to run it, so
//! "who may connect" was never a question ego had to answer. An inbound
//! endpoint makes it one, and
//! `ARCHITECTURE.md`
//! puts the answer here, above every host protocol, rather than once inside
//! each of them.
//!
//! The module is policy and holds no I/O: it decides which endpoint kinds
//! exist, which are refused, and which authentication each kind compels. The
//! listener that binds is the thin half and lives outside `ego-core`; the
//! protocols that serve — A2A over `hyper`, ACP over `ByteStreams` — compose
//! above the listener and consume only the admitted policy and credential,
//! never transport I/O owned by this module.

use std::fmt;
use std::net::SocketAddr;

use crate::credentials::SecretMaterial;
use crate::permit::Confinement;

/// One already-loaded bearer secret, compared without interpreting HTTP.
///
/// Protocol adapters own where their secret comes from and how an
/// `Authorization` value is parsed. This type owns only the policy common to
/// every bearer-authenticated endpoint: secret lifetime and byte equality.
pub struct BearerCredential {
    secret: SecretMaterial,
}

impl BearerCredential {
    pub fn new(secret: SecretMaterial) -> Self {
        Self { secret }
    }

    /// Whether `presented` is exactly the configured secret.
    ///
    /// Empty configured secrets admit nobody. Protocol adapters already refuse
    /// them at startup, and keeping that refusal true here prevents an omitted
    /// adapter check from turning an empty value into anonymous access.
    pub fn matches(&self, presented: &[u8]) -> bool {
        let expected = self.secret.expose_secret_bytes();
        !expected.is_empty() && constant_time_eq(presented, expected)
    }
}

impl fmt::Debug for BearerCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BearerCredential")
            .finish_non_exhaustive()
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// What the operator asked to listen on, before anything has judged it.
///
/// Two variants rather than three, because "loopback" and "public" are not
/// things an operator asks for — they are what an address *turns out to be*.
/// Admission makes that distinction; a request cannot pre-empt it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// A unix socket on POSIX, a named pipe on Windows. The name is passed
    /// through: resolving it to a path or a pipe name is the listener's job,
    /// and this module does not know which platform it is on.
    Local { name: String },
    /// An address, whatever it turns out to be reachable from.
    Tcp(SocketAddr),
}

/// How a peer proves it may be here.
///
/// The two are not interchangeable and the difference is the reason the local
/// kind is preferred. A peer identity cannot be replayed: the peer either
/// holds the user's identity or the kernel never delivers the connection. A
/// bearer credential is a secret at rest, presentable by any local process
/// that reaches the endpoint, and exists for clients that cannot speak a
/// socket or a pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    /// The operating system answered: directory mode on POSIX, the pipe's
    /// security descriptor on Windows.
    PeerIdentity,
    /// A credential the operator configured, required before routing.
    BearerCredential,
}

/// An endpoint that has been through this module.
///
/// The wrapper is not ceremony. A `SocketAddr` is what `bind` takes, so any
/// code holding one can open a socket anywhere; holding an `Admitted` is the
/// evidence that the reachability question was asked and answered, and the
/// only way to learn which authentication the answer compels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// Reachable only by the current user, enforced by the operating system.
    Local { name: String },
    /// Reachable by any process on this machine.
    Loopback(SocketAddr),
    /// Reachable from off this machine, because the operator said so.
    Public(SocketAddr),
}

impl Admitted {
    /// The authentication this kind compels, which is a property of the
    /// transport and never of the protocol served over it.
    pub fn authentication(&self) -> Authentication {
        match self {
            Self::Local { .. } => Authentication::PeerIdentity,
            Self::Loopback(_) | Self::Public(_) => Authentication::BearerCredential,
        }
    }

    /// Narrow the operator's confinement ceiling for this endpoint.
    ///
    /// A publicly reachable peer may select at most Workspace execution. Local
    /// and loopback endpoints add no restriction, and a public endpoint never
    /// weakens an already stricter operator choice.
    pub fn cap_confinement(&self, configured: Confinement) -> Confinement {
        match (self, configured) {
            (Self::Public(_), Confinement::Off | Confinement::Adaptive) => Confinement::Workspace,
            _ => configured,
        }
    }
}

impl fmt::Display for Admitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { name } => write!(f, "{name}"),
            Self::Loopback(address) | Self::Public(address) => write!(f, "{address}"),
        }
    }
}

/// Why an endpoint was refused, in terms an operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The address is reachable from outside this machine and the operator did
    /// not say that is what they wanted.
    NotLoopback { address: SocketAddr },
    /// A local endpoint with no name. Refused rather than defaulted, because
    /// the default would be a path this module cannot see the filesystem to
    /// choose.
    UnnamedLocal,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback { address } => write!(
                f,
                "{address} is reachable from outside this machine; pass \
                 '--allow-public-bind' only if you intend to allow remote connections"
            ),
            Self::UnnamedLocal => write!(
                f,
                "the local endpoint has no name; set '--listen' to a socket path \
                 (a pipe name on Windows)"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Whether the operator has asked for a publicly reachable endpoint.
///
/// A `bool` at the call site would read `admit(requested, true)`, which says
/// nothing about what is being permitted. This is the one decision on this
/// path that cannot be taken back once a packet has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicBind {
    Refused,
    RequestedByOperator,
}

/// Admit `requested`, refusing anything off-loopback the operator did not ask
/// for.
///
/// `0.0.0.0` and `::` are refused by the same rule that refuses a LAN address,
/// and deliberately so: an unspecified address is the *most* reachable of the
/// three, not a neutral default.
pub fn admit(requested: Requested, public: PublicBind) -> Result<Admitted, Refusal> {
    match requested {
        Requested::Local { name } if name.is_empty() => Err(Refusal::UnnamedLocal),
        Requested::Local { name } => Ok(Admitted::Local { name }),
        Requested::Tcp(address) if address.ip().is_loopback() => Ok(Admitted::Loopback(address)),
        Requested::Tcp(address) if public == PublicBind::RequestedByOperator => {
            Ok(Admitted::Public(address))
        }
        Requested::Tcp(address) => Err(Refusal::NotLoopback { address }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(address: &str) -> SocketAddr {
        address.parse().expect("a test address is well formed")
    }

    fn tcp(address: &str) -> Requested {
        Requested::Tcp(parse(address))
    }

    #[test]
    fn loopback_is_admitted_without_the_operator_asking_for_anything() {
        for address in ["127.0.0.1:8080", "[::1]:8080", "127.5.5.5:1"] {
            let admitted = admit(tcp(address), PublicBind::Refused)
                .expect("loopback needs no further permission");
            assert_eq!(admitted, Admitted::Loopback(parse(address)));
        }
    }

    /// The unspecified address is the case a reviewer is most likely to assume
    /// is harmless, so it is named rather than left to the loopback check.
    #[test]
    fn every_address_reachable_from_outside_is_refused_including_the_unspecified_one() {
        for address in ["0.0.0.0:8080", "[::]:8080", "192.168.1.10:8080"] {
            let refusal = admit(tcp(address), PublicBind::Refused)
                .expect_err("a reachable address was admitted silently");
            assert_eq!(
                refusal,
                Refusal::NotLoopback {
                    address: parse(address)
                }
            );
            let rendered = refusal.to_string();
            assert!(rendered.contains("--allow-public-bind"), "{rendered}");
            assert!(
                rendered.contains("only if you intend to allow remote connections"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn the_operator_can_ask_for_a_public_bind_and_then_it_is_theirs() {
        let admitted = admit(tcp("0.0.0.0:8080"), PublicBind::RequestedByOperator)
            .expect("an explicit request is the operator's to make");
        assert_eq!(admitted, Admitted::Public(parse("0.0.0.0:8080")));
    }

    /// A public bind is an escalation of reachability, not of trust: it does
    /// not turn a loopback address into a public endpoint, because the address
    /// is what decides the kind.
    #[test]
    fn asking_for_a_public_bind_does_not_reclassify_a_loopback_address() {
        let admitted = admit(tcp("127.0.0.1:8080"), PublicBind::RequestedByOperator)
            .expect("loopback is admitted either way");
        assert_eq!(admitted, Admitted::Loopback(parse("127.0.0.1:8080")));
    }

    #[test]
    fn a_local_endpoint_is_admitted_and_needs_no_public_permission() {
        let admitted = admit(
            Requested::Local {
                name: "/run/user/1000/ego.sock".into(),
            },
            PublicBind::Refused,
        )
        .expect("a local endpoint is the preferred kind");
        assert_eq!(
            admitted,
            Admitted::Local {
                name: "/run/user/1000/ego.sock".into()
            }
        );
    }

    #[test]
    fn a_local_endpoint_without_a_name_is_refused_rather_than_defaulted() {
        let refusal = admit(
            Requested::Local {
                name: String::new(),
            },
            PublicBind::Refused,
        )
        .expect_err("an unnamed local endpoint was admitted");
        assert_eq!(refusal, Refusal::UnnamedLocal);
    }

    /// The whole point of the seam: a protocol above it can ask what it must
    /// enforce without knowing which transport it got.
    #[test]
    fn only_the_local_kind_is_authenticated_by_the_operating_system() {
        let local = admit(
            Requested::Local {
                name: "ego.sock".into(),
            },
            PublicBind::Refused,
        )
        .expect("local");
        assert_eq!(local.authentication(), Authentication::PeerIdentity);

        for (address, public) in [
            ("127.0.0.1:8080", PublicBind::Refused),
            ("192.168.1.10:8080", PublicBind::RequestedByOperator),
        ] {
            let admitted = admit(tcp(address), public).expect("admitted");
            assert_eq!(
                admitted.authentication(),
                Authentication::BearerCredential,
                "{address} must not be served without a credential"
            );
        }
    }
}
