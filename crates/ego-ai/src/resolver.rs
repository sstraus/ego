//! What turns `provider/model` into a [`ResolvedModel`], as a choice the row
//! makes rather than a branch the caller writes.
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §4
//! names the line this replaces: `crates/ego-cli/src/model.rs` asked
//! `if slug.provider == catalogue::OLLAMA` and called one of two functions.
//! That branch is the same mistake the wire seam was built to remove, one
//! layer up — a caller that must know which providers are daemons in order to
//! resolve any of them — and it fails in the ordinary way: the second daemon
//! is a second arm, written by whoever adds it, in a crate that should not
//! have needed touching.
//!
//! # Two producers, and no third kept warm
//!
//! [`Supplied`] reads what an operator wrote down; [`OllamaDaemon`] asks the
//! daemon. `2026-08-21-what-a-model-resolves-to.md` is the argument for why
//! those are genuinely different kinds of answer rather than two paths to one:
//! a hosted model's ceilings and prices are facts nobody can discover, and a
//! local model's are facts nobody can write down.
//!
//! The ADR's §4 sketch has a third, `XaiLanguageModels` — prices asked of the
//! provider per key. It is absent here because `AGENTS.md` forbids a variant
//! kept warm for a producer that does not exist, and it arrives with the xAI
//! row.
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

use crate::catalogue::{Catalogue, ModelSlug, ResolveError, ResolvedModel};
use crate::http::Http;
use crate::provider::ProviderRow;

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
}

/// How a provider's models are turned into the facts a turn needs.
///
/// Dyn-compatible, so a row can hold one, which means the future is boxed
/// rather than an `async fn`. That is the same shape [`Http`] uses and for the
/// same reason.
pub trait ModelResolver: Send + Sync + fmt::Debug {
    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>>;
}

/// Resolved from what an operator supplied, keyed by full slug.
///
/// The right answer for every hosted provider: nothing about a hosted model's
/// price or ceilings is discoverable from its API, so what this build knows is
/// what it was told.
#[derive(Debug)]
pub struct Supplied;

impl ModelResolver for Supplied {
    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        // Ready rather than an `async` block: this opens nothing and waits for
        // nothing, and a future that is already done says so.
        Box::pin(std::future::ready(at.catalogue.resolve_hosted(at.slug)))
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
    fn resolve<'a>(
        &'a self,
        at: &'a Resolution<'a>,
    ) -> BoxFuture<'a, Result<ResolvedModel, ResolveError>> {
        Box::pin(crate::ollama::resolve(at.row, &at.slug.model, at.http))
    }
}
