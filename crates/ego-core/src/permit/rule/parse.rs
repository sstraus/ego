//! Splitting `head(pattern)` apart, and every way that can fail.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §1, §2
//! and §"Failure semantics". There is exactly one entry point —
//! [`Rule::parse`](super::Rule::parse) — because a second one is how the
//! flags path and the frontmatter path start disagreeing about what a rule
//! means.

use std::fmt;

/// The effect a rule names. Never a tool.
///
/// A tool-named grammar's coverage is a function of the tool inventory, so
/// every new tool is a silent hole in every rule the operator ever wrote —
/// and the hole is invisible, because the rule still reads correctly. §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Read,
    Write,
    List,
    Search,
    Exec,
    Net,
    Tool,
}

impl Head {
    pub fn as_str(self) -> &'static str {
        match self {
            Head::Read => "read",
            Head::Write => "write",
            Head::List => "list",
            Head::Search => "search",
            Head::Exec => "exec",
            Head::Net => "net",
            Head::Tool => "tool",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "read" => Head::Read,
            "write" => Head::Write,
            "list" => Head::List,
            "search" => Head::Search,
            "exec" => Head::Exec,
            "net" => Head::Net,
            "tool" => Head::Tool,
            _ => return None,
        })
    }

    /// What this build is missing before a rule with this head can match
    /// anything — `None` when nothing is missing.
    ///
    /// Reads [`OPERATIONS`], so a head that gains an operation stops being
    /// unenforceable by editing one table entry.
    pub fn awaiting(self) -> Option<&'static str> {
        OPERATIONS
            .iter()
            .find(|(head, _)| *head == self)
            .and_then(|(_, awaiting)| *awaiting)
    }
}

/// Whether this build produces operations of each head's shape, and what
/// would supply the ones it does not (§6a).
///
/// **Data, not a `match`**, so the refusal in [`Unenforceable`] is derived and
/// a head costs one entry. `None` means an operation of that shape exists
/// today; `Some(awaiting)` names the thing whose arrival changes that.
///
/// The grammar publishes seven heads and this workspace produces four kinds of
/// operation. That gap has an unacceptable default: an operator who writes
/// `exec(rm -rf /)` in a deny list gets a rule that matches nothing, and
/// **cannot tell that apart from a boundary that works**. "Vacuous rather than
/// fail-open" is a distinction visible only to us. So the rule parses, gets a
/// stable ID, and is refused at load with a true statement about
/// enforceability rather than a false one about grammar.
const OPERATIONS: &[(Head, Option<&str>)] = &[
    (Head::Read, None),
    (Head::Write, None),
    (Head::List, None),
    (Head::Search, None),
    (Head::Exec, Some("a shell tool")),
    (Head::Net, Some("a tool that reaches the network")),
    (Head::Tool, Some("an MCP client")),
];

/// A well-formed rule this build cannot enforce (§6a).
///
/// Deliberately **not** a [`RuleParseError`]. The grammar is complete and this
/// rule is a valid sentence in it; what is missing is an operation to match.
/// Saying "parse error" would be a temporary lie about a permanent grammar,
/// and it would churn the rule's ID on the day the head became usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unenforceable {
    /// The rule as it canonically renders, so the message names what the
    /// operator can go and remove.
    pub rule: String,
    pub head: Head,
    awaiting: &'static str,
}

impl Unenforceable {
    pub(super) fn new(rule: String, head: Head, awaiting: &'static str) -> Self {
        Self {
            rule,
            head,
            awaiting,
        }
    }
}

impl fmt::Display for Unenforceable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            rule,
            head,
            awaiting,
        } = self;
        write!(
            f,
            "{rule} cannot be enforced: no tool in this build produces {head} \
             operations. Remove the rule or omit it until {awaiting} exists."
        )
    }
}

impl std::error::Error for Unenforceable {}

impl fmt::Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to tell an operator who wrote a head that does not exist.
///
/// The error is where the teaching goes, which is why `bash(...)` is a hard
/// error rather than an alias for `exec(...)`. An alias is a second spelling,
/// and §5's whole job is that there are none — either the two share an ID,
/// and `rules ls` renders one of them back as something the operator did not
/// write, or they do not, and `revoke` on one leaves the other standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadHint {
    /// The name is a tool, and this is the effect that tool performs.
    Effect(Head),
    /// The name is one typo away from a head.
    Typo(Head),
}

/// Tools an operator is likely to name, and the effect each one performs.
///
/// Not an alias table — nothing here parses. It exists so the refusal can
/// name the right head instead of leaving the operator to guess, at the exact
/// moment they are thinking about the boundary they wanted.
const TOOLS: &[(&str, Head)] = &[
    ("bash", Head::Exec),
    ("sh", Head::Exec),
    ("zsh", Head::Exec),
    ("shell", Head::Exec),
    ("cmd", Head::Exec),
    ("powershell", Head::Exec),
    ("run", Head::Exec),
    ("curl", Head::Net),
    ("wget", Head::Net),
    ("fetch", Head::Net),
    ("web", Head::Net),
    ("http", Head::Net),
    ("cat", Head::Read),
    ("edit", Head::Write),
    ("glob", Head::List),
    ("ls", Head::List),
    ("grep", Head::Search),
    ("rg", Head::Search),
    ("ripgrep", Head::Search),
];

const HEADS: &[Head] = &[
    Head::Read,
    Head::Write,
    Head::List,
    Head::Search,
    Head::Exec,
    Head::Net,
    Head::Tool,
];

fn hint(name: &str) -> Option<HeadHint> {
    if let Some((_, head)) = TOOLS.iter().find(|(tool, _)| *tool == name) {
        return Some(HeadHint::Effect(*head));
    }
    HEADS
        .iter()
        .find(|head| one_typo_apart(name, head.as_str()))
        .map(|head| HeadHint::Typo(*head))
}

