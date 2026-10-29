//! One canonical permission grammar for flags, configuration, agent files, and management.
//! Rules match prepared operations, not shell text, and do not choose permit scope. Relative
//! paths use the match-time root.
//!
//! All seven heads currently have producers. `Rule::load` still checks build enforceability so
//! a future head cannot silently become a valid rule that matches nothing. Grammar identity
//! remains separate from current producer availability.

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
    ///
    /// `name` may be [`TOOL_NAME_WILDCARD`], which covers every tool of that one
    /// server and never another server's.
    Tool {
        server: String,
        name: String,
    },
}

/// The whole-name wildcard of `tool(<server>/*)`.
pub const TOOL_NAME_WILDCARD: &str = "*";

impl Rule {
    /// Does this rule cover every tool of one server (`tool(<server>/*)`)?
    ///
    /// A layer a repository controls must not grant that: a checked-in file
    /// would approve every tool of a server, including ones added later.
    pub fn is_tool_wildcard(&self) -> bool {
        matches!(self, Rule::Tool { name, .. } if name == TOOL_NAME_WILDCARD)
    }

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

    /// Parse a rule and refuse it if this build has no matching operation producer.
    /// Unenforceable is distinct from malformed: the canonical grammar and stable ID do not
    /// change when a producer becomes available.
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

    /// Allow authority excludes control paths regardless of rule provenance.
    /// Denies use `matches` so the guard never weakens a refusal.
    pub(crate) fn matches_allow(&self, op: &Op, root: &Path) -> bool {
        let path = match op {
            Op::Read { path } | Op::Write { path, .. } => Some(path.as_path()),
            Op::List { root, .. } | Op::Search { root, .. } => Some(root.as_path()),
            _ => None,
        };
        if path.is_some_and(|path| {
            path.components().any(|part| {
                matches!(part, std::path::Component::Normal(name)
                    if name.to_str().is_some_and(|name|
                        name.eq_ignore_ascii_case(".git")
                            || name.eq_ignore_ascii_case(".ego")
                            || name.eq_ignore_ascii_case(".claude")))
            })
        }) {
            return false;
        }
        self.matches(op, root)
    }

    /// Match per operation. Every operation in a call needs coverage, potentially from
    /// different rules. Relative patterns use `root`; absolute patterns do not.
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
                server == tool.server() && (name == TOOL_NAME_WILDCARD || name == tool.name())
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
