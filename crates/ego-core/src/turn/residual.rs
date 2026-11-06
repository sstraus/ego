//! Serves the session's residual-write desk while one exec call runs.
//!
//! The executor saw a refused write it cannot open alone; this side owns the
//! call, the task and the judge. One request becomes one journaled decision,
//! and a grant reaches the desk only after its event is durable.

use super::*;
use crate::permit::PreparedCall;
use crate::permit::judge::{JudgeTaskContext, ResidualVerdict, decode_residual};
use crate::permit::residual::{ResidualDecision, ResidualWriteDesk, ResidualWriteRequest};

/// Borrowed session authority for the residual requests of one exec call.
pub(super) struct ResidualServer<'a, 'session> {
    pub journal: &'a SharedJournal<'session>,
    pub cancel: &'a CancelToken,
    pub gate: &'a SharedBudgetGate,
    pub session_id: SessionId,
    pub invocation: RunInvocationId,
    pub admission: &'a EvidenceAdmissionService,
    pub evidence: &'a dyn EvidenceStore,
    pub source: &'a dyn GenerationModelSource,
    pub retry_pause: &'a dyn RetryPause,
    pub judge: Option<&'a JudgeContext>,
    pub main: Option<&'a ModelRef>,
    pub turn: TurnId,
    /// The call's `ToolCallStarted`, parent of every decision.
    pub started: EventId,
    pub call: PreparedCall,
    pub context: JudgeTaskContext,
}

impl ResidualServer<'_, '_> {
    /// Run `work` and answer the desk until it completes.
    ///
    /// Requests are served one at a time while `work` is not polled. The
    /// executor raises a request only after its child exited and waits for
    /// the answer, so nothing it owns is starved for the bounded decision.
    /// An effect is never dropped: a store failure drops the request, which
    /// the executor reads as no opinion, and still awaits `work`.
    pub async fn serve<F: Future>(
        self,
        desk: &ResidualWriteDesk,
        work: F,
    ) -> Result<F::Output, StoreError> {
        let mut work = Box::pin(work);
        loop {
            let requests = select(Box::pin(desk.next()), Box::pin(desk.next_owner()));
            let request = match select(work.as_mut(), requests).await {
                Either::Left((output, _)) => return Ok(output),
                Either::Right((Either::Left((Some(request), _)), _)) => request,
                Either::Right((Either::Right((Some(request), _)), _)) => {
                    if let Err(failure) = self.journal.append(
                        self.turn,
                        Some(ItemId::new()),
                        Some(self.started),
                        EventKind::SandboxOwnerGranted {
                            grant: request.grant.clone(),
                        },
                    ) {
                        drop(request);
                        let _ = work.await;
                        return Err(failure.into());
                    }
                    desk.answer_owner(request);
                    continue;
                }
                _ => return Ok(work.await),
            };
            if self.judge.is_none() {
                // Nothing was decided, so nothing is journaled: the executor
                // asks its human as it would without a desk.
                desk.answer(
                    request,
                    ResidualDecision::Unavailable {
                        reason: "no judge is configured for this session".to_owned(),
                    },
                );
                continue;
            }
            match self.journaled(&request, desk.limits().deadline()).await {
                Ok(decision) => desk.answer(request, decision),
                Err(failure) => {
                    drop(request);
                    let _ = work.await;
                    return Err(failure);
                }
            }
        }
    }

    async fn journaled(
        &self,
        request: &ResidualWriteRequest,
        deadline: std::time::Duration,
    ) -> Result<ResidualDecision, StoreError> {
        let (decision, attempt) = self.decide(request, deadline).await?;
        let recommended = match &decision {
            ResidualDecision::Granted { recommended, .. } => Some(*recommended),
            _ => None,
        };
        self.journal.append(
            self.turn,
            Some(ItemId::new()),
            Some(self.started),
            EventKind::SandboxResidualDecided {
                path: request.evidence.candidate.clone(),
                outcome: decision.outcome(),
                recommended,
                reason: decision.reason().to_owned(),
                attempt,
            },
        )?;
        Ok(decision)
    }

