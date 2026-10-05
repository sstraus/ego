//! One ordered, versioned, redacted health projection.
//!
//! `ego doctor`, `ego doctor --json` and `/doctor` are three renderings of the
//! value built here and of nothing else. That is the whole point of the module:
//! a diagnosis that differs between the headless command and the interactive
//! surface is two diagnoses, and an operator comparing them has no way to learn
//! which one is lying.
//!
//! Observation never mutates what it observes. Nothing in this module logs in,
//! refreshes a credential or a catalogue, reloads an extension, rewrites
//! config, resolves a hosted model, sends a provider request, uploads anything,
//! or widens egress — the fact providers hand over what their own authority
//! already knows, and a fact nobody knows is reported as unknown rather than
//! guessed at or quietly dropped.
//!
//! See `docs/decisions/2026-09-03-doctor-observes-one-redacted-health-snapshot.md`.

/// The narrowly scoped operator-visible perimeter.
pub mod perimeter;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::event::time::Timestamp;

/// The shape of [`HealthSnapshot`], so a reader can tell which one it has.
pub const HEALTH_SNAPSHOT_VERSION: u32 = 1;

/// What one probe found.
///
/// The derived `Ord` is the severity fold and the variant order below is
/// therefore load-bearing. `NotChecked` outranks `Ok` on purpose: a snapshot
/// that looked at eight things and skipped the ninth must not summarise itself
/// as healthy, because "not checked" being indistinguishable from "ok" is the
/// exact failure this projection exists to prevent. It stays below `Warning`
/// just as deliberately — not looking is not an operational fault, and the
/// exit code agrees (see [`HealthSnapshot::exit_code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    NotChecked,
    Warning,
    Error,
}

impl HealthStatus {
    /// Whether this is something an operator has to fix for ego to work.
    ///
    /// Only `Error`. A warning is a thing worth saying and not a thing that
    /// failed, and an installation with an optional credential missing is a
    /// working installation.
    pub fn is_operational_error(self) -> bool {
        matches!(self, Self::Error)
    }
}

/// A subsystem, in the order every rendering prints it.
///
/// Ordered by what a diagnosis depends on rather than alphabetically: config
/// decides where everything else is read from, credentials and the session
/// store are what a run needs before it can start, and the admitted extensions
/// come last because they are the part an operator can lose without losing ego.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSection {
    Configuration,
    Credentials,
    SessionStore,
    Sandbox,
    Catalogue,
    Mcp,
    Skills,
    Hooks,
    Acp,
}

impl HealthSection {
    /// Every section, in rendering order.
    ///
    /// A rendering iterates this rather than the sections it was handed, so a
    /// subsystem that produced no facts at all still appears — silently
    /// omitting it would say "nothing wrong here" in the one case where
    /// nothing was asked.
    pub const ORDER: [Self; 9] = [
        Self::Configuration,
        Self::Credentials,
        Self::SessionStore,
        Self::Sandbox,
        Self::Catalogue,
        Self::Mcp,
        Self::Skills,
        Self::Hooks,
        Self::Acp,
    ];

    /// The name a rendering prints and a JSON reader keys on.
    pub fn label(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Credentials => "credentials",
            Self::SessionStore => "session_store",
            Self::Sandbox => "sandbox",
            Self::Catalogue => "catalogue",
            Self::Mcp => "mcp",
            Self::Skills => "skills",
            Self::Hooks => "hooks",
            Self::Acp => "acp",
        }
    }
}

/// Which probe produced an item, and about what.
///
/// `probe` is `&'static str` so it cannot be assembled out of the data being
/// diagnosed: an id built from a filename changes when the filename does, and
/// then it is not an id. `subject` carries the instance — which provider,
/// which server — for the probes that run once per thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthItemId {
    pub probe: &'static str,
    pub subject: Option<String>,
}

impl HealthItemId {
    /// A probe that runs once.
    pub fn probe(probe: &'static str) -> Self {
        Self {
            probe,
            subject: None,
        }
    }

    /// A probe that runs once per named thing.
    pub fn about(probe: &'static str, subject: impl Into<String>) -> Self {
        Self {
            probe,
            subject: Some(subject.into()),
        }
    }
}

