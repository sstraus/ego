//! Per-request authority over a session-owned stdio channel.

use super::*;
use crate::session_stdio::uncertain;

async fn invoke(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpStdioOperationFactory,
    generation: Arc<str>,
    options: McpClientOptions,
    hook_scope: Option<&EffectScope>,
) -> Result<(OperationOutcome, ego_core::permit::McpPreparedEnvelope), McpError> {
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected stdio call has no envelope"))?;
    let spec = call
        .ops()
        .iter()
        .find_map(|op| {
            if let ego_core::permit::Op::Spawn { spec } = op {
                Some(spec)
            } else {
                None
            }
        })
        .ok_or_else(|| authority("selected stdio call has no spawn authority"))?;
    if envelope.transport_fingerprint()
        != stdio_transport_fingerprint(config, envelope.admission(), spec)
    {
        return Err(authority("selected stdio fingerprint changed"));
    }
    let request = operation_request(&envelope)?;
    let claim = io.claim_mcp_stdio(call).map_err(McpError::from)?;
    if claim.envelope() != &envelope {
        return Err(authority("guarded stdio claim changed its envelope"));
    }
    let (session, child) = factory
        .session
        .as_ref()
        .expect("session factory selected above");
    let mut state = tokio::time::timeout_at(
        tokio::time::Instant::from_std(options.deadline()),
        child.lock(),
    )
    .await
    .map_err(|_| {
        McpError::with_delivery(
            McpFailure::Transport,
            "stdio request expired waiting for the channel",
            DeliveryCertainty::DefinitelyUnsent,
        )
    })?;
    session
        .authorize(
            &mut state,
            server_name(envelope.subject().server())?,
            claim,
            generation,
            options,
        )
        .await?;
    let mut lease = crate::session_stdio::RequestLease::new(&mut state);
    let client = lease.client.as_mut().expect("authorized client");
    let cancellation = client.cancellation();
    let mut request = Box::pin(request.invoke(client));
    let outcome = loop {
        if io.is_cancelled()
            || session.is_closed()
            || hook_scope.is_some_and(EffectScope::is_cancelled)
        {
            cancellation.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut request).await;
            break Err(McpError::with_delivery(
                McpFailure::Cancelled,
                "stdio MCP request cancelled",
                DeliveryCertainty::PossiblySent,
            ));
        }
        tokio::select! {
            outcome = &mut request => break outcome,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(options.deadline())) => {
                cancellation.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut request).await;
                break Err(uncertain("stdio MCP request timed out"));
            }
            _ = tokio::time::sleep(Duration::from_millis(2)) => {}
        }
    };
    drop(request);
    match outcome {
        Ok(outcome) => {
            lease.complete();
            Ok((outcome, envelope))
        }
        Err(error) => {
            // Nothing retries this operation, including after possibly-sent failures.
            drop(lease);
            state.stop().await;
            Err(error)
        }
    }
}

pub(super) async fn execute(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpStdioOperationFactory,
    admission: Arc<McpServerAdmission>,
    generation: Arc<str>,
) -> Result<ToolExecution, McpError> {
    let (outcome, envelope) = invoke(
        config,
        call,
        io,
        factory,
        Arc::clone(&generation),
        factory.limits.options()?,
        None,
    )
    .await?;
    let schema = tool_schema_command(
        &outcome,
        &envelope,
        admission,
        &generation,
        factory.collapsed.as_ref(),
    )?;
    let execution = project_outcome(
        outcome,
        &envelope,
        io,
        factory.continuations.as_ref(),
        DeliveryCertainty::PossiblySent,
        CancellationClassification::Completed,
    )
    .await?;
    if let (Some(collapsed), Some(command)) = (factory.collapsed.as_ref(), schema) {
        collapsed.admission.reconcile(command).await?;
    }
    Ok(execution)
}

pub(super) async fn execute_hook(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpStdioOperationFactory,
    scope: EffectScope,
) -> Result<McpHookExecution, McpError> {
    ensure_hook_scope(&scope)?;
    let generation = call
        .mcp_envelope()
        .ok_or_else(|| authority("hook has no envelope"))?
        .admission()
        .capture()
        .to_owned();
    let (outcome, _) = invoke(
        config,
        call,
        io,
        factory,
        Arc::from(generation),
        factory.limits.hook_options(&scope)?,
        Some(&scope),
    )
    .await?;
    project_hook_outcome(outcome, io, DeliveryCertainty::PossiblySent)
}
