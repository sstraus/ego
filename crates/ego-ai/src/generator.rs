//! The binding: the wire format, the transport and the attempt, joined.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `docs/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
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
use ego_core::generator::port::Generator;
use ego_core::generator::types::{ErrorKind, GenRequest};
use futures::{StreamExt, stream};

use crate::http::Http;
use crate::provider::{Credentials, ProviderRow};
use crate::wire;

/// A [`Generator`] that sends a profile's own wire format over an [`Http`].
///
/// It no longer names a mapping. The row does — `row.wire` — so this type is
/// the same code for every one of the six providers, and adding a seventh
/// changes nothing here. That is the ADR §5 test in its narrowest form: if
/// adding a provider made this file grow, the seam would be in the wrong
/// place.
pub struct WireGenerator {
    row: ProviderRow,
    /// `Arc`, not a borrow, and that is forced rather than preferred:
    /// [`AttemptStream::new`] takes a `BoxStream<'static, _>`, so the future
    /// that calls [`Http::execute`] outlives any borrow of `self`.
    http: Arc<dyn Http>,
    /// Where the row's credential strategy is resolved, once per request.
    ///
    /// Held here rather than resolved into the row, because the row must not
    /// hold a secret — `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md`
    /// §3. This holds no secret either: it holds the *means* of getting one.
    creds: Arc<dyn Credentials>,
}

impl WireGenerator {
    pub fn new(row: ProviderRow, http: Arc<dyn Http>, creds: Arc<dyn Credentials>) -> Self {
        WireGenerator { row, http, creds }
    }

    /// Which provider this generates against.
    pub fn provider(&self) -> &str {
        &self.row.id
    }
}

impl Generator for WireGenerator {
    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        // Resolved before the body is built, and refused the same way a
        // request the format cannot express is refused. It should not happen:
        // `model::resolve` already called `Auth::ensure_available`, so by the
        // time a generation exists the variable was there. Should-not-happen
        // is not the same as cannot, and the honest answer to "the credential
        // vanished mid-run" is the one below — never a `Bearer ` header sent
        // to be blamed on the provider.
        //
        // `Invalid` and deliberately not `Refused`, whose doc names a
        // provider's answer — a 401, a 403, an exhausted balance. Nothing was
        // asked here, so there is no such answer to report. `Invalid` is the
        // one classification certain without a provider, and it is also the
        // one that exits the way this cause already exits at resolution: a
        // credential nobody configured is the operator's to fix, which
        // `CLI.md` §4.3 spells 2 and not 4.
        let headers = match self.row.headers(self.creds.as_ref()) {
            Ok(headers) => headers,
            Err(missing) => {
                let events = wire::failed(
                    missing.to_string(),
                    DeliveryCertainty::DefinitelyUnsent,
                    ErrorKind::Invalid,
                );
                return AttemptStream::new(attempt, stream::iter(events).boxed());
            }
        };
        let request = match self.row.wire.build_request(
            self.row.endpoint.clone(),
            headers,
            self.row.adaptations,
            &req,
        ) {
            Ok(request) => request,
            // Refused before a socket exists. `DefinitelyUnsent` is honest
            // here for the same reason `HttpError::refused_before_sending` is
            // allowed to assert it: the decision is a pure function of the
            // request — no DNS, no connection, no bytes.
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

        let http = Arc::clone(&self.http);
        // The row's own declaration of where it states a charge, carried to
        // the decoder rather than looked up there: one mapping is spoken by
        // providers that report a cost and by providers that do not, and a
        // `match` on the provider id inside a wire format is the special case
        // the metering ADR §8 forbids.
        let cost = self.row.cost.clone();
        let wire = self.row.wire;
        let inner = stream::once(async move { http.execute(request).await })
            .flat_map(move |response| wire.decode(response, cost.clone()))
            .boxed();
        AttemptStream::new(attempt, inner)
    }
}
