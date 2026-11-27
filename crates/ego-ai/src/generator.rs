//! The binding: the wire format, the transport and the attempt, joined.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `docs/internals/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
//!
//! This module composes and returns. It folds no delivery certainty and
//! decides no stop reason, because both already have exactly one owner:
//! [`HttpError`](crate::http::HttpError) carries the transport's reading of
//! delivery in both variants, [`WireFormat::decode`](crate::wire::WireFormat::decode) turns that into
//! events, and `AttemptStream` folds them monotonically and
//! keeps only the first terminal frame. A second fold here would be a
//! competing spelling of an existing one, invisible until the two disagreed.

use std::sync::Arc;

use ego_core::generator::attempt::{AttemptStream, DeliveryCertainty, GenerationAttempt};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::port::{Generator, RequestExpressibilityError};
use ego_core::generator::types::{ErrorKind, GenRequest};
use futures::{StreamExt, stream};
use url::Url;

use crate::http::{AdmittedHttpProfile, Http};
use crate::provider::{ProviderRow, ResolvedCredential};
use crate::tool_names::ToolNames;
use crate::wire;
use crate::wire::WireFormat;

/// The request-shape facts a resolver can prove entirely from operator-
/// supplied hosted configuration.
///
/// This is deliberately not a resolved generation model: it carries no
/// credential, metering, rates, pricing or generation authority.
pub struct StaticRequestProfile {
    base: Url,
    request_model: String,
    capabilities: ModelCapabilities,
    wire: &'static dyn WireFormat,
}

impl StaticRequestProfile {
    pub(crate) fn new(
        base: Url,
        request_model: String,
        capabilities: ModelCapabilities,
        wire: &'static dyn WireFormat,
    ) -> Self {
        Self {
            base,
            request_model,
            capabilities,
            wire,
        }
    }

    pub fn request_model(&self) -> &str {
        &self.request_model
    }

    pub fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    pub fn output_ceiling(&self) -> Option<u64> {
        self.capabilities
            .max_output_tokens
            .or(self.capabilities.max_context_tokens)
    }

    /// Prove that the row-selected mapping can encode this request without a
    /// credential, transport or provider resolution.
    pub fn ensure_expressible(
        &self,
        request: &GenRequest,
    ) -> Result<(), RequestExpressibilityError> {
        let target = wire::RequestTarget::new(&self.base, &[]);
        self.wire
            .build_request(&target, &ToolNames::new(request).encode(request))
            .map(drop)
            .map_err(|refused| RequestExpressibilityError::new(refused.to_string()))
    }
}

/// A [`Generator`] that sends a profile's own wire format over an [`Http`].
///
/// The resolved model supplies the mapping beside the row, so dynamic model
/// authorities cannot expose a plausible protocol before resolution.
pub struct WireGenerator {
    connection_id: Option<String>,
    cache_ttl_seconds: Option<std::num::NonZeroU64>,
    row: ProviderRow,
    service_tier: crate::service_tier::ServiceTier,
    wire: &'static dyn WireFormat,
    profile: AdmittedHttpProfile,
    /// `Arc`, not a borrow, and that is forced rather than preferred:
    /// [`AttemptStream::new`] takes a `BoxStream<'static, _>`, so the future
    /// that executes the admitted profile outlives any borrow of `self`.
    http: Arc<dyn Http>,
    /// The row's credential, already resolved.
    ///
    /// The one copy in the process, and it lives here because this is the one
    /// thing that must send it — the row must not hold a secret
    /// (`docs/internals/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3) and
    /// neither may a `ResolvedModel`, which is `Clone` and travels to the
    /// ledger.
    ///
    /// Resolved rather than resolvable, which is
    /// `2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §6's whole
    /// point: this field used to be an `Arc<dyn Credentials>` and every
    /// generation re-read the environment through it, so a variable rotated
    /// mid-run changed behaviour silently and every generation was fallible
    /// for a condition settled at startup. Now the failure has exactly one
    /// site, and it is the site that can name the variable to set.
    credential: ResolvedCredential,
    generation_role: ego_core::event::domain::GenerationRole,
    protocol_filter: Option<(
        ego_core::config::ProtocolFilterConfig,
        Arc<dyn crate::protocol_filter::ProtocolFilterCommand>,
    )>,
    codex_session: Arc<futures::lock::Mutex<crate::codex_session::Session>>,
}