    async fn decide(
        &self,
        request: &ResidualWriteRequest,
        deadline: std::time::Duration,
    ) -> Result<(ResidualDecision, Option<AttemptRef>), StoreError> {
        let unavailable = |reason: &str| {
            Ok((
                ResidualDecision::Unavailable {
                    reason: reason.to_owned(),
                },
                None,
            ))
        };
        let bound = self.call.ops().iter().any(|op| {
            matches!(op, crate::permit::Op::Spawn { spec }
                if spec.argv() == request.spec.argv() && spec.cwd() == request.spec.cwd())
        });
        if !bound {
            return unavailable("the refusal does not belong to the running call");
        }
        let Some(judge) = self.judge else {
            return unavailable("no judge is configured for this session");
        };
        let profile = match select(
            self.source
                .resolve(GenerationModelRequest::new(GenerationRole::Judge)),
            self.retry_pause.pause(deadline),
        )
        .await
        {
            Either::Left((Ok(profile), _)) => profile,
            Either::Left((Err(error), _)) => {
                tracing::warn!(%error, "no judge model for the residual write");
                return unavailable("the judge model could not be resolved");
            }
            Either::Right(_) => return unavailable("the judge did not answer in time"),
        };
        let input = JudgeInput::residual_write(
            &self.call,
            &judge.confinement,
            judge.exec_caps,
            &request.evidence,
        );
        let config = JudgeConfig {
            identity: profile.identity.clone(),
            request_model: profile.request_model.clone(),
            max_output_tokens: profile.output_ceiling(),
        };
        let Some(presented) = input.presented_candidate(&config, &self.context, self.main) else {
            return unavailable("the candidate path could not be presented");
        };

        // The nested judge has no interactive control and its own stop: the
        // deadline or the run's cancel ends it, and it still settles its
        // reservation before returning.
        let stop = CancelToken::new();
        let mut detached = None;
        let mut generator = judge::JudgeGeneration {
            journal: self.journal,
            cancel: &stop,
            control: &mut detached,
            gate: self.gate,
            session_id: self.session_id,
            invocation: self.invocation,
            admission: self.admission,
            evidence: self.evidence,
        };
        let mut generation =
            Box::pin(generator.run(self.turn, &profile, &input, &self.context, self.main));
        let ended = select(
            self.retry_pause.pause(deadline),
            Box::pin(self.cancel.0.cancelled.clone()),
        );
        let generated = match select(generation.as_mut(), ended).await {
            Either::Left((generated, _)) => generated?,
            Either::Right(_) => {
                stop.cancel(CancelCause::Timeout);
                generation.await?
            }
        };
        let (decoder, attempt) = match generated {
            judge::JudgeGenerationResult::Completed { decoder, attempt }
                if !stop.is_cancelled() =>
            {
                (decoder, attempt)
            }
            judge::JudgeGenerationResult::Completed { .. }
            | judge::JudgeGenerationResult::Ended(_) => {
                return unavailable("the judge did not answer in time");
            }
            judge::JudgeGenerationResult::Unavailable => {
                return unavailable("the judge could not run");
            }
        };
        let decision = match decode_residual(&decoder, &presented) {
            Ok(ResidualVerdict::Write { scope, reason }) => {
                match (request.admissible)(&request.evidence.candidate) {
                    Ok(()) => ResidualDecision::Granted {
                        path: request.evidence.candidate.clone(),
                        recommended: scope,
                        reason,
                    },
                    Err(refused) => ResidualDecision::Refused { reason: refused },
                }
            }
            Ok(ResidualVerdict::None { reason }) => ResidualDecision::Refused { reason },
            Err(invalid) => ResidualDecision::Unavailable {
                reason: invalid.to_owned(),
            },
        };
        Ok((decision, Some(attempt)))
    }
}
