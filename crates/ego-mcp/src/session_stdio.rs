//! ACP session ownership of guarded stdio children; no request is replayed.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use ego_core::config::McpServerName;
use ego_core::event::id::SessionId;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{Confinement, McpStdioClaim, ProcessExecutor, SpawnSpec};
use tokio::sync::Mutex as AsyncMutex;

use crate::client::McpClientInventory;
use crate::{
    McpClient, McpClientOptions, McpDuplexEffects, McpDuplexEffectsFactory, McpError, McpFailure,
};

/// Transport owner for the stdio servers of exactly one ACP session.
/// The session scope supplies its measured process adapter and closes this owner.
pub struct McpStdioSession {
    self_ref: Weak<Self>,
    session: SessionId,
    management: Arc<dyn McpDuplexEffectsFactory>,
    process: Arc<dyn ProcessExecutor>,
    confinement: Confinement,
    closed: AtomicBool,
    children: Mutex<HashMap<McpServerName, Arc<AsyncMutex<SessionChild>>>>,
}

pub(crate) struct SessionChild {
    pub(crate) client: Option<McpClient>,
    binding: Option<SpawnSpec>,
    generation: Option<Arc<str>>,
    failures: u32,
    retry_at: Option<Instant>,
    reap: Option<tokio::task::JoinHandle<()>>,
}

impl SessionChild {
    fn new() -> Self {
        Self {
            client: None,
            binding: None,
            generation: None,
            failures: 0,
            retry_at: None,
            reap: None,
        }
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(reap) = self.reap.take() {
            let _ = reap.await;
        }
        if let Some(client) = self.client.take() {
            client.cancellation().cancel();
            // Interrupt drives the OS supervisor and joins reap, independent of
            // a caller abandoning its operation future.
            let _ = client.settle().await;
        }
        self.binding = None;
        self.generation = None;
    }

    pub(crate) async fn failed(&mut self) {
        self.stop().await;
        self.mark_failed();
    }

    fn mark_failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
        // 250 ms, 500 ms, ... capped at 30 s, across explicit requests only.
        let millis = 250_u64.saturating_mul(1_u64 << self.failures.saturating_sub(1).min(7));
        self.retry_at = Instant::now().checked_add(Duration::from_millis(millis.min(30_000)));
    }

    fn ready(&self) -> Result<(), McpError> {
        if self
            .retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return Err(unsent("stdio MCP restart is backed off; retry later"));
        }
        Ok(())
    }

    pub(crate) fn completed(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }
}

/// Taking the client out of its slot makes an abandoned request invalidate the channel.
pub(crate) struct RequestLease<'a> {
    state: &'a mut SessionChild,
    pub(crate) client: Option<McpClient>,
}

impl<'a> RequestLease<'a> {
    pub(crate) fn new(state: &'a mut SessionChild) -> Self {
        let client = state.client.take();
        Self { state, client }
    }

    pub(crate) fn complete(mut self) {
        self.state.client = self.client.take();
        self.state.completed();
    }
}

impl Drop for RequestLease<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.cancellation().cancel();
            self.state.reap = Some(tokio::spawn(async move {
                let _ = client.settle().await;
            }));
            self.state.binding = None;
            self.state.generation = None;
            self.state.mark_failed();
        }
    }
}