impl WireGenerator {
    pub fn new(
        row: ProviderRow,
        wire: &'static dyn WireFormat,
        http: Arc<dyn Http>,
        credential: ResolvedCredential,
    ) -> Self {
        let profile = match credential.base_override() {
            Some(base) => AdmittedHttpProfile::from_urls(row.id(), std::slice::from_ref(base))
                .expect("a resolved provider base is an admitted HTTP route"),
            None => row.http_profile.clone(),
        };
        WireGenerator {
            connection_id: None,
            cache_ttl_seconds: None,
            row,
            service_tier: crate::service_tier::ServiceTier::Standard,
            wire,
            profile,
            http,
            credential,
            codex_session: Arc::default(),
            protocol_filter: None,
            generation_role: ego_core::event::domain::GenerationRole::Main,
        }
    }

    /// Bind private continuation and cache evidence to a stable local identity.
    /// Display names and token refreshes do not change this identity.
    pub fn with_connection_id(mut self, connection_id: Option<String>) -> Self {
        self.connection_id = connection_id;
        self
    }

    pub fn matches_connection_id(&self, connection_id: Option<&str>) -> bool {
        self.connection_id.as_deref() == connection_id
    }

    /// A user estimate for provider-managed lifetimes; never sent on the wire.
    pub fn with_cache_ttl_seconds(mut self, ttl: Option<std::num::NonZeroU64>) -> Self {
        self.cache_ttl_seconds = ttl;
        self
    }

    /// Bind a tier to this generation profile, not mutable process state.
    pub fn with_service_tier(mut self, tier: crate::service_tier::ServiceTier) -> Self {
        self.service_tier = tier;
        self
    }

    /// Bind the resolved role used by the startup context.
    pub fn with_generation_role(mut self, role: ego_core::event::domain::GenerationRole) -> Self {
        self.generation_role = role;
        self
    }

    /// Bind an explicitly selected user filter to this provider generator.
    pub fn with_protocol_filter(
        mut self,
        config: ego_core::config::ProtocolFilterConfig,
        command: Arc<dyn crate::protocol_filter::ProtocolFilterCommand>,
    ) -> Self {
        if config.enabled {
            self.protocol_filter = Some((config, command));
        }
        self
    }

    pub fn service_tier(&self) -> crate::service_tier::ServiceTier {
        self.service_tier
    }

    /// Whether a fresh resolution can retain this run-owned transport session.
    pub fn matches_resolution(
        &self,
        row: &ProviderRow,
        wire: &'static dyn WireFormat,
        credential: &ResolvedCredential,
    ) -> bool {
        self.row.id() == row.id()
            && self.row.base() == row.base()
            && self.row.http_profile == row.http_profile
            && self.row.extra_headers == row.extra_headers
            && std::ptr::eq(self.wire, wire)
            && self.credential.base_override() == credential.base_override()
            && self.credential.headers() == credential.headers()
            && match (&self.credential.signer, &credential.signer) {
                (Some(current), Some(next)) => Arc::ptr_eq(current, next),
                (None, None) => true,
                _ => false,
            }
    }

    /// Which provider this generates against.
    pub fn provider(&self) -> &str {
        self.row.id()
    }

