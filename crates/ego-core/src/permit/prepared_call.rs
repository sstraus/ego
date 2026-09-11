//! `PreparedCall`: canonical operations, resolved at `prepare` time — never
//! prose, never a rendered shell string (`ARCHITECTURE.md` §3.2, §3.4).
//! This is what the policy engine classifies to mint a `Permit`, and what a
//! judge model evaluates in `auto` mode.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::foreign::{
    ForeignToolId, ForeignTransportTarget, HttpRequest, HttpRequestDigest, HttpRoute,
    InvocationDigest, NetworkTarget, PreparedConnect, Subject,
};
use super::path::CanonPath;
use super::spawn::SpawnSpec;
use crate::event::hook::HookId;
use crate::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use crate::store::evidence::EvidenceRead;

/// One canonical operation a `Tool::execute` intends to perform.
///
/// [`GuardedIo`](super::GuardedIo) covers both the filesystem operations and
/// exact argv-native process execution. Network operations remain owned by
/// their transport seams (`ARCHITECTURE.md` §3.4 axis 4).
///
/// [`Op::List`] and [`Op::Search`] exist so that a tool which walks a subtree
/// does not have to be described as one that reads it. See
/// `docs/decisions/2026-08-21-reading-without-reading-everything.md` §1–2:
/// expressing a glob as `Read { path: root }` would hand it the authority to
/// read every byte under `root`, which is not a rounding error in the
/// description — it is a much larger grant, arrived at because no other
/// variant existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// One named file's contents.
    Read { path: CanonPath },
    /// The names under `root` matching `pattern`, and nothing else about
    /// them.
    List { root: CanonPath, pattern: String },
    /// The lines under `root` matching `query`.
    ///
    /// The query is part of the operation, so a permit minted for one search
    /// authorises *that* search: a tool approved to look for `TODO` cannot
    /// then go looking for private keys.
    Search { root: CanonPath, query: String },
    /// A named file, after [`Change`] is applied to it.
    Write { path: CanonPath, change: Change },
    /// One exact argv-native process specification.
    Spawn { spec: SpawnSpec },
    /// Complete tool evidence addressed inside the current session.
    ReadEvidence { request: EvidenceRead },
    /// One exact foreign operation and the route on which it may be invoked.
    Opaque {
        tool: ForeignToolId,
        params_digest: InvocationDigest,
        transport: ForeignTransportTarget,
    },
    /// One exact logical or literal network authority.
    Connect {
        target: NetworkTarget,
        request_digest: HttpRequestDigest,
    },
    /// One request-scoped release of named secret sources into named HTTP
    /// header sinks. No rule grammar can grant this operation persistently.
    SecretEgress { authority: SecretEgressAuthority },
    /// One MCP target whose exact request bytes are minted later by the
    /// admitted protocol profile. This does not grant ordinary HTTP.
    ProtocolConnect { target: NetworkTarget },
}

/// What a [`Op::Write`] does to the file it names.
///
/// The variation is here, in the payload, and **not** in a sibling top-level
/// `Op` variant. Two variants both meaning "write to this path" would need
/// [`Op::name`], [`Op::mutates`], `Rule::matches` and `scope_of` — four places
/// — to independently agree they are the same authority, with nothing holding
/// them to it. One `Op::Write` has one arm in each of the four.
///
/// That is the same test [`Op::List`] and [`Op::Search`] pass and these two
/// fail: those are separate variants because they are separate *authorities*
/// (see `2026-08-21-reading-without-reading-everything.md` §1–2). Replacing a
/// whole file and replacing a string inside it are one authority applied two
/// ways.
///
/// **[`Change::Replace`] does not determine the resulting bytes** and
/// [`Change::Whole`] does. That is a real difference in what a party
/// authorising this call is being shown, and it is unavoidable: the bytes an
/// edit produces are a function of the bytes already there, and the only
/// moment that could compute them is `prepare` — which has no read authority,
/// because it runs before a `Permit` exists. See
/// `docs/decisions/2026-08-23-an-edit-is-two-authorities-and-a-conditional-write.md`.
/// What is *not* weakened is the authority granted: both reach exactly the one
/// path the op names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Every byte of the file, replaced.
    Whole(Vec<u8>),
    /// The one occurrence of `old_string`, replaced by `new_string`.
    ///
    /// One, not the first: a tool preparing this must refuse a call whose
    /// `old_string` is not in the file exactly once. Choosing among several
    /// would be choosing a place the call never named.
    Replace {
        old_string: String,
        new_string: String,
    },
}

