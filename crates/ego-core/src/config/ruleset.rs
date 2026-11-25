//! The resolved ruleset: rules paired with the layers that carry them.
//!
//! the design notes decision 2.
//! [`PermCfg`](super::PermCfg) stays the *layer* schema — what one file
//! deserialises into. This is the *resolved* form, and the difference is the
//! whole point: by the time `resolve` has merged six layers, the question
//! "which file do I edit to lift this" has no answer left in a bare
//! `Vec<Rule>`.

use super::layer::PermCfg;
use super::source::Source;
use crate::permit::Op;
use crate::permit::rule::Rule;
use std::path::Path;

/// Documentation and package hosts a development session fetches constantly.
/// Hosts that fetch an origin named in the request path (the Go module proxy and
/// pkg.go.dev) stay out: they would be a question-free channel for workspace text.
/// Rules only: a user `deny` still wins, and every redirect and DNS check of the
/// guarded transport still applies.
const DEVELOPMENT_HOST_RULES: &[&str] = &[
    "net(docs.rs:443)",
    "net(crates.io:443)",
    "net(static.crates.io:443)",
    "net(index.crates.io:443)",
    "net(doc.rust-lang.org:443)",
    "net(registry.npmjs.org:443)",
    "net(www.npmjs.com:443)",
    "net(pypi.org:443)",
    "net(files.pythonhosted.org:443)",
    "net(docs.python.org:443)",
    "net(developer.mozilla.org:443)",
    "net(nodejs.org:443)",
];

const CREDENTIAL_READ_RULES: &[&str] = &[
    "read(/**/.env*)",
    "read(/**/.ssh/**)",
    "read(/**/.aws/**)",
    "read(/**/.netrc)",
    "read(/**/Keychains/**)",
    "read(/**/*.keychain)",
    "read(/**/*.keychain-db)",
];

/// Which layers carry a rule.
///
/// A **set**, because "the user config denies this" is true or false and
/// cannot be true twice — so dedup is structural rather than a `contains`
/// check somebody can forget. One byte over eight known origins, which also
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

    /// True for an `exec(...)` rule that names one exact argv, with no wildcard token, and
    /// that the owner wrote: the user file or a launch option. A project file, a local file
    /// or an agent file can be written by the model and never pre-approves a change of
    /// ego's own authority.
    pub(crate) fn is_exact_exec(&self) -> bool {
        (self.origins.contains(Source::User) || self.origins.contains(Source::Flags))
            && matches!(&self.rule, Rule::Exec(pattern) if pattern.is_exact())
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

    /// Reuse the exact user rule authority for protected filesystem paths.
    pub(crate) fn exact_owner_allow(&self, op: &Op, root: &Path) -> bool {
        self.origins.contains(Source::User) && self.rule.matches_exact_path(op, root)
    }

    /// A user `net` rule that names hosts is the owner's grant for them.
    pub(crate) fn owner_net_allow(&self, op: &Op) -> bool {
        self.origins.contains(Source::User) && self.rule.matches_owner_net(op)
    }

    /// Match a deny, allowing only an effective user grant to lift defaults.
    pub(crate) fn denies(&self, op: &Op, root: &Path, allow: &[SourcedRule]) -> bool {
        let direct = self.rule.matches(op, root)
            && !(self.origins == OriginSet::just(Source::Default)
                && allow.iter().any(|entry| {
                    entry.origins.contains(Source::User)
                        && if matches!(op, Op::Read { path } if Ruleset::is_credential_path(path)) {
                            entry.exact_owner_allow(op, root)
                        } else {
                            entry.rule.matches_allow(op, root, true)
                        }
                }));
        if direct {
            return true;
        }
        let (Rule::Exec(pattern), Op::Spawn { spec }) = (&self.rule, op) else {
            return false;
        };
        crate::permit::shell::static_commands(spec.argv()).is_some_and(|commands| {
            commands.iter().any(|argv| {
                pattern.matches(argv)
                    && !(self.origins == OriginSet::just(Source::Default)
                        && allow.iter().any(|entry| {
                            entry.origins.contains(Source::User)
                                && matches!(&entry.rule, Rule::Exec(pattern) if pattern.matches(argv))
                        }))
            })
        })
    }

    /// Every authority needed by an operation, including all literal commands
    /// inside a shell list. Direct shell grants keep their exact semantics.
    pub(crate) fn allowing<'a>(allow: &'a [Self], op: &Op, root: &Path) -> Option<Vec<&'a Self>> {
        if let Some(entry) = allow.iter().find(|entry| {
            entry
                .rule
                .matches_allow(op, root, entry.origins.contains(Source::User))
        }) {
            return Some(vec![entry]);
        }
        let Op::Spawn { spec } = op else {
            return None;
        };
        let cwd = spec.cwd()?.as_path();
        if !cwd.starts_with(root)
            || cwd
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return None;
        }
        crate::permit::shell::static_commands(spec.argv())?
            .iter()
            .map(|argv| {
                allow.iter().find(
                    |entry| matches!(&entry.rule, Rule::Exec(pattern) if pattern.matches(argv)),
                )
            })
            .collect()
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
    /// **Authority order differs from merge precedence.**
    /// [`Source::rank`] orders layers for *merging*, lowest first, so `flags`
    /// beats everything and wins a value. Authority over a deny runs the
    /// other way: `system` is the layer nothing above can lift (`CLI.md` §7,
    /// "non-relaxable deny/budget ceilings — the only source of ceilings"), so
    /// the most authoritative origin is the lowest-ranked one.
    ///
    /// Built-in defaults are least authoritative, below operator rules.
    /// Two orderings over one enum is exactly the
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
    /// The owner's `web_private_network = true`: web admission may dial a
    /// loopback, LAN or shared-space (Tailscale) address for any admitted
    /// call. Only the user layer sets it; never a rule or a mode.
    pub web_private_network: bool,
}

