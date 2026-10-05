//! Config precedence sources and workspace trust.

/// Which precedence layer a config value came from.
///
/// Ordered lowest-precedence first for readability; [`resolve`](super::resolve)
/// applies the layers highest-precedence-last (`flags` beats everything).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Built-in credential rules, below operator-written authority.
    Default,
    System,
    User,
    Project,
    Local,
    Env,
    #[serde(rename = "agent-file")]
    AgentFile,
    Flags,
}

impl Source {
    /// Every origin, most authoritative first (built-in defaults last).
    ///
    /// [`OriginSet`] uses this order for attribution; merge precedence
    /// places defaults before every file layer instead.
    ///
    /// [`OriginSet`]: super::OriginSet
    pub const ALL: [Source; 8] = [
        Source::System,
        Source::User,
        Source::Project,
        Source::Local,
        Source::Env,
        Source::AgentFile,
        Source::Flags,
        Source::Default,
    ];

    /// Precedence rank, lowest first. Used to order layers before merging —
    /// `resolve` does not trust the caller's slice order.
    pub(super) fn rank(self) -> u8 {
        match self {
            Source::Default => 0,
            Source::System => 1,
            Source::User => 2,
            Source::Project => 3,
            Source::Local => 4,
            Source::Env => 5,
            Source::AgentFile => 6,
            Source::Flags => 7,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Source::Default => "default",
            Source::System => "system",
            Source::User => "user",
            Source::Project => "project",
            Source::Local => "local",
            Source::Env => "env",
            Source::AgentFile => "agent-file",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Trusted,
    Untrusted,
}
