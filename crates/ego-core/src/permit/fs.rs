//! `Fs`, the raw filesystem seam, and `GuardedIo`, the permit-checking
//! facade tools actually receive.
//!
//! Filesystem authority consumed by guarded adapters.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::{self, BoxFuture};

use super::SessionProcessAction;
use super::foreign::{
    DuplexHandle, DuplexSpawner, GuardedHttp, HttpRequest, HttpRoute, OneHop, OneHopResponse,
    PreparedConnect, Resolver, TransportError, TransportFailure,
};
use super::grant::{ForeignClaim, Permit, PermitDenied};
use super::path::CanonPath;
use super::prepared_call::{Op, PreparedCall};
use super::scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanRequest};
use super::spawn::{
    DetachedExecMonitor, DetachedExecSnapshot, ProcessError, ProcessExecutor, SpawnSpec,
};
use super::tool::{BEFORE_IMAGE_LIMIT, BeforeImage, ToolError, ToolResult};
use crate::event::id::DetachedExecId;
use crate::hook::EffectScope;
use crate::store::evidence::{EvidencePage, EvidenceRead, EvidenceRef, EvidenceStore};
use crate::suspension::{ContinuationError, GuardedMcpResume};

/// Why a filesystem operation failed, as opposed to being *denied*.
///
/// A refusal and an empty successful result convey different facts.
/// Keep them distinct rather than flattening both into a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound {
        path: CanonPath,
    },
    NotAFile {
        path: CanonPath,
    },
    /// A bounded read found more bytes than its caller admitted.
    ByteLimitExceeded {
        path: CanonPath,
    },
    /// A traversal was rooted at something that is not a directory.
    ///
    /// The mirror of [`FsError::NotAFile`], and the two are not
    /// interchangeable: one says "you asked to read a directory", the other
    /// "you asked to walk a file". A model told the wrong one goes looking in
    /// the wrong place.
    NotADirectory {
        path: CanonPath,
    },
    /// Too many symlinks were chased. A loop, or a chain deep enough to be
    /// indistinguishable from one.
    Loop {
        path: CanonPath,
    },
    /// Anything the platform reported that the variants above do not name.
    /// Carries the message rather than `std::io::Error`, which is neither
    /// `Clone` nor `PartialEq` and would infect every type that holds one.
    Io {
        path: CanonPath,
        message: String,
    },
}

impl FsError {
    pub fn path(&self) -> &CanonPath {
        match self {
            FsError::NotFound { path }
            | FsError::NotAFile { path }
            | FsError::ByteLimitExceeded { path }
            | FsError::NotADirectory { path }
            | FsError::Loop { path }
            | FsError::Io { path, .. } => path,
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound { path } => write!(f, "{path}: no such file or directory"),
            FsError::NotAFile { path } => write!(f, "{path}: not a file"),
            FsError::ByteLimitExceeded { path } => write!(f, "{path}: file exceeds byte limit"),
            FsError::NotADirectory { path } => write!(f, "{path}: not a directory"),
            FsError::Loop { path } => write!(f, "{path}: too many levels of symbolic links"),
            FsError::Io { path, message } => write!(f, "{path}: {message}"),
        }
    }
}

impl std::error::Error for FsError {}

/// Read-only path resolution available at `prepare` time. Deliberately
/// smaller than [`Fs`]: a tool being prepared has no `Permit` yet, so it
/// must not be able to read or write through this seam — only ask what a
/// path currently resolves to.
pub trait PathResolver: Send + Sync {
    /// Resolve `path` to its current canonical target.
    ///
    /// Fallible, because a real filesystem cannot canonicalize what is not
    /// there — and "not there" is exactly what `prepare` is handed when the
    /// model names a file that does not exist.
    fn canonicalize(&self, path: &Path) -> Result<CanonPath, FsError>;
}

/// The raw filesystem seam. Unchecked: nothing here consults a `Permit`.
/// Evidence returned by one exact filesystem write attempt.
#[derive(Debug)]
pub struct FsWriteAttempt {
    pub result: Result<(), FsError>,
    pub checkpoint: Option<String>,
}

/// Guarded write evidence, retained even when the filesystem reports failure.
#[derive(Debug)]
pub struct GuardedWriteAttempt {
    pub result: Result<(), IoDenied>,
    pub before_image: Option<BeforeImage>,
    pub checkpoint: Option<String>,
    pub possibly_modified: bool,
}

pub trait Fs: PathResolver {
    fn write_attempt(&self, path: &CanonPath, bytes: &[u8]) -> FsWriteAttempt {
        FsWriteAttempt {
            result: self.write(path, bytes),
            checkpoint: None,
        }
    }
    /// User-visible receipt for the last successful builtin write.
    fn write_notice(&self, _path: &CanonPath) -> Option<String> {
        None
    }

    /// The checkpoint the last successful builtin write of `path` created.
    fn last_checkpoint(&self, _path: &CanonPath) -> Option<String> {
        None
    }

    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, FsError>;

    /// Read a regular file within `limit`, or return `ByteLimitExceeded`.
    /// OS adapters must reject special files before a potentially blocking open
    /// and bound the read before allocation (one sentinel byte may detect overflow).
    fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, FsError> {
        let bytes = self.read(path)?;
        if bytes.len() > limit {
            return Err(FsError::ByteLimitExceeded { path: path.clone() });
        }
        Ok(bytes)
    }

