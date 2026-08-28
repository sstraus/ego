//! The permission rule grammar: one language for four surfaces.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md`.
//! `--allow` on the command line, `permissions.allow` in four config layers,
//! `allow:` in an agent file's frontmatter and `ego rules ls` are supposed to
//! speak one language, and `CLI.md:29-31` states that as a requirement: "The
//! permission rule you pass as `--allow`, write in config, and see in
//! `/permissions` is the same string."
//!
//! What makes it more than a shared string is here: a **canonical AST**, so
//! that two spellings of one predicate are one value with one identity, and
//! matching against the *structure* of a [`PreparedCall`](super::PreparedCall)
//! rather than against rendered text.
//!
//! This module opens no file and knows no workspace root. The root is a
//! match-time parameter, which is what keeps a rule written in a committed
//! `.ego/config.toml` portable to every checkout.
//!
//! ## What a rule does not decide
//!
//! A rule decides **whether to ask**, and never how wide a grant is.
//! `2026-08-21-a-permit-nobody-can-widen.md` §1 settles that: the scope of a
//! permit comes from the call's own operations, and a rule that matched could
//! not widen it if it wanted to. Nothing in this module constructs a
//! [`Scope`](super::Scope), and nothing here can.
//!
//! ## What is here and what is waiting
//!
//! All seven heads parse, canonicalise and take an ID, because the grammar is
//! published as one thing — `ARCHITECTURE.md:252` makes that a gate. Five can
//! currently match: `read`, `write`, `list`, `search`, and `exec`. `net` and
//! `tool` still name operations no tool produces.
//!
//! A rule with one of those two heads is well-formed, has a stable ID and
//! renders in `rules ls` — and [`Rule::load`] **refuses it**, by name, saying
//! that no tool in this build produces operations of that shape. Not a parse
//! error, which would lie about the grammar and churn the ID later; and not a
//! silent non-match, which is the worse failure: an operator who writes
//! `exec(rm -rf /)` in a deny list cannot tell a rule that matches nothing
//! apart from a boundary that works. §6a.
//!
//! It self-heals. When the operations arrive, [`Rule::matches`] gains an arm
//! and one entry in `parse.rs`'s `OPERATIONS` table changes; no rule is
//! rewritten and no ID is recomputed.

mod argv;
mod endpoint;
mod glob;
mod id;
mod parse;
mod pattern;

use std::fmt;
use std::path::Path;

pub use argv::{ArgvPattern, ArgvToken};
pub use endpoint::{Endpoint, Host};
pub use glob::Glob;
pub use id::{BadRuleId, GRAMMAR_VERSION, RuleId};
pub use parse::{Head, HeadHint, RuleParseError, Unenforceable};
pub use pattern::{PathPattern, Segment};

use super::prepared_call::Op;

/// A permission rule: a predicate over canonical operations.
///
/// **No effect token.** Whether a rule allows or denies is decided by which
/// list holds it — `--allow` against `--deny`, `permissions.allow` against
/// `permissions.deny` — because that is what all four surfaces already spell
/// that way, and because a rule carrying its own polarity would make
/// `--deny 'allow read(x)'` a sentence somebody has to decide the meaning of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    Read(PathPattern),
    Write(PathPattern),
    List(PathPattern),
    Search(PathPattern),
    Exec(ArgvPattern),
    Net(Endpoint),
    /// `tool(<server>/<name>)`: the one identity predicate.
    ///
    /// MCP tools are a remote trust domain whose effects ego cannot see, so
    /// their calls have no honest operations to enumerate. No effect rule
    /// ever covers one — `write(**)` does not authorise an MCP tool that
    /// writes, because ego has no evidence that it writes and none that it
    /// does not — and only this can. Without it such a tool is either
    /// unapprovable or approvable by a rule that is a lie.
    ///
    /// It never covers a builtin. A builtin approvable by identity instead of
    /// by effect would be a way around the whole effect table.
    Tool {
        server: String,
        name: String,
    },
}

impl Rule {
    /// The one entry point. There is no second one, in this crate or any
    /// other: a rule in frontmatter is a rule, and a second parser is how the
    /// two surfaces start disagreeing about what one string means.
    pub fn parse(raw: &str) -> Result<Self, RuleParseError> {
        let (head, body) = parse::split(raw)?;
        Ok(match head {
            Head::Read => Rule::Read(PathPattern::parse(body)?),
            Head::Write => Rule::Write(PathPattern::parse(body)?),
            Head::List => Rule::List(PathPattern::parse(body)?),
            Head::Search => Rule::Search(PathPattern::parse(body)?),
            Head::Exec => Rule::Exec(ArgvPattern::parse(body)?),
            Head::Net => Rule::Net(Endpoint::parse(body)?),
            Head::Tool => {
                let (server, name) = tool_id(body)?;
                Rule::Tool { server, name }
            }
        })
    }