impl McpStdioSession {
    pub fn new(
        session: SessionId,
        management: Arc<dyn McpDuplexEffectsFactory>,
        process: Arc<dyn ProcessExecutor>,
        confinement: Confinement,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            session,
            management,
            process,
            confinement,
            closed: AtomicBool::new(false),
            children: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn open(&self) -> Result<(), McpError> {
        if self.closed.load(Ordering::Acquire) {
            Err(unsent("stdio MCP session is closed"))
        } else {
            Ok(())
        }
    }

    fn child(&self, server: &McpServerName) -> Result<Arc<AsyncMutex<SessionChild>>, McpError> {
        self.open()?;
        let mut children = self
            .children
            .lock()
            .map_err(|_| unsent("stdio session lock poisoned"))?;
        self.open()?;
        Ok(Arc::clone(children.entry(server.clone()).or_insert_with(
            || Arc::new(AsyncMutex::new(SessionChild::new())),
        )))
    }

    pub(crate) async fn admit(
        &self,
        server: McpServerName,
        spec: SpawnSpec,
        options: McpClientOptions,
    ) -> Result<(Arc<AsyncMutex<SessionChild>>, McpClientInventory), McpError> {
        let spec = self
            .process
            .observe(&spec)
            .map_err(|_| unsent("stdio executable could not be observed"))?
            .session_lifetime();
        if spec.identity().is_none_or(|identity| !identity.complete()) {
            return Err(unsent("stdio executable identity is incomplete"));
        }
        let child = self.child(&server)?;
        let mut state = child.lock().await;
        self.open()?;
        if state.reap.is_some() {
            state.stop().await;
        }
        if state.binding.as_ref() != Some(&spec) {
            state.stop().await;
        }
        state.ready()?;
        if state.client.is_none() {
            self.connect(&mut state, server, spec, options).await?;
        }
        let client = state.client.as_mut().expect("client connected above");
        client.begin_stdio_operation(options)?;
        let inventory = match tokio::time::timeout_at(
            tokio::time::Instant::from_std(options.deadline()),
            client.inventory(),
        )
        .await
        {
            Ok(Ok(inventory)) => inventory,
            outcome => {
                state.failed().await;
                return Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => uncertain("stdio MCP inventory timed out"),
                });
            }
        };
        state.completed();
        drop(state);
        Ok((child, inventory))
    }

    async fn connect(
        &self,
        state: &mut SessionChild,
        server: McpServerName,
        spec: SpawnSpec,
        options: McpClientOptions,
    ) -> Result<(), McpError> {
        self.open()?;
        self.process
            .revalidate(&spec)
            .map_err(|_| unsent("stdio executable identity changed"))?;
        // Publish the failed-attempt backoff before an await: abandoning discovery
        // cannot bypass it. Keep the undiscovered child in the slot for joined reap.
        state.mark_failed();
        tracing::info!(session = %self.session, server = %server, cause = "stdio_start", attempt = state.failures, outcome = "starting", "ACP stdio MCP child");
        let effects = self.management.create(server.clone(), spec.clone())?;
        state.client = Some(McpClient::connect_stdio(server.clone(), options, effects)?);
        let result = tokio::time::timeout_at(
            tokio::time::Instant::from_std(options.deadline()),
            state.client.as_mut().expect("new client").discover(),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                if let Err(error) = self.open() {
                    state.stop().await;
                    return Err(error);
                }
                state.binding = Some(spec);
                tracing::info!(session = %self.session, server = %server, cause = "stdio_start", attempt = state.failures, outcome = "discovered", "ACP stdio MCP child");
                Ok(())
            }
            outcome => {
                tracing::warn!(session = %self.session, server = %server, cause = "stdio_start", attempt = state.failures, outcome = "failed", "ACP stdio MCP child");
                state.stop().await;
                Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => uncertain("stdio MCP discovery timed out"),
                })
            }
        }
    }

    pub(crate) async fn authorize(
        &self,
        state: &mut SessionChild,
        server: McpServerName,
        claim: McpStdioClaim,
        generation: Arc<str>,
        options: McpClientOptions,
    ) -> Result<(), McpError> {
        self.open()?;
        if state.reap.is_some() {
            state.stop().await;
        }
        if claim.confinement() != self.confinement
            || claim
                .spec()
                .identity()
                .is_none_or(|identity| !identity.complete())
        {
            state.stop().await;
            return Err(unsent(
                "stdio session confinement or executable binding changed",
            ));
        }
        let spec = claim.spec().clone().session_lifetime();
        if state.binding.as_ref() != Some(&spec)
            || state
                .generation
                .as_ref()
                .is_some_and(|bound| bound != &generation)
        {
            state.stop().await;
        }
        state.ready()?;
        if state.client.is_none() {
            self.connect(state, server, claim.into_spawn_spec(), options)
                .await?;
        }
        state.generation = Some(generation);
        state
            .client
            .as_mut()
            .expect("client connected above")
            .begin_stdio_operation(options)
    }

    /// Prevent new requests and join settlement of every child before session close returns.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let children = self
            .children
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for child in children {
            child.lock().await.stop().await;
        }
    }
}