    /// Replace the whole file at `path` with `bytes`.
    ///
    /// A persistent implementation publishes the replacement atomically and
    /// makes both its contents and directory entry durable before returning
    /// success. An in-memory test double has no durability boundary, but must
    /// still make the replacement visible as one operation.
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), FsError>;

    /// Walk the subtree `req` names, honouring its skips and limits.
    ///
    /// **An implementation must not descend into a directory symlink.** A walk
    /// resolves many paths where a read resolves one, and a symlinked
    /// directory inside an in-scope root is an escape the root check cannot
    /// see. Refusing to follow them closes it, and makes cycles impossible
    /// rather than merely detectable. A symlinked *file* is still resolved and
    /// still reported, because that is one path and the permit check handles
    /// it.
    fn scan(&self, req: &ScanRequest) -> Result<Scan, FsError>;
}

/// Every method re-derives the current canonical target before checking the permit:
/// trusting a path resolved back at `prepare` time would let a symlink swapped in between
/// `prepare` and `execute` slip a write through to a target that was never checked.
///
/// That is the window this seam closes, and it is worth being precise about which one it
/// is. The check and the use still name the target by *path*, not by handle, so a
/// component swapped between `allows_read` and the open is resolved a second time and is
/// not re-checked.
pub trait GuardedIo<'s>: Send + Sync {
    fn write_attempt(&self, path: &CanonPath, bytes: &[u8]) -> GuardedWriteAttempt {
        let result = self.write(path, bytes);
        GuardedWriteAttempt {
            possibly_modified: true,
            result,
            before_image: Some(BeforeImage::Omitted),
            checkpoint: None,
        }
    }
    fn write_notice(&self, _path: &CanonPath) -> Option<String> {
        None
    }

    fn last_checkpoint(&self, _path: &CanonPath) -> Option<String> {
        None
    }

    /// Atomically and durably replace one permit-checked whole file.
    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), IoDenied>;

    /// [`write`](Self::write), and report what the write replaced for the
    /// terminal diff. Needs only the write authority: the image never goes to
    /// the model. A default implementation reports [`BeforeImage::Omitted`].
    fn write_capturing(&self, path: &CanonPath, bytes: &[u8]) -> Result<BeforeImage, IoDenied> {
        self.write(path, bytes)?;
        Ok(BeforeImage::Omitted)
    }

    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, IoDenied>;

    fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, IoDenied> {
        let bytes = self.read(path)?;
        if bytes.len() > limit {
            return Err(FsError::ByteLimitExceeded { path: path.clone() }.into());
        }
        Ok(bytes)
    }

    /// The names under `req.root`, and nothing else about them.
    fn list(&self, req: &ScanRequest) -> Result<Scan, IoDenied>;

    /// The lines under `req.root` that `matcher` accepts.
    ///
    /// The guarded seam reads; the caller supplies judgment and receives
    /// verdicts. This permits content search without granting subtree read access.
    fn search(&self, req: &ScanRequest, matcher: &dyn LineMatcher) -> Result<Found, IoDenied>;

    /// Execute the exact process specification admitted by this permit.
    fn exec<'a>(&'a self, spec: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>>;

    fn exec_background<'a>(
        &'a self,
        _id: DetachedExecId,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<(), ProcessError>> {
        Box::pin(future::ready(Err(ProcessError::Invariant(
            "guarded I/O has no background process owner".to_owned(),
        ))))
    }

    fn background_status(&self, _id: DetachedExecId) -> Result<DetachedExecSnapshot, ProcessError> {
        Err(ProcessError::Invariant(
            "guarded I/O has no background process owner".into(),
        ))
    }

    fn background_cancel(&self, _id: DetachedExecId) -> Result<(), ProcessError> {
        Err(ProcessError::Invariant(
            "guarded I/O has no background process owner".into(),
        ))
    }

    fn background_monitor(
        &self,
        _id: DetachedExecId,
        _monitor: DetachedExecMonitor,
    ) -> Result<(), ProcessError> {
        Err(ProcessError::Invariant(
            "guarded I/O has no background process owner".into(),
        ))
    }

    /// Read the exact bounded evidence request admitted inside this session.
    fn read_evidence<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, ToolError>>;

    fn guarded_http(
        &self,
        call: PreparedCall,
        request: HttpRequest,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>>;

    /// Claim one selected MCP HTTP operation after policy has authorized its
    /// semantic call. The returned capability is non-clone and binds one
    /// protocol lifecycle to the exact admitted route; its protocol owner
    /// enforces exactly one rendered selected request.
    fn claim_mcp_http(&self, call: PreparedCall) -> Result<McpHttpClaim, TransportError>;

    /// Follow one same-origin redirect of the admitted native web request.
    ///
    /// The seam owns the bound: the hop reuses the exact admitted route (scheme,
    /// host, port), sends a header-free and body-free GET for `path`, and is
    /// refused before the first admitted web request and after
    /// [`MAX_WEB_REDIRECTS`] hops. Network rules match only host and port, so a
    /// hop inherits no authority the policy did not already grant.
    fn guarded_web_redirect(
        &self,
        _path: &str,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(future::ready(Err(authority_mismatch())))
    }

    fn claim_mcp_acp(&self, _call: PreparedCall) -> Result<McpAcpClaim, TransportError> {
        Err(authority_mismatch())
    }

    /// Consume the exact selected stdio request authority without spawning.
    fn claim_mcp_stdio(&self, _call: PreparedCall) -> Result<McpStdioClaim, TransportError> {
        Err(authority_mismatch())
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn take_mcp_resume(&self) -> Result<GuardedMcpResume<'s>, ContinuationError> {
        Err(ContinuationError::AuthorityMismatch)
    }

    fn guarded_duplex(
        &self,
        call: PreparedCall,
    ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>>;

    /// Persist complete sanitized foreign evidence in the session store.
    fn store_evidence(&self, bytes: &[u8]) -> Result<EvidenceRef, ToolError>;
}

/// One selected MCP HTTP lifecycle authority.
///
/// The value is deliberately non-clone. Its protocol owner may borrow it for
/// the fixed management frames surrounding one selected operation, while that
/// owner separately enforces that the fingerprinted selected frame is spent
/// exactly once.
pub struct McpHttpClaim {
    envelope: super::prepared_call::McpPreparedEnvelope,
    route: super::foreign::HttpRoute,
    http: GuardedHttp,
}

/// A spent, non-clone authority for one selected call over an existing ACP channel.
/// Fresh semantic request authority, independent of a session child lifetime.
pub struct McpStdioClaim {
    envelope: super::prepared_call::McpPreparedEnvelope,
    spec: SpawnSpec,
    confinement: super::Confinement,
}

impl McpStdioClaim {
    pub fn envelope(&self) -> &super::prepared_call::McpPreparedEnvelope {
        &self.envelope
    }
    pub fn spec(&self) -> &SpawnSpec {
        &self.spec
    }
    pub fn confinement(&self) -> super::Confinement {
        self.confinement
    }
    /// Consume spawn authority only when a fresh child is required.
    pub fn into_spawn_spec(self) -> SpawnSpec {
        self.spec.session_lifetime()
    }
}

pub struct McpAcpClaim {
    envelope: super::prepared_call::McpPreparedEnvelope,
}

impl McpAcpClaim {
    pub fn envelope(&self) -> &super::prepared_call::McpPreparedEnvelope {
        &self.envelope
    }
}

impl McpHttpClaim {
    pub fn envelope(&self) -> &super::prepared_call::McpPreparedEnvelope {
        &self.envelope
    }

    pub fn exchange(
        &self,
        request: HttpRequest,
        deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let connect = PreparedConnect::new(self.route.target().clone(), &request);
        self.http
            .execute_admitted(self.route.clone(), connect, request, deadline)
    }
}

/// Why a guarded operation did not produce what was asked for: the permit
/// said no, or the filesystem did.
///
/// Two variants and not one string, because a model that is told "denied"
/// should stop asking, and a model that is told "not found" should try
/// somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoDenied {
    Permit(PermitDenied),
    Fs(FsError),
}

