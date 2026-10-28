//! Shared controlled and metered judge generation.

use super::*;

/// Session authority borrowed for one opinion. This cannot mint a tool permit
/// or publish a sandbox grant; callers must recheck their own live policy.
pub struct JudgeGeneration<'a, 'session, 'control> {
    pub journal: &'a SharedJournal<'session>,
    pub cancel: &'a CancelToken,
    pub control: &'a mut Option<&'control mut InteractiveControl>,
    pub gate: &'a SharedBudgetGate,
    pub session_id: SessionId,
    pub invocation: RunInvocationId,
    pub admission: &'a EvidenceAdmissionService,
    pub evidence: &'a dyn EvidenceStore,
}

pub enum JudgeGenerationResult {
    Unavailable,
    Ended(ExitReason),
    Completed {
        decoder: JudgeDecoder,
        attempt: AttemptRef,
    },
}

impl JudgeGeneration<'_, '_, '_> {
    pub async fn run(
        &mut self,
        turn: TurnId,
        profile: &GenerationProfile,
        input: &JudgeInput,
        context: &crate::permit::judge::JudgeTaskContext,
        main: Option<&ModelRef>,
    ) -> Result<JudgeGenerationResult, StoreError> {
        let config = JudgeConfig {
            identity: profile.identity.clone(),
            request_model: profile.request_model.clone(),
            max_output_tokens: profile.output_ceiling(),
        };
        let mut request = input.request_with_context(&config, context, main);
        request.thinking = profile.thinking;

        if !self.admit_request(turn, profile, &request)? {
            return Ok(JudgeGenerationResult::Unavailable);
        }
        let budget = self
            .gate
            .snapshot()
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        self.journal.publish(&PublicEvent::metering_selected(
            turn,
            &profile.metering,
            budget.ledger(),
            profile
                .rates
                .as_ref()
                .map_or(&RateSource::None, |rates| &rates.source),
        ))?;
        let quote = profile.rates.as_ref().map(|rates| &rates.quote);
        let reservation =
            match self
                .gate
                .reserve(&profile.metering, quote, &request, &profile.capabilities)
            {
                Ok(reservation) => reservation,
                Err(denied) => {
                    tracing::warn!(%denied, "the budget gate refused the judge generation");
                    let reason = if matches!(&denied, SharedBudgetError::Denied(_)) {
                        ExitReason::Budget
                    } else {
                        ExitReason::Internal
                    };
                    return Ok(JudgeGenerationResult::Ended(reason));
                }
            };
        self.journal.reservation_cost_anomaly(
            turn,
            &profile.identity.0,
            &profile.metering,
            &reservation,
            profile.pricing_unavailable,
        )?;
        let attempt = match GenerationAttempt::new(
            GenerationId::new(),
            reservation,
            &profile.metering,
            quote,
            &request,
            &profile.capabilities,
        ) {
            Ok(attempt) => attempt,
            Err(denied) => {
                tracing::error!(%denied, "the judge reservation did not authorise its request");
                return Ok(JudgeGenerationResult::Ended(ExitReason::Internal));
            }
        };
        let reference = attempt.attempt();
        admit_generation_evidence(
            self.session_id,
            self.invocation,
            self.admission,
            self.evidence,
            profile,
            reference,
            GenerationRole::Judge,
            &request.tools,
        )?;
        self.journal.append(
            turn,
            None,
            None,
            EventKind::GenerationStarted {
                attempt: reference,
                role: GenerationRole::Judge,
                snapshot: None,
            },
        )?;

        let mut stream = profile.generator.generate(request, attempt);
        let JudgeObservation {
            decoder,
            usage,
            provider_cost,
            interrupted,
            mut failure,
        } = self.observe_stream(turn, &mut stream).await;

        if failure.is_none() && (usage.is_some() || provider_cost.is_some()) {
            let observed = usage.unwrap_or(Usage::IDENTITY);
            let cost_delta = settled_cost(
                &profile.metering,
                quote,
                usage.map_or(UsageReport::Missing, UsageReport::Reported),
                provider_cost,
                self.gate
                    .snapshot()
                    .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?
                    .currency(),
            );
            if let Err(cause) = self.journal.append(
                turn,
                None,
                None,
                EventKind::UsageUpdated {
                    usage: observed,
                    cost_delta,
                    attempt: Some(reference),
                },
            ) {
                failure = Some(cause.into());
            }
        }

        let settlement = match self.gate.settle(stream) {
            Ok(settlement) => settlement,
            Err(denied) => {
                failure.map_or(Ok(()), Err)?;
                tracing::error!(%denied, "the gate refused to settle its judge attempt");
                return Ok(JudgeGenerationResult::Ended(ExitReason::Internal));
            }
        };
        failure.map_or(Ok(()), Err)?;
        if !settlement.reconciled.exact {
            self.journal.cost_anomaly(
                turn,
                &profile.identity.0,
                settlement.reconciled.charged,
                profile.pricing_unavailable,
            )?;
        }
        self.journal.append(
            turn,
            None,
            None,
            EventKind::AttemptSettled {
                attempt: settlement.attempt,
                outcome: settlement.outcome,
            },
        )?;

        let (raw, truncated) = decoder.response_evidence();
        self.journal.append(
            turn,
            None,
            None,
            EventKind::JudgeResponse {
                attempt: reference,
                text: raw.to_owned(),
                truncated,
            },
        )?;

        if interrupted {
            return Ok(JudgeGenerationResult::Ended(ExitReason::Cancelled(
                self.cancel.cause().unwrap_or(CancelCause::User),
            )));
        }

        Ok(JudgeGenerationResult::Completed {
            decoder,
            attempt: reference,
        })
    }
    async fn observe_stream(
        &mut self,
        turn: TurnId,
        stream: &mut AttemptStream,
    ) -> JudgeObservation {
        let mut decoder = JudgeDecoder::new();
        let mut usage: Option<Usage> = None;
        let mut provider_cost: Option<Money> = None;
        let mut interrupted = false;
        let mut failure = None;
        loop {
            let event = match await_controlled(
                self.journal,
                self.cancel,
                self.control,
                turn,
                stream.next(),
                InterruptDisposition::Return,
                None,
            )
            .await
            {
                Controlled::Completed(Some(event)) => event,
                Controlled::Completed(None) => break,
                Controlled::Interrupted => {
                    interrupted = true;
                    break;
                }
                Controlled::Failed(cause) => {
                    failure = Some(cause);
                    break;
                }
            };
            decoder.observe(&event);
            match &event {
                AiEvent::Usage(delta) => {
                    let mut total = usage.unwrap_or(Usage::IDENTITY);
                    total.add(delta);
                    usage = Some(total);
                }
                AiEvent::ProviderCost { cost } => provider_cost = Some(*cost),
                _ => {}
            }
        }

        JudgeObservation {
            decoder,
            usage,
            provider_cost,
            interrupted,
            failure,
        }
    }

    fn admit_request(
        &self,
        turn: TurnId,
        profile: &GenerationProfile,
        request: &GenRequest,
    ) -> Result<bool, StoreError> {
        let gaps = profile.capabilities.request_gaps(request);
        if !gaps.is_empty() {
            self.journal.append(
                turn,
                None,
                None,
                EventKind::Error {
                    message: gaps
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(false);
        }
        if let Err(refused) = profile.generator.ensure_expressible(request) {
            self.journal.append(
                turn,
                None,
                None,
                EventKind::Error {
                    message: refused.to_string(),
                    kind: ErrorKind::Unclassified,
                },
            )?;
            return Ok(false);
        }
        Ok(true)
    }
}

struct JudgeObservation {
    decoder: JudgeDecoder,
    usage: Option<Usage>,
    provider_cost: Option<Money>,
    interrupted: bool,
    failure: Option<StoreError>,
}