    fn build_request(
        &self,
        req: &GenRequest,
    ) -> Result<(crate::http::HttpRequest, ToolNames), wire::RequestRefused> {
        for block in req
            .messages
            .messages()
            .iter()
            .flat_map(|message| &message.content)
        {
            if let ego_core::generator::types::ContentBlock::Reasoning { state }
            | ego_core::generator::types::ContentBlock::ProviderContent { state, .. } = block
                && state.connection != self.connection_id
            {
                return Err(wire::RequestRefused::new(
                    "private provider history belongs to another or unbound connection; start a new session for this connection",
                ));
            }
        }
        if let Some((config, _)) = &self.protocol_filter {
            config.validate().map_err(wire::RequestRefused::new)?;
            if self.credential.signer.is_some() {
                return Err(wire::RequestRefused::new(
                    "protocol_filter cannot rewrite a SigV4-signed Bedrock request",
                ));
            }
            if self.wire.websocket() {
                return Err(wire::RequestRefused::new(
                    "protocol_filter v1 does not support rewriting WebSocket transports",
                ));
            }
        }
        if req.web_search.is_some() && !self.supports_web_search() {
            return Err(wire::RequestRefused::new(
                "this provider does not support native hosted search; no fallback is used",
            ));
        }
        let mut headers = self.credential.headers();
        for (name, value) in &self.row.extra_headers {
            headers.retain(|existing| !existing.name().eq_ignore_ascii_case(name));
            headers.push(
                ego_core::permit::HttpHeader::new(name, value.as_bytes().to_vec())
                    .expect("a validated provider profile header is valid"),
            );
        }
        let base = self.credential.base_override().unwrap_or(self.row.base());
        let target = wire::RequestTarget::new(base, &headers);
        let names = ToolNames::new(req);
        let encoded = names.encode(req);
        let mut request = self.wire.build_request(&target, &encoded)?;
        if self.service_tier == crate::service_tier::ServiceTier::Fast || self.row.id() == "openai"
        {
            let slug = format!("{}/{}", self.row.id(), req.model);
            self.service_tier
                .validate(&slug)
                .map_err(wire::RequestRefused::new)?;
            let support = (self.service_tier == crate::service_tier::ServiceTier::Fast)
                .then(|| crate::service_tier::fast_support(&slug).expect("validated fast support"));
            let mut body: serde_json::Value =
                serde_json::from_slice(request.body.as_deref().ok_or_else(|| {
                    wire::RequestRefused::new("service tier requires a JSON request body")
                })?)
                .map_err(|error| wire::RequestRefused::new(error.to_string()))?;
            // OpenAI omission inherits the project tier, which may be Fast.
            let (field, value) = support
                .as_ref()
                .map_or(("service_tier", "default"), |support| {
                    (support.wire_field, support.wire_value)
                });
            body[field] = value.into();
            request.body = Some(
                serde_json::to_vec(&body)
                    .map_err(|error| wire::RequestRefused::new(error.to_string()))?,
            );
            if let Some(beta) = support.and_then(|support| support.beta_header) {
                // Preserve beta capabilities already requested by the wire.
                let previous = request
                    .headers
                    .iter()
                    .find(|header| header.name().eq_ignore_ascii_case("anthropic-beta"))
                    .map(|header| String::from_utf8_lossy(header.value()).into_owned());
                request
                    .headers
                    .retain(|header| !header.name().eq_ignore_ascii_case("anthropic-beta"));
                let value = previous
                    .map_or_else(|| beta.to_owned(), |previous| format!("{previous},{beta}"));
                request.headers.push(
                    ego_core::permit::HttpHeader::new("anthropic-beta", value.into_bytes())
                        .expect("verified beta header"),
                );
            }
        }
        for (index, (original, tool)) in req.tools.iter().zip(&encoded.tools).enumerate() {
            tracing::debug!(target: "ego_ai::tool_names", index, original_name = %original.name,
                provider_name = %tool.name, "provider tool declaration");
        }
        if let Some(signer) = &self.credential.signer {
            signer
                .sign(&mut request, std::time::SystemTime::now())
                .map_err(wire::RequestRefused::new)?;
        }
        trace_request_tool_names(&request);
        Ok((request, names))
    }
}

