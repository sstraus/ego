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
use ego_core::generator::port::{Generator, RequestExpressibilityError};
use ego_core::generator::types::{ErrorKind, GenRequest};
use futures::{StreamExt, stream};

use crate::http::{AdmittedHttpProfile, Http};
use crate::provider::{ProviderRow, ResolvedCredential};
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
    profile: AdmittedHttpProfile,
    /// `Arc`, not a borrow, and that is forced rather than preferred:
    /// [`AttemptStream::new`] takes a `BoxStream<'static, _>`, so the future
    /// that executes the admitted profile outlives any borrow of `self`.
    http: Arc<dyn Http>,
    /// The row's credential, already resolved.
    ///
    /// The one copy in the process, and it lives here because this is the one
    /// thing that must send it — the row must not hold a secret
    /// (`docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3) and
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
}

impl WireGenerator {
    pub fn new(row: ProviderRow, http: Arc<dyn Http>, credential: ResolvedCredential) -> Self {
        let profile = match credential.base_override() {
            Some(base) => AdmittedHttpProfile::from_urls(row.id(), std::slice::from_ref(base))
                .expect("a resolved provider base is an admitted HTTP route"),
            None => row.http_profile.clone(),
        };
        WireGenerator {
            row,
            profile,
            http,
            credential,
        }
    }

    /// Which provider this generates against.
    pub fn provider(&self) -> &str {
        self.row.id()
    }

    fn build_request(
        &self,
        req: &GenRequest,
    ) -> Result<crate::http::HttpRequest, wire::RequestRefused> {
        let mut headers = self.credential.headers();
        for (name, value) in &self.row.extra_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
        let base = self.credential.base_override().unwrap_or(self.row.base());
        let target = wire::RequestTarget::new(base, &headers);
        self.row.wire.build_request(&target, req)
    }
}

impl Generator for WireGenerator {
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
        let request = match self.build_request(&req) {
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

        let http = Arc::clone(&self.http);
        let profile = self.profile.clone();
        // The row's own declaration of where it states a charge, carried to
        // the decoder rather than looked up there: one mapping is spoken by
        // providers that report a cost and by providers that do not, and a
        // `match` on the provider id inside a wire format is the special case
        // the metering ADR §8 forbids.
        let cost = self.row.cost.clone();
        let wire = self.row.wire;
        let inner = stream::once(async move { profile.execute(http.as_ref(), request).await })
            .flat_map(move |response| wire.decode(response, cost.clone()))
            .boxed();
        AttemptStream::new(attempt, inner)
    }
}
