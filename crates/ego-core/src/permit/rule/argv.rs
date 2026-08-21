//! `exec(...)` patterns: a token vector, never a string.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §6. The
//! pattern is tokenised once, at parse time, so that `git   *` and `git *`
//! are one rule with one ID; matching then runs token by token against a
//! canonical argv. A pattern is never re-rendered into a shell string, for
//! any purpose — `ego rules ls` renders the token vector.
//!
//! **This half is only worth anything if the argv is the real argv.** §6's
//! other half is a requirement on whatever tool spawns processes: its
//! `prepare` must decompose a command into one canonical operation per stage,
//! as a recogniser rather than an interpreter, and anything outside the
//! subset it recognises must become an operation no `exec(...)` rule can
//! match. Nothing in this workspace spawns anything yet, so that half has
//! nothing to attach to and is not implemented here. Until it exists, an
//! `exec` rule parses, canonicalises and gets an ID, and matches no call —
//! because no call carries an argv.

use std::fmt;

use super::glob::{Elem, Glob, GlobBuilder, matches_sequence};
use super::parse::RuleParseError;

/// One token of a canonicalised `exec` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgvToken {
    /// A token pattern. A `*` inside it globs within that token: `cargo-*`.
    One(Glob),
    /// A bare `*` token that is not last: matches exactly one token.
    AnyOne,
    /// A bare `*` token in the final position: matches zero or more remaining
    /// tokens. This is what makes `exec(git *)` mean "any git command".
    Rest,
}

/// A canonicalised `exec` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgvPattern {
    tokens: Vec<ArgvToken>,
}

impl ArgvPattern {
    pub fn tokens(&self) -> &[ArgvToken] {
        &self.tokens
    }

    /// Tokenise the text between `exec(` and `)`.
    ///
    /// POSIX-ish, and only as much of it as §6 lists: runs of whitespace
    /// separate tokens, `'…'` and `"…"` group, `\` escapes. Nothing expands —
    /// there is no command substitution here, no parameter expansion and no
    /// globbing against the filesystem, because a *pattern* that consulted
    /// the world would mean something different on every machine.
    ///
    /// Quoting groups; it does not disarm a `*`. `\*` is the literal star and
    /// `*` is the wildcard, one spelling each, so `'*'` is a literal star for
    /// the same reason `'a'` is the letter `a`.
    pub fn parse(raw: &str) -> Result<Self, RuleParseError> {
        let mut tokens: Vec<ArgvToken> = Vec::new();
        let mut current = GlobBuilder::default();
        let mut started = false;
        let mut chars = raw.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                ch if ch.is_whitespace() => {
                    if started {
                        tokens.push(ArgvToken::One(current.build()));
                        current = GlobBuilder::default();
                        started = false;
                    }
                }
                '*' => {
                    started = true;
                    current.star();
                }
                '\\' => {
                    started = true;
                    let Some(escaped) = chars.next() else {
                        return Err(RuleParseError::UnbalancedQuote(raw.trim().to_string()));
                    };
                    current.literal(escaped);
                }
                '\'' | '"' => {
                    started = true;
                    let quote = ch;
                    loop {
                        let Some(inner) = chars.next() else {
                            return Err(RuleParseError::UnbalancedQuote(raw.trim().to_string()));
                        };
                        if inner == quote {
                            break;
                        }
                        // A backslash escapes inside double quotes and is an
                        // ordinary character inside single ones, which is the
                        // one place the two quotes differ.
                        if inner == '\\' && quote == '"' {
                            let Some(escaped) = chars.next() else {
                                return Err(RuleParseError::UnbalancedQuote(
                                    raw.trim().to_string(),
                                ));
                            };
                            current.literal(escaped);
                        } else {
                            current.literal(inner);
                        }
                    }
                }
                ch => {
                    started = true;
                    current.literal(ch);
                }
            }
        }

        if started {
            tokens.push(ArgvToken::One(current.build()));
        }

        if tokens.is_empty() {
            return Err(RuleParseError::EmptyPattern);
        }

        // A bare `*` means one token in the middle and any number at the end,
        // and that distinction is resolved *here* rather than at match time.
        // A pattern whose meaning depends on where the matcher happens to be
        // looking is a pattern nobody can read off `rules ls`.
        let last = tokens.len() - 1;
        for (at, token) in tokens.iter_mut().enumerate() {
            let ArgvToken::One(glob) = token else {
                continue;
            };
            if glob.is_bare_star() {
                *token = if at == last {
                    ArgvToken::Rest
                } else {
                    ArgvToken::AnyOne
                };
            }
        }

        Ok(Self { tokens })
    }

    /// Does this pattern cover `argv`?
    pub fn matches(&self, argv: &[String]) -> bool {
        let elements: Vec<Elem<'_>> = self
            .tokens
            .iter()
            .map(|token| match token {
                ArgvToken::One(glob) => Elem::One(glob),
                ArgvToken::AnyOne => Elem::AnyOne,
                ArgvToken::Rest => Elem::Rest,
            })
            .collect();
        let target: Vec<&str> = argv.iter().map(String::as_str).collect();
        matches_sequence(&elements, &target)
    }

    /// The canonical bytes this pattern contributes to a rule's ID.
    pub fn canonical_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.tokens.len() as u32).to_le_bytes());
        for token in &self.tokens {
            match token {
                ArgvToken::One(glob) => {
                    out.push(0);
                    glob.canonical_bytes(out);
                }
                ArgvToken::AnyOne => out.push(1),
                ArgvToken::Rest => out.push(2),
            }
        }
    }
}

impl fmt::Display for ArgvPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, token) in self.tokens.iter().enumerate() {
            if at > 0 {
                f.write_str(" ")?;
            }
            match token {
                ArgvToken::One(glob) => write!(f, "{glob}")?,
                ArgvToken::AnyOne | ArgvToken::Rest => f.write_str("*")?,
            }
        }
        Ok(())
    }
}
