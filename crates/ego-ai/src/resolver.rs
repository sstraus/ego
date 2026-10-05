//! What turns `provider/model` into a [`ResolvedModel`], as a choice the row
//! makes rather than a branch the caller writes.
//!
//! `ARCHITECTURE.md` §4
//! names the line this replaces: `crates/ego-cli/src/model.rs` asked
//! `if slug.provider == catalogue::OLLAMA` and called one of two functions.
//! That branch is the same mistake the wire seam was built to remove, one
//! layer up — a caller that must know which providers are daemons in order to
//! resolve any of them — and it fails in the ordinary way: the second daemon
//! is a second arm, written by whoever adds it, in a crate that should not
//! have needed touching.
//!
//! # Two sources of model facts
//!
//! [`Supplied`] reads what an operator wrote down; [`OllamaDaemon`] asks the
//! daemon. `2026-08-21-what-a-model-resolves-to.md` is the argument for why
//! those are genuinely different kinds of answer rather than two paths to one:
//! a hosted model's ceilings and prices are facts nobody can discover, and a
//! local model's are facts nobody can write down.
//!
//! # A name, or the thing itself
//!
//! The ADR specifies `resolver: ResolverId` discharged through a registry when
//! the table is built, and that is right *for the world where a row can be
//! written in a configuration file* — a file can hold a name and cannot hold a
//! trait object. This build has no such file: rows are constructed in Rust by
//! [`ProviderRow`](crate::provider::ProviderRow)'s own constructors, which can
//! name the thing directly. So the field is spelled the way `wire` already is,
//! for the reason given there: a resolver is code, every value of it is a unit
//! struct with no state, and there are exactly as many as this crate compiles.
//!
//! The id and its registry land with the file that needs them. Nothing here
//! has to be undone to add them — a registry maps a name onto exactly these
//! values — which is why this is not the deferral `AGENTS.md` forbids.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::catalogue::{Catalogue, ModelDiscoveryReport, ModelSlug, ResolveError, ResolvedModel};
use crate::generator::StaticRequestProfile;
use crate::http::Http;
use crate::provider::{ProviderRow, ResolvedCredential};
use crate::wire::WireFormat;
use ego_core::public::MeteringKind;

/// Everything a resolver may look at, and nothing else.
///
/// A struct rather than four parameters because the two implementations need
/// disjoint halves of it — [`Supplied`] never opens a socket, [`OllamaDaemon`]
/// never reads the supplied table — and a signature that named only what one
/// of them used would have to change when the other arrived.
pub struct Resolution<'a> {
    /// The table the row came from, holding what was supplied for hosted
    /// slugs.
    pub catalogue: &'a Catalogue,
    /// The row `slug.provider` selected, already found.
    ///
    /// Passed rather than looked up again: the caller has it, the lookup has
    /// already succeeded, and re-doing it would give both implementations a
    /// `?` on an error that cannot happen.
    pub row: &'a ProviderRow,
    /// The slug being resolved, whole — a resolver may need the provider half
    /// as well as the model half.
    pub slug: &'a ModelSlug,
    /// The transport, for a resolver that asks something. `--offline` is a
    /// property of this value (`http.rs`), so a resolver cannot reach the
    /// network the run refused.
    pub http: &'a Arc<dyn Http>,
    /// The credential this run resolved for [`row`](Self::row), for a resolver
    /// whose endpoint authenticates.
    ///
    /// **Borrowed, and that is what keeps §6 true.** The rule §6 states is not
    /// "one holder" but *one read*: the check that refuses a missing key and
    /// the header that goes on the wire must be the same read, or they can
    /// disagree about whether a key exists. A resolver that called
    /// `std::env::var` itself would be a second read — of a variable that may
    /// have been rotated since the first — and would put a credential lookup
    /// back inside a library that `AGENTS.md` says does not own the process
    /// environment.
    ///
    /// So it arrives as a borrow of the value the caller already resolved. It
    /// is reachable for the length of one resolution and cannot outlive it,
    /// which is the property that matters: a
    /// [`ResolvedModel`](crate::catalogue::ResolvedModel) is `Clone` and
    /// `Debug` and travels to the budget gate and the ledger, and nothing here
    /// may put this into one.
    pub credential: &'a ResolvedCredential,
}

/// Inputs shared by resolver-selected model enumeration.
pub struct Enumeration<'a> {
    pub catalogue: &'a Catalogue,
    pub row: &'a ProviderRow,
    pub http: &'a Arc<dyn Http>,
    pub credential: Option<&'a ResolvedCredential>,
}

/// Inputs a resolver may use for the request-shape facts that are already
/// operator-supplied. There is deliberately no transport or credential.
pub struct StaticResolution<'a> {
    pub catalogue: &'a Catalogue,
    pub slug: &'a ModelSlug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverCredentialRequirement {
    None,
    Required,
}

/// How a provider's models are turned into the facts a turn needs.
///
/// Dyn-compatible, so a row can hold one, which means the future is boxed
/// rather than an `async fn`. That is the same shape [`Http`] uses and for the
/// same reason.
pub trait ModelResolver: Send + Sync + fmt::Debug {
    /// The wire mapping whose request shape is already authoritative from
    /// operator-supplied hosted facts, without resolving a credential or
    /// consulting a provider or local daemon.
    ///
    /// Required rather than defaulted so every new resolver states whether
    /// it can participate in the pre-effect static request proof.
    fn supplied_request_mapping(
        &self,
        at: &StaticResolution<'_>,
    ) -> Result<Option<StaticRequestProfile>, ResolveError>;

