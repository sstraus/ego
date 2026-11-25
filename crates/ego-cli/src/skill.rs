//! Crate-private discovery and immutable admission of `SKILL.md` files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ego_core::config::{ConfigLayer, ModelRef, Source, Sourced, Trust, resolve};
use ego_core::event::hook::HookLifecycle;
use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillInvocationId, SkillSourceLocation,
};
use ego_core::hook::{HookConfigEntry, HookHandlerConfig, HookMatcher};
use ego_core::permit::rule::Rule;
use ego_core::skill::{SkillCatalogue as CoreSkillCatalogue, SkillSpec};
use ego_core::subagent::ChildWait;

use crate::yaml_envelope::{self, MAX_FILE_BYTES, Node};

const PORTABLE_TOOLS: &[&str] = &[
    "ask_question",
    "edit",
    "exec",
    "glob",
    "grep",
    "read",
    "read_tool_result",
    "write",
];

pub(crate) use ego_core::event::skill::SkillSourceAuthority;

#[derive(Clone, Debug)]
pub(crate) struct SkillDiscovery {
    roots: Vec<crate::capability_roots::CapabilityRoot>,
    command_roots: Vec<crate::capability_roots::CapabilityRoot>,
    trusted_project: bool,
}

impl SkillDiscovery {
    #[cfg(test)]
    pub(crate) fn new(
        system: PathBuf,
        user: PathBuf,
        project: PathBuf,
        trusted_project: bool,
    ) -> Self {
        use crate::capability_roots::CapabilityRoot;
        let mut roots = Vec::new();
        if trusted_project {
            roots.push(CapabilityRoot {
                source: SkillSourceAuthority::Project,
                path: project,
            });
        }
        roots.push(CapabilityRoot {
            source: SkillSourceAuthority::User,
            path: user,
        });
        roots.push(CapabilityRoot {
            source: SkillSourceAuthority::System,
            path: system,
        });
        Self {
            roots,
            command_roots: Vec::new(),
            trusted_project,
        }
    }

    pub(crate) fn for_workspace(workspace: &Path, trusted_project: bool) -> Option<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut roots = crate::capability_roots::discover_capability_roots(
            workspace,
            trusted_project,
            Path::new("/etc/ego"),
            &crate::config::ego_home()?,
            home.as_deref(),
            "skills",
        );
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|path| path.join(".codex")));
        let claude_config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|path| path.join(".claude")));
        crate::capability_roots::register_external_skill_roots(
            &mut roots,
            codex_home.as_deref(),
            claude_config.as_deref(),
            &crate::capability_roots::claude_managed_directory(),
            &crate::capability_roots::codex_managed_directory(),
        );
        Some(Self {
            command_roots: crate::capability_roots::discover_capability_roots(
                workspace,
                trusted_project,
                Path::new("/etc/ego"),
                &crate::config::ego_home()?,
                home.as_deref(),
                "commands",
            ),
            roots,
            trusted_project,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkillDiagnostic {
    name: String,
    source: SkillSourceAuthority,
    source_path: PathBuf,
    message: String,
    collision: bool,
}

impl SkillDiagnostic {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn source(&self) -> SkillSourceAuthority {
        self.source
    }
    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
    pub(crate) fn is_collision(&self) -> bool {
        self.collision
    }
}

impl fmt::Display for SkillDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "skill `{}` at {}: {}",
            self.name,
            self.source_path.display(),
            self.message
        )
    }
}

#[derive(Debug)]
pub(crate) struct SkillCatalogueError(String);

impl fmt::Display for SkillCatalogueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SkillCatalogueCapture {
    catalogue: Arc<CoreSkillCatalogue>,
    diagnostics: Arc<[SkillDiagnostic]>,
    discovery_notes: Arc<[String]>,
    snapshot_digest: Arc<str>,
    source_paths: Arc<BTreeMap<String, PathBuf>>,
    attempt: u64,
    publication: u64,
}

const BUNDLED_CONTEXT_PATH: &str = "bundled/context-management/SKILL.md";

#[derive(Clone, Copy, Debug)]
pub(crate) enum SkillBadge {
    Project(Option<&'static str>),
    User,
    System,
    BuiltIn,
}

impl SkillBadge {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Project(_) => "[project]",
            Self::User => "[user]",
            Self::System => "[system]",
            Self::BuiltIn => "[built-in]",
        }
    }

    pub(crate) fn namespace(self) -> Option<&'static str> {
        match self {
            Self::Project(namespace) => namespace,
            _ => None,
        }
    }

    pub(crate) fn style(self, depth: crate::render::Depth, light: bool) -> crate::render::Style {
        use crate::render::{Depth, Rgb, Style};
        // The sixteen-color palette can map muted hues to bright theme colors.
        if depth != Depth::TrueColor {
            return Style::PLAIN;
        }
        let colors = match self {
            Self::Project(_) => (Rgb(156, 175, 198), Rgb(75, 96, 120)),
            Self::User => (Rgb(162, 179, 158), Rgb(80, 102, 77)),
            Self::System => (Rgb(190, 169, 149), Rgb(115, 91, 72)),
            Self::BuiltIn => (Rgb(167, 169, 174), Rgb(94, 98, 106)),
        };
        Style::fg(if light { colors.1 } else { colors.0 })
    }
}

pub(crate) fn light_skill_palette() -> bool {
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .is_some_and(|background| matches!(background, 7 | 9..=15 | 250..=255))
}

impl SkillCatalogueCapture {
    pub(crate) fn descriptors(&self) -> &[SkillSpec] {
        self.catalogue.specs()
    }

    pub(crate) fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn descriptor(&self, name: &str) -> Option<&SkillSpec> {
        self.catalogue
            .specs()
            .iter()
            .find(|spec| spec.name() == name)
    }

    pub(crate) fn snapshot_digest(&self) -> &str {
        &self.snapshot_digest
    }

    pub(crate) fn core(&self) -> Arc<CoreSkillCatalogue> {
        Arc::clone(&self.catalogue)
    }

    pub(crate) fn retained_last_good(&self) -> bool {
        self.attempt != self.publication
    }