/// Which kind of operation a denial or a question is about.
///
/// A closed vocabulary rather than the `&'static str` this used to be. Two
/// reasons, and the second is the one that forced it:
///
/// - It is closed in fact — there are exactly six [`Op`] variants — so a
///   type that admits any string admits values the rest of the system cannot
///   act on.
/// - It is recorded in a durable event (`EventKind::PermissionRequested`),
///   and `&'static str` cannot deserialise: there is no owned data to borrow
///   from. `String` would decode, and would also let a hand-edited log
///   reappear as an op nothing can match on. This decodes to one of four
///   values or fails.
///
/// `Copy`, so carrying it costs nothing and no producer allocates on the path
/// that has already decided to stop and ask somebody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpName {
    Read,
    List,
    Search,
    Write,
    Spawn,
    ReadEvidence,
    Opaque,
    Connect,
    SecretEgress,
}

impl fmt::Display for OpName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            OpName::Read => "read",
            OpName::List => "list",
            OpName::Search => "search",
            OpName::Write => "write",
            OpName::Spawn => "spawn",
            OpName::ReadEvidence => "read_evidence",
            OpName::Opaque => "opaque",
            OpName::Connect => "connect",
            OpName::SecretEgress => "secret_egress",
        };
        write!(f, "{name}")
    }
}

impl Op {
    /// The violating-operation name a denial or an approval prompt shows.
    pub fn name(&self) -> OpName {
        match self {
            Op::Read { .. } => OpName::Read,
            Op::List { .. } => OpName::List,
            Op::Search { .. } => OpName::Search,
            Op::Write { .. } => OpName::Write,
            Op::Spawn { .. } => OpName::Spawn,
            Op::ReadEvidence { .. } => OpName::ReadEvidence,
            Op::Opaque { .. } => OpName::Opaque,
            Op::Connect { .. } | Op::ProtocolConnect { .. } => OpName::Connect,
            Op::SecretEgress { .. } => OpName::SecretEgress,
        }
    }

    /// Does this operation change the world?
    ///
    /// The one axis the mode floor and ceiling turn on
    /// (`2026-08-21-a-permit-nobody-can-widen.md` §6): `plan` refuses every
    /// mutating op outright, and `default` reviews unresolved *sensitive*
    /// actions rather than every action, because `ARCHITECTURE.md:238` counts
    /// prompt fatigue as a security bug. A read inside the workspace that
    /// stopped the run to ask would be that bug.
    ///
    /// Derived from the variant and not from a table, because unlike §6a's
    /// head→operations mapping this is not a fact about *this build* — a
    /// write mutates in every build there will ever be.
    pub fn mutates(&self) -> bool {
        match self {
            Op::Read { .. } | Op::List { .. } | Op::Search { .. } | Op::ReadEvidence { .. } => {
                false
            }
            Op::Write { .. }
            | Op::Spawn { .. }
            | Op::Opaque { .. }
            | Op::Connect { .. }
            | Op::SecretEgress { .. }
            | Op::ProtocolConnect { .. } => true,
        }
    }

    /// The path this operation names: the file for a read or write, the root
    /// for a traversal, or a spawn's explicit cwd. A spawn may omit its cwd.
    pub fn path(&self) -> Option<&CanonPath> {
        match self {
            Op::Read { path } | Op::Write { path, .. } => Some(path),
            Op::List { root, .. } | Op::Search { root, .. } => Some(root),
            Op::Spawn { spec } => spec.cwd(),
            Op::ReadEvidence { .. }
            | Op::Opaque { .. }
            | Op::Connect { .. }
            | Op::SecretEgress { .. }
            | Op::ProtocolConnect { .. } => None,
        }
    }

    /// The path used in permission diagnostics for an operation.
    ///
    /// A spawn with no explicit cwd runs at the run workspace, so that is its
    /// diagnostic subject. This does not turn the cwd into a filesystem grant:
    /// process reach is enforced independently by `RunConfinement`.
    pub(crate) fn subject_path<'a>(&'a self, workspace: &'a CanonPath) -> Option<&'a CanonPath> {
        match self {
            Op::Spawn { spec } => Some(spec.cwd().unwrap_or(workspace)),
            Op::ReadEvidence { .. }
            | Op::Opaque { .. }
            | Op::Connect { .. }
            | Op::SecretEgress { .. }
            | Op::ProtocolConnect { .. } => None,
            _ => self.path(),
        }
    }