fn trace_request_tool_names(request: &crate::http::HttpRequest) {
    // A projection of the serialized request, with no prompt, schemas,
    // headers or credentials. This records what the wire actually built.
    if tracing::enabled!(target: "ego_ai::tool_names", tracing::Level::DEBUG)
        && let Some(body) = &request.body
        && let Ok(body) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(tools) = body.get("tools").and_then(serde_json::Value::as_array)
    {
        let sent: Vec<_> = tools
            .iter()
            .map(|tool| {
                tool.get("name").or_else(|| {
                    tool.get("function")
                        .and_then(|function| function.get("name"))
                })
            })
            .collect();
        tracing::debug!(target: "ego_ai::tool_names", request_tool_names = ?sent,
            "serialized provider request tool names");
    }
}

impl Generator for WireGenerator {
    fn cache_request_identity(
        &self,
        req: &GenRequest,
        previous_input_items: Option<usize>,
    ) -> Option<ego_core::usage::CacheRequestIdentity> {
        if self.protocol_filter.is_some()
            || req.cache.as_ref()?.retention
                == ego_core::generator::capabilities::CacheRetention::None
        {
            return None;
        }
        let (request, _) = self.build_request(req).ok()?;
        let credential = crate::usage_limits::credential_fingerprint(&request.headers);
        let scope = ego_core::digest::sha256_hex(
            &serde_json::to_vec(&(
                self.row.id(),
                &self.connection_id,
                req.model.as_str(),
                request.url.as_str(),
                credential,
                &self.row.extra_headers,
            ))
            .ok()?,
        );
        self.wire
            .cache_request_identity(request.body.as_deref()?, scope, previous_input_items)
    }

    fn prompt_cache_policy(&self, req: &GenRequest) -> ego_core::usage::PromptCachePolicy {
        // A user filter can rewrite cache controls; its output has no declared TTL contract.
        let mut policy = if self.protocol_filter.is_some() {
            ego_core::usage::PromptCachePolicy::default()
        } else {
            self.wire.prompt_cache_policy(req)
        };
        if policy.ttl_seconds.is_none()
            && req.cache.as_ref().is_some_and(|cache| {
                cache.retention != ego_core::generator::capabilities::CacheRetention::None
            })
        {
            policy.ttl_seconds = self.cache_ttl_seconds;
            // An override explicitly selects the user's request-to-request estimate.
            policy.refresh_on_read = self.cache_ttl_seconds.is_some();
        }
        policy
    }

    fn release_idle_connection(&self, session: ego_core::event::id::SessionId) {
        // An active stream owns the lock and its socket, not idle state. Only
        // this session's completed connection may be released here.
        if let Some(mut owner) = self.codex_session.try_lock() {
            owner.release_idle_connection(session);
        }
    }

    fn supports_web_search(&self) -> bool {
        self.wire.supports_web_search()
    }

    fn supports_default_web_search(&self, model: &str) -> bool {
        self.wire.supports_web_search()
            && self.wire.supports_default_web_search(
                model,
                self.credential.base_override().unwrap_or(self.row.base()),
                &self.credential,
                &self.row.credential,
            )
    }

    fn ensure_expressible(&self, req: &GenRequest) -> Result<(), RequestExpressibilityError> {
        self.build_request(req)
            .map(drop)
            .map_err(|refused| RequestExpressibilityError::new(refused.to_string()))
    }

    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        // No credential branch here, and its absence is the change §6 bought.
        // A missing key is refused once, at resolution, where the message can
        // name the variable to set and the exit code can be `CLI.md` §4.3's 2
        // rather than a provider's 4. What used to sit here was the same
        // refusal re-derived per generation, for a condition that could not
        // have changed since — and it re-read the environment to do it, so a
        // rotated variable silently changed a running turn.
        let (request, names) = match self.build_request(&req) {
            Ok(request) => request,
            // Refused before a socket exists. `DefinitelyUnsent` is honest
            // here because the decision is a pure function of the request —
            // no DNS, no connection, no bytes.
            //
            // Returning an empty stream instead would settle as an abort with
            // a missing usage report, which says "the provider was called and
            // went silent" about a provider that was never called.
            //
            // `Invalid`, and the one classification in the tree that needs
            // no provider at all to be certain of: `build_request` refused
            // because this wire format cannot express this request, which
            // is a fact about the request and identical on every retry.
            Err(refused) => {
                let events = wire::failed(
                    refused.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                    ErrorKind::Invalid,
                );
                return AttemptStream::new(attempt, stream::iter(events).boxed());
            }
        };