impl fmt::Display for IoDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoDenied::Permit(denied) => write!(f, "{denied}"),
            IoDenied::Fs(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for IoDenied {}

impl From<PermitDenied> for IoDenied {
    fn from(denied: PermitDenied) -> Self {
        IoDenied::Permit(denied)
    }
}

impl From<FsError> for IoDenied {
    fn from(err: FsError) -> Self {
        IoDenied::Fs(err)
    }
}

/// The only implementation of [`GuardedIo`] in this crate: a [`Permit`]
/// paired with the raw effect ports it governs.
///
/// **Owns the permit rather than borrowing it.** The seam that enforces a
/// grant is where the grant should end up: once a `GuardedEffects` exists the
/// permit is inside it, unreachable, and it dies with the seam. A borrowed
/// permit meant somebody upstream still held one for the duration of the
/// call, and that somebody was the only remaining place a permit and a
/// foreign call could be brought together.
///
/// It is also the only shape that compiles. With `permit: &'a Permit`, a
/// caller cannot both hand an [`Authorized`](super::Authorized) to
/// `Tool::execute` by value and keep a `GuardedEffects` built from that same
/// permit — `E0505`, move out of a borrowed value, whose suggested fix is
/// `Clone` on `Authorized`. That is the one thing Invariant W forbids.
pub struct GuardedEffects<'effects, R = GuardedMcpResume<'effects>> {
    permit: Permit,
    fs: &'effects dyn Fs,
    process: &'effects dyn ProcessExecutor,
    evidence: &'effects dyn EvidenceStore,
    http: GuardedHttp,
    duplex: Arc<dyn DuplexSpawner>,
    foreign_spent: AtomicBool,
    /// The admitted native web route, whether an owner grant admitted its
    /// private addresses, and the redirect hops it spent.
    web_redirects: Mutex<Option<(HttpRoute, bool, u8)>>,
    resume: Mutex<Option<R>>,
    cancel: Option<crate::turn::CancelToken>,
}

impl<'effects> GuardedEffects<'effects> {
    pub fn new(
        permit: Permit,
        fs: &'effects dyn Fs,
        process: &'effects dyn ProcessExecutor,
        evidence: &'effects dyn EvidenceStore,
        resolver: Arc<dyn Resolver>,
        one_hop: Arc<dyn OneHop>,
        duplex: Arc<dyn DuplexSpawner>,
    ) -> Self {
        Self::new_with_resume(
            permit, fs, process, evidence, resolver, one_hop, duplex, None,
        )
    }
}

impl<'effects, 'resume> GuardedEffects<'effects, GuardedMcpResume<'resume>> {
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_resume(
        permit: Permit,
        fs: &'effects dyn Fs,
        process: &'effects dyn ProcessExecutor,
        evidence: &'effects dyn EvidenceStore,
        resolver: Arc<dyn Resolver>,
        one_hop: Arc<dyn OneHop>,
        duplex: Arc<dyn DuplexSpawner>,
        resume: Option<GuardedMcpResume<'resume>>,
    ) -> Self {
        Self {
            permit,
            fs,
            process,
            evidence,
            http: GuardedHttp::new(resolver, one_hop),
            duplex,
            foreign_spent: AtomicBool::new(false),
            web_redirects: Mutex::new(None),
            resume: Mutex::new(resume),
            cancel: None,
        }
    }

    pub fn with_cancel(mut self, cancel: Option<crate::turn::CancelToken>) -> Self {
        self.cancel = cancel;
        self
    }

    fn claim_foreign(
        &self,
        call: &PreparedCall,
        kind: ForeignKind,
        request: Option<&HttpRequest>,
    ) -> Result<ForeignClaim, TransportError> {
        self.permit.allows_call(call)?;
        let claim = ForeignClaim::from_call(call).ok_or_else(authority_mismatch)?;
        if !claim.invocations.is_empty() {
            self.permit
                .allows_foreign(&claim.invocations, &claim.transport)?;
        }
        if let Some(connect) = &claim.connect {
            self.permit.allows_network(connect)?;
        }
        if let Some(target) = &claim.protocol_target {
            self.permit.allows_protocol_network(target)?;
        }
        if let Some(spawn) = &claim.spawn {
            self.permit
                .allows_spawn(spawn)
                .map_err(|_| authority_mismatch())?;
        }
        match kind {
            ForeignKind::Http
                if claim
                    .transport
                    .http_route()
                    .zip(claim.connect.as_ref())
                    .zip(request)
                    .is_some_and(|((route, connect), request)| {
                        route.target() == connect.target() && connect.matches(request)
                    })
                    && claim.spawn.is_none() => {}
            ForeignKind::SelectedMcp
                if call.mcp_envelope().is_some()
                    && claim.protocol_target.as_ref()
                        == claim
                            .transport
                            .http_route()
                            .map(super::foreign::HttpRoute::target)
                    && claim.connect.is_none()
                    && claim.spawn.is_none()
                    && request.is_none() => {}
            ForeignKind::Acp
                if call.mcp_envelope().is_some()
                    && claim.transport.is_acp()
                    && claim.connect.is_none()
                    && claim.protocol_target.is_none()
                    && claim.spawn.is_none()
                    && request.is_none() => {}
            ForeignKind::Duplex
                if claim.transport.is_stdio()
                    && claim.connect.is_none()
                    && request.is_none()
                    && claim.spawn.is_some() => {}
            _ => return Err(authority_mismatch()),
        }
        self.foreign_spent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| authority_spent())?;
        Ok(claim)
    }

    pub fn guarded_http(
        &self,
        call: PreparedCall,
        request: HttpRequest,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        if let Some((route, expected)) = call.web_http_envelope() {
            let checked = self.permit.allows_call(&call).and_then(|()| {
                self.permit
                    .allows_network(&PreparedConnect::new(route.target().clone(), &request))
            });
            let route = route.clone();
            let matches = expected == &request && call.ops().len() == 1;
            let owner_endpoint = self.permit.owner_private_network();
            let http = self.http.clone();
            let spent = self.foreign_spent.swap(true, Ordering::AcqRel);
            if checked.is_ok() && matches && !spent {
                *self
                    .web_redirects
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((route.clone(), owner_endpoint, 0));
            }
            return Box::pin(async move {
                checked?;
                if !matches || spent {
                    return Err(authority_mismatch());
                }
                let deadline = http.begin_call();
                let connect = PreparedConnect::new(route.target().clone(), &request);
                http.execute_web_admitted(route, connect, request, deadline, owner_endpoint)
                    .await
            });
        }
        let claim = self.claim_foreign(&call, ForeignKind::Http, Some(&request));
        let http = self.http.clone();
        Box::pin(async move {
            let claim = claim?;
            let deadline = http.begin_call();
            let route = claim
                .transport
                .into_http_route()
                .ok_or_else(authority_mismatch)?;
            let connect = claim.connect.ok_or_else(authority_mismatch)?;
            http.execute_admitted(route, connect, request, deadline)
                .await
        })
    }

    pub fn guarded_web_redirect(
        &self,
        path: &str,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let admitted = {
            let mut state = self
                .web_redirects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.as_mut() {
                None => Err(authority_mismatch()),
                Some((_, _, hops)) if *hops >= MAX_WEB_REDIRECTS => Err(authority_spent()),
                Some((route, owner_endpoint, hops)) => {
                    *hops += 1;
                    HttpRequest::get(path)
                        .map(|request| (route.clone(), *owner_endpoint, request))
                        .map_err(|_| authority_mismatch())
                }
            }
        };
        let http = self.http.clone();
        Box::pin(async move {
            let (route, owner_endpoint, request) = admitted?;
            let deadline = http.begin_call();
            let connect = PreparedConnect::new(route.target().clone(), &request);
            http.execute_web_admitted(route, connect, request, deadline, owner_endpoint)
                .await
        })
    }

    pub fn exec_handler<'b>(
        &'b self,
        spec: &'b SpawnSpec,
        input: &'b [u8],
        scope: EffectScope,
    ) -> BoxFuture<'b, Result<ToolResult, ProcessError>> {
        if let Err(denied) = self.permit.allows_spawn(spec) {
            return Box::pin(future::ready(Err(ProcessError::Invariant(
                denied.to_string(),
            ))));
        }
        self.process.execute_handler(spec, input, scope)
    }

    pub fn store_handler_evidence(&self, bytes: &[u8]) -> Result<EvidenceRef, ToolError> {
        <Self as GuardedIo<'resume>>::store_evidence(self, bytes)
    }

    pub fn guarded_hook_http(
        &self,
        call: PreparedCall,
        request: HttpRequest,
        scope: EffectScope,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        if scope.is_cancelled() {
            return Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::Cancelled,
            ))));
        }
        if std::time::Instant::now() >= scope.deadline() {
            return Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::TimedOut,
            ))));
        }
        let envelope = call.hook_http_envelope().cloned();
        let claim = envelope.and_then(|envelope| {
            if envelope.request_digest() != &super::HttpRequestDigest::of(&request) {
                return None;
            }
            let connect = PreparedConnect::from_parts(
                envelope.route().target().clone(),
                envelope.request_digest().clone(),
            );
            let mut saw_connect = false;
            let mut saw_secret = envelope.secret().is_none();
            for op in call.ops() {
                match op {
                    Op::Connect {
                        target,
                        request_digest,
                    } if target == connect.target()
                        && request_digest == connect.request_digest()
                        && !saw_connect =>
                    {
                        saw_connect = true
                    }
                    Op::SecretEgress { authority }
                        if envelope.secret() == Some(authority) && !saw_secret =>
                    {
                        saw_secret = true;
                    }
                    _ => return None,
                }
            }
            (saw_connect && saw_secret).then_some((envelope, connect))
        });
        let claimed = claim
            .ok_or_else(authority_mismatch)
            .and_then(|(envelope, connect)| {
                self.permit.allows_network(&connect)?;
                if let Some(secret) = envelope.secret() {
                    self.permit.allows_secret_egress(secret)?;
                }
                self.foreign_spent
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| authority_spent())?;
                Ok((envelope, connect))
            });
        let http = self.http.clone();
        Box::pin(async move {
            let (envelope, connect) = claimed?;
            http.execute_admitted(envelope.route().clone(), connect, request, scope.deadline())
                .await
        })
    }

    pub fn claim_mcp_http(&self, call: PreparedCall) -> Result<McpHttpClaim, TransportError> {
        self.http.admit_mcp()?;
        let envelope = call
            .mcp_envelope()
            .cloned()
            .ok_or_else(authority_mismatch)?;
        let claim = self.claim_foreign(&call, ForeignKind::SelectedMcp, None)?;
        let route = claim
            .transport
            .into_http_route()
            .ok_or_else(authority_mismatch)?;
        claim.protocol_target.ok_or_else(authority_mismatch)?;
        Ok(McpHttpClaim {
            envelope,
            route,
            http: self.http.clone(),
        })
    }

    pub fn claim_mcp_acp(&self, call: PreparedCall) -> Result<McpAcpClaim, TransportError> {
        let envelope = call
            .mcp_envelope()
            .cloned()
            .ok_or_else(authority_mismatch)?;
        self.claim_foreign(&call, ForeignKind::Acp, None)?;
        Ok(McpAcpClaim { envelope })
    }

    pub fn claim_mcp_stdio(&self, call: PreparedCall) -> Result<McpStdioClaim, TransportError> {
        let envelope = call
            .mcp_envelope()
            .cloned()
            .ok_or_else(authority_mismatch)?;
        let claim = self.claim_foreign(&call, ForeignKind::Duplex, None)?;
        let spec = claim.spawn.ok_or_else(authority_mismatch)?;
        self.process
            .revalidate(&spec)
            .map_err(|_| authority_mismatch())?;
        let confinement = self.process.confinement().ok_or_else(authority_mismatch)?;
        Ok(McpStdioClaim {
            envelope,
            spec,
            confinement,
        })
    }

    pub fn guarded_duplex(
        &self,
        call: PreparedCall,
    ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        let claim = self.claim_foreign(&call, ForeignKind::Duplex, None);
        let duplex = Arc::clone(&self.duplex);
        Box::pin(async move {
            let claim = claim?;
            let spec = claim.spawn.ok_or_else(authority_mismatch)?;
            duplex.spawn(spec).await
        })
    }

    /// Re-resolve `root`, check it against the permit with `allows`, and hand
    /// back a request rooted at the *fresh* target.
    ///
    /// Validate the walk request before opening any path.
    fn admit(
        &self,
        req: &ScanRequest,
        allows: fn(&Permit, &CanonPath) -> Result<(), PermitDenied>,
    ) -> Result<ScanRequest, IoDenied> {
        let fresh = self.fs.canonicalize(req.root.as_path())?;
        allows(&self.permit, &fresh)?;
        Ok(ScanRequest {
            root: fresh,
            skip_dirs: req.skip_dirs.clone(),
            respect_ignore: req.respect_ignore,
            limits: req.limits.clone(),
        })
    }

    /// Re-derive what a write is about to land on, which may not exist yet.
    ///
    /// [`read`](GuardedIo::read) can simply canonicalize: a file it is about
    /// to read is on disk or the call was a mistake. A write's target
    /// frequently is not on disk — creating a file is most of what writing is
    /// for — and `Fs::canonicalize` cannot resolve what is not there, so the
    /// two cases are separated here rather than in every caller:
    ///
    /// - **The target exists.** It is judged as what it *is*. A final
    ///   component that is a symlink out of the workspace resolves to its
    ///   target, so the permit is shown the file the bytes would actually
    ///   reach — the same rule `Fs::scan` follows when it reports a symlinked
    ///   file under its canonical name.
    /// - **It does not.** The *parent* is resolved and the final component
    ///   appended, so every component but the last is still chased. The last
    ///   is the one a creating write must be allowed not to find.
    ///
    /// A missing parent stays `NotFound`, naming the parent. A write is not a
    /// `mkdir -p`: creating directories is an effect the call did not
    /// describe and the permit was never shown.
    fn write_target(&self, path: &CanonPath) -> Result<CanonPath, IoDenied> {
        match self.fs.canonicalize(path.as_path()) {
            Ok(fresh) => Ok(fresh),
            Err(FsError::NotFound { .. }) => {
                let named = path.as_path();
                let (Some(parent), Some(name)) = (named.parent(), named.file_name()) else {
                    // A path that is a root, or ends in `..`. There is no file
                    // for a write to name here, and the error says which path
                    // was refused rather than which component was missing.
                    return Err(FsError::NotAFile { path: path.clone() }.into());
                };
                let parent = self.fs.canonicalize(parent)?;
                Ok(CanonPath::new(parent.as_path().join(name)))
            }
            Err(other) => Err(other.into()),
        }
    }

    fn exact_target(&self, prepared: &CanonPath, fresh: CanonPath) -> Result<CanonPath, IoDenied> {
        if &fresh != prepared {
            return Err(FsError::Io {
                path: prepared.clone(),
                message: "prepared target changed before use".into(),
            }
            .into());
        }
        Ok(fresh)
    }

    /// Check every entry a traversal yielded, and fail the whole operation if
    /// any of them left the authorised subtree.
    ///
    /// Whole-operation, not per-entry: dropping the offenders and returning
    /// the rest is the silent post-filtering the previous method exists to
    /// avoid. An escape means the tree moved under the walk, and a partial
    /// answer assembled from a tree that moved is not worth handing to a
    /// model.
    fn all_in_scope(
        &self,
        entries: &[Entry],
        allows: fn(&Permit, &CanonPath) -> Result<(), PermitDenied>,
    ) -> Result<(), IoDenied> {
        for entry in entries {
            allows(&self.permit, &entry.path)?;
        }
        Ok(())
    }
}