/// Are `a` and `b` one typo apart — one insertion, deletion, substitution or
/// transposition?
///
/// Deliberately not a general edit distance: two edits from `read` reaches
/// words that mean something else, and a suggestion that is wrong is worse
/// than no suggestion, because the operator writes it down.
///
/// Transposition is in because `raed` for `read` is the commonest typo there
/// is, and a plain Levenshtein count charges two for it — which would leave
/// the single most likely mistake with no suggestion at all.
fn one_typo_apart(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }

    let shared = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if shared == a.len() && shared == b.len() {
        return true;
    }

    if a.len() != b.len() {
        // One insertion or one deletion: skip the extra character on the
        // longer side and the tails have to agree.
        let (short, long) = if a.len() < b.len() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        return short[shared..] == long[shared + 1..];
    }

    // One substitution.
    if a[shared + 1..] == b[shared + 1..] {
        return true;
    }

    // Two neighbours swapped.
    shared + 1 < a.len()
        && a[shared] == b[shared + 1]
        && a[shared + 1] == b[shared]
        && a[shared + 2..] == b[shared + 2..]
}

/// Why a rule string is not a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleParseError {
    /// A head that is not one of the seven. Always an error, and never a rule
    /// that matches nothing: a rule that matches nothing is the worst failure
    /// mode available here, because the operator believes a boundary exists.
    UnknownHead {
        head: String,
        rule: String,
        hint: Option<HeadHint>,
    },
    /// A real head with no parentheses. `exec` alone would mean "any argv",
    /// which is a real thing to want and is spelled `exec(*)`; two spellings
    /// of one predicate is what §5 exists to prevent, so the shorter one is
    /// refused and names the longer.
    MissingPattern { head: Head, rule: String },
    /// Not `head(pattern)` at all.
    Malformed(String),
    /// `read()`, or a pattern that canonicalises to nothing.
    EmptyPattern,
    /// A `..` that survived normalisation. Refused rather than rewritten
    /// into a working rule that reaches outside the workspace.
    EscapingPattern(String),
    /// `***`, or a `**` that is not a whole segment. There are two
    /// constructs and this is neither.
    BadStar(String),
    /// An `exec(...)` pattern whose quoting does not close.
    UnbalancedQuote(String),
    /// A `net(...)` pattern that is not a bare authority.
    BadEndpoint { pattern: String, why: String },
    /// A `tool(...)` pattern that is not `<server>/<name>`.
    BadToolId(String),
}

impl fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleParseError::UnknownHead { head, rule, hint } => {
                write!(f, "unknown rule head `{head}` in `{rule}`.")?;
                match hint {
                    Some(HeadHint::Effect(effect)) => write!(
                        f,
                        "\nRules name effects, not tools. Did you mean `{effect}(…)`?\
                         \n`{effect}` covers every tool with that effect, including {head}."
                    ),
                    Some(HeadHint::Typo(head)) => write!(f, "\nDid you mean `{head}(…)`?"),
                    None => write!(
                        f,
                        "\nRules name effects, not tools. The heads are \
                         read, write, list, search, exec, net and tool."
                    ),
                }
            }
            RuleParseError::MissingPattern { head, rule } => write!(
                f,
                "`{rule}` has no pattern. `{head}` on its own is not a rule; \
                 write `{head}(*)` for the widest one."
            ),
            RuleParseError::Malformed(rule) => {
                write!(f, "`{rule}` is not a rule. A rule is `head(pattern)`.")
            }
            RuleParseError::EmptyPattern => write!(f, "a rule's pattern may not be empty"),
            RuleParseError::EscapingPattern(pattern) => write!(
                f,
                "`{pattern}` leaves the workspace. A `..` that survives \
                 normalisation is refused, not rewritten."
            ),
            RuleParseError::BadStar(pattern) => write!(
                f,
                "`{pattern}`: `*` matches within one path segment and `**` \
                 matches whole segments. There is no third construct."
            ),
            RuleParseError::UnbalancedQuote(pattern) => {
                write!(f, "`{pattern}`: a quote is never closed")
            }
            RuleParseError::BadEndpoint { pattern, why } => {
                write!(f, "`{pattern}` is not an endpoint: {why}")
            }
            RuleParseError::BadToolId(pattern) => write!(
                f,
                "`{pattern}` is not a tool identity. Write `tool(<server>/<name>)`."
            ),
        }
    }
}

impl std::error::Error for RuleParseError {}

/// Split a rule string into its head and the raw text between the
/// parentheses.
///
/// The closing parenthesis is the *last* character, not the first `)` found,
/// so an `exec` pattern may contain one: `exec(echo (hi))` is a command with
/// parentheses in it, not a truncated rule.
pub fn split(raw: &str) -> Result<(Head, &str), RuleParseError> {
    let rule = raw.trim();

    let Some(open) = rule.find('(') else {
        return Err(match Head::parse(rule) {
            Some(head) => RuleParseError::MissingPattern {
                head,
                rule: rule.to_string(),
            },
            None if rule.is_empty() || rule.contains(char::is_whitespace) => {
                RuleParseError::Malformed(rule.to_string())
            }
            None => RuleParseError::UnknownHead {
                head: rule.to_string(),
                rule: rule.to_string(),
                hint: hint(rule),
            },
        });
    };

    let name = rule[..open].trim();
    let Some(head) = Head::parse(name) else {
        return Err(RuleParseError::UnknownHead {
            head: name.to_string(),
            rule: rule.to_string(),
            hint: hint(name),
        });
    };

    let Some(body) = rule[open + 1..].strip_suffix(')') else {
        return Err(RuleParseError::Malformed(rule.to_string()));
    };

    Ok((head, body))
}
