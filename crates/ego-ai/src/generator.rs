//! The binding: the wire format, the transport and the attempt, joined.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `docs/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
//!
//! This module composes and returns. It folds no delivery certainty and
//! decides no stop reason, because both already have exactly one owner:
//! [`HttpError`](crate::http::HttpError) carries the transport's reading of
//! delivery in both variants, [`decode`](crate::wire::openai_completions::decode)
//! turns that into events, and `AttemptStream` folds them monotonically and
//! keeps only the first terminal frame. A second fold here would be a
//! competing spelling of an existing one, invisible until the two disagreed.

use std::sync::Arc;

use ego_core::generator::attempt::{AttemptStream, DeliveryCertainty, GenerationAttempt};
use ego_core::generator::port::Generator;
use ego_core::generator::types::GenRequest;
use futures::{StreamExt, stream};

use crate::http::Http;
use crate::provider::ProviderRow;
use crate::wire::{self, openai_completions};

/// A [`Generator`] that sends `openai-completions` over an [`Http`].
///
/// Names the wire format directly rather than selecting one, for the reason
/// `wire/mod.rs` gives: there is one implementation, and a seam built to a
/// second one that does not exist yet is built to an imagined requirement.
pub struct WireGenerator {
    row: ProviderRow,
    /// `Arc`, not a borrow, and that is forced rather than preferred:
    /// [`AttemptStream::new`] takes a `BoxStream<'static, _>`, so the future
    /// that calls [`Http::execute`] outlives any borrow of `self`.
    http: Arc<dyn Http>,
}

impl WireGenerator {
    pub fn new(row: ProviderRow, http: Arc<dyn Http>) -> Self {
        WireGenerator { row, http }
    }

    /// Which provider this generates against.
    pub fn provider(&self) -> &str {
        &self.row.id
    }
}

impl Generator for WireGenerator {
    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        let request = match openai_completions::build_request(
            self.row.endpoint.clone(),
            self.row.headers(),
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
            Err(refused) => {
                let events = wire::failed(refused.to_string(), DeliveryCertainty::DefinitelyUnsent);
                return AttemptStream::new(attempt, stream::iter(events).boxed());
            }
        };

        let http = Arc::clone(&self.http);
        // The row's own declaration of where it states a charge, carried to
        // the decoder rather than looked up there: `openai_completions` is
        // spoken by providers that report a cost and by providers that do
        // not, and a `match` on the provider id inside a wire format is the
        // special case the metering ADR §8 forbids.
        let cost = self.row.cost.clone();
        let inner = stream::once(async move { http.execute(request).await })
            .flat_map(move |response| openai_completions::decode(response, cost.clone()))
            .boxed();
        AttemptStream::new(attempt, inner)
    }
}