        let prepared_prefix = Arc::new(std::sync::OnceLock::new());
        if self.protocol_filter.is_none()
            && let Some(prefix) = request
                .body
                .as_deref()
                .and_then(|body| self.wire.cache_prefix(body))
                .and_then(|digest| {
                    req.cache
                        .as_ref()
                        .map(|cache| ego_core::generator::attempt::CachePrefix {
                            session: cache.session_id,
                            provider: self.row.id().to_owned(),
                            model: req.model.clone(),
                            digest,
                        })
                })
        {
            let _ = prepared_prefix.set(prefix);
        }
        let signer = self.credential.signer.clone();
        let diagnostic_headers = if signer.is_some() {
            request
                .headers
                .iter()
                .filter(|header| header.is_secret())
                .cloned()
                .collect()
        } else {
            self.credential.headers()
        };
        let connection_id = self.connection_id.clone();
        let publish_prefix = Arc::clone(&prepared_prefix);
        let filter = self.protocol_filter.clone();
        let generation_role = self.generation_role;
        let attempt_ref = attempt.attempt();
        tracing::debug!(target: "ego_ai::tool_names", provider = self.row.id(), model = %req.model,
            "provider generation request");
        let http = Arc::clone(&self.http);
        let quota_http = Arc::clone(&self.http);
        let quota_credential =
            crate::usage_limits::credential_fingerprint(&self.credential.headers());
        let profile = self.profile.clone();
        // The row's own declaration of where it states a charge, carried to
        // the decoder rather than looked up there: one mapping is spoken by
        // providers that report a cost and by providers that do not, and a
        // `match` on the provider id inside a wire format is the special case
        // the metering ADR §8 forbids.
        let cost = self.row.cost.clone();
        let wire = self.wire;
        let websocket = wire.websocket();
        let follow_redirects = self.credential.signer.is_none();
        let session = self.codex_session.clone();
        let provider = self.row.id().to_owned();
        let limit_model = format!("{}/{}", provider, req.model);
        let decode_provider = provider.clone();
        let hosted_search = req.web_search;
        let (filter_events, local_events) = futures::channel::mpsc::unbounded();
        let inner = stream::once(async move {
            let mut request = request;
            let filtered = filter.is_some();
            let response_filter = if let Some((config, command)) = filter {
                crate::protocol_filter::apply(
                    &config,
                    command.as_ref(),
                    crate::protocol_filter::FilterIdentity {
                        provider: &provider,
                        model: &req.model,
                        generation_role,
                        wire_format: wire.name(),
                    },
                    attempt_ref,
                    &mut request,
                    &filter_events,
                )
                .await?
            } else {
                None
            };
            if filtered
                && let Some(prefix) = request
                    .body
                    .as_deref()
                    .and_then(|body| wire.cache_prefix(body))
                    .and_then(|digest| {
                        req.cache
                            .as_ref()
                            .map(|cache| ego_core::generator::attempt::CachePrefix {
                                session: cache.session_id,
                                provider: provider.clone(),
                                model: req.model.clone(),
                                digest,
                            })
                    })
            {
                let _ = publish_prefix.set(prefix);
            }
            let response = if websocket {
                crate::codex_session::execute(
                    session,
                    http,
                    profile,
                    request,
                    req.cache.as_ref().map(|cache| cache.session_id),
                    crate::codex_session::Quota {
                        provider: provider.clone(),
                        credential: quota_credential.clone(),
                    },
                )
                .await
            } else {
                profile
                    .execute_with_redirects(http.as_ref(), request, follow_redirects)
                    .await
                    .map(|response| (response, ego_core::usage::CacheTransport::HttpFull))
            };
            if let Ok((response, _)) = &response {
                quota_http.observe_rate_limits(&provider, &quota_credential, &response.headers);
            }
            Ok::<_, crate::protocol_filter::FilterFailure>(match (response, response_filter) {
                (Ok((response, transport)), Some(filter)) => filter
                    .wrap(response)
                    .await
                    .map(|response| (response, transport)),
                (response, _) => response,
            })
        })
        .flat_map(move |result| {
            let response = match result {
                Ok(response) => response,
                Err(failure) => {
                    use ego_core::generator::port::AiEvent;
                    let diagnostic = failure.message();
                    return stream::iter([
                        AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                        AiEvent::Error {
                            message: diagnostic.clone(),
                            kind: failure.kind(),
                        },
                        AiEvent::LocalFailure { cause: diagnostic },
                        AiEvent::Stop(ego_core::generator::types::StopReason::Error),
                    ])
                    .boxed();
                }
            };
            let transport = response.as_ref().ok().map(|(_, transport)| {
                ego_core::generator::port::AiEvent::CacheTransportObserved(*transport)
            });
            let response = response.map(|(response, _)| response);
            let limits = response
                .as_ref()
                .ok()
                .map(|response| {
                    crate::usage_limits::response_limits(&decode_provider, &response.headers)
                })
                .unwrap_or_default();
            let observation =
                (!limits.is_empty()).then(|| ego_core::generator::port::AiEvent::ProviderLimits {
                    provider: decode_provider.clone(),
                    model: limit_model.clone(),
                    limits,
                });
            stream::iter(transport)
                .chain(stream::iter(observation))
                .chain(wire.decode_generation(response, cost.clone(), hosted_search))
                .boxed()
        })
        .map(move |mut event| {
            use ego_core::generator::port::AiEvent;
            match &mut event {
                AiEvent::ReasoningState(state) | AiEvent::ProviderContent { state, .. } => {
                    state.connection = connection_id.clone();
                }
                _ => {}
            }
            // Providers can reflect the credential in a typed error field.
            // Mask it before either the host or attempt receipt observes it.
            let diagnostic = match &mut event {
                AiEvent::Error { message, .. } => Some(message),
                AiEvent::ProviderFailure { cause, .. } | AiEvent::LocalFailure { cause } => {
                    Some(cause)
                }
                _ => None,
            };
            if let Some(diagnostic) = diagnostic {
                if let Some(signer) = &signer {
                    signer.redact_diagnostic(diagnostic, &diagnostic_headers);
                } else {
                    for header in diagnostic_headers
                        .iter()
                        .filter(|header| header.is_secret())
                    {
                        let value = header.value();
                        let value = value.strip_prefix(b"Bearer ").unwrap_or(value);
                        if let Ok(value) = std::str::from_utf8(value)
                            && !value.is_empty()
                        {
                            *diagnostic = diagnostic.replace(value, "[REDACTED]");
                        }
                    }
                }
            }
            event
        })
        .flat_map(move |event| stream::iter(names.restore(event)))
        .boxed();
        // Polling the provider/filter can queue a local diagnostic and return
        // its terminal error in the same poll. Drain that diagnostic first;
        // a caller may legitimately stop consuming at the terminal marker.
        let mut inner = inner.fuse();
        let mut local_events = local_events.fuse();
        let mut pending = None;
        let ordered = stream::poll_fn(move |cx| {
            use futures::stream::FusedStream;
            use std::task::Poll;
            if let Poll::Ready(Some(local)) = local_events.poll_next_unpin(cx) {
                return Poll::Ready(Some(local));
            }
            if let Some(event) = pending.take() {
                return Poll::Ready(Some(event));
            }
            let polled = inner.poll_next_unpin(cx);
            if let Poll::Ready(Some(local)) = local_events.poll_next_unpin(cx) {
                if let Poll::Ready(Some(event)) = polled {
                    pending = Some(event);
                }
                return Poll::Ready(Some(local));
            }
            match polled {
                Poll::Ready(Some(event)) => Poll::Ready(Some(event)),
                _ if inner.is_terminated() && local_events.is_terminated() => Poll::Ready(None),
                _ => Poll::Pending,
            }
        });
        AttemptStream::new(attempt, ordered.boxed()).with_prepared_cache_prefix(prepared_prefix)
    }
}
