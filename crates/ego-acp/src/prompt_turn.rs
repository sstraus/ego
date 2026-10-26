//! Shared ACP prompt validation, admission, execution and settlement.
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn prompt_handler<S, E, C, I>(
    sessions: Sessions,
    unadmitted: UnadmittedSessions,
    controller: Arc<RunController<S, E, C, I>>,
    spawner: Arc<dyn ExecutionSpawner>,
    ids: Arc<dyn IdSource>,
    bindings_builder: Arc<dyn RunBindingsBuilder>,
    client_capabilities: Arc<Mutex<ClientInit>>,
    controls: Option<Arc<dyn SessionControls>>,
    confinement: Option<Arc<dyn SessionConfinementAdmission>>,
    trust: Option<Arc<dyn SessionTrustAdmission>>,
    lifecycle: Option<Arc<dyn LifecycleOperations>>,
) -> PromptHandler
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    Arc::new(
        move |request: v1::PromptRequest, responder: PromptReply, cx: ConnectionTo<Client>| {
            let sessions = sessions.clone();
            let unadmitted = unadmitted.clone();
            let controller = Arc::clone(&controller);
            let spawner = Arc::clone(&spawner);
            let ids = Arc::clone(&ids);
            let bindings_builder = Arc::clone(&bindings_builder);
            let client_capabilities = Arc::clone(&client_capabilities);
            let controls = controls.clone();
            let confinement = confinement.clone();
            let trust = trust.clone();
            let lifecycle = lifecycle.clone();
            Box::pin(async move {
                let id = request.session_id.clone();
                let durable_session = match parse_session(&id) {
                    Ok(id) => id,
                    Err(error) => return responder.respond_with_error(error),
                };
                if let Err(error) = unadmitted.refuse(durable_session) {
                    return responder.respond_with_error(error);
                }
                // Read the durable run even if a failed load left no local
                // attachment. Retrying an uncertain effect cannot recover it.
                match controller.status_by_session(durable_session).await {
                    Ok(snapshot) if snapshot.status() == RunStatus::Uncertain => {
                        let recovery = format!(
                            "run {} has an uncertain effect; close this session with `session/close` and start a new chat with `session/new`",
                            snapshot.run_id(),
                        );
                        let message = match snapshot.settlement() {
                            Some(settlement) => failure_message(&recovery, settlement),
                            None => recovery,
                        };
                        return responder.respond_with_error(Error::new(
                            ErrorCode::InternalError.into(),
                            message,
                        ));
                    }
                    Ok(_) | Err(RunError::Store(StoreError::NotFound)) => {}
                    Err(error) => {
                        return responder
                            .respond_with_error(Error::internal_error().data(error.to_string()));
                    }
                }
                // Serialized, so a numeric `41` and a string `"41"` stay
                // two different requests exactly as JSON-RPC says they are.
                let command = responder.command();
                // Derived, not copied: this value is written into the
                // durable run transaction, so serializing the request into
                // it would persist the prompt a second time -- and, once a
                // block can carry image bytes, would write those bytes into
                // the event stream that
                // `docs/decisions/2026-08-29-images-are-session-blobs-
                // before-they-are-wire-parts.md` keeps them out of.
                let digest = match RequestDigest::of(&request) {
                    Ok(digest) => digest,
                    Err(error) => {
                        return responder
                            .respond_with_error(Error::invalid_params().data(error.to_string()));
                    }
                };
                let correlation = match sessions
                    .begin_prompt(
                        &id,
                        ids.as_ref(),
                        &command,
                        digest.as_str(),
                        responder.is_resume(),
                    )
                    .await
                {
                    BeginPrompt::Ready(correlation) => correlation,
                    BeginPrompt::Busy => {
                        return responder.respond_with_error(Error::invalid_request().data(
                                format!(
                                    "session {id} already has a turn running, and this agent runs one turn per session: wait for the prompt in flight to answer before sending the next"
                                ),
                            ));
                    }
                    BeginPrompt::Unknown => {
                        return responder.respond_with_error(Error::invalid_params().data(
                            format!(
                                "there is no open session {id}: create one with `session/new` first"
                            ),
                        ));
                    }
                };
                if let Some(lifecycle) = &lifecycle {
                    match lifecycle.hold(durable_session) {
                        Ok(HoldState::Paused) => {
                            sessions.refuse(&id, correlation.generation);
                            return responder.respond_with_error(Error::new(
                                SESSION_HELD,
                                "the session is paused; continue it with '_ego/resume'",
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            sessions.refuse(&id, correlation.generation);
                            return responder.respond_with_error(lifecycle_error(&error));
                        }
                    }
                }
                let prompt = match prompt_content(&request.prompt) {
                    Ok(prompt) => prompt,
                    Err(refusal) => {
                        sessions.refuse(&id, correlation.generation);
                        return responder.respond_with_error(Error::invalid_params().data(refusal));
                    }
                };
                let client = client_capabilities
                    .lock()
                    .expect("initialize capability lock poisoned")
                    .clone();
                let capabilities = client.capabilities;
                let bindings = session_bindings(
                    bindings_builder.as_ref(),
                    cx.clone(),
                    id.clone(),
                    &client,
                    &correlation.root,
                    correlation.mcp.as_ref(),
                    Arc::clone(&correlation.session_approval),
                );
                let (ingress, control) = ego_core::turn::interactive_control();
                sessions.install_steering(&id, correlation.generation, ingress);
                let bindings = bindings.with_interactive_control(control);
                // Everything below runs off the event loop. `selected_trust`
                // awaits the host, and an ACP handler that awaits a client
                // round-trip stops the connection from processing messages
                // -- including the very answer it is waiting for. `spawn`
                // is the SDK's documented way out, and the permission seat
                // already reaches the host through it.
                let controller = Arc::clone(&controller);
                let spawner = Arc::clone(&spawner);
                let sessions = sessions.clone();
                let trust = trust.clone();
                let lifecycle = lifecycle.clone();
                let controls = controls.clone();
                let confinement = confinement.clone();
                let connection = cx.clone();
                let generation = correlation.generation;
                cx.spawn(async move {
                        let PromptCorrelation {
                            root,
                            request_id,
                            run,
                            mut withdrawn,
                            ..
                        } = correlation;
                        let outcome = match run {
                            None => {
                                // Closing or cancelling a session is effective
                                // even while its first confinement probe is
                                // still pending. The connection's background
                                // task may finish the probe for later sessions.
                                let initialized = if confinement.is_some() {
                                    let measurement = std::pin::pin!(initialize_confinement(
                                        confinement.as_ref(),
                                        durable_session,
                                        &root,
                                    ));
                                    match futures::future::select(&mut withdrawn, measurement).await
                                    {
                                        Either::Left(_) => {
                                            sessions.refuse(&id, generation);
                                            return responder.respond(v1::PromptResponse::new(
                                                v1::StopReason::Cancelled,
                                            ));
                                        }
                                        Either::Right((result, _)) => result,
                                    }
                                } else {
                                    // With no probe to wait for, cancellation must reach the
                                    // admitted run instead of withdrawing the prompt early.
                                    Ok(())
                                };
                                if let Err(error) = initialized {
                                    sessions.refuse(&id, generation);
                                    return responder.respond_with_error(error);
                                }
                                let selections = match selected_confinement(
                                    controls.as_ref(),
                                    confinement.as_ref(),
                                    durable_session,
                                ) {
                                    Ok(selections) => selections,
                                    Err(error) => {
                                        sessions.refuse(&id, generation);
                                        return responder.respond_with_error(error);
                                    }
                                };
                                // Between the confinement pick and the start:
                                // preparation derives the config layers, the
                                // instruction file, the project MCP servers,
                                // the skills and the hooks from this answer,
                                // and none of them can be re-decided once a
                                // run exists.
                                //
                                // Raced against a withdrawal, and the
                                // withdrawal is polled first: a host that took
                                // its prompt back while the question was open
                                // gets no run at all. That is not the same act
                                // as refusing the question, which answers it
                                // and runs bare.
                                let selections = match trust_question(
                                    trust.as_ref(),
                                    capabilities,
                                    &root,
                                ) {
                                    // Nothing is asked, so nothing is waited
                                    // on, and this prompt reaches admission
                                    // exactly as a prompt on a router without
                                    // a trust port does -- including where a
                                    // `session/cancel` lands on it.
                                    TrustChoice::None => selections,
                                    TrustChoice::Remembered(decision) => selections.with_trust(decision),
                                    TrustChoice::Question(request, fingerprint) => {
                                        let answered = {
                                            let question = std::pin::pin!(selected_trust(
                                                request,
                                                AcpQuestionAdapter::live(
                                                    id.clone(),
                                                    capabilities,
                                                    connection.clone(),
                                                ),
                                                selections,
                                                trust.as_ref().expect("question needs trust admission").as_ref(),
                                                &root,
                                                &fingerprint,
                                            ));
                                            match futures::future::select(withdrawn, question)
                                                .await
                                            {
                                                Either::Left(_) => None,
                                                Either::Right((selections, _)) => Some(selections),
                                            }
                                        };
                                        let Some(selections) = answered else {
                                            sessions.refuse(&id, generation);
                                            return responder.respond(v1::PromptResponse::new(
                                                v1::StopReason::Cancelled,
                                            ));
                                        };
                                        selections
                                    }
                                };
                                controller
                                    .start(
                                        StartRequest::new(
                                            durable_session,
                                            root,
                                            request_id,
                                            digest,
                                            selections,
                                            prompt,
                                        ),
                                        bindings,
                                    )
                                    .await
                            }
                            Some((run_id, revision)) => {
                                let cancel = ego_core::turn::CancelToken::new();
                                let admission = std::pin::pin!(controller.continue_run(
                                    ContinueRequest::new(run_id, request_id, digest, revision, prompt),
                                    bindings.with_cancel(cancel.clone()),
                                ));
                                match futures::future::select(&mut withdrawn, admission).await {
                                    Either::Right((outcome, _)) => outcome,
                                    Either::Left((_, admission)) => {
                                        cancel.cancel(ego_core::event::domain::CancelCause::User);
                                        let outcome = admission.await;
                                        if outcome.is_err() {
                                            sessions.refuse(&id, generation);
                                            return responder.respond(v1::PromptResponse::new(v1::StopReason::Cancelled));
                                        }
                                        outcome
                                    }
                                }
                            }
                        };
                        let (subscription, execution, withdrawn) = match outcome {
                            Ok(RunCommandOutcome::Admitted(admission)) => {
                                let subscription = admission.subscription();
                                let key = admission.cancel_key().clone();
                                // A cancellation that arrived while this
                                // admission was being written had no key to
                                // name, so it left a flag instead, and this is
                                // the one read that cannot miss it. The
                                // admission itself is never raced --
                                // abandoning a half-written transaction would
                                // leave a run nobody manages.
                                let withdrawn = sessions
                                    .install_admission(
                                        &id,
                                        generation,
                                        admission.run_id(),
                                        admission.revision(),
                                        key.clone(),
                                    )
                                    .then_some(key);
                                (subscription, Some(admission.into_execution()), withdrawn)
                            }
                            Ok(RunCommandOutcome::Replayed(observation)) => {
                                (observation.subscription(), None, None)
                            }
                            Err(error) => {
                                sessions.refuse(&id, generation);
                                return responder.respond_with_error(
                                    Error::internal_error().data(error.to_string()),
                                );
                            }
                        };
                        // The execution settles the admitted revision before
                        // it returns, so the awaited subscription can only
                        // report *that* the turn failed. The diagnostic that
                        // names *why* travels beside it and is answered to the
                        // same request.
                        let (diagnostic_tx, diagnostic_rx) = oneshot::channel();
                        match execution {
                            Some(execution) => {
                                let controller = Arc::clone(&controller);
                                spawner.spawn(Box::pin(async move {
                                    let outcome = controller.execute(execution).await;
                                    if let Err(error) = &outcome {
                                        tracing::warn!(%error, "admitted ACP execution failed");
                                    }
                                    let _ = diagnostic_tx
                                        .send(outcome.err().map(|error| error.to_string()));
                                }));
                            }
                            // A replay explains nothing new, so the sender is
                            // dropped rather than held: a receiver still
                            // waiting on a message nobody will send would
                            // never answer the request at all.
                            None => drop(diagnostic_tx),
                        }
                        // After the execution is under way, and deliberately:
                        // the run was admitted, so it cancels the way every
                        // other admitted run does -- asked to stop at its next
                        // boundary. Applying it ahead of the execution would
                        // instead leave an admitted run that never started,
                        // which is a third state neither the controller nor a
                        // host has a word for.
                        if let Some(key) = withdrawn
                            && let Err(error) = controller.cancel_admitted(&key).await
                        {
                            tracing::warn!(
                                %error,
                                "an ACP cancellation received during admission could not be applied"
                            );
                        }
                        let snapshot = subscription.await;
                        let cancel_requested = sessions.finish(&id, generation, &snapshot);
                        if let Some(lifecycle) = &lifecycle {
                            match lifecycle.meta(durable_session) {
                                Ok(meta) if meta.summary.is_some() => {
                                    if let Err(error) = emit_info(&connection, &id, &meta, Some(lifecycle.as_ref())) {
                                        tracing::warn!(%error, "ACP session info update could not be delivered");
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => tracing::warn!(%error, "ACP session metadata could not be read after prompt"),
                            }
                        }
                        if cancel_requested && matches!(snapshot.settlement(), Some(RunSettlement::PermissionRequired { .. })) {
                            return responder.respond(v1::PromptResponse::new(v1::StopReason::Cancelled));
                        }
                        match prompt_result(&snapshot) {
                            Ok(stop_reason) => {
                                responder.respond(v1::PromptResponse::new(stop_reason))
                            }
                            Err(message) => {
                                let diagnostic = diagnostic_rx.await.ok().flatten();
                                let code = if matches!(snapshot.settlement(), Some(RunSettlement::Paused)) {
                                    SESSION_HELD
                                } else {
                                    i32::from(ErrorCode::InternalError)
                                };
                                let error = Error::new(code, message);
                                responder.respond_with_error(match diagnostic {
                                    Some(diagnostic) => error.data(diagnostic),
                                    None => error,
                                })
                            }
                        }
                    })?;
                Ok(())
            })
        },
    )
}
