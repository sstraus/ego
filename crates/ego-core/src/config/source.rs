//! Config precedence sources and workspace trust.

/// Which precedence layer a config value came from.
///
/// Ordered lowest-precedence first for readability; [`resolve`](super::resolve)
/// applies the layers highest-precedence-last (`flags` beats everything).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    System,
    User,
    Project,
    Local,
    Env,
    Flags,
}

impl Source {
    /// Every layer, in rank order.
    ///
    /// Declaration order and rank order are the same, and [`OriginSet`] relies
    /// on that to iterate a set of layers most-authoritative-first without
    /// sorting.
    ///
    /// [`OriginSet`]: super::OriginSet
    pub const ALL: [Source; 6] = [
        Source::System,
        Source::User,
        Source::Project,
        Source::Local,
        Source::Env,
        Source::Flags,
    ];

    /// Precedence rank, lowest first. Used to order layers before merging —
    /// `resolve` does not trust the caller's slice order.
    pub(super) fn rank(self) -> u8 {
        match self {
            Source::System => 0,
            Source::User => 1,
            Source::Project => 2,
            Source::Local => 3,
            Source::Env => 4,
            Source::Flags => 5,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Source::System => "system",
            Source::User => "user",
            Source::Project => "project",
            Source::Local => "local",
            Source::Env => "env",
            Source::Flags => "flags",
        };
        write!(f, "{name}")
    }
}

/// A config layer tagged with the [`Source`] it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub source: Source,
    pub value: T,
}

/// Whether the current workspace has been interactively trusted by the user.
///
/// This is the pure input the trust gate needs to decide
/// [`Posture`](super::Posture). Persisting trust decisions (the trust store)
/// involves file I/O and lives outside `ego-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Untrusted,
}