    pub(crate) fn badge(&self, spec: &SkillSpec) -> Option<SkillBadge> {
        if spec.command_template().is_some() {
            return None;
        }
        let path = self.source_paths.get(spec.name()).map(PathBuf::as_path);
        Some(match spec.source() {
            SkillSourceAuthority::Project => {
                // Use the winning discovery path, not a symlink target.
                let namespace = path
                    .and_then(|path| path.ancestors().nth(3)?.file_name())
                    .and_then(|name| {
                        [".ego", ".agents", ".claude", ".codex", ".github"]
                            .into_iter()
                            .find(|label| name == *label)
                    });
                SkillBadge::Project(namespace)
            }
            SkillSourceAuthority::User => SkillBadge::User,
            SkillSourceAuthority::System if path == Some(Path::new(BUNDLED_CONTEXT_PATH)) => {
                SkillBadge::BuiltIn
            }
            SkillSourceAuthority::System => SkillBadge::System,
        })
    }

    pub(crate) fn render_summary(&self, caps: crate::render::Capabilities, light: bool) -> String {
        use crate::render::{Line, Span, table::Table};
        let table = Table {
            header: ["Skill", "Origin", "Description"]
                .into_iter()
                .map(Line::plain)
                .collect(),
            rows: self
                .descriptors()
                .iter()
                .map(|spec| {
                    let mut origin = Line::empty();
                    if let Some(badge) = self.badge(spec) {
                        origin.push(Span::new(badge.label(), badge.style(caps.depth, light)));
                        if let Some(namespace) = badge.namespace() {
                            origin.push(Span::plain(format!(" {namespace}")));
                        }
                    }
                    vec![
                        Line::plain(format!("/{}", spec.name())),
                        origin,
                        Line::plain(&spec.descriptor().description),
                    ]
                })
                .collect(),
            align: Vec::new(),
        };
        let mut rows = vec![format!("skills: {} admitted", self.descriptors().len())];
        rows.extend(
            table
                .render_listing(caps.width.saturating_sub(2))
                .into_iter()
                .map(|line| line.render(caps.depth)),
        );
        if self.retained_last_good() {
            rows.push("skills: retained last good catalogue; /skills diagnose".into());
        }
        rows.push(
            "/skills details · /skills diagnose · /skills rescan · /skills invoke NAME [arguments]"
                .into(),
        );
        rows.join("\n")
    }

    pub(crate) fn render_list(&self) -> String {
        let state = if self.retained_last_good() {
            "retained_last_good"
        } else {
            "current"
        };
        let mut rows = vec![format!(
            "skills: catalogue_digest={} state={state}",
            self.snapshot_digest()
        )];
        if self.descriptors().is_empty() {
            rows.push("skills: none admitted".to_owned());
        }
        for spec in self.descriptors() {
            let descriptor = spec.descriptor();
            let arguments = descriptor
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            let source_authority = match descriptor.source {
                SkillSourceAuthority::System => "system",
                SkillSourceAuthority::User => "user",
                SkillSourceAuthority::Project => "project",
            };
            rows.push(format!(
                "/{}{arguments}\n  description={}\n  source_authority={}\n  location={}\n  source_digest={}\n  user_invocable={}\n  model_invocable={}\n  availability=admitted",
                spec.name(),
                descriptor.description,
                source_authority,
                self.source_paths.get(spec.name()).map(|path| path.display().to_string()).unwrap_or_else(|| descriptor.location.to_string()),
                descriptor.source_digest,
                descriptor.user_invocable,
                descriptor.model_invocable,
            ));
        }
        rows.push(self.render_admission_errors());
        rows.join("\n")
    }

