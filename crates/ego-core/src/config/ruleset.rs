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

/// Which layers carry a rule.
///
/// A **set**, because "the user config denies this" is true or false and
/// cannot be true twice — so dedup is structural rather than a `contains`
/// check somebody can forget. One byte over six known layers, which also
/// keeps [`PolicyDenied`] small enough to return by value.
///
/// Iteration is most-authoritative-first, so the first element is the
/// [`authority`](SourcedRule::authority) and no caller has to sort.
///
/// Serialised as the list of layers it holds, not as its bitset. The bits are
/// how it is stored, not what it means, and a `4` in an audit log is a number
/// somebody has to decode by hand against an enum's declaration order — which
/// is the kind of coupling that breaks silently when a layer is inserted.
///
/// [`PolicyDenied`]: crate::permit::PolicyDenied
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "Vec<Source>", into = "Vec<Source>")]
pub struct OriginSet(u8);

impl OriginSet {
    pub fn just(source: Source) -> Self {
        let mut set = Self::default();
        set.insert(source);
        set
    }

    pub fn insert(&mut self, source: Source) {
        self.0 |= 1 << source.rank();
    }

    pub fn contains(self, source: Source) -> bool {
        self.0 & (1 << source.rank()) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The layers in this set, most authoritative first.
    pub fn iter(self) -> impl Iterator<Item = Source> {
        Source::ALL.into_iter().filter(move |s| self.contains(*s))
    }

    /// Everything in `self` that is not in `other`.
    pub fn without(self, other: Source) -> Self {
        Self(self.0 & !(1 << other.rank()))
    }
}

impl From<Vec<Source>> for OriginSet {
    fn from(sources: Vec<Source>) -> Self {
        let mut set = Self::default();
        for source in sources {
            set.insert(source);
        }
        set
    }
}

impl From<OriginSet> for Vec<Source> {
    fn from(set: OriginSet) -> Self {
        set.iter().collect()
    }
}

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
    origins: OriginSet,
}

impl SourcedRule {
    /// A rule carried by one layer.
    pub fn new(rule: Rule, origin: Source) -> Self {
        Self {
            rule,
            origins: OriginSet::just(origin),
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
        self.origins.insert(origin);
    }

    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// Every layer that carries this rule, most authoritative first.
    ///
    /// For rendering a subordinate clause, never for re-deriving the winner:
    /// that is [`authority`](Self::authority)'s job and it lives in one place
    /// on purpose (decision 5b).
    pub fn origins(&self) -> OriginSet {
        self.origins
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
            .next()
            .expect("a SourcedRule is constructed with an origin and never loses one")
    }

    /// The winner, and every other layer that also carries this rule.
    ///
    /// Split here rather than at the renderer, so decision 5a — *lead with
    /// the winner, the rest is a subordinate clause* — is a property of the
    /// value instead of a habit at each call site. Every source in the second
    /// half lost to the first, which is what makes "removing that line
    /// changes nothing" true of all of them and not just of the common case.
    ///
    /// The second half is empty for the ordinary single-layer rule, and that
    /// case must read as one answer rather than as a list of length one.
    pub fn attribution(&self) -> (Source, OriginSet) {
        let winner = self.authority();
        (winner, self.origins.without(winner))
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
