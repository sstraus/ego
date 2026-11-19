//! `exec(...)` patterns: a token vector, never a string.
//!
//! Tokenize patterns once at parse time so whitespace aliases have one
//! rule ID. Match canonical argv token by token; never render a pattern
//! back into a shell command, including for `ego rules ls`.
//!
//! **This half is only worth anything if the argv is the real argv.** The
//! argv-native exec producer supplies that other half: it constructs one
//! [`SpawnSpec`](crate::permit::SpawnSpec), and matching reads its exact token
//! vector without rendering or reparsing it.

use std::ffi::OsString;
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

    /// Tokenize exec patterns with whitespace, grouping quotes, and backslash escapes. Perform
    /// no shell expansion or filesystem globbing. Escaped stars are literal; unescaped stars
    /// remain wildcards under the grammar's quoting rules.
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

    /// Match a host-native argv without repairing non-Unicode arguments.
    ///
    /// The rule grammar is Unicode text. An argument outside that grammar is
    /// therefore not covered by an `exec(...)` rule; lossy conversion here
    /// would silently authorize a different value from the one executed.
    pub fn matches_os(&self, argv: &[OsString]) -> bool {
        let Some(argv) = argv
            .iter()
            .map(|argument| argument.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        self.matches(&argv)
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
                ArgvToken::One(glob) => glob.fmt_argv(f)?,
                ArgvToken::AnyOne | ArgvToken::Rest => f.write_str("*")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: persisted exact exec rules turn literal stars into wildcards or split arguments.
    #[test]
    fn stored_argv_preserves_literals_empty_arguments_and_wildcards() {
        for text in [
            r"program literal\* two\ words ''",
            r"program wildcard* *",
            r#"program \\ \"quoted\""#,
            r"program \'quoted\'",
        ] {
            let before = ArgvPattern::parse(text).unwrap();
            let after = ArgvPattern::parse(&before.to_string()).unwrap();
            assert_eq!(before, after, "{text}");
        }
    }
}