    /// Parse a rule **and refuse it if this build cannot enforce it** (§6a).
    ///
    /// This is what every surface that *stores* a rule calls — config layers
    /// today, frontmatter and `--allow` when they arrive. [`parse`](Self::parse)
    /// is the grammar alone and is what `rules explain` wants: rendering a rule
    /// nobody can enforce is fine, believing in it is not.
    ///
    /// The refusal is not a parse error, and the distinction is the whole
    /// point. A rule with an unenforceable head is a valid sentence with a
    /// stable ID that renders correctly in `rules ls`; what it lacks is
    /// anything to match. Calling that a grammar error would churn the ID on
    /// the day the head became usable, and would be a temporary message about
    /// a permanent grammar.
    pub fn load(raw: &str) -> Result<Self, RuleLoadError> {
        let rule = Self::parse(raw)?;
        if let Some(awaiting) = rule.head().awaiting() {
            return Err(RuleLoadError::Unenforceable(Unenforceable::new(
                rule.to_string(),
                rule.head(),
                awaiting,
            )));
        }
        Ok(rule)
    }

    pub fn head(&self) -> Head {
        match self {
            Rule::Read(_) => Head::Read,
            Rule::Write(_) => Head::Write,
            Rule::List(_) => Head::List,
            Rule::Search(_) => Head::Search,
            Rule::Exec(_) => Head::Exec,
            Rule::Net(_) => Head::Net,
            Rule::Tool { .. } => Head::Tool,
        }
    }

    /// Does this rule cover `op`?
    ///
    /// Per operation, and never per call. A call with several operations is
    /// matched by several rules and *every* operation must be covered — which
    /// is why §4 refuses a conjunction across heads: `exec(git *) &&
    /// write(src/**)` describes a call with two operations, and that is
    /// already the semantics.
    ///
    /// `root` anchors relative path patterns and is ignored by absolute ones.
    pub fn matches(&self, op: &Op, root: &Path) -> bool {
        match (self, op) {
            (Rule::Read(pattern), Op::Read { path }) => pattern.matches(path.as_path(), root),
            (Rule::Write(pattern), Op::Write { path, .. }) => pattern.matches(path.as_path(), root),
            (Rule::List(pattern), Op::List { root: at, .. }) => pattern.matches(at.as_path(), root),
            (Rule::Search(pattern), Op::Search { root: at, .. }) => {
                pattern.matches(at.as_path(), root)
            }
            (Rule::Exec(pattern), Op::Spawn { spec }) => pattern.matches_os(spec.argv()),
            (Rule::Net(endpoint), Op::Connect { target, .. } | Op::ProtocolConnect { target }) => {
                endpoint.matches(target.host(), target.port())
            }
            (Rule::Tool { server, name }, Op::Opaque { tool, .. }) => {
                server == tool.server() && name == tool.name()
            }
            _ => false,
        }
    }

    /// This rule's stable identity, computed over its canonical AST.
    pub fn id(&self) -> RuleId {
        let mut bytes = Vec::new();
        bytes.push(self.head() as u8);
        match self {
            Rule::Read(pattern)
            | Rule::Write(pattern)
            | Rule::List(pattern)
            | Rule::Search(pattern) => pattern.canonical_bytes(&mut bytes),
            Rule::Exec(pattern) => pattern.canonical_bytes(&mut bytes),
            Rule::Net(endpoint) => endpoint.canonical_bytes(&mut bytes),
            Rule::Tool { server, name } => {
                for part in [server, name] {
                    bytes.extend_from_slice(&(part.len() as u32).to_le_bytes());
                    bytes.extend_from_slice(part.as_bytes());
                }
            }
        }
        RuleId::of(&bytes)
    }
}

impl fmt::Display for Rule {
    /// The canonical spelling — what `ego rules ls` shows. Renders the AST,
    /// never the text the operator typed, so two spellings of one rule are
    /// shown identically and an `exec` pattern is never re-rendered into a
    /// shell string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.head())?;
        match self {
            Rule::Read(pattern)
            | Rule::Write(pattern)
            | Rule::List(pattern)
            | Rule::Search(pattern) => write!(f, "{pattern}")?,
            Rule::Exec(pattern) => write!(f, "{pattern}")?,
            Rule::Net(endpoint) => write!(f, "{endpoint}")?,
            Rule::Tool { server, name } => write!(f, "{server}/{name}")?,
        }
        f.write_str(")")
    }
}

/// Why a string in a permission list did not become a usable rule.
///
/// Two arms because they are two different facts about the world, and an
/// operator acts on them differently: one is a sentence the grammar does not
/// contain, the other is a sentence this build has nothing to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleLoadError {
    Parse(RuleParseError),
    Unenforceable(Unenforceable),
}

impl fmt::Display for RuleLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleLoadError::Parse(err) => err.fmt(f),
            RuleLoadError::Unenforceable(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RuleLoadError {}

impl From<RuleParseError> for RuleLoadError {
    fn from(err: RuleParseError) -> Self {
        RuleLoadError::Parse(err)
    }
}

fn tool_id(body: &str) -> Result<(String, String), RuleParseError> {
    let trimmed = body.trim();
    let bad = || RuleParseError::BadToolId(trimmed.to_string());

    let (server, name) = trimmed.split_once('/').ok_or_else(bad)?;
    let (server, name) = (server.trim(), name.trim());
    if server.is_empty() || name.is_empty() || name.contains('/') {
        return Err(bad());
    }

    Ok((server.to_string(), name.to_string()))
}