/// Project protected assignments before any bytes or matching verdicts reach a tool.
/// An explicit owner disclosure (`Permit::discloses`) returns the file bytes unchanged.
fn credential_keys(permit: &Permit, path: &CanonPath, bytes: Vec<u8>) -> Result<Vec<u8>, IoDenied> {
    if !crate::config::Ruleset::is_credential_path(path) || permit.discloses(path) {
        return Ok(bytes);
    }
    let refused = || FsError::Io {
        path: path.clone(),
        message: "protected credential file has no supported key names format".into(),
    };
    let name = path
        .as_path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let env = name.starts_with(".env");
    let aws = path
        .as_path()
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == ".aws")
        && matches!(name, "credentials" | "config");
    if !env && !aws {
        return Err(refused().into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| refused())?;
    let mut keys = String::new();
    for line in text.lines() {
        let line = line.trim();
        let line = if env {
            line.strip_prefix("export ").unwrap_or(line).trim_start()
        } else {
            line
        };
        if line.is_empty()
            || line.starts_with('#')
            || (aws && (line.starts_with(';') || (line.starts_with('[') && line.ends_with(']'))))
        {
            keys.push('\n');
            continue;
        }
        let (key, _) = line.split_once('=').ok_or_else(refused)?;
        let key = key.trim();
        if key.is_empty()
            || !key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || byte == b'_' || (aws && matches!(byte, b'-' | b'.'))
            })
            || (env && key.as_bytes()[0].is_ascii_digit())
        {
            return Err(refused().into());
        }
        keys.push_str(key);
        keys.push('\n');
    }
    Ok(keys.into_bytes())
}