impl std::fmt::Display for HealthItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.subject {
            Some(subject) => write!(f, "{}:{subject}", self.probe),
            None => f.write_str(self.probe),
        }
    }
}

/// The directories an operator is allowed to be shown a path inside.
///
/// Doctor's output is something people paste into issues. A path under
/// `EGO_HOME` or the workspace says where to look; an absolute path from
/// anywhere else is somebody's home directory layout, and ego has no business
/// publishing it because a subsystem happened to mention it in an error.
#[derive(Debug, Clone, Default)]
pub struct VisibleRoots {
    roots: Vec<PathBuf>,
}

impl VisibleRoots {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
        }
    }

    fn admits(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }
}

/// Text that was going to be printed and must not be.
///
/// Carries the probe and the reason, never the text. Returning the offending
/// string in the error would put it one `{err}` away from the output it was
/// just kept out of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionRefusal {
    pub probe: &'static str,
    pub reason: RedactionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionReason {
    /// An absolute path outside every operator-visible root.
    PathOutsideVisibleRoots,
}

impl std::fmt::Display for RedactionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            RedactionReason::PathOutsideVisibleRoots => write!(
                f,
                "{} named an absolute path outside the operator-visible roots",
                self.probe
            ),
        }
    }
}

/// Prose a rendering may print, having been checked first.
///
/// The only constructor is [`HealthDetail::sanitized`], so "was this
/// redacted?" is not a question a caller can get wrong — an unchecked string
/// cannot become a `HealthDetail` at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct HealthDetail(String);

impl HealthDetail {
    /// Accept `text` for printing, or refuse it.
    ///
    /// Refusal is the whole contract: the caller gets an `Err` it must handle
    /// rather than a scrubbed string it will not notice, because a detail that
    /// silently lost half its content is a diagnosis that silently became
    /// wrong.
    pub fn sanitized(
        probe: &'static str,
        text: impl Into<String>,
        roots: &VisibleRoots,
    ) -> Result<Self, RedactionRefusal> {
        let text = text.into();
        for token in text.split_whitespace() {
            let token = token.trim_matches(|c: char| c.is_ascii_punctuation() && c != '/');
            let path = Path::new(token);
            if path.is_absolute() && !roots.admits(path) {
                return Err(RedactionRefusal {
                    probe,
                    reason: RedactionReason::PathOutsideVisibleRoots,
                });
            }
        }
        Ok(Self(text))
    }

    /// Text with no path in it at all, and so nothing to check.
    ///
    /// For the fixed sentences a probe writes itself. Taking `&'static str`
    /// is what makes it safe: a literal in this repository cannot contain a
    /// path from the machine ego is running on.
    pub fn fixed(text: &'static str) -> Self {
        Self(text.to_owned())
    }

    /// Publish only the typed admitted perimeter under the 2026-10-04 exception.
    ///
    /// Its paths originate from RunConfinement and admitted ProfileRoot values;
    /// its remaining fields are closed vocabulary. Other details still sanitize.
    pub fn perimeter(perimeter: &perimeter::Perimeter) -> Self {
        Self(perimeter.text())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HealthDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One thing that was looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthItem {
    pub id: HealthItemId,
    pub status: HealthStatus,
    pub detail: HealthDetail,
    /// The authority the fact came from, named so a reader can go and check it.
    pub source: HealthDetail,
    /// When the fact was true, where its authority records that.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<Timestamp>,
    /// One local thing an operator can do about it.
    ///
    /// Local because doctor diagnoses and does not repair: the remediation is
    /// a command to type, never one this process ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<HealthDetail>,
}

impl HealthItem {
    pub fn new(
        id: HealthItemId,
        status: HealthStatus,
        detail: HealthDetail,
        source: HealthDetail,
    ) -> Self {
        Self {
            id,
            status,
            detail,
            source,
            observed_at: None,
            remediation: None,
        }
    }

    pub fn observed_at(mut self, observed_at: Timestamp) -> Self {
        self.observed_at = Some(observed_at);
        self
    }

    pub fn remediation(mut self, remediation: HealthDetail) -> Self {
        self.remediation = Some(remediation);
        self
    }

    /// The item a refused detail leaves behind.
    ///
    /// An `error`, because a probe that tried to print a path it had no right
    /// to is a defect in ego and deserves to be as loud as a broken subsystem.
    /// It names the probe and the reason and carries none of what was refused,
    /// which is the difference between reporting the refusal and undoing it.
    pub fn refused(section: HealthSection, refusal: &RedactionRefusal) -> Self {
        Self::new(
            HealthItemId::about("doctor.redaction_refused", refusal.probe),
            HealthStatus::Error,
            HealthDetail(refusal.to_string()),
            HealthDetail(format!("the {} probes", section.label())),
        )
        .remediation(HealthDetail::fixed(
            "report this: doctor withheld a diagnostic rather than print it",
        ))
    }
}

/// Every item one subsystem produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthSectionReport {
    pub section: HealthSection,
    pub items: Vec<HealthItem>,
}

impl HealthSectionReport {
    pub fn new(section: HealthSection, items: Vec<HealthItem>) -> Self {
        Self { section, items }
    }

