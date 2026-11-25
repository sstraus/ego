//! Request-only content policy at external tool and hook dispatch boundaries.

use futures::future::BoxFuture;
use serde_json::Value;

use crate::hook::EffectScope;

pub struct EgressRequest<'a> {
    pub direction: &'static str,
    pub destination: &'a str,
    pub payload: Value,
    pub turn: Option<crate::event::id::TurnId>,
    pub scope: Option<&'a EffectScope>,
}

/// A user-selected local filter may refuse delivery, never authorize a route
/// or rewrite the dispatched request.
pub trait EgressFilter: Send + Sync {
    fn filter<'a>(&'a self, request: EgressRequest<'a>) -> BoxFuture<'a, Result<(), &'static str>>;
}