impl<'effects, 'resume> GuardedIo<'resume> for GuardedEffects<'effects, GuardedMcpResume<'resume>> {
    fn write_notice(&self, path: &CanonPath) -> Option<String> {
        self.fs.write_notice(path)
    }

    fn last_checkpoint(&self, path: &CanonPath) -> Option<String> {
        self.fs.last_checkpoint(path)
    }

    fn write(&self, path: &CanonPath, bytes: &[u8]) -> Result<(), IoDenied> {
        // Re-resolve now, at use time, rather than trusting `path` — see
        // the trait doc comment. `write_target` and not `canonicalize`,
        // because a write's target is allowed not to exist yet.
        let fresh = self.exact_target(path, self.write_target(path)?)?;
        self.permit.allows_write(&fresh)?;
        Ok(self.fs.write(&fresh, bytes)?)
    }

    fn write_attempt(&self, path: &CanonPath, bytes: &[u8]) -> GuardedWriteAttempt {
        let fresh = match self.write_target(path).and_then(|fresh| {
            let fresh = self.exact_target(path, fresh)?;
            self.permit.allows_write(&fresh)?;
            Ok(fresh)
        }) {
            Ok(fresh) => fresh,
            Err(error) => {
                return GuardedWriteAttempt {
                    result: Err(error),
                    before_image: None,
                    checkpoint: None,
                    possibly_modified: false,
                };
            }
        };
        let protected =
            crate::config::Ruleset::is_credential_path(&fresh) && !self.permit.discloses(&fresh);
        let image = if protected {
            BeforeImage::Omitted
        } else {
            match self.fs.read_bounded(&fresh, BEFORE_IMAGE_LIMIT) {
                Ok(old) => String::from_utf8(old).map_or(BeforeImage::Omitted, BeforeImage::Text),
                Err(FsError::NotFound { .. }) => BeforeImage::New,
                Err(_) => BeforeImage::Omitted,
            }
        };
        let attempt = self.fs.write_attempt(&fresh, bytes);
        GuardedWriteAttempt {
            result: attempt.result.map_err(Into::into),
            before_image: Some(image),
            checkpoint: attempt.checkpoint,
            possibly_modified: true,
        }
    }

    fn write_capturing(&self, path: &CanonPath, bytes: &[u8]) -> Result<BeforeImage, IoDenied> {
        let fresh = self.exact_target(path, self.write_target(path)?)?;
        self.permit.allows_write(&fresh)?;
        let protected =
            crate::config::Ruleset::is_credential_path(&fresh) && !self.permit.discloses(&fresh);
        let image = if protected {
            BeforeImage::Omitted
        } else {
            match self.fs.read_bounded(&fresh, BEFORE_IMAGE_LIMIT) {
                Ok(old) => String::from_utf8(old).map_or(BeforeImage::Omitted, BeforeImage::Text),
                Err(FsError::NotFound { .. }) => BeforeImage::New,
                Err(_) => BeforeImage::Omitted,
            }
        };
        self.fs.write(&fresh, bytes)?;
        Ok(image)
    }

    fn read(&self, path: &CanonPath) -> Result<Vec<u8>, IoDenied> {
        let fresh = self.exact_target(path, self.fs.canonicalize(path.as_path())?)?;
        self.permit.allows_read(&fresh)?;
        credential_keys(&self.permit, &fresh, self.fs.read(&fresh)?)
    }

    fn read_bounded(&self, path: &CanonPath, limit: usize) -> Result<Vec<u8>, IoDenied> {
        let fresh = self.exact_target(path, self.fs.canonicalize(path.as_path())?)?;
        self.permit.allows_read(&fresh)?;
        credential_keys(&self.permit, &fresh, self.fs.read_bounded(&fresh, limit)?)
    }

    fn list(&self, req: &ScanRequest) -> Result<Scan, IoDenied> {
        let admitted = self.admit(req, Permit::allows_list)?;
        let scan = self.fs.scan(&admitted)?;
        self.all_in_scope(&scan.entries, Permit::allows_list)?;
        Ok(scan)
    }

    fn search(&self, req: &ScanRequest, matcher: &dyn LineMatcher) -> Result<Found, IoDenied> {
        let admitted = self.admit(req, Permit::allows_search)?;
        let scan = match self.fs.scan(&admitted) {
            Ok(scan) => scan,
            // Search authority can name one file; list authority still requires
            // a directory. Keep the same re-resolution and byte budget below.
            Err(FsError::NotADirectory { path }) if path == admitted.root => {
                if admitted.limits.max_entries == 0 {
                    Scan::new(Vec::new(), true)
                } else {
                    Scan::new(
                        vec![Entry {
                            relative: path.as_path().file_name().unwrap_or_default().into(),
                            path,
                            kind: EntryKind::File,
                        }],
                        false,
                    )
                }
            }
            Err(error) => return Err(error.into()),
        };
        self.all_in_scope(&scan.entries, Permit::allows_search)?;

        let mut matches = Vec::new();
        let mut budget = admitted.limits.max_bytes;
        let mut truncated = scan.truncated;

        for entry in scan.entries.iter().filter(|e| e.kind == EntryKind::File) {
            // The scan's path was true when the directory entry was visited,
            // not necessarily now. Resolve it again immediately before the
            // read and cash the search authority against that fresh target.
            // A swapped symlink that leaves the grant is a denial of the
            // whole search; bytes from the escaped target are never opened.
            let fresh = match self.fs.canonicalize(entry.path.as_path()) {
                Ok(fresh) => fresh,
                // Preserve the traversal contract below: a file that
                // disappears or becomes unreadable mid-search is skipped.
                Err(_) => continue,
            };
            self.permit.allows_search(&fresh)?;

            // A file the OS refuses mid-traversal is skipped, not fatal: one
            // unreadable file in a tree is a fact about that file, and
            // abandoning the whole search over it would make grep unusable in
            // any directory containing a socket or a root-owned log.
            let limit = usize::try_from(budget).unwrap_or(usize::MAX);
            let bytes = match self.fs.read_bounded(&fresh, limit) {
                Ok(bytes) => bytes,
                Err(FsError::ByteLimitExceeded { .. }) => {
                    truncated = true;
                    break;
                }
                Err(_) => continue,
            };
            budget -= bytes.len() as u64;

            let Ok(bytes) = credential_keys(&self.permit, &fresh, bytes) else {
                // Unsupported credential formats are not searchable as raw bytes.
                continue;
            };

            // Lossy, and on purpose: a binary file is not a reason to fail a
            // text search, and a matcher that never accepts a replacement
            // character simply will not match one.
            for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if matcher.matches(line) {
                    matches.push(Match {
                        path: fresh.clone(),
                        line: index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }

        Ok(Found { matches, truncated })
    }

    fn exec<'a>(&'a self, spec: &'a SpawnSpec) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        if let Err(denied) = self.permit.allows_spawn(spec) {
            return Box::pin(future::ready(Err(ProcessError::Invariant(
                denied.to_string(),
            ))));
        }
        self.process.execute(spec)
    }

    fn exec_background<'a>(
        &'a self,
        id: DetachedExecId,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<(), ProcessError>> {
        if let Err(denied) = self.permit.allows_spawn(spec) {
            return Box::pin(future::ready(Err(ProcessError::Invariant(
                denied.to_string(),
            ))));
        }
        self.process.start_background(id, spec)
    }

    fn background_status(&self, id: DetachedExecId) -> Result<DetachedExecSnapshot, ProcessError> {
        self.permit
            .allows_session_process(id, SessionProcessAction::Inspect)
            .map_err(|denied| ProcessError::Invariant(denied.to_string()))?;
        self.process.background_status(id)
    }

    fn background_cancel(&self, id: DetachedExecId) -> Result<(), ProcessError> {
        self.permit
            .allows_session_process(id, SessionProcessAction::Cancel)
            .map_err(|denied| ProcessError::Invariant(denied.to_string()))?;
        self.process.cancel_background(id)
    }

    fn background_monitor(
        &self,
        id: DetachedExecId,
        monitor: DetachedExecMonitor,
    ) -> Result<(), ProcessError> {
        // Watching new lines reads the same output `exec_status` reads.
        self.permit
            .allows_session_process(id, SessionProcessAction::Inspect)
            .map_err(|denied| ProcessError::Invariant(denied.to_string()))?;
        self.process.monitor_background(id, monitor)
    }

    fn read_evidence<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, ToolError>> {
        if let Err(denied) = self.permit.allows_evidence(request) {
            return Box::pin(future::ready(Err(denied.into())));
        }
        Box::pin(async move { self.evidence.read_page(request).await.map_err(Into::into) })
    }

    fn guarded_http(
        &self,
        call: PreparedCall,
        request: HttpRequest,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        GuardedEffects::guarded_http(self, call, request)
    }

    fn claim_mcp_http(&self, call: PreparedCall) -> Result<McpHttpClaim, TransportError> {
        GuardedEffects::claim_mcp_http(self, call)
    }

    fn guarded_web_redirect(
        &self,
        path: &str,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        GuardedEffects::guarded_web_redirect(self, path)
    }

    fn claim_mcp_acp(&self, call: PreparedCall) -> Result<McpAcpClaim, TransportError> {
        GuardedEffects::claim_mcp_acp(self, call)
    }

    fn claim_mcp_stdio(&self, call: PreparedCall) -> Result<McpStdioClaim, TransportError> {
        GuardedEffects::claim_mcp_stdio(self, call)
    }

    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(crate::turn::CancelToken::is_cancelled)
    }

    fn take_mcp_resume(&self) -> Result<GuardedMcpResume<'resume>, ContinuationError> {
        self.resume
            .lock()
            .map_err(|_| ContinuationError::Unavailable("resume authority lock poisoned".into()))?
            .take()
            .ok_or(ContinuationError::AuthorityMismatch)
    }

    fn guarded_duplex(
        &self,
        call: PreparedCall,
    ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        GuardedEffects::guarded_duplex(self, call)
    }

    fn store_evidence(&self, bytes: &[u8]) -> Result<EvidenceRef, ToolError> {
        let mut writer = self.evidence.begin()?;
        if let Err(error) = writer.append(bytes) {
            let _ = writer.abort();
            return Err(error.into());
        }
        writer.commit().map_err(Into::into)
    }
}

enum ForeignKind {
    Http,
    SelectedMcp,
    Acp,
    Duplex,
}

/// Same-origin redirect hops one admitted `web_fetch` request may follow.
pub const MAX_WEB_REDIRECTS: u8 = 5;

fn authority_mismatch() -> TransportError {
    TransportError::definitely_unsent(TransportFailure::AuthorityMismatch)
}

fn authority_spent() -> TransportError {
    TransportError::definitely_unsent(TransportFailure::AuthoritySpent)
}