    fn enumeration_credential(&self) -> ResolverCredentialRequirement {
        ResolverCredentialRequirement::None
    }

    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>>;

    fn enumerate<'a>(
        &'a self,
        at: &'a Enumeration<'a>,
    ) -> BoxFuture<'a, Result<ModelDiscoveryReport, String>>;
}

/// Resolved from what an operator supplied, keyed by full slug.
///
/// The right answer for every hosted provider: nothing about a hosted model's
/// price or ceilings is discoverable from its API, so what this build knows is
/// what it was told.
#[derive(Debug)]
pub struct Supplied {
    wire: &'static dyn WireFormat,
}

impl Supplied {
    pub const fn new(wire: &'static dyn WireFormat) -> Self {
        Self { wire }
    }
}

impl ModelResolver for Supplied {
    fn supplied_request_mapping(
        &self,
        at: &StaticResolution<'_>,
    ) -> Result<Option<StaticRequestProfile>, ResolveError> {
        at.catalogue
            .static_request_profile_with_mapping(at.slug, self.wire)
            .map(Some)
    }

    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        // Ready rather than an `async` block: this opens nothing and waits for
        // nothing, and a future that is already done says so.
        Box::pin(std::future::ready(
            at.catalogue
                .resolve_hosted(at.slug, self.wire)
                .map(|mut resolved| {
                    if let Some(cost) = &at.row.cost {
                        resolved.metering = crate::catalogue::provider_reported(cost.as_ref());
                    }
                    resolved
                }),
        ))
    }

    fn enumerate<'a>(
        &'a self,
        at: &'a Enumeration<'a>,
    ) -> BoxFuture<'a, Result<ModelDiscoveryReport, String>> {
        Box::pin(std::future::ready(
            at.catalogue.enumerate_hosted(at.row.id(), None),
        ))
    }
}

/// Builtin- or operator-supplied capabilities under a provider subscription
/// policy. Rates in `models.toml` cannot turn a flat-fee row back into
/// currency.
#[derive(Debug)]
pub struct SubscriptionSupplied {
    revision: &'static str,
    wire: &'static dyn WireFormat,
}

impl SubscriptionSupplied {
    pub const fn new(revision: &'static str, wire: &'static dyn WireFormat) -> Self {
        Self { revision, wire }
    }
}

impl ModelResolver for SubscriptionSupplied {
    fn supplied_request_mapping(
        &self,
        at: &StaticResolution<'_>,
    ) -> Result<Option<StaticRequestProfile>, ResolveError> {
        at.catalogue
            .static_request_profile_with_mapping(at.slug, self.wire)
            .map(Some)
    }

    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        Box::pin(async move {
            let mut resolved = at.catalogue.resolve_hosted(at.slug, self.wire)?;
            resolved.metering = crate::catalogue::subscription(self.revision);
            resolved.rates = None;
            Ok(resolved)
        })
    }

    fn enumerate<'a>(
        &'a self,
        at: &'a Enumeration<'a>,
    ) -> BoxFuture<'a, Result<ModelDiscoveryReport, String>> {
        Box::pin(std::future::ready(
            at.catalogue
                .enumerate_hosted(at.row.id(), Some(MeteringKind::Subscription)),
        ))
    }
}

/// Resolved by asking the local daemon what it actually serves.
///
/// `crate::ollama`'s module documentation is the argument: the daemon reports
/// two different context lengths and only the served one is true of the
/// request about to be sent, so it is measured rather than read.
#[derive(Debug)]
pub struct OllamaDaemon;

impl ModelResolver for OllamaDaemon {
    fn supplied_request_mapping(
        &self,
        _at: &StaticResolution<'_>,
    ) -> Result<Option<StaticRequestProfile>, ResolveError> {
        Ok(None)
    }

    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        Box::pin(crate::ollama::resolve(at.row, &at.slug.model, at.http))
    }

    fn enumerate<'a>(
        &'a self,
        at: &'a Enumeration<'a>,
    ) -> BoxFuture<'a, Result<ModelDiscoveryReport, String>> {
        Box::pin(crate::ollama::enumerate(at.row, at.http))
    }
}

#[derive(Debug)]
pub struct KimiManaged;

impl ModelResolver for KimiManaged {
    fn supplied_request_mapping(
        &self,
        _at: &StaticResolution<'_>,
    ) -> Result<Option<StaticRequestProfile>, ResolveError> {
        Ok(None)
    }

    fn enumeration_credential(&self) -> ResolverCredentialRequirement {
        ResolverCredentialRequirement::Required
    }

    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        Box::pin(crate::kimi::resolve(
            at.row,
            &at.slug.model,
            at.http,
            at.credential,
        ))
    }

    fn enumerate<'a>(
        &'a self,
        at: &'a Enumeration<'a>,
    ) -> BoxFuture<'a, Result<ModelDiscoveryReport, String>> {
        let Some(credential) = at.credential else {
            return Box::pin(std::future::ready(Err(format!(
                "{} model discovery requires its resolved credential",
                at.row.id()
            ))));
        };
        Box::pin(crate::kimi::enumerate(at.row, at.http, credential))
    }
}
