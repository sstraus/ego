//! The binding: the wire format, the transport and the attempt, joined.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `ARCHITECTURE.md`.
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
    request_model: String,
    capabilities: ModelCapabilities,
    wire: &'static dyn WireFormat,
}

impl StaticRequestProfile {
    pub(crate) fn new(
        request_model: String,
        capabilities: ModelCapabilities,
        wire: &'static dyn WireFormat,
    ) -> Self {
        Self {
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
        let base = Url::parse("https://request-shape.invalid/")
            .expect("the static request-shape base is a URL");
        let target = wire::RequestTarget::new(&base, &[]);
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
    /// (`ARCHITECTURE.md` §3) and
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
            row,
            service_tier: crate::service_tier::ServiceTier::Standard,
            wire,
            profile,
            http,
            credential,
            codex_session: Arc::default(),
        }
    }

    /// Bind a tier to this generation profile, not mutable process state.
    pub fn with_service_tier(mut self, tier: crate::service_tier::ServiceTier) -> Self {
        self.service_tier = tier;
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
    }

    /// Which provider this generates against.
    pub fn provider(&self) -> &str {
        self.row.id()
    }

    fn build_request(
        &self,
        req: &GenRequest,
    ) -> Result<(crate::http::HttpRequest, ToolNames), wire::RequestRefused> {
        if req.web_search.is_some() && !self.supports_web_search() {
            return Err(wire::RequestRefused::new(
                "this provider does not support native hosted search; no fallback is used",
            ));
        }
        let dialect = match self.row.id() {
            "openai" => "openai-responses",
            "anthropic" => "anthropic-messages",
            _ => "",
        };
        if req.messages.messages().iter().flat_map(|message| &message.content).any(|block| matches!(block, ego_core::generator::types::ContentBlock::ProviderContent { state } if state.dialect != dialect || dialect.is_empty())) {
            return Err(wire::RequestRefused::new("provider-owned search content cannot be replayed to another provider"));
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
        if self.service_tier == crate::service_tier::ServiceTier::Fast {
            let slug = format!("{}/{}", self.row.id(), req.model);
            self.service_tier
                .validate(&slug)
                .map_err(wire::RequestRefused::new)?;
            let support = crate::service_tier::fast_support(&slug).expect("validated fast support");
            let mut body: serde_json::Value =
                serde_json::from_slice(request.body.as_deref().ok_or_else(|| {
                    wire::RequestRefused::new("fast requires a JSON request body")
                })?)
                .map_err(|error| wire::RequestRefused::new(error.to_string()))?;
            body[support.wire_field] = support.wire_value.into();
            request.body = Some(
                serde_json::to_vec(&body)
                    .map_err(|error| wire::RequestRefused::new(error.to_string()))?,
            );
            if let Some(beta) = support.beta_header {
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
    fn release_idle_connection(&self, session: ego_core::event::id::SessionId) {
        // An active stream owns the lock and its socket, not idle state. Only
        // this session's completed connection may be released here.
        if let Some(mut owner) = self.codex_session.try_lock() {
            owner.release_idle_connection(session);
        }
    }

    fn supports_web_search(&self) -> bool {
        matches!(self.row.id(), "openai" | "anthropic")
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

        tracing::debug!(target: "ego_ai::tool_names", provider = self.row.id(), model = %req.model,
            "provider generation request");
        let http = Arc::clone(&self.http);
        let profile = self.profile.clone();
        // The row's own declaration of where it states a charge, carried to
        // the decoder rather than looked up there: one mapping is spoken by
        // providers that report a cost and by providers that do not, and a
        // `match` on the provider id inside a wire format is the special case
        // the metering ADR §8 forbids.
        let cost = self.row.cost.clone();
        let wire = self.wire;
        let websocket = wire.websocket();
        let session = self.codex_session.clone();
        let inner = stream::once(async move {
            if websocket {
                crate::codex_session::execute(
                    session,
                    http,
                    profile,
                    request,
                    req.cache.as_ref().map(|cache| cache.session_id),
                )
                .await
            } else {
                profile.execute(http.as_ref(), request).await
            }
        })
        .flat_map(move |response| wire.decode(response, cost.clone()))
        .flat_map(move |event| stream::iter(names.restore(event)))
        .boxed();
        AttemptStream::new(attempt, inner)
    }
}