    /// The exact closed diagnostic subject for this operation.
    pub fn subject(&self, _workspace: &CanonPath) -> Subject {
        match self {
            Op::Read { path } | Op::Write { path, .. } => Subject::Path(path.clone()),
            Op::List { root, .. } | Op::Search { root, .. } => Subject::Path(root.clone()),
            Op::Spawn { spec } => Subject::Process(spec.clone()),
            Op::ReadEvidence { request } => Subject::Evidence(request.clone()),
            Op::Opaque {
                tool,
                params_digest,
                ..
            } => Subject::Foreign {
                tool: tool.clone(),
                params_digest: params_digest.clone(),
            },
            Op::Connect { target, .. } | Op::ProtocolConnect { target } => {
                Subject::Network(target.clone())
            }
            Op::SecretEgress { authority } => Subject::Network(authority.target.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEgressAuthority {
    hook: HookId,
    target: NetworkTarget,
    request_digest: HttpRequestDigest,
    sources: Vec<String>,
    header_sinks: Vec<String>,
}

impl SecretEgressAuthority {
    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn target(&self) -> &NetworkTarget {
        &self.target
    }

    pub fn request_digest(&self) -> &HttpRequestDigest {
        &self.request_digest
    }

    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    pub fn header_sinks(&self) -> &[String] {
        &self.header_sinks
    }
}

/// A tool call reduced to its canonical operations by `Tool::prepare`.
///
/// An empty prepared call is not a meaningful policy subject and callers must
/// not be able to mutate the operation list after preparation:
///
/// ```compile_fail
/// use ego_core::permit::PreparedCall;
///
/// let _empty = PreparedCall::new("empty", Vec::new());
/// ```
///
/// ```compile_fail
/// use ego_core::event::hook::HookId;
/// use ego_core::permit::{HttpRequest, HttpRoute, HttpScheme, NetworkTarget, PreparedCall};
///
/// let target = NetworkTarget::new("example.com", 443).unwrap();
/// let route = HttpRoute::new(HttpScheme::Https, target).unwrap();
/// let request = HttpRequest::get("/health").unwrap();
/// let mut call = PreparedCall::hook_http(
///     HookId::parse(&format!("h1:{}", "00".repeat(32))).unwrap(),
///     route,
///     &request,
///     Vec::new(),
/// );
/// call.ops.clear();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCall {
    tool: String,
    ops: Vec<Op>,
    mcp: Option<Box<McpPreparedEnvelope>>,
    hook_http: Option<Box<HookHttpPreparedEnvelope>>,
    provenance: PreparedCallProvenance,
}

/// Unforgeable provenance assigned by this module's typed constructors.
/// Generic tool preparation can append operations but cannot promote a native
/// call into a foreign delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedCallProvenance {
    Native,
    CapturedForeign,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookHttpPreparedEnvelope {
    hook: HookId,
    route: HttpRoute,
    request_digest: HttpRequestDigest,
    secret: Option<SecretEgressAuthority>,
}

impl HookHttpPreparedEnvelope {
    pub fn hook(&self) -> HookId {
        self.hook
    }

    pub fn route(&self) -> &HttpRoute {
        &self.route
    }

    pub fn request_digest(&self) -> &HttpRequestDigest {
        &self.request_digest
    }

    pub fn secret(&self) -> Option<&SecretEgressAuthority> {
        self.secret.as_ref()
    }
}

/// Protocol facts captured before policy classifies an MCP operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPreparedEnvelope {
    subject: McpSubject,
    operation: McpOperationKind,
    admission: McpAdmissionFingerprint,
    transport_fingerprint: String,
    transport: ForeignTransportTarget,
    parameters: Vec<u8>,
}

impl McpPreparedEnvelope {
    pub fn subject(&self) -> &McpSubject {
        &self.subject
    }

    pub fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn admission(&self) -> &McpAdmissionFingerprint {
        &self.admission
    }

    pub fn transport_fingerprint(&self) -> &str {
        &self.transport_fingerprint
    }

    pub fn transport(&self) -> &ForeignTransportTarget {
        &self.transport
    }

    pub fn parameters(&self) -> &[u8] {
        &self.parameters
    }
}

impl PreparedCall {
    pub fn new(tool: impl Into<String>, first: Op) -> Self {
        Self {
            tool: tool.into(),
            ops: vec![first],
            mcp: None,
            hook_http: None,
            provenance: PreparedCallProvenance::Native,
        }
    }

    pub fn with_op(mut self, additional: Op) -> Self {
        self.ops.push(additional);
        self
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    pub fn into_parts(self) -> (String, Vec<Op>) {
        (self.tool, self.ops)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mcp(
        subject: McpSubject,
        operation: McpOperationKind,
        invocations: Vec<(ForeignToolId, InvocationDigest)>,
        admission: McpAdmissionFingerprint,
        transport_fingerprint: impl Into<String>,
        transport: ForeignTransportTarget,
        parameters: &Value,
    ) -> Result<Self, crate::mcp::McpProjectionError> {
        subject.validate()?;
        if !operation.matches(&subject) || transport.http_route().is_none() {
            return Err(crate::mcp::McpProjectionError::InvalidIdentity);
        }
        let transport_fingerprint = transport_fingerprint.into();
        if transport_fingerprint.trim().is_empty()
            || transport_fingerprint.chars().any(char::is_control)
        {
            return Err(crate::mcp::McpProjectionError::InvalidFingerprint);
        }
        let parameters = serde_json::to_vec(parameters)
            .map_err(|_| crate::mcp::McpProjectionError::InvalidContent)?;
        let mut ops = invocations
            .into_iter()
            .map(|(tool, params_digest)| Op::Opaque {
                tool,
                params_digest,
                transport: transport.clone(),
            })
            .collect::<Vec<_>>();
        let route = transport.http_route().expect("validated HTTP transport");
        ops.push(Op::ProtocolConnect {
            target: route.target().clone(),
        });
        Ok(Self {
            tool: subject.operation_name().to_owned(),
            ops,
            mcp: Some(Box::new(McpPreparedEnvelope {
                subject,
                operation,
                admission,
                transport_fingerprint,
                transport,
                parameters,
            })),
            hook_http: None,
            provenance: PreparedCallProvenance::CapturedForeign,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mcp_stdio(
        subject: McpSubject,
        operation: McpOperationKind,
        invocations: Vec<(ForeignToolId, InvocationDigest)>,
        admission: McpAdmissionFingerprint,
        transport_fingerprint: impl Into<String>,
        server: &str,
        spec: SpawnSpec,
        parameters: &Value,
    ) -> Result<Self, crate::mcp::McpProjectionError> {
        subject.validate()?;
        if !operation.matches(&subject) {
            return Err(crate::mcp::McpProjectionError::InvalidIdentity);
        }
        let transport_fingerprint = transport_fingerprint.into();
        if transport_fingerprint.trim().is_empty()
            || transport_fingerprint.chars().any(char::is_control)
        {
            return Err(crate::mcp::McpProjectionError::InvalidFingerprint);
        }
        let transport = ForeignTransportTarget::stdio(server)
            .map_err(|_| crate::mcp::McpProjectionError::InvalidIdentity)?;
        let parameters = serde_json::to_vec(parameters)
            .map_err(|_| crate::mcp::McpProjectionError::InvalidContent)?;
        let mut ops = invocations
            .into_iter()
            .map(|(tool, params_digest)| Op::Opaque {
                tool,
                params_digest,
                transport: transport.clone(),
            })
            .collect::<Vec<_>>();
        ops.push(Op::Spawn { spec });
        Ok(Self {
            tool: subject.operation_name().to_owned(),
            ops,
            mcp: Some(Box::new(McpPreparedEnvelope {
                subject,
                operation,
                admission,
                transport_fingerprint,
                transport,
                parameters,
            })),
            hook_http: None,
            provenance: PreparedCallProvenance::CapturedForeign,
        })
    }

    pub fn mcp_envelope(&self) -> Option<&McpPreparedEnvelope> {
        self.mcp.as_deref()
    }

    pub fn hook_http(
        hook: HookId,
        route: HttpRoute,
        request: &HttpRequest,
        mut secret_sources: Vec<String>,
    ) -> Self {
        secret_sources.sort();
        secret_sources.dedup();
        let request_digest = HttpRequestDigest::of(request);
        let mut header_sinks = request
            .headers()
            .iter()
            .filter(|header| header.is_secret())
            .map(|header| header.name().to_owned())
            .collect::<Vec<_>>();
        header_sinks.sort();
        header_sinks.dedup();
        let secret = (!secret_sources.is_empty() || !header_sinks.is_empty()).then(|| {
            SecretEgressAuthority {
                hook,
                target: route.target().clone(),
                request_digest: request_digest.clone(),
                sources: secret_sources,
                header_sinks,
            }
        });
        let mut ops = vec![Op::Connect {
            target: route.target().clone(),
            request_digest: request_digest.clone(),
        }];
        if let Some(authority) = &secret {
            ops.push(Op::SecretEgress {
                authority: authority.clone(),
            });
        }
        Self {
            tool: format!("hook_http:{hook}"),
            ops,
            mcp: None,
            hook_http: Some(Box::new(HookHttpPreparedEnvelope {
                hook,
                route,
                request_digest,
                secret,
            })),
            provenance: PreparedCallProvenance::Native,
        }
    }

    pub fn hook_http_envelope(&self) -> Option<&HookHttpPreparedEnvelope> {
        self.hook_http.as_deref()
    }

    pub(crate) fn apply_verified_resume_parameters(
        &mut self,
        parameters: &Value,
    ) -> Result<(), crate::mcp::McpProjectionError> {
        let envelope = self
            .mcp
            .as_deref_mut()
            .ok_or(crate::mcp::McpProjectionError::InvalidIdentity)?;
        envelope.parameters = serde_json::to_vec(parameters)
            .map_err(|_| crate::mcp::McpProjectionError::InvalidContent)?;
        Ok(())
    }

    /// Stable within the current contract: every canonical operation, MCP
    /// envelope field and delegation provenance participates, in order,
    /// without exposing their values.
    pub fn authority_fingerprint(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"ego:prepared-call:v2\0");
        digest.update(format!("{self:?}").as_bytes());
        digest.finalize().into()
    }

    pub(super) fn has_captured_foreign_provenance(&self) -> bool {
        self.provenance == PreparedCallProvenance::CapturedForeign
    }

    pub fn foreign_http(
        tool: ForeignToolId,
        params_digest: InvocationDigest,
        transport: ForeignTransportTarget,
        request: &HttpRequest,
    ) -> Self {
        let connect = transport
            .http_route()
            .map(|route| PreparedConnect::new(route.target().clone(), request));
        let rendered = tool.to_string();
        let mut ops = vec![Op::Opaque {
            tool,
            params_digest,
            transport,
        }];
        if let Some(connect) = connect {
            ops.push(Op::Connect {
                target: connect.target().clone(),
                request_digest: connect.request_digest().clone(),
            });
        }
        Self {
            tool: rendered,
            ops,
            mcp: None,
            hook_http: None,
            provenance: PreparedCallProvenance::CapturedForeign,
        }
    }

    pub fn foreign_stdio(
        tool: ForeignToolId,
        params_digest: InvocationDigest,
        server: &str,
        spec: SpawnSpec,
    ) -> Self {
        let rendered = tool.to_string();
        let transport = ForeignTransportTarget::stdio(server)
            .expect("a prepared stdio call must carry a validated server identity");
        Self {
            tool: rendered,
            ops: vec![
                Op::Opaque {
                    tool,
                    params_digest,
                    transport,
                },
                Op::Spawn { spec },
            ],
            mcp: None,
            hook_http: None,
            provenance: PreparedCallProvenance::CapturedForeign,
        }
    }

    pub fn foreign_dispatch(
        outer_tool: ForeignToolId,
        outer_digest: InvocationDigest,
        inner_tool: ForeignToolId,
        inner_digest: InvocationDigest,
        transport: ForeignTransportTarget,
        request: &HttpRequest,
    ) -> Self {
        let connect = transport
            .http_route()
            .map(|route| PreparedConnect::new(route.target().clone(), request));
        let rendered = outer_tool.to_string();
        let mut ops = vec![
            Op::Opaque {
                tool: outer_tool,
                params_digest: outer_digest,
                transport: transport.clone(),
            },
            Op::Opaque {
                tool: inner_tool,
                params_digest: inner_digest,
                transport,
            },
        ];
        if let Some(connect) = connect {
            ops.push(Op::Connect {
                target: connect.target().clone(),
                request_digest: connect.request_digest().clone(),
            });
        }
        Self {
            tool: rendered,
            ops,
            mcp: None,
            hook_http: None,
            provenance: PreparedCallProvenance::CapturedForeign,
        }
    }
}