    pub(crate) fn render_diagnostics(&self) -> String {
        std::iter::once(self.render_admission_errors())
            .chain(self.discovery_notes.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn render_admission_errors(&self) -> String {
        if self.diagnostics.is_empty() {
            "skills: no admission diagnostics".to_owned()
        } else {
            self.diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SkillCatalogueView {
    publication: Arc<RwLock<SkillPublication>>,
}

impl SkillCatalogueView {
    pub(crate) fn capture(&self) -> Result<SkillCatalogueCapture, SkillCatalogueError> {
        self.publication
            .read()
            .map_err(|_| {
                SkillCatalogueError("the skill catalogue publication is unavailable".to_owned())
            })
            .map(|publication| publication.capture())
    }
}

#[derive(Clone, Debug)]
struct SkillPublication {
    catalogue: Arc<CoreSkillCatalogue>,
    diagnostics: Arc<[SkillDiagnostic]>,
    discovery_notes: Arc<[String]>,
    snapshot_digest: Arc<str>,
    source_paths: Arc<BTreeMap<String, PathBuf>>,
    attempt: u64,
    publication: u64,
}

impl SkillPublication {
    fn capture(&self) -> SkillCatalogueCapture {
        SkillCatalogueCapture {
            catalogue: Arc::clone(&self.catalogue),
            diagnostics: Arc::clone(&self.diagnostics),
            discovery_notes: Arc::clone(&self.discovery_notes),
            snapshot_digest: Arc::clone(&self.snapshot_digest),
            source_paths: Arc::clone(&self.source_paths),
            attempt: self.attempt,
            publication: self.publication,
        }
    }

    fn identity(&self) -> SkillPublicationIdentity {
        SkillPublicationIdentity {
            attempt: self.attempt,
            publication: self.publication,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SkillPublicationIdentity {
    attempt: u64,
    publication: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillCatalogue {
    discovery: SkillDiscovery,
    fixed_actions: Arc<[String]>,
    profile_layers: Option<Arc<[Sourced<ConfigLayer>]>>,
    publication: Arc<RwLock<SkillPublication>>,
}

#[derive(Debug)]
pub(crate) struct PreparedSkillRescan {
    base: SkillPublicationIdentity,
    prospective: SkillPublication,
    published: bool,
}

impl PreparedSkillRescan {
    pub(crate) fn capture(&self) -> SkillCatalogueCapture {
        self.prospective.capture()
    }

    pub(crate) fn published(&self) -> bool {
        self.published
    }
}

impl SkillCatalogue {
    pub(crate) fn discover(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
    ) -> Result<Self, SkillCatalogueError> {
        Self::admit(discovery, fixed_actions, None)
    }

    pub(crate) fn discover_with_profile_layers(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: &[Sourced<ConfigLayer>],
    ) -> Result<Self, SkillCatalogueError> {
        Self::admit(discovery, fixed_actions, Some(profile_layers))
    }

    fn admit(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: Option<&[Sourced<ConfigLayer>]>,
    ) -> Result<Self, SkillCatalogueError> {
        let candidate = Candidate::build(discovery.clone(), fixed_actions, profile_layers)?;
        let publication = candidate.publish(0, 0);
        Ok(Self {
            discovery,
            fixed_actions: fixed_actions
                .iter()
                .map(|action| (*action).to_owned())
                .collect(),
            profile_layers: profile_layers.map(|layers| Arc::from(layers.to_vec())),
            publication: Arc::new(RwLock::new(publication)),
        })
    }

    pub(crate) fn view(&self) -> SkillCatalogueView {
        SkillCatalogueView {
            publication: Arc::clone(&self.publication),
        }
    }

    pub(crate) fn capture(&self) -> Result<SkillCatalogueCapture, SkillCatalogueError> {
        self.view().capture()
    }

    pub(crate) fn prepare_rescan(&self) -> Result<PreparedSkillRescan, SkillCatalogueError> {
        let base = self.publication.read().map_err(|_| {
            SkillCatalogueError("the skill catalogue publication is unavailable".to_owned())
        })?;
        let base_identity = base.identity();
        let attempt = base.attempt.checked_add(1).ok_or_else(|| {
            SkillCatalogueError("the skill catalogue attempt counter overflowed".to_owned())
        })?;
        let retained_catalogue = Arc::clone(&base.catalogue);
        let retained_digest = Arc::clone(&base.snapshot_digest);
        let base_source_paths = Arc::clone(&base.source_paths);
        let retained_publication = base.publication;
        drop(base);

        let fixed_actions = self
            .fixed_actions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let candidate = Candidate::build(
            self.discovery.clone(),
            &fixed_actions,
            self.profile_layers.as_deref(),
        )?;
        let failed_diagnostics = (!candidate.diagnostics.is_empty())
            .then(|| Arc::<[SkillDiagnostic]>::from(candidate.diagnostics.clone()));
        let published = failed_diagnostics.is_none();
        let prospective = match failed_diagnostics {
            Some(diagnostics) => SkillPublication {
                catalogue: retained_catalogue,
                diagnostics,
                discovery_notes: candidate.discovery_notes.into(),
                snapshot_digest: retained_digest,
                source_paths: base_source_paths,
                attempt,
                publication: retained_publication,
            },
            None => candidate.publish(attempt, attempt),
        };
        Ok(PreparedSkillRescan {
            base: base_identity,
            prospective,
            published,
        })
    }

    pub(crate) fn commit_rescan(
        &self,
        prepared: PreparedSkillRescan,
    ) -> Result<SkillCatalogueCapture, SkillCatalogueError> {
        let mut publication = self.publication.write().map_err(|_| {
            SkillCatalogueError("the skill catalogue publication is unavailable".to_owned())
        })?;
        if publication.identity() != prepared.base {
            return Err(SkillCatalogueError(
                "the skill catalogue publication changed before the prepared rescan could commit"
                    .to_owned(),
            ));
        }
        *publication = prepared.prospective;
        Ok(publication.capture())
    }
}

/// Skills the owner hid by name, from the user, system and flag layers only.
#[derive(Debug, Default)]
struct SkillVisibility {
    hide: BTreeSet<String>,
    off: BTreeSet<String>,
}

impl SkillVisibility {
    fn from_layers(layers: &[Sourced<ConfigLayer>]) -> Self {
        let mut visibility = Self::default();
        for layer in layers {
            if !matches!(layer.source, Source::User | Source::System | Source::Flags) {
                continue;
            }
            if let Some(skills) = &layer.value.skills {
                visibility.hide.extend(skills.hide.iter().cloned());
                visibility.off.extend(skills.off.iter().cloned());
            }
        }
        visibility
    }
}

struct Candidate {
    admitted: Vec<(SkillSpec, PathBuf)>,
    diagnostics: Vec<SkillDiagnostic>,
    discovery_notes: Vec<String>,
}

impl Candidate {
    fn build(
        discovery: SkillDiscovery,
        fixed_actions: &[&str],
        profile_layers: Option<&[Sourced<ConfigLayer>]>,
    ) -> Result<Self, SkillCatalogueError> {
        let mut fixed_unique = BTreeSet::new();
        for action in fixed_actions {
            if action.is_empty() || !fixed_unique.insert(*action) {
                return Err(SkillCatalogueError(format!(
                    "fixed action vocabulary contains invalid or duplicate name `{action}`"
                )));
            }
        }
        let profile_trust = if discovery.trusted_project {
            Trust::Trusted
        } else {
            Trust::Untrusted
        };
        let mut admitted = vec![(bundled_context_skill(), PathBuf::from(BUNDLED_CONTEXT_PATH))];
        let mut diagnostics = Vec::new();
        {
            let (commands, errors) = crate::command::discover_roots(&discovery.command_roots);
            admitted.extend(commands);
            diagnostics.extend(
                errors
                    .into_iter()
                    .map(|(name, source, path, message)| diagnostic(name, source, path, message)),
            );
        }
        let mut seen_paths = BTreeSet::new();
        let mut discovery_notes = Vec::new();
        if !discovery.trusted_project {
            discovery_notes.push("project skill roots skipped: workspace is not trusted; approve the workspace at launch to load its skills".to_owned());
        }
        for root in discovery.roots {
            discovery_notes.push(discover_root(
                &root,
                &mut seen_paths,
                &mut admitted,
                &mut diagnostics,
            ));
        }

        if let Some(profile_layers) = profile_layers {
            admitted.retain(|(spec, source_path)| {
                let Some(profile) = spec.agent() else {
                    return true;
                };
                match resolve(profile_layers, profile_trust, Some(profile)) {
                    Ok(_) => true,
                    Err(error) => {
                        let hint = match profile {
                            "Explore" | "Plan" => format!(
                                "; ego has no built-in `{profile}` agent, define [profile.{profile}] in the configuration to map it"
                            ),
                            _ => String::new(),
                        };
                        diagnostics.push(SkillDiagnostic {
                            name: spec.name().to_owned(),
                            source: spec.source(),
                            source_path: source_path.clone(),
                            message: format!(
                                "field `agent` must select an existing profile: {error}{hint}"
                            ),
                            collision: false,
                        });
                        false
                    }
                }
            });
        }

        let fixed = fixed_unique;
        let bundled_collision = admitted
            .iter()
            .skip(1)
            .any(|(spec, _)| spec.name() == "context-management");
        let mut names = BTreeMap::<String, PathBuf>::new();
        admitted.retain(|(spec, source_path)| {
            let name = spec.name();
            if fixed.contains(name) || (bundled_collision && name == "context-management") {
                diagnostics.push(SkillDiagnostic {
                    name: name.to_owned(),
                    source: spec.source(),
                    source_path: source_path.clone(),
                    message: format!("collides with fixed action `{name}`"),
                    collision: true,
                });
                false
            } else {
                // Roots arrive in precedence order. The bundled capability is
                // reserved and always wins over filesystem definitions.
                if let Some(winner) = names.get(name) {
                    discovery_notes.push(format!(
                        "skill `{name}` at {}: shadowed by {}",
                        source_path.display(),
                        winner.display()
                    ));
                    false
                } else {
                    names.insert(name.to_owned(), source_path.clone());
                    true
                }
            }
        });
        // The owner's `[skills]` choice applies last, so it wins over every root.
        if let Some(layers) = profile_layers {
            let visibility = SkillVisibility::from_layers(layers);
            admitted.retain_mut(|(spec, _)| {
                let name = spec.name().to_owned();
                if visibility.off.contains(&name) {
                    return false;
                }
                if visibility.hide.contains(&name) {
                    // A skill only the model could use has nothing left to offer.
                    if !spec.descriptor().user_invocable {
                        return false;
                    }
                    spec.hide_from_model();
                }
                true
            });
        }
        admitted.sort_by(|(left, _), (right, _)| left.name().cmp(right.name()));
        diagnostics.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.source_path.cmp(&right.source_path))
        });
        Ok(Self {
            admitted,
            diagnostics,
            discovery_notes,
        })
    }

    fn publish(self, attempt: u64, publication: u64) -> SkillPublication {
        let source_paths = Arc::new(
            self.admitted
                .iter()
                .map(|(spec, path)| (spec.name().to_owned(), path.clone()))
                .collect(),
        );
        let catalogue =
            CoreSkillCatalogue::new(self.admitted.into_iter().map(|(spec, _)| spec).collect())
                .expect("the candidate already rejects duplicate and unreachable skills");
        let snapshot_digest: Arc<str> = catalogue.generation().to_string().into();
        SkillPublication {
            catalogue: Arc::new(catalogue),
            diagnostics: self.diagnostics.into(),
            discovery_notes: self.discovery_notes.into(),
            snapshot_digest,
            source_paths,
            attempt,
            publication,
        }
    }
}

fn bundled_context_skill() -> SkillSpec {
    let body = include_str!("skills/context-management.md");
    let source = SkillSourceAuthority::System;
    let location = SkillSourceLocation::parse("skills/context-management/SKILL.md")
        .expect("bundled skill location");
    let digest = SkillContentDigest::parse(&ego_core::digest::sha256_hex(body.as_bytes()))
        .expect("bundled skill digest");
    SkillSpec::new(SkillDescriptorEvidence {
        id: SkillId::derive(source, &location, "context-management", &digest),
        name: "context-management".into(),
        description: "Decide when to compact or clear context at a safe sub-task boundary, and continue the active goal.".into(),
        trigger: None, argument_hint: None, source, location, source_digest: digest,
        user_invocable: true, model_invocable: true,
    }, body.as_bytes().to_vec()).expect("bundled context skill")
}

fn discover_root(
    root: &crate::capability_roots::CapabilityRoot,
    seen_paths: &mut BTreeSet<PathBuf>,
    admitted: &mut Vec<(SkillSpec, PathBuf)>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> String {
    let label = root.path.display();
    let entries = root
        .canonical_skill_root()
        .and_then(|canonical| fs::read_dir(&canonical).map(|entries| (canonical, entries)));
    let (canonical_root, entries) = match entries {
        Ok(found) => found,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return format!("skill root {label}: not found");
        }
        Err(error) => {
            diagnostics.push(diagnostic(
                "<root>".to_owned(),
                root.source,
                root.path.clone(),
                format!("skill root could not be scanned: {error}"),
            ));
            return format!("skill root {label}: not scanned");
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| root.path.join(entry.file_name()).join("SKILL.md"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    let count = paths.len();
    for path in paths {
        let directory_name = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("<invalid>")
            .to_owned();
        if let Ok(canonical) = fs::canonicalize(&path)
            && seen_paths.contains(&canonical)
        {
            continue;
        }
        match load_skill(root.source, &canonical_root, &path, &directory_name) {
            Ok(spec) => {
                if let Ok(canonical) = fs::canonicalize(&path) {
                    seen_paths.insert(canonical);
                }
                admitted.push((spec, path));
            }
            Err(message) => {
                diagnostics.push(diagnostic(directory_name, root.source, path, message))
            }
        }
    }
    if count == 0 {
        format!("skill root {label}: no SKILL.md files")
    } else {
        format!("skill root {label}: scanned {count} SKILL.md file(s)")
    }
}

fn load_skill(
    source: SkillSourceAuthority,
    canonical_root: &Path,
    requested_path: &Path,
    directory_name: &str,
) -> Result<SkillSpec, String> {
    let path = fs::canonicalize(requested_path)
        .map_err(|error| format!("could not be resolved: {error}"))?;
    // User-owned roots can link to skills maintained elsewhere. Project and
    // system roots must not admit files outside their declared authority.
    if source != SkillSourceAuthority::User && !path.starts_with(canonical_root) {
        return Err("resolves outside its canonical skill root".to_owned());
    }
    let mut file = fs::File::open(&path).map_err(|error| format!("could not be read: {error}"))?;
    if file
        .metadata()
        .map_err(|error| format!("could not be read: {error}"))?
        .len()
        > MAX_FILE_BYTES
    {
        return Err(format!("exceeds {MAX_FILE_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("exceeds {MAX_FILE_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "is not valid UTF-8".to_owned())?;
    let source_digest = SkillContentDigest::parse(&ego_core::digest::sha256_hex(text.as_bytes()))
        .map_err(|error| error.to_string())?;
    let (header, body) = yaml_envelope::split(&text, false)?;
    let mut entries = if header.trim().is_empty() {
        BTreeMap::new()
    } else {
        match yaml_envelope::mapping(header) {
            Ok(entries) => entries,
            // Text that is not YAML at all (an unquoted `description: Use when: x`)
            // loads with no fields, as in the reference format, unless the header
            // names a field that restricts the skill: dropping that field would
            // widen who may invoke it or what it may use.
            Err(message) if message.starts_with("contains invalid YAML") => {
                if let Some(key) = restricting_key(header) {
                    return Err(format!(
                        "{message}; the header names `{key}`, so the skill is refused instead of loaded without it. Quote a value that contains `: `"
                    ));
                }
                BTreeMap::new()
            }
            Err(message) => return Err(message),
        }
    };
    validate_field_names(&entries)?;

    let name =
        take_optional_string(&mut entries, "name")?.unwrap_or_else(|| directory_name.to_owned());
    validate_skill_name(&name)?;
    // An over-long description is cut, not refused: the listing shows a bounded
    // prefix whatever the author wrote.
    let description = truncate_chars(
        take_optional_string(&mut entries, "description")?
            .unwrap_or_else(|| body.trim().split("\n\n").next().unwrap_or(&name).to_owned()),
        MAX_DESCRIPTION_CHARS,
    );

    let allowed_tools = take_tools(&mut entries, "allowed-tools")?;
    let disallowed_tools = take_tools(&mut entries, "disallowed-tools")?.unwrap_or_default();
    // `license` is portable metadata that nothing acts on.
    entries.remove("license");
    let compatibility = take_optional_string(&mut entries, "compatibility")?;
    if compatibility
        .as_deref()
        .is_some_and(|value| value.chars().count() > 500)
    {
        return Err("field `compatibility` exceeds 500 characters".to_owned());
    }
    let trigger = take_optional_string(&mut entries, "when_to_use")?;
    let argument_hint = take_argument_hint(&mut entries)?;
    let model = take_optional_string(&mut entries, "model")?
        .filter(|model| !model.eq_ignore_ascii_case("inherit"))
        .map(ModelRef);
    let effort = take_optional_string(&mut entries, "effort")?.map(|value| value.to_lowercase());
    if let Some(value) = effort.as_deref()
        && ego_core::skill::parse_skill_effort(value).is_none()
    {
        return Err(format!("field `effort` has unsupported value `{value}`"));
    }
    let user_invocable = take_optional_bool(&mut entries, "user-invocable")?.unwrap_or(true);
    let model_invocable =
        !take_optional_bool(&mut entries, "disable-model-invocation")?.unwrap_or(false);
    if !user_invocable && !model_invocable {
        return Err(
            "fields `user-invocable` and `disable-model-invocation` make the skill unreachable"
                .to_owned(),
        );
    }
    if matches!(entries.get("arguments"), Some(Node::Scalar { .. })) {
        let value = take_optional_string(&mut entries, "arguments")?.expect("present scalar");
        let names = value
            .split_whitespace()
            .map(|value| Node::Scalar {
                value: value.to_owned(),
                plain: false,
            })
            .collect();
        entries.insert("arguments".to_owned(), Node::Sequence(names));
    }
    let arguments = take_string_sequence(&mut entries, "arguments")?;
    if let Some(argument) = arguments
        .iter()
        .find(|argument| !valid_identifier(argument))
    {
        return Err(format!(
            "field `arguments` contains invalid identifier `{argument}`"
        ));
    }
    // `resource-paths` is ego's own policy: the skill-relative files a model may
    // read. `paths` keeps the meaning it has in the reference format, a hint
    // for when the skill applies, and never restricts anything.
    let paths = take_string_sequence(&mut entries, "resource-paths")?;
    let activation = take_activation_paths(&mut entries);
    let trigger = match (trigger, activation.is_empty()) {
        (trigger, true) => trigger,
        (trigger, false) => {
            let hint = format!(
                "Applies when working with files matching: {}",
                activation.join(", ")
            );
            Some(match trigger {
                Some(trigger) => format!("{trigger} {hint}"),
                None => hint,
            })
        }
    };
    let metadata = take_string_mapping(&mut entries, "metadata")?;

    let context = take_optional_string(&mut entries, "context")?;
    if context.as_deref().is_some_and(|value| value != "fork") {
        return Err("field `context` supports only exact value `fork`".to_owned());
    }
    let agent = take_optional_string(&mut entries, "agent")?;
    if agent.is_some() && context.as_deref() != Some("fork") {
        return Err("field `agent` requires `context: fork`".to_owned());
    }
    if let Some(agent) = &agent {
        validate_profile_name(agent).map_err(|message| format!("field `agent` {message}"))?;
    }
    // The reference format's default subagent is the child with no special role.
    // In ego that is a child with no profile, so the name maps to nothing.
    let agent = agent.filter(|agent| !agent.eq_ignore_ascii_case("general-purpose"));
    let background = take_optional_bool(&mut entries, "background")?.unwrap_or(false);
    let bare = take_optional_bool(&mut entries, "bare")?.unwrap_or(false);
    let hooks = match entries.remove("hooks") {
        None => Vec::new(),
        Some(Node::Sequence(hooks)) => hooks
            .into_iter()
            .enumerate()
            .map(|(ordinal, hook)| project_hook_entry(hook, ordinal))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("field `hooks` must be a sequence".to_owned()),
    };
    if let Some(shell) = take_optional_string(&mut entries, "shell")?
        && !matches!(shell.as_str(), "bash" | "powershell")
    {
        return Err("field `shell` must be exactly `bash` or `powershell`".to_owned());
    }
    // Remaining fields are inert extensions, not invocation authority.

    let location = SkillSourceLocation::parse(&format!("skills/{name}/SKILL.md"))
        .map_err(|error| error.to_string())?;
    let descriptor = SkillDescriptorEvidence {
        id: SkillId::derive(source, &location, &name, &source_digest),
        name,
        description,
        trigger,
        argument_hint,
        source,
        location,
        source_digest,
        user_invocable,
        model_invocable,
    };
    let mut spec = SkillSpec::new(descriptor, body.as_bytes().to_vec())
        .map_err(|error| error.to_string())?
        .with_resource_root(ego_core::permit::CanonPath::new(
            path.parent().expect("canonical skill file has a parent"),
        ))
        .with_model_selection(model, effort)
        .with_disallowed_tools(disallowed_tools)
        .with_arguments(arguments)
        .with_metadata(metadata)
        .with_resource_paths(paths)
        .map_err(|error| error.to_string())?
        .with_hook_entries(hooks);
    if let Some(tools) = allowed_tools {
        spec = spec.with_tool_ceiling(tools);
    }
    if context.as_deref() == Some("fork") {
        spec = spec.with_fork(if background {
            ChildWait::Background
        } else {
            ChildWait::Foreground
        });
    } else if background {
        return Err("field `background` requires `context: fork`".to_owned());
    } else if bare {
        // Only a fork opens a run of its own to hold a posture. Read on a
        // skill that runs inside its parent's turn, `bare` would be a word
        // that changes nothing, which is worse than a refusal.
        return Err("field `bare` requires `context: fork`".to_owned());
    }
    spec = spec.with_bare(bare);
    if let Some(agent) = agent {
        spec = spec.with_profile(agent);
    }
    Ok(spec)
}

/// Fields whose loss would widen a skill: visibility, tools, execution context.
const RESTRICTING_KEYS: &[&str] = &[
    "allowed-tools",
    "allowed_tools",
    "disallowed-tools",
    "disallowed_tools",
    "disable-model-invocation",
    "user-invocable",
    "context",
    "agent",
    "bare",
    "hooks",
    "model",
    "effort",
    "resource-paths",
];

/// The first restricting field named at the start of a header line, found as
/// text because the header did not parse.
fn restricting_key(header: &str) -> Option<&'static str> {
    header.lines().find_map(|line| {
        RESTRICTING_KEYS.iter().copied().find(|key| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    })
}

pub(crate) fn validate_field_names(entries: &BTreeMap<String, Node>) -> Result<(), String> {
    for (old, canonical) in [
        ("allowed_tools", "allowed-tools"),
        ("disallowed_tools", "disallowed-tools"),
    ] {
        if entries.contains_key(old) {
            return Err(format!(
                "field `{old}` is unsupported; use exact field `{canonical}`"
            ));
        }
    }
    Ok(())
}

pub(crate) fn take_optional_string(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    yaml_envelope::scalar(node)
        .map(Some)
        .ok_or_else(|| format!("field `{key}` must be a non-empty string"))
}

fn take_optional_bool(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<bool>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    match node {
        Node::Scalar { value, plain: true }
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            ) =>
        {
            Ok(Some(true))
        }
        Node::Scalar { value, plain: true }
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0"
            ) =>
        {
            Ok(Some(false))
        }
        _ => Err(format!("field `{key}` must be a boolean")),
    }
}

fn take_string_sequence(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Vec<String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(Vec::new());
    };
    let Node::Sequence(nodes) = node else {
        return Err(format!("field `{key}` must be a sequence of strings"));
    };
    let mut values = Vec::new();
    let mut unique = BTreeSet::new();
    for node in nodes {
        let Some(value) = yaml_envelope::scalar(node) else {
            return Err(format!("field `{key}` must contain only non-empty strings"));
        };
        if !unique.insert(value.clone()) {
            return Err(format!("field `{key}` contains duplicate `{value}`"));
        }
        values.push(value);
    }
    Ok(values)
}

fn take_string_mapping(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<BTreeMap<String, String>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(BTreeMap::new());
    };
    let Node::Mapping(nodes) = node else {
        return Ok(BTreeMap::new());
    };
    // Free-form data for outside tools: entries that are not scalars are
    // skipped, and a number or a boolean is kept as its text.
    Ok(nodes
        .into_iter()
        .filter_map(|(key_node, value_node)| match (key_node, value_node) {
            (Node::Scalar { value: map_key, .. }, Node::Scalar { value, .. })
                if !map_key.is_empty() =>
            {
                Some((map_key, value))
            }
            _ => None,
        })
        .collect())
}

const MAX_DESCRIPTION_CHARS: usize = 1536;

fn truncate_chars(value: String, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => value[..end].to_owned(),
        None => value,
    }
}

/// Activation globs: a comma-separated string or a list. Anything else is
/// ignored, because the globs only inform the model.
fn take_activation_paths(entries: &mut BTreeMap<String, Node>) -> Vec<String> {
    let words = match entries.remove("paths") {
        Some(Node::Scalar { value, .. }) => value.split(',').map(str::to_owned).collect(),
        Some(Node::Sequence(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Node::Scalar { value, .. } => Some(value),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    words
        .into_iter()
        .map(|word: String| word.trim().to_owned())
        .filter(|word| !word.is_empty() && !word.chars().any(char::is_control))
        .take(32)
        .collect()
}

/// `[issue-number]` is a YAML list, and it is the spelling the reference
/// format documents for a hint. Read it back as the bracketed text.
pub(crate) fn take_argument_hint(
    entries: &mut BTreeMap<String, Node>,
) -> Result<Option<String>, String> {
    if matches!(entries.get("argument-hint"), Some(Node::Sequence(_)))
        && let Some(Node::Sequence(items)) = entries.remove("argument-hint")
    {
        let words = items
            .into_iter()
            .map(|item| match item {
                Node::Scalar { value, .. } if !value.is_empty() => Ok(value),
                _ => Err("field `argument-hint` must be a string or a list of strings".to_owned()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if words.is_empty() {
            return Err("field `argument-hint` must not be an empty list".to_owned());
        }
        return Ok(Some(format!("[{}]", words.join(", "))));
    }
    take_optional_string(entries, "argument-hint")
}

pub(crate) fn take_tools(
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    let nodes = match node {
        Node::Sequence(nodes) => nodes,
        scalar @ Node::Scalar { .. } => vec![scalar],
        _ => return Err(format!("field `{key}` must be a string or list of strings")),
    };
    let mut tools = BTreeSet::new();
    for node in nodes {
        let value = yaml_envelope::scalar(node)
            .ok_or_else(|| format!("field `{key}` must contain non-empty strings"))?;
        let mut depth = 0usize;
        for tool in value
            .split(|c: char| {
                match c {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                depth == 0 && (c == ',' || c.is_whitespace())
            })
            .filter(|tool| !tool.is_empty())
        {
            if key == "disallowed-tools" {
                if let Some(tool) = reference_tool(tool) {
                    tools.insert(tool.to_owned());
                }
            } else {
                if !PORTABLE_TOOLS.contains(&tool) {
                    if reference_tool(tool).is_some() || tool.contains('(') {
                        return Err(format!(
                            "field `{key}` names `{tool}`, which is not an ego tool name; in ego the field narrows the tools a skill may use, so name them as {}",
                            PORTABLE_TOOLS.join(", ")
                        ));
                    }
                    return Err(format!("field `{key}` contains unknown tool `{tool}`"));
                }
                tools.insert(tool.to_owned());
            }
        }
    }
    Ok(Some(tools.into_iter().collect()))
}

fn reference_tool(tool: &str) -> Option<&str> {
    Some(match tool {
        "Bash" => "exec",
        "Read" => "read",
        "Write" => "write",
        "Edit" | "MultiEdit" => "edit",
        "Glob" => "glob",
        "Grep" => "grep",
        "AskUserQuestion" => "ask_question",
        "WebFetch" => "web_fetch",
        "NotebookEdit" => "notebook_edit",
        "Task" | "Agent" => "task",
        tool if PORTABLE_TOOLS.contains(&tool)
            || matches!(tool, "web_fetch" | "notebook_edit" | "task") =>
        {
            tool
        }
        _ => return None,
    })
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(format!("field `name` has invalid skill name `{name}`"));
    }
    Ok(())
}

fn validate_profile_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("must name an existing profile with a canonical identifier");
    }
    Ok(())
}

fn valid_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn diagnostic(
    name: String,
    source: SkillSourceAuthority,
    source_path: PathBuf,
    message: impl Into<String>,
) -> SkillDiagnostic {
    SkillDiagnostic {
        name,
        source,
        source_path,
        message: message.into(),
        collision: false,
    }
}

/// Project the closed invocation-scoped hook envelope without reading a skill body.
pub(crate) fn project_skill_hooks(
    header: &str,
    logical_location: &str,
    invocation: SkillInvocationId,
) -> Result<Vec<HookConfigEntry>, String> {
    let location = SkillSourceLocation::parse(logical_location)
        .map_err(|error| format!("invalid skill hook source location: {error}"))?;
    let mut root = yaml_envelope::mapping(header)?;
    let version = root
        .remove("version")
        .ok_or_else(|| "skill hook envelope is missing required integer `version: 1`".to_owned())?;
    match version {
        Node::Scalar { value, plain: true } if value == "1" => {}
        _ => return Err("skill hook envelope `version` must be the integer 1".to_owned()),
    }
    for key in root.keys() {
        if key != "hooks" {
            return Err(format!("unsupported skill hook envelope field `{key}`"));
        }
    }
    let Some(hooks) = root.remove("hooks") else {
        return Ok(Vec::new());
    };
    let Node::Sequence(hooks) = hooks else {
        return Err("skill hook field `hooks` must be a sequence".to_owned());
    };
    hooks
        .into_iter()
        .enumerate()
        .map(|(ordinal, hook)| {
            Ok(project_hook_entry(hook, ordinal)?.bind_skill(location.as_str(), invocation))
        })
        .collect()
}

fn project_hook_entry(node: Node, ordinal: usize) -> Result<HookConfigEntry, String> {
    let mut fields = hook_mapping(node, &format!("hooks[{ordinal}]"))?;
    reject_unknown_hook_fields(&fields, &["event", "matcher", "once", "handler"])?;
    let event = hook_required_string(&mut fields, "event")?;
    let event =
        HookLifecycle::parse(&event).ok_or_else(|| format!("unsupported hook event `{event}`"))?;
    let matcher = hook_optional_string(&mut fields, "matcher")?
        .map(project_hook_matcher)
        .transpose()?;
    let once = hook_optional_bool(&mut fields, "once")?.unwrap_or(false);
    let handler = fields
        .remove("handler")
        .ok_or_else(|| "skill hook is missing required field `handler`".to_owned())?;
    let handler = project_hook_handler(handler)?;
    Ok(HookConfigEntry::new(event, matcher, handler).with_once(once))
}

fn project_hook_matcher(value: String) -> Result<HookMatcher, String> {
    if let Ok(rule) = Rule::load(&value) {
        return Ok(HookMatcher::Permission(rule));
    }
    if value.is_empty()
        || value.len() > 16 * 1024
        || value.chars().any(char::is_control)
        || (value.starts_with('/') && value.ends_with('/'))
    {
        return Err(format!("invalid exact hook matcher `{value}`"));
    }
    Ok(HookMatcher::Exact(value))
}

fn project_hook_handler(node: Node) -> Result<HookHandlerConfig, String> {
    let mut fields = hook_mapping(node, "handler")?;
    let handler_type = hook_required_string(&mut fields, "type")?;
    match handler_type.as_str() {
        "command" => {
            reject_unknown_hook_fields(&fields, &["command", "args", "shell", "timeout"])?;
            let args = hook_optional_strings(&mut fields, "args")?;
            let shell = hook_optional_string(&mut fields, "shell")?;
            if args.is_some() && shell.is_some() {
                return Err(
                    "skill hook command fields `args` and `shell` are mutually exclusive"
                        .to_owned(),
                );
            }
            if let Some(shell) = shell.as_deref()
                && !matches!(shell, "bash" | "powershell")
            {
                return Err(
                    "skill hook field `shell` must be exactly `bash` or `powershell`".to_owned(),
                );
            }
            Ok(HookHandlerConfig::Command {
                command: hook_required_string(&mut fields, "command")?,
                args,
                shell,
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "http" => {
            reject_unknown_hook_fields(
                &fields,
                &["url", "headers", "allowed_env_vars", "timeout"],
            )?;
            Ok(HookHandlerConfig::Http {
                url: hook_required_string(&mut fields, "url")?,
                headers: hook_optional_string_map(&mut fields, "headers")?.unwrap_or_default(),
                allowed_env_vars: hook_optional_strings(&mut fields, "allowed_env_vars")?,
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "mcp_tool" => {
            reject_unknown_hook_fields(&fields, &["server", "tool", "input", "timeout"])?;
            let input = fields
                .remove("input")
                .ok_or_else(|| "skill hook is missing required field `input`".to_owned())?;
            let input = hook_json_object(input, "input")?;
            Ok(HookHandlerConfig::McpTool {
                server: hook_required_string(&mut fields, "server")?,
                tool: hook_required_string(&mut fields, "tool")?,
                input,
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        "prompt" => {
            reject_unknown_hook_fields(&fields, &["prompt", "model", "timeout"])?;
            Ok(HookHandlerConfig::Prompt {
                prompt: hook_required_string(&mut fields, "prompt")?,
                model: hook_optional_string(&mut fields, "model")?,
                timeout_seconds: hook_optional_timeout(&mut fields)?,
            })
        }
        _ => Err(format!("unsupported hook handler `{handler_type}`")),
    }
}

fn hook_mapping(node: Node, field: &str) -> Result<BTreeMap<String, Node>, String> {
    let Node::Mapping(entries) = node else {
        return Err(format!("skill hook field `{field}` must be a mapping"));
    };
    let mut mapped = BTreeMap::new();
    for (key, value) in entries {
        let Node::Scalar { value: key, .. } = key else {
            return Err(format!("skill hook field `{field}` has a non-string key"));
        };
        if mapped.insert(key.clone(), value).is_some() {
            return Err(format!(
                "skill hook field `{field}` contains duplicate `{key}`"
            ));
        }
    }
    Ok(mapped)
}

fn hook_json_object(node: Node, field: &str) -> Result<serde_json::Value, String> {
    let value = hook_json_value(node, field)?;
    if !value.is_object() {
        return Err(format!("skill hook field `{field}` must be an object"));
    }
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| format!("skill hook field `{field}` is not JSON: {error}"))?;
    if bytes.len() > 64 * 1024 {
        return Err(format!(
            "skill hook field `{field}` exceeds 65536 encoded bytes"
        ));
    }
    Ok(value)
}

fn hook_json_value(node: Node, field: &str) -> Result<serde_json::Value, String> {
    match node {
        Node::Scalar { value, plain: true } => match value.as_str() {
            "null" => Ok(serde_json::Value::Null),
            "true" => Ok(serde_json::Value::Bool(true)),
            "false" => Ok(serde_json::Value::Bool(false)),
            _ => match serde_json::from_str::<serde_json::Value>(&value) {
                Ok(value @ serde_json::Value::Number(_)) => Ok(value),
                _ => Ok(serde_json::Value::String(value)),
            },
        },
        Node::Scalar {
            value,
            plain: false,
        } => Ok(serde_json::Value::String(value)),
        Node::Sequence(values) => values
            .into_iter()
            .map(|value| hook_json_value(value, field))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        Node::Mapping(entries) => {
            let mut object = serde_json::Map::new();
            for (key, value) in entries {
                let Some(key) = yaml_envelope::scalar(key) else {
                    return Err(format!(
                        "skill hook field `{field}` object keys must be non-empty strings"
                    ));
                };
                let value = hook_json_value(value, field)?;
                if object.insert(key.clone(), value).is_some() {
                    return Err(format!(
                        "skill hook field `{field}` contains duplicate object key `{key}`"
                    ));
                }
            }
            Ok(serde_json::Value::Object(object))
        }
    }
}

fn reject_unknown_hook_fields(
    fields: &BTreeMap<String, Node>,
    allowed: &[&str],
) -> Result<(), String> {
    if let Some(field) = fields
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("unsupported skill hook field `{field}`"));
    }
    Ok(())
}

fn hook_required_string(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<String, String> {
    hook_optional_string(fields, field)?
        .ok_or_else(|| format!("skill hook is missing required field `{field}`"))
}

fn hook_optional_string(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<String>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    yaml_envelope::scalar(value)
        .map(Some)
        .ok_or_else(|| format!("skill hook field `{field}` must be a non-empty string"))
}

fn hook_optional_bool(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    match value {
        Node::Scalar { value, plain: true } if value == "true" => Ok(Some(true)),
        Node::Scalar { value, plain: true } if value == "false" => Ok(Some(false)),
        _ => Err(format!("skill hook field `{field}` must be a boolean")),
    }
}

fn hook_optional_strings(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    let Node::Sequence(values) = value else {
        return Err(format!(
            "skill hook field `{field}` must be a string sequence"
        ));
    };
    values
        .into_iter()
        .map(|value| {
            yaml_envelope::scalar(value)
                .ok_or_else(|| format!("skill hook field `{field}` must contain non-empty strings"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn hook_optional_string_map(
    fields: &mut BTreeMap<String, Node>,
    field: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(value) = fields.remove(field) else {
        return Ok(None);
    };
    let Node::Mapping(values) = value else {
        return Err(format!(
            "skill hook field `{field}` must be a string mapping"
        ));
    };
    values
        .into_iter()
        .map(|(name, value)| {
            let name = yaml_envelope::scalar(name)
                .ok_or_else(|| format!("skill hook field `{field}` has a non-string name"))?;
            let value = yaml_envelope::scalar(value)
                .ok_or_else(|| format!("skill hook field `{field}` has a non-string value"))?;
            Ok((name, value))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn hook_optional_timeout(fields: &mut BTreeMap<String, Node>) -> Result<Option<u64>, String> {
    let Some(value) = fields.remove("timeout") else {
        return Ok(None);
    };
    let Node::Scalar { value, plain: true } = value else {
        return Err("skill hook field `timeout` must be an integer".to_owned());
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "skill hook field `timeout` must be an integer".to_owned())?;
    if !(1..=3600).contains(&seconds) {
        return Err("skill hook field `timeout` must be between 1 and 3600 seconds".to_owned());
    }
    Ok(Some(seconds))
}

#[cfg(test)]
mod tests;