impl Ruleset {
    /// Whether a deny rule already covers the sandbox widening the owner would be asked
    /// about: a kept "keep blocked" answer is not asked again. Execution has no path rule.
    pub fn denies_sandbox(&self, grant: &crate::permit::SandboxGrant, workspace: &Path) -> bool {
        let op = match grant.access {
            crate::permit::SandboxAccess::Read => Op::Read {
                path: grant.path.clone(),
            },
            crate::permit::SandboxAccess::Write => Op::Write {
                path: grant.path.clone(),
                change: crate::permit::Change::Whole(Vec::new()),
            },
            crate::permit::SandboxAccess::Exec => return false,
        };
        self.deny
            .iter()
            .any(|entry| entry.denies(&op, workspace, &self.allow))
    }

    /// Whether a canonical path is covered by built-in credential protection.
    pub fn is_credential_path(path: &crate::permit::CanonPath) -> bool {
        static RULES: std::sync::OnceLock<Vec<Rule>> = std::sync::OnceLock::new();
        let rules = RULES.get_or_init(|| {
            CREDENTIAL_READ_RULES
                .iter()
                .map(|raw| Rule::load(raw).expect("valid built-in credential rule"))
                .collect()
        });
        let op = Op::Read { path: path.clone() };
        rules.iter().any(|rule| rule.matches(&op, Path::new("/")))
    }

    /// Install credential read defaults without replacing operator rules.
    pub fn with_credential_defaults(mut self) -> Self {
        // Absolute patterns also cover roots outside the workspace. These are
        // ordinary read rules, not filesystem or subprocess confinement.
        for raw in CREDENTIAL_READ_RULES {
            Self::merge(
                &mut self.deny,
                Rule::load(raw).expect("valid built-in credential rule"),
                Source::Default,
            );
        }
        self
    }

    /// Allow fetches from common documentation and package hosts without a
    /// question. Operator rules stay first and a deny still wins.
    pub fn with_development_host_defaults(mut self) -> Self {
        for raw in DEVELOPMENT_HOST_RULES {
            Self::merge(
                &mut self.allow,
                Rule::load(raw).expect("valid built-in development host rule"),
                Source::Default,
            );
        }
        self
    }

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
