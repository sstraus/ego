//! `Glob`: the one wildcard matcher in the grammar.
//!
//! A path segment and an `exec` token are the same shape — a run of literal
//! characters with `*` standing for "any run, including none" — and they are
//! matched by the same code, because two implementations of one wildcard is
//! how `src/*.rs` and `cargo-*` start disagreeing about what `*` means.
//!
//! What differs between them is only how the *text* becomes pieces: a path
//! segment has no escapes (§5 lists two constructs and no third), while an
//! `exec` token honours `\` (§6). So the callers build the pieces and this
//! module owns what they mean.

/// One run of a wildcard pattern. Canonical form alternates: no two `Lit`s
/// and no two `Star`s are ever adjacent, so `a**b` and `a*b` are one value
/// and therefore one [`RuleId`](super::RuleId).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Lit(String),
    Star,
}

/// A pattern over one segment or one token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    pieces: Vec<Piece>,
}

/// Builds a [`Glob`] from characters, deciding for itself which `*` is a
/// wildcard. Keeps the canonical alternation as it goes.
#[derive(Default)]
pub struct GlobBuilder {
    pieces: Vec<Piece>,
}

impl GlobBuilder {
    /// A literal character, whatever it is — including a `*` the caller has
    /// decided was escaped.
    pub fn literal(&mut self, ch: char) {
        match self.pieces.last_mut() {
            Some(Piece::Lit(lit)) => lit.push(ch),
            _ => self.pieces.push(Piece::Lit(ch.to_string())),
        }
    }

    /// A wildcard. Adjacent wildcards collapse, which is what makes two
    /// spellings one rule rather than two.
    pub fn star(&mut self) {
        if !matches!(self.pieces.last(), Some(Piece::Star)) {
            self.pieces.push(Piece::Star);
        }
    }

    pub fn build(self) -> Glob {
        Glob {
            pieces: self.pieces,
        }
    }
}

impl Glob {
    /// The pattern with no wildcards in it, if it has none. What lets a
    /// caller tell `src` from `sr*c` — [`PathPattern`](super::PathPattern)
    /// needs it to decide whether a `..` may cancel the segment before it.
    pub fn as_literal(&self) -> Option<&str> {
        match self.pieces.as_slice() {
            [Piece::Lit(lit)] => Some(lit),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Exactly one wildcard and nothing else: the bare `*`.
    pub fn is_bare_star(&self) -> bool {
        matches!(self.pieces.as_slice(), [Piece::Star])
    }

    /// Match the whole of `text`, with no case folding and no Unicode
    /// normalisation — the comparison is against what the platform's own
    /// `canonicalize` returned, exactly (§5).
    ///
    /// The two-pointer form and not a recursion over pieces: a recursion
    /// branches at every wildcard and costs exponentially on operator-
    /// supplied input, which is the same objection §4 raises to regular
    /// expressions. This backtracks to the most recent wildcard only, which
    /// is `O(pattern × text)`.
    pub fn matches(&self, text: &str) -> bool {
        let pattern = self.flattened();
        let text: Vec<char> = text.chars().collect();

        let (mut p, mut t) = (0usize, 0usize);
        // Where to resume if the run this wildcard swallowed turns out to be
        // one character too short.
        let mut star: Option<usize> = None;
        let mut resume = 0usize;

        while t < text.len() {
            match pattern.get(p) {
                Some(None) => {
                    star = Some(p);
                    resume = t;
                    p += 1;
                }
                Some(Some(ch)) if *ch == text[t] => {
                    p += 1;
                    t += 1;
                }
                _ => match star {
                    Some(at) => {
                        p = at + 1;
                        resume += 1;
                        t = resume;
                    }
                    None => return false,
                },
            }
        }

        pattern[p..].iter().all(Option::is_none)
    }

    /// `None` is a wildcard, `Some(c)` is that character.
    fn flattened(&self) -> Vec<Option<char>> {
        self.pieces
            .iter()
            .flat_map(|piece| match piece {
                Piece::Lit(lit) => lit.chars().map(Some).collect::<Vec<_>>(),
                Piece::Star => vec![None],
            })
            .collect()
    }

    /// The canonical bytes this glob contributes to a rule's ID.
    ///
    /// Length-prefixed so the encoding is injective: without it, a `Lit("a")`
    /// followed by a `Star` and a `Lit("a*")` alone would serialise the same
    /// and two different rules would share an ID.
    pub fn canonical_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.pieces.len() as u32).to_le_bytes());
        for piece in &self.pieces {
            match piece {
                Piece::Lit(lit) => {
                    out.push(0);
                    out.extend_from_slice(&(lit.len() as u32).to_le_bytes());
                    out.extend_from_slice(lit.as_bytes());
                }
                Piece::Star => out.push(1),
            }
        }
    }
}

/// One element of a sequence pattern: a path's segments or a command's
/// tokens, which are the same problem twice.
pub enum Elem<'a> {
    /// Exactly one item, if the glob accepts it.
    One(&'a Glob),
    /// Exactly one item, whatever it is.
    AnyOne,
    /// Zero or more items.
    Rest,
}

/// Match a sequence pattern against a sequence.
///
/// A table rather than the obvious recursion. A `Rest` can begin at any
/// offset, so recursing branches at every one of them and a pattern with
/// several costs exponentially — inside a security decision, on operator-
/// supplied input. §4 refuses regular expressions partly for unbounded
/// matching cost, and accepting it here through the back door would be the
/// same mistake with a smaller vocabulary. This is `O(pattern × target)`.
pub fn matches_sequence(pattern: &[Elem<'_>], target: &[&str]) -> bool {
    // `reach[j]`: the pattern consumed so far accounts for exactly the first
    // `j` items of `target`.
    let mut reach = vec![false; target.len() + 1];
    reach[0] = true;

    for element in pattern {
        let mut next = vec![false; target.len() + 1];
        match element {
            Elem::Rest => {
                // Once a position is reachable, every position after it is.
                let mut open = false;
                for (j, slot) in next.iter_mut().enumerate() {
                    open |= reach[j];
                    *slot = open;
                }
            }
            // Exactly one item, whatever it is: every reachable position
            // shifts along by one.
            Elem::AnyOne => next[1..].copy_from_slice(&reach[..target.len()]),
            Elem::One(glob) => {
                for j in 0..target.len() {
                    if reach[j] && glob.matches(target[j]) {
                        next[j + 1] = true;
                    }
                }
            }
        }
        reach = next;
    }

    reach[target.len()]
}

impl std::fmt::Display for Glob {
    /// Renders the canonical spelling — what `ego rules ls` shows, which is
    /// the AST and never the text the operator typed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for piece in &self.pieces {
            match piece {
                Piece::Lit(lit) => f.write_str(lit)?,
                Piece::Star => f.write_str("*")?,
            }
        }
        Ok(())
    }
}