impl McpDuplexEffectsFactory for McpStdioSession {
    fn create(
        &self,
        server: McpServerName,
        spec: SpawnSpec,
    ) -> Result<Box<dyn McpDuplexEffects>, McpError> {
        self.open()?;
        self.management.create(server, spec)
    }

    fn session_stdio(&self) -> Option<Arc<McpStdioSession>> {
        self.self_ref.upgrade()
    }
}

fn unsent(message: &str) -> McpError {
    McpError::with_delivery(
        McpFailure::Authority,
        message,
        DeliveryCertainty::DefinitelyUnsent,
    )
}
pub(crate) fn uncertain(message: &str) -> McpError {
    McpError::with_delivery(
        McpFailure::Transport,
        message,
        DeliveryCertainty::PossiblySent,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity};
    use ego_core::permit::{
        CanonPath, CleanupCertainty, DuplexEnd, DuplexHandle, DuplexSession, DuplexSettlement,
        EnvPolicy, ProcessError, ToolResult,
    };
    use futures::future::BoxFuture;
    use futures::io::{AsyncRead, AsyncWrite};
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    // Owned process-port observation, not a fixture of OS executable discovery.
    struct ObservedProcess;
    impl ProcessExecutor for ObservedProcess {
        fn observe(&self, spec: &SpawnSpec) -> Result<SpawnSpec, ProcessError> {
            Ok(spec.clone().with_execution_identity(
                spec.env_policy().clone(),
                ExecutionIdentity {
                    target: CanonPath::new("/owned-server"),
                    files: vec![ExecutableFileIdentity {
                        named: CanonPath::new("/owned-server"),
                        resolved: CanonPath::new("/owned-server"),
                        content: Some([1; 32]),
                        platform: vec![],
                    }],
                    failures: vec![],
                    dynamic_code: false,
                },
            ))
        }
        fn execute<'a>(
            &'a self,
            _: &'a SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            unreachable!("duplex boundary owns launch")
        }
    }

    #[derive(Default)]
    struct Effects {
        starts: Arc<AtomicUsize>,
        reaps: Arc<AtomicUsize>,
        fail_open: bool,
        discovered: bool,
        calls: Arc<AtomicUsize>,
        request_id: Arc<Mutex<Option<serde_json::Value>>>,
        cancelled_ids: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    struct OpenEffect {
        starts: Arc<AtomicUsize>,
        reaps: Arc<AtomicUsize>,
        fail_open: bool,
        discovered: bool,
        calls: Arc<AtomicUsize>,
        request_id: Arc<Mutex<Option<serde_json::Value>>>,
        cancelled_ids: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl McpDuplexEffectsFactory for Effects {
        fn create(
            &self,
            _: McpServerName,
            _: SpawnSpec,
        ) -> Result<Box<dyn McpDuplexEffects>, McpError> {
            Ok(Box::new(OpenEffect {
                starts: self.starts.clone(),
                reaps: self.reaps.clone(),
                fail_open: self.fail_open,
                discovered: self.discovered,
                calls: self.calls.clone(),
                request_id: self.request_id.clone(),
                cancelled_ids: self.cancelled_ids.clone(),
            }))
        }
    }
    impl McpDuplexEffects for OpenEffect {
        fn open(
            self: Box<Self>,
            _: McpServerName,
        ) -> crate::McpFuture<'static, Result<DuplexHandle, McpError>> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::AcqRel);
                if self.fail_open {
                    return Err(unsent("owned launch failed"));
                }
                // Existing current-protocol discovery envelope used by the owned
                // protocol/admission fixtures. After it, the owned peer stays silent.
                let response = if self.discovered {
                    br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}}
"#.to_vec()
                } else {
                    vec![]
                };
                Ok(Box::new(SilentDuplex {
                    reaps: self.reaps,
                    input: futures::io::Cursor::new(response),
                    writes: vec![],
                    calls: self.calls,
                    request_id: self.request_id,
                    cancelled_ids: self.cancelled_ids,
                }) as DuplexHandle)
            })
        }
    }
    struct SilentDuplex {
        reaps: Arc<AtomicUsize>,
        input: futures::io::Cursor<Vec<u8>>,
        writes: Vec<u8>,
        calls: Arc<AtomicUsize>,
        request_id: Arc<Mutex<Option<serde_json::Value>>>,
        cancelled_ids: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl AsyncRead for SilentDuplex {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.input.position() as usize >= self.input.get_ref().len() {
                Poll::Pending
            } else {
                Pin::new(&mut self.input).poll_read(cx, bytes)
            }
        }
    }
    impl AsyncWrite for SilentDuplex {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes.extend_from_slice(bytes);
            while let Some(end) = self.writes.iter().position(|byte| *byte == b'\n') {
                let line = self.writes.drain(..=end).collect::<Vec<_>>();
                let frame: serde_json::Value =
                    serde_json::from_slice(&line).expect("owned peer frame");
                if frame["method"] == "tools/call" {
                    *self.request_id.lock().expect("request id") = Some(frame["id"].clone());
                    self.calls.fetch_add(1, Ordering::AcqRel);
                }
                if frame["method"] == "notifications/cancelled" {
                    self.cancelled_ids
                        .lock()
                        .expect("cancelled ids")
                        .push(frame["params"]["requestId"].clone());
                }
            }
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl DuplexSession for SilentDuplex {
        fn delivery(&self) -> DeliveryCertainty {
            DeliveryCertainty::PossiblySent
        }
        fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
            self.interrupt()
        }
        fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
            self.reaps.fetch_add(1, Ordering::AcqRel);
            Box::pin(async {
                DuplexSettlement::new(
                    DuplexEnd::Interrupted,
                    CleanupCertainty::Complete,
                    DeliveryCertainty::PossiblySent,
                    Ok(ToolResult::builtin("reaped")),
                )
            })
        }
    }
    fn spec() -> SpawnSpec {
        SpawnSpec::new(
            vec!["/owned-server".into()],
            Some(CanonPath::new("/")),
            Some(Duration::from_secs(30)),
            EnvPolicy::Inherited,
        )
        .expect("spec")
    }
    fn options() -> McpClientOptions {
        McpClientOptions::new(4096, Instant::now() + Duration::from_millis(40)).expect("options")
    }

    /// Catches an ignored discovery deadline, lost duplex on future drop, and immediate restart storms.
    #[tokio::test]
    async fn discovery_timeout_reaps_child_and_immediate_readmission_is_backed_off() {
        let effects = Arc::new(Effects::default());
        let session = McpStdioSession::new(
            SessionId::new(),
            effects.clone(),
            Arc::new(ObservedProcess),
            Confinement::Off,
        );
        let server: McpServerName = "owned".parse().expect("server");
        assert!(
            session
                .admit(server.clone(), spec(), options())
                .await
                .is_err()
        );
        assert_eq!(
            effects.reaps.load(Ordering::Acquire),
            1,
            "timed-out discovery abandoned its child"
        );
        assert!(session.admit(server, spec(), options()).await.is_err());
        assert_eq!(
            effects.starts.load(Ordering::Acquire),
            1,
            "timeout triggered an immediate second spawn"
        );
        session.close().await;
        assert!(
            session
                .admit("owned".parse().expect("server"), spec(), options())
                .await
                .is_err(),
            "closed session restarted"
        );
    }

    /// Catches a failed spawn being retried for every queued request without backoff.
    #[tokio::test]
    async fn failed_spawn_is_not_retried_in_a_tight_loop() {
        let effects = Arc::new(Effects {
            fail_open: true,
            ..Effects::default()
        });
        let session = McpStdioSession::new(
            SessionId::new(),
            effects.clone(),
            Arc::new(ObservedProcess),
            Confinement::Off,
        );
        for _ in 0..10 {
            assert!(
                session
                    .admit("owned".parse().expect("server"), spec(), options())
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            effects.starts.load(Ordering::Acquire),
            1,
            "failed spawn storm"
        );
    }
    /// Catches lost selected-request cancellation IDs, a first-call deadline
    /// reused on later calls, dropped-future child leaks, and uncertain replay.
    #[tokio::test]
    async fn selected_request_cancel_timeout_and_drop_reap_without_replay() {
        for mode in ["cancel", "timeout", "drop"] {
            let effects = Arc::new(Effects {
                discovered: true,
                ..Effects::default()
            });
            let session = McpStdioSession::new(
                SessionId::new(),
                effects.clone(),
                Arc::new(ObservedProcess),
                Confinement::Off,
            );
            let server: McpServerName = "owned".parse().expect("server");
            let startup = McpClientOptions::new(4096, Instant::now() + Duration::from_secs(2))
                .expect("startup options");
            let (child, _) = session
                .admit(server.clone(), spec(), startup)
                .await
                .expect("admitted peer");
            let mut state = child.lock().await;
            state
                .client
                .as_mut()
                .expect("client")
                .begin_stdio_operation(options())
                .expect("fresh request");
            let mut lease = RequestLease::new(&mut state);
            let cancel = lease.client.as_ref().expect("client").cancellation();
            let mut request = Box::pin(
                lease
                    .client
                    .as_mut()
                    .expect("client")
                    .call_tool_once(rmcp::model::CallToolRequestParams::new("ping")),
            );
            tokio::select! {
                outcome = &mut request => panic!("{mode}: request ended before peer observed it: {}", outcome.is_err()),
                _ = async { while effects.calls.load(Ordering::Acquire) == 0 { tokio::time::sleep(Duration::from_millis(1)).await; } } => {}
            }
            if mode == "cancel" {
                cancel.cancel();
            }
            if mode != "drop" {
                let outcome = tokio::time::timeout(Duration::from_secs(1), &mut request)
                    .await
                    .expect("bounded selected request");
                assert!(outcome.is_err(), "{mode}: silent peer returned success");
            }
            drop(request);
            drop(lease);
            drop(state);
            // The next real admission joins the outstanding reap before it
            // reports backoff. It must not send or automatically replay anything.
            assert!(
                session.admit(server, spec(), options()).await.is_err(),
                "{mode}: failed request bypassed backoff"
            );
            assert_eq!(
                effects.reaps.load(Ordering::Acquire),
                1,
                "{mode}: child not reaped before retry returned"
            );
            assert_eq!(
                effects.starts.load(Ordering::Acquire),
                1,
                "{mode}: premature restart"
            );
            assert_eq!(
                effects.calls.load(Ordering::Acquire),
                1,
                "{mode}: uncertain call replayed"
            );
            let expected = effects
                .request_id
                .lock()
                .expect("id")
                .clone()
                .expect("known selected id");
            assert_eq!(
                *effects.cancelled_ids.lock().expect("cancelled ids"),
                vec![expected],
                "{mode}: missing/wrong/duplicate cancellation id"
            );
            if mode == "timeout" {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let fresh = McpClientOptions::new(4096, Instant::now() + Duration::from_secs(2))
                    .expect("restart options");
                session
                    .admit("owned".parse().expect("server"), spec(), fresh)
                    .await
                    .expect("explicit restart after backoff");
                assert_eq!(
                    effects.starts.load(Ordering::Acquire),
                    2,
                    "authorized restart remained unavailable after backoff"
                );
                assert_eq!(
                    effects.calls.load(Ordering::Acquire),
                    1,
                    "restart replayed the earlier uncertain call"
                );
            }
            session.close().await;
        }
    }

    // Catches admission waiting beyond its own deadline behind a stalled discovery.
    #[tokio::test]
    async fn queued_admission_deadline_does_not_wait_for_stalled_discovery() {
        let effects = Arc::new(Effects::default());
        let session = McpStdioSession::new(
            SessionId::new(),
            effects.clone(),
            Arc::new(ObservedProcess),
            Confinement::Off,
        );
        let server: McpServerName = "owned".parse().expect("server");
        let first_options = McpClientOptions::new(4096, Instant::now() + Duration::from_secs(5))
            .expect("first options");
        let mut first = Box::pin(session.admit(server.clone(), spec(), first_options));
        tokio::select! {
            result = &mut first => panic!("discovery unexpectedly ended: {}", result.is_err()),
            _ = async {
                while effects.starts.load(Ordering::Acquire) == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            } => {}
        }
        let queued = session.admit(server, spec(), options());
        let outcome = tokio::time::timeout(Duration::from_millis(300), queued).await;
        drop(first);
        session.close().await;
        assert!(
            outcome.is_ok(),
            "queued admission ignored its own deadline behind discovery"
        );
        assert!(outcome.expect("bounded admission").is_err());
        assert_eq!(
            effects.starts.load(Ordering::Acquire),
            1,
            "expired admission opened another child"
        );
    }
}
