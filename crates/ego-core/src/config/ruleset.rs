//! The resolved ruleset: rules paired with the layers that carry them.
//!
//! `docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md` decision 2.
//! [`PermCfg`](super::PermCfg) stays the *layer* schema — what one file
//! deserialises into. This is the *resolved* form, and the difference is the
//! whole point: by the time `resolve` has merged six layers, the question
//! "which file do I edit to lift this" has no answer left in a bare
//! `Vec<Rule>`.

use super::layer::PermCfg;
use super::source::Source;
use crate::permit::rule::Rule;

/// A resolved rule and every layer that carries it.
///
/// `origins` is non-empty and deduplicated, and it is *every* layer rather
/// than the winning one — decision 5. Keeping only the winner would discard
/// the fact that answers the operator's immediate next question ("I deleted
/// my line and it is still denied"), at identical cost, which is the deferred
/// refactoring debt `AGENTS.md:22-37` forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedRule {
    rule: Rule,
    origins: Vec<Source>,
}

impl SourcedRule {
    /// A rule carried by one layer.
    pub fn new(rule: Rule, origin: Source) -> Self {
        Self {
            rule,
            origins: vec![origin],
        }
    }

    /// Record that `origin` also carries this rule. Idempotent.
    ///
    /// A union, never a replacement — decision 4. "Replace the origin when
    /// the new one outranks the old" would put a second opinion about
    /// precedence here, beside the one in [`authority`](Self::authority), and
    /// two places that both decide which layer wins is one place too many.
    /// That is the shape of the defect this whole document is about.
    pub fn also_carried_by(&mut self, origin: Source) {
        if !self.origins.contains(&origin) {
            self.origins.push(origin);
        }
    }

    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// Every layer that carries this rule, in the order they were merged.
    ///
    /// For rendering a subordinate clause, never for re-deriving the winner:
    /// that is [`authority`](Self::authority)'s job and it lives in one place
    /// on purpose (decision 5b).
    pub fn origins(&self) -> &[Source] {
        &self.origins
    }

    /// The layer whose copy an operator cannot get around — decision 1.
    ///
    /// **This is the *minimum* rank, and the inversion is deliberate.**
    /// [`Source::rank`] orders layers for *merging*, lowest first, so `flags`
    /// beats everything and wins a value. Authority over a deny runs the
    /// other way: `system` is the layer nothing above can lift (`CLI.md` §7,
    /// "non-relaxable deny/budget ceilings — the only source of ceilings"), so
    /// the most authoritative origin is the lowest-ranked one.
    ///
    /// Two orderings over one enum, pointing opposite ways, is exactly the
    /// thing a second call site gets backwards. It is computed here and
    /// nowhere else.
    ///
    /// The only operational question an attribution answers is *can I change
    /// this*, so reporting a layer the operator can edit when editing it
    /// changes nothing is worse than reporting nothing: it is a false remedy,
    /// and they cannot tell it from a true one until they have tried it.
    pub fn authority(&self) -> Source {
        self.origins
            .iter()
            .copied()
            .min_by_key(|source| source.rank())
            .expect("a SourcedRule is constructed with an origin and never loses one")
    }
}

/// The resolved allow and deny lists, each rule carrying its origins.
///
/// Order within each list is the order `resolve` produced, and it is
/// load-bearing: `PolicyEngine` matches first-rule-wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ruleset {
    pub allow: Vec<SourcedRule>,
    pub deny: Vec<SourcedRule>,
}

impl Ruleset {
    /// The block one layer contributes, every rule attributed to that layer.
    ///
    /// This is what the merge loop does when a layer states `permissions`, and
    /// it is a whole block rather than an addition because `resolve` replaces
    /// a layer's `permissions` wholesale.
    pub fn from_layer(perm: PermCfg, origin: Source) -> Self {
        let mut resolved = Self::default();
        for rule in perm.allow {
            Self::merge(&mut resolved.allow, rule, origin);
        }
        for rule in perm.deny {
            Self::merge(&mut resolved.deny, rule, origin);
        }
        resolved
    }

    /// Add `rule` from `origin`, merging into the existing entry if this
    /// predicate is already present.
    ///
    /// The dedup that used to live inline in `resolve` as
    /// `if !deny.contains(&entry) { deny.push(entry) }` — which is where the
    /// origin was being silently dropped.
    pub fn merge(list: &mut Vec<SourcedRule>, rule: Rule, origin: Source) {
        match list.iter_mut().find(|entry| entry.rule == rule) {
            Some(entry) => entry.also_carried_by(origin),
            None => list.push(SourcedRule::new(rule, origin)),
        }
    }
}