    /// The worst thing in this section, or `Ok` when it is empty.
    pub fn status(&self) -> HealthStatus {
        self.items
            .iter()
            .map(|item| item.status)
            .max()
            .unwrap_or(HealthStatus::Ok)
    }
}

/// A subsystem willing to say what its own authority already knows.
///
/// A port, so `ego-core` fixes the vocabulary and the satellite crates supply
/// the facts — the credential store knows about credentials and this module
/// does not, but neither of them gets to decide on its own what `warning`
/// means.
///
/// `observe` takes `&self` and returns facts. There is no `&mut self` and
/// nothing here hands a probe a way to write: the type is the first place the
/// "observation never mutates" rule is enforced, before any test of it.
pub trait HealthProbe {
    /// Which section these facts are printed under.
    fn section(&self) -> HealthSection;

    /// What this subsystem knows, already sanitized against `roots`.
    ///
    /// Returning `Err` is how a probe says it was about to print something it
    /// should not; the collector turns that into a visible refusal rather than
    /// letting the probe choose between leaking and staying silent.
    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal>;
}

/// One complete diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthSnapshot {
    pub v: u32,
    pub sections: Vec<HealthSectionReport>,
}

impl HealthSnapshot {
    /// Assemble the sections into rendering order, including the empty ones.
    ///
    /// A section nobody produced facts for becomes an empty report rather than
    /// a gap, so every rendering prints all nine and an operator can see that
    /// a subsystem was considered.
    pub fn new(reports: impl IntoIterator<Item = HealthSectionReport>) -> Self {
        let produced = reports.into_iter().collect::<Vec<_>>();
        let sections = HealthSection::ORDER
            .into_iter()
            .map(|section| {
                let items = produced
                    .iter()
                    .filter(|report| report.section == section)
                    .flat_map(|report| report.items.iter().cloned())
                    .collect();
                HealthSectionReport::new(section, items)
            })
            .collect();
        Self {
            v: HEALTH_SNAPSHOT_VERSION,
            sections,
        }
    }

    /// Ask every probe, in order, and keep whatever each one managed to say.
    ///
    /// One probe's failure is one probe's item. Somebody diagnosing a broken
    /// credential store still needs to be told their MCP servers are fine, so
    /// nothing here short-circuits and no `?` escapes.
    pub fn observe(probes: &[&dyn HealthProbe], roots: &VisibleRoots) -> Self {
        Self::new(probes.iter().map(|probe| {
            let section = probe.section();
            let items = match probe.observe(roots) {
                Ok(items) => items,
                Err(refusal) => vec![HealthItem::refused(section, &refusal)],
            };
            HealthSectionReport::new(section, items)
        }))
    }

    /// The worst thing anywhere in the snapshot.
    pub fn status(&self) -> HealthStatus {
        self.sections
            .iter()
            .map(HealthSectionReport::status)
            .max()
            .unwrap_or(HealthStatus::Ok)
    }

    /// What the root command exits with.
    ///
    /// 1 only when something is operationally broken. A missing optional
    /// credential and a fact nobody could check are both worth printing and
    /// neither is a reason to fail a script.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.status().is_operational_error())
    }

    /// Every item, in the order they are printed.
    pub fn items(&self) -> impl Iterator<Item = &HealthItem> {
        self.sections.iter().flat_map(|report| report.items.iter())
    }
}
