//! Precedence resolution and the trust gate. Pure functions over injected
//! sources — no file I/O (`plans/m0a-contracts.md` Step 12).

use super::error::ConfigError;
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::layer::{
    AuthProviderLayer, ConfigLayer, McpLayerEntry, McpServerConfig, McpServerName, Mode, ModelRef,
    UiLayer,
};
use super::ruleset::{OriginSet, Ruleset};
use super::source::{Source, Sourced, Trust};
use crate::ceiling::{Ceilings, Clamped};
use crate::generator::types::ThinkingLevel;
use crate::permit::Confinement;
use crate::permit::rule::Rule;

/// Ambient-loading posture (`CLI.md` §4.5): whether project-local config,
/// hooks, skills and MCP servers may load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    Bare,
    Ambient,
}

/// Safe provenance of one explicitly selected profile after the trust gate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActiveProfileProvenance {
    pub name: String,
    pub selected_by: Source,
    pub definitions: OriginSet,
}

/// The exact authority that supplied a winning MCP entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConfigProvenance {
    Layer { source: Source },
    Profile { source: Source, name: String },
}

/// One resolved MCP entry. Tombstones remain visible to management while
/// configured-only consumers receive no server from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedMcpEntry {
    Configured {
        server: McpServerConfig,
        provenance: McpConfigProvenance,
        /// False when the entry says `enabled = false`: configured, but
        /// attached only by an explicit selection.
        enabled: bool,
    },
    Tombstone {
        provenance: McpConfigProvenance,
    },
}

/// Complete effective MCP configuration after profile, precedence, and trust.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedMcpConfig {
    entries: BTreeMap<McpServerName, ResolvedMcpEntry>,
}

impl ResolvedMcpConfig {
    pub fn entry(&self, name: &McpServerName) -> Option<&ResolvedMcpEntry> {
        self.entries.get(name)
    }

    pub fn configured(&self, name: &McpServerName) -> Option<&McpServerConfig> {
        match self.entry(name) {
            Some(ResolvedMcpEntry::Configured { server, .. }) => Some(server),
            Some(ResolvedMcpEntry::Tombstone { .. }) | None => None,
        }
    }

    pub fn entries(&self) -> &BTreeMap<McpServerName, ResolvedMcpEntry> {
        &self.entries
    }
}

/// The fully resolved config: one layer's worth of values, after precedence
/// and the trust gate have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub max_output_tokens: Option<std::num::NonZeroU64>,
    /// Fields reduced by an explicitly selected host profile ceiling.
    pub profile_warnings: Vec<String>,
    pub cache: crate::generator::capabilities::CacheRetention,
    pub model: Option<ModelRef>,
    pub thinking: Option<ThinkingLevel>,
    pub mode: Option<Mode>,
    /// What decided `mode`.
    pub mode_provenance: super::SettingProvenance,
    /// The system maximum remains available to implicit interactive defaults.
    pub max_mode: Option<Mode>,
    /// Each system declaration must contain every admitted extra root.
    pub root_ceilings: Vec<Vec<crate::permit::RootCfg>>,
    pub sandbox: Option<Confinement>,
    pub exec: super::ExecLayer,
    pub network: super::Network,
    pub orchestration: crate::orchestrator::Mode,
    /// What decided `sandbox`.
    pub sandbox_provenance: super::SettingProvenance,
    /// Winning layer for the active profile roots.
    pub roots_source: Option<Source>,
    /// Additional user-selected read and executable roots, before canonicalization.
    pub exec_paths: Vec<PathBuf>,
    pub permissions: Ruleset,
    pub judge: Option<ModelRef>,
    pub posture: Posture,
    /// The selected profile and every eligible layer that defined it.
    pub active_profile: Option<ActiveProfileProvenance>,
    /// CI profile roots retain the layer that won per-field precedence.
    pub session_dir: Option<Sourced<PathBuf>>,
    pub artifact_dir: Option<Sourced<PathBuf>>,
    /// Extra roots the active profile opens, before resolution.
    pub roots: Vec<crate::permit::RootCfg>,
    /// What this run may spend, after precedence and the system clamp.
    ///
    /// `2026-08-21-a-run-with-no-ceiling.md` §4: both fields are `None`
    /// unless somebody wrote a ceiling, because ego imposes none of its own.
    pub ceilings: Ceilings,
    /// What the system clamp reduced, per field, so the run can say so.
    ///
    /// Separate from `ceilings` rather than folded into it: what is in force
    /// and what was asked for are two numbers, and a single field would have
    /// to be one of them.
    pub clamped: Clamped,
    /// Provider login-command and identity-header configuration, after layer
    /// precedence. Values are non-secret.
    pub auth: std::collections::HashMap<String, AuthProviderLayer>,
    /// Interactive presentation fields after per-field precedence.
    pub ui: UiLayer,
    /// Effective configured entries and winning tombstones with provenance.
    pub mcp: ResolvedMcpConfig,
    /// Optional user-configured mdkb prompt recall executable.
    pub mdkb_prompt_command: Option<PathBuf>,
    /// What the trust gate threw away, so a run can say so.
    ///
    /// Dropping a Project or Local layer under [`Posture::Bare`] is correct
    /// and this does not change it. Dropping it *in silence* is what made an
    /// operator's `sandbox` key in a workspace `.ego/config.local.toml` look
    /// applied when nobody had read it — the configuration was not overridden,
    /// it was discarded, and no surface anywhere said which. Empty when the
    /// posture is ambient, and empty when the discarded layers set nothing.
    pub discarded: Vec<Discarded>,
}

/// One layer the trust gate discarded, and the settings it was carrying.
///
/// The settings are named rather than counted. "Two settings were dropped"
/// sends somebody reading a file to find out which; naming them is the whole
/// value of saying anything at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discarded {
    pub source: Source,
    pub settings: Vec<&'static str>,
}

impl std::fmt::Display for Discarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} configuration set {}, and none of it was applied: \
             ambient project configuration is disabled by the bare posture",
            self.source,
            join(&self.settings)
        )
    }
}

/// Every setting this layer actually carries, in the words its file spells.
///
/// An exhaustive match rather than a list of the interesting ones: a field
/// added to [`ConfigLayer`] and forgotten here would be discarded as silently
/// as `sandbox` was, which is the failure this whole mechanism exists to stop.
/// `bare` is deliberately absent — it is read from every layer regardless of
/// eligibility, so it is never discarded and saying it was would be false.
fn settings_in(layer: &ConfigLayer) -> Vec<&'static str> {
    let ConfigLayer {
        max_output_tokens,
        cache,
        model,
        thinking,
        mode,
        max_mode,
        roots,
        sandbox,
        exec,
        network,
        orchestration,
        exec_paths,
        permissions,
        budget,
        bare: _,
        profile,
        auth,
        ui,
        mcp,
        mdkb_prompt_command,
        hooks,
    } = layer;
    let mut settings = Vec::new();
    for (present, name) in [
        (max_output_tokens.is_some(), "max_output_tokens"),
        (cache.is_some(), "cache"),
        (model.is_some(), "model"),
        (thinking.is_some(), "thinking"),
        (mode.is_some(), "mode"),
        (max_mode.is_some(), "max_mode"),
        (roots.is_some(), "roots"),
        (sandbox.is_some(), "sandbox"),
        (exec.is_some(), "exec"),
        (network.is_some(), "network"),
        (orchestration.is_some(), "orchestration"),
        (exec_paths.is_some(), "exec_paths"),
        (permissions.is_some(), "permissions"),
        (budget.is_some(), "budget"),
        (!profile.is_empty(), "profile tables"),
        (!auth.is_empty(), "auth"),
        (ui.is_some(), "ui"),
        (!mcp.servers.is_empty(), "mcp servers"),
        (mdkb_prompt_command.is_some(), "mdkb_prompt_command"),
        (!hooks.is_empty(), "hooks"),
    ] {
        if present {
            settings.push(name);
        }
    }
    settings
}

/// `a`, `a and b`, `a, b and c` — a list a person reads rather than a debug
/// rendering of a `Vec`.
fn join(settings: &[&'static str]) -> String {
    match settings {
        [] => "nothing".to_owned(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Resolves layered config into one [`Effective`] value.
///
/// Precedence is `flags > env > local > project > user > system`,
/// independent of `layers`' input order. Within a layer, an active
/// `[profile.NAME]` table (if the layer defines one under `active_profile`)
/// overrides that same layer's own fields before cross-layer precedence is
/// applied — a profile changes what its own layer contributes, it does not
/// change that layer's rank.
///
/// **Trust gate** (`CLI.md` §4.5): ambient discovery is trust-gated, and
/// that includes project-local *config*, not just hooks/skills/MCP. When the
/// resolved posture is [`Posture::Bare`] — because the workspace is
/// [`Trust::Untrusted`], or because any layer's `bare` flag forced it even
/// while trusted — the `project` and `local` layers are dropped entirely
/// before anything else is computed: they cannot affect `model`, `mode`,
/// `permissions` or which `[profile.NAME]` tables are available. `bare`
/// itself is read from every layer regardless of eligibility, since asking
/// for *more* restriction from an otherwise-excluded source is never a
/// security problem.
///
/// **System ceiling** (`ARCHITECTURE.md` §3.4): a `deny` entry from any
/// `system`-sourced layer — its own base `permissions.deny`, and its active
/// `[profile.NAME]` override's `permissions.deny` if one is selected — is
/// collected into a ceiling *before* the ordinary precedence merge runs, and
/// re-applied after. No other layer's `allow`, and no profile override that
/// merely omits the entry, can pull it back out. Multiple `system` layers
/// union rather than overwrite. Deny entries from any other source are
/// ordinary config and can be relaxed by a higher-precedence layer.
///
/// Returns [`ConfigError::UnknownProfile`] if `active_profile` is given but
/// no eligible layer defines that profile — missing from one layer is
/// normal, missing from all of them is a materially different
/// configuration and must not run silently.
pub fn resolve(
    layers: &[Sourced<ConfigLayer>],
    trust: Trust,
    active_profile: Option<&str>,
) -> Result<Effective, ConfigError> {
    resolve_inner(layers, trust, active_profile, None, None)
}

/// Resolve one process-captured profile selection without rereading its
/// environment or guessing its source.
pub fn resolve_selected(
    layers: &[Sourced<ConfigLayer>],
    trust: Trust,
    active_profile: Option<(&str, Source)>,
) -> Result<Effective, ConfigError> {
    resolve_inner(
        layers,
        trust,
        active_profile.map(|(name, _)| name),
        active_profile.map(|(_, source)| source),
        None,
    )
}

/// Resolve a selected profile under an optional host-owned profile ceiling.
/// The host clamp precedes the administrator checks; system refusals remain unchanged.
pub fn resolve_with_profile_ceiling(
    layers: &[Sourced<ConfigLayer>],
    trust: Trust,
    active_profile: Option<(&str, Source)>,
    ceiling: Option<&Effective>,
) -> Result<Effective, ConfigError> {
    resolve_inner(
        layers,
        trust,
        active_profile.map(|(name, _)| name),
        active_profile.map(|(_, source)| source),
        ceiling,
    )
}

fn resolve_inner(
    layers: &[Sourced<ConfigLayer>],
    trust: Trust,
    active_profile: Option<&str>,
    selected_by: Option<Source>,
    profile_ceiling: Option<&Effective>,
) -> Result<Effective, ConfigError> {
    let mut ordered: Vec<&Sourced<ConfigLayer>> = layers.iter().collect();
    ordered.sort_by_key(|l| l.source.rank());

    let mut bare = None;
    for sourced in &ordered {
        if let Some(b) = sourced.value.bare {
            bare = Some(b);
        }
    }
    let posture = if trust == Trust::Untrusted || bare == Some(true) {
        Posture::Bare
    } else {
        Posture::Ambient
    };

    let mut discarded = Vec::new();
    let eligible: Vec<&Sourced<ConfigLayer>> = ordered
        .into_iter()
        .filter(|l| {
            let keep =
                posture == Posture::Ambient || !matches!(l.source, Source::Project | Source::Local);
            // Recorded on the way past, where the reason for the drop is still
            // in hand. A later pass would have to reconstruct which layers
            // were eligible, and could reconstruct it wrongly.
            if !keep {
                let settings = settings_in(&l.value);
                if !settings.is_empty() {
                    discarded.push(Discarded {
                        source: l.source,
                        settings,
                    });
                }
            }
            keep
        })
        .collect();

    let active_profile_name = active_profile;
    let resolved_active_profile =
        resolve_active_profile(&eligible, active_profile_name, selected_by)?;
    let mut max_output_tokens = None;
    let mut cache = crate::generator::capabilities::CacheRetention::Short;
    let mut model = None;
    let mut thinking = None;
    let mut mode = None;
    let mut sandbox = None;
    let mut exec = super::ExecLayer::default();
    let mut network = super::Network::default();
    let mut orchestration = crate::orchestrator::Mode::Observe;
    let mut mode_provenance = super::SettingProvenance::Default;
    let mut sandbox_provenance = super::SettingProvenance::Default;
    let mut roots_source = None;
    let mut exec_paths = Vec::new();
    let mut permissions = Ruleset::default();
    let mut judge = None;
    let mut ceilings = Ceilings::default();
    let mut auth = std::collections::HashMap::new();
    let mut ui = UiLayer::default();
    let mut mcp = ResolvedMcpConfig::default();
    let mut mdkb_prompt_command = None;
    let mut session_dir = None;
    let mut artifact_dir = None;
    let mut roots = Vec::new();
    let mut root_restrictions = Vec::new();

    for sourced in &eligible {
        if sourced.source == Source::User {
            mdkb_prompt_command = sourced.value.mdkb_prompt_command.clone();
        }
        let profile = active_profile_name.and_then(|name| sourced.value.profile.get(name));

        apply_mcp_entries(
            &mut mcp,
            &sourced.value.mcp.servers,
            McpConfigProvenance::Layer {
                source: sourced.source,
            },
        );
        if let (Some(profile), Some(name)) = (profile, active_profile_name) {
            apply_mcp_entries(
                &mut mcp,
                &profile.mcp.servers,
                McpConfigProvenance::Profile {
                    source: sourced.source,
                    name: name.to_owned(),
                },
            );
        }

        merge_generation_fields(
            sourced,
            profile,
            &mut max_output_tokens,
            &mut cache,
            &mut model,
            &mut thinking,
        );
        merge_confinement(
            sourced,
            profile,
            active_profile_name.map(|name| (name, selected_by)),
            &mut mode,
            &mut mode_provenance,
            &mut sandbox,
            &mut sandbox_provenance,
        );
        merge_exec_controls(&mut exec, sourced, profile);
        if let Some(value) = profile
            .and_then(|profile| profile.network)
            .or(sourced.value.network)
        {
            network = value;
        }
        if let Some(config) = profile
            .and_then(|p| p.orchestration)
            .or(sourced.value.orchestration)
        {
            orchestration = config.mode;
        }
        if let Some(paths) = profile
            .and_then(|profile| profile.exec_paths.as_ref())
            .or(sourced.value.exec_paths.as_ref())
        {
            exec_paths = paths.clone();
        }
        if let Some(perm) = profile
            .and_then(|p| p.permissions.clone())
            .or_else(|| sourced.value.permissions.clone())
        {
            judge = perm.judge.clone();
            // Still a replacement, not a merge: a layer that writes
            // `permissions` states the whole block. What changed is that the
            // rules now carry which layer stated them, so a later attribution
            // is reading a recorded fact instead of guessing from list
            // membership. A profile's permissions come from this same layer,
            // so the origin is the same either way.
            permissions = Ruleset::from_layer(perm, sourced.source);
        }
        if let Some(budget) = profile
            .and_then(|p| p.budget.as_ref())
            .or(sourced.value.budget.as_ref())
        {
            // Per field, so a layer naming a token ceiling does not erase a
            // money ceiling a lower one set. They are independent
            // instruments — §9's token ceiling needs no rates and binds on
            // every metering class — and merging them into one slot would
            // make naming either a way to drop the other.
            ceilings.apply(budget);
        }
        if let Some(path) = profile.and_then(|profile| profile.session_dir.clone()) {
            session_dir = Some(Sourced {
                source: sourced.source,
                value: path,
            });
        }
        if let Some(selected) = profile
            .and_then(|p| p.roots.as_ref())
            .or(sourced.value.roots.as_ref())
        {
            select_roots(
                sourced.source,
                selected,
                sandbox,
                &mut roots,
                &mut roots_source,
                &mut root_restrictions,
            );
        }
        if let Some(path) = profile.and_then(|profile| profile.artifact_dir.clone()) {
            artifact_dir = Some(Sourced {
                source: sourced.source,
                value: path,
            });
        }
        merge_auth(&mut auth, &sourced.value.auth);
        if let Some(template) = sourced
            .value
            .ui
            .as_ref()
            .and_then(|ui| ui.statusline.as_ref())
            .and_then(|statusline| statusline.template.clone())
        {
            ui.statusline.get_or_insert_default().template = Some(template);
        }
        // Installing executable status producers is operator authority, not project content.
        if matches!(
            sourced.source,
            Source::System | Source::User | Source::Flags
        ) && let Some(providers) = sourced
            .value
            .ui
            .as_ref()
            .and_then(|ui| ui.statusline.as_ref())
            .and_then(|line| line.providers.clone())
        {
            ui.statusline.get_or_insert_default().providers = Some(providers);
        }
    }

    // The ceiling is gathered independently of the ordinary merge above, so
    // a `[profile.NAME]` override that clears `deny` — or a later layer
    // that overwrites it — can never make a system-layer deny disappear.
    let system_deny = system_denies(&eligible, active_profile_name);
    // A **union of origins, not a skip**. This loop used to read
    //
    // ```rust
    // if !deny.contains(&entry) { deny.push(entry); }
    // ```
    //
    // which is correct about enforcement — one deny is one entry — and threw
    // away the only fact an attribution needs. A predicate denied in both
    // `/etc/ego/config.toml` and the operator's own config arrived here
    // already present, was skipped, and kept whichever origin the merge loop
    // happened to leave: the *user* copy, the one whose deletion changes
    // nothing. See
    // `docs/architecture.md`.
    for entry in system_deny {
        Ruleset::merge(&mut permissions.deny, entry, Source::System);
    }
    // Both lists are carried to the engine intact. Precedence between an
    // allow and a deny is decided **per call**, by matching each against the
    // canonical `PreparedCall` — never here, by comparing two lists.
    //
    // This function used to end with
    //
    // ```rust
    // allow.retain(|entry| !deny.contains(entry));
    // ```
    //
    // which spelled "deny beats allow" as *string equality between two
    // lists*: `deny = ["exec(git *)"]` left `allow = ["exec(git push)"]`
    // standing, because the strings differ, and the resolved config then said
    // the push was allowed. No better comparison rescues it — those two rules
    // are neither equal nor unrelated, and the only place their relationship
    // is decidable is against an actual call. See
    // `docs/architecture.md` §6 and
    // `docs/architecture.md` §7.
    //
    // The system-deny ceiling above is a *different* job and stays: it
    // guarantees a `/etc/ego` entry is present in the list at all, which no
    // per-call evaluation can supply if the merge dropped it.

    // A repository replacement must not inherit an operator's blanket grant.
    // Keep denies intact: losing server authority cannot relax a restriction.
    permissions.allow.retain(|entry| {
        let Rule::Tool { server, .. } = entry.rule() else {
            return true;
        };
        if !entry.rule().is_tool_wildcard() {
            return true;
        }
        let Ok(name) = server.parse::<McpServerName>() else {
            return false;
        };
        matches!(
            mcp.entry(&name),
            Some(ResolvedMcpEntry::Configured {
                provenance: McpConfigProvenance::Layer {
                    source: Source::User
                } | McpConfigProvenance::Profile {
                    source: Source::System | Source::User,
                    ..
                },
                ..
            })
        )
    });

    // The same construction one field over: a `system` ceiling is gathered
    // outside the ordinary merge and re-applied after it, so no
    // higher-precedence layer and no profile override can raise it. `min`
    // rather than "system wins", because `CLI.md:311` binds everyone to the
    // administrator's cap and not to the administrator's *figure* — an
    // operator asking for less keeps their own.
    let system_ceiling = system_budget_ceiling(&eligible, active_profile_name);
    let clamped = ceilings.clamp(&system_ceiling);

    let mut profile_warnings = Vec::new();
    if let Some(ceiling) = profile_ceiling {
        let maximum = ceiling.mode.unwrap_or(Mode::Default);
        let selected = mode.unwrap_or(Mode::Default);
        if !selected.within(maximum) {
            profile_warnings.push(format!(
                "mode {selected} exceeds profile ceiling {maximum}; using {maximum}"
            ));
            mode = Some(maximum);
            mode_provenance = super::SettingProvenance::ProfileCeiling;
        }
        let maximum = ceiling.sandbox.unwrap_or_default();
        let selected = sandbox.unwrap_or_default();
        if !selected.within(maximum) {
            profile_warnings.push(format!(
                "sandbox {selected} exceeds profile ceiling {maximum}; using {maximum}"
            ));
            sandbox = Some(maximum);
            sandbox_provenance = super::SettingProvenance::ProfileCeiling;
        }
    }

    let (mut max_mode, root_ceilings) = system_authority_ceilings(
        &eligible,
        active_profile_name,
        mode,
        sandbox,
        root_restrictions,
    )?;

    if let Some(ceiling) = profile_ceiling {
        let maximum = ceiling.mode.unwrap_or(Mode::Default);
        if max_mode.is_none_or(|system| maximum.within(system)) {
            max_mode = Some(maximum);
        }
    }

    if let Some(limit) = profile_ceiling.and_then(|ceiling| ceiling.max_output_tokens) {
        max_output_tokens = Some(max_output_tokens.map_or(limit, |value| value.min(limit)));
    }
    // System output limits cannot be relaxed by lower layers.
    for layer in layers.iter().filter(|layer| layer.source == Source::System) {
        if let Some(limit) = layer.value.max_output_tokens {
            max_output_tokens = Some(max_output_tokens.map_or(limit, |value| value.min(limit)));
        }
    }
    Ok(Effective {
        max_output_tokens,
        profile_warnings,
        cache,
        model,
        thinking,
        mode,
        mode_provenance,
        max_mode,
        root_ceilings,
        sandbox,
        exec,
        network,
        sandbox_provenance,
        roots_source,
        orchestration,
        exec_paths,
        permissions: permissions.with_credential_defaults(),
        judge,
        posture,
        active_profile: resolved_active_profile,
        session_dir,
        artifact_dir,
        roots,
        ceilings,
        clamped,
        auth,
        ui,
        mcp,
        mdkb_prompt_command,
        discarded,
    })
}

fn system_budget_ceiling(
    eligible: &[&Sourced<ConfigLayer>],
    active_profile_name: Option<&str>,
) -> Ceilings {
    let mut system_ceiling = Ceilings::default();
    for sourced in eligible.iter().filter(|l| l.source == Source::System) {
        if let Some(budget) = &sourced.value.budget {
            system_ceiling.apply(budget);
        }
        if let Some(budget) = active_profile_name
            .and_then(|name| sourced.value.profile.get(name))
            .and_then(|profile| profile.budget.as_ref())
        {
            system_ceiling.apply(budget);
        }
    }
    system_ceiling
}

fn select_roots(
    source: Source,
    selected: &[crate::permit::RootCfg],
    sandbox: Option<Confinement>,
    roots: &mut Vec<crate::permit::RootCfg>,
    roots_source: &mut Option<Source>,
    root_restrictions: &mut Vec<Vec<crate::permit::RootCfg>>,
) {
    if matches!(source, Source::Project | Source::Local) {
        let inherited =
            if roots_source.is_none() && sandbox.unwrap_or_default() != Confinement::Adaptive {
                vec![crate::permit::RootCfg {
                    path: "~/Gits".into(),
                    access: crate::permit::RootAccess::ReadWrite,
                }]
            } else {
                roots.clone()
            };
        root_restrictions.push(inherited);
    }
    *roots = selected.to_vec();
    *roots_source = Some(source);
}

fn merge_confinement(
    sourced: &Sourced<ConfigLayer>,
    profile: Option<&super::layer::ProfileLayer>,
    profile_origin: Option<(&str, Option<Source>)>,
    mode: &mut Option<Mode>,
    mode_provenance: &mut super::SettingProvenance,
    sandbox: &mut Option<Confinement>,
    sandbox_provenance: &mut super::SettingProvenance,
) {
    if let Some(m) = profile.and_then(|p| p.mode) {
        *mode = Some(m);
        *mode_provenance = super::SettingProvenance::layer(sourced.source, profile_origin);
    } else if let Some(m) = sourced.value.mode {
        *mode = Some(m);
        *mode_provenance = super::SettingProvenance::layer(sourced.source, None);
    }
    if let Some(value) = profile.and_then(|profile| profile.sandbox) {
        *sandbox = Some(value);
        *sandbox_provenance = super::SettingProvenance::layer(sourced.source, profile_origin);
    } else if let Some(value) = sourced.value.sandbox {
        *sandbox = Some(value);
        *sandbox_provenance = super::SettingProvenance::layer(sourced.source, None);
    }
}

fn merge_generation_fields(
    sourced: &Sourced<ConfigLayer>,
    profile: Option<&super::layer::ProfileLayer>,
    max_output_tokens: &mut Option<std::num::NonZeroU64>,
    cache: &mut crate::generator::capabilities::CacheRetention,
    model: &mut Option<ModelRef>,
    thinking: &mut Option<ThinkingLevel>,
) {
    if let Some(value) = profile
        .and_then(|p| p.max_output_tokens)
        .or(sourced.value.max_output_tokens)
    {
        *max_output_tokens = Some(value);
    }
    if let Some(value) = sourced.value.cache
        && (value != crate::generator::capabilities::CacheRetention::Long
            || sourced.source == Source::User)
    {
        *cache = value;
    }
    if let Some(m) = profile
        .and_then(|p| p.model.clone())
        .or_else(|| sourced.value.model.clone())
    {
        *model = Some(m);
    }
    if let Some(value) = profile
        .and_then(|profile| profile.thinking)
        .or(sourced.value.thinking)
    {
        *thinking = Some(value);
    }
}

fn resolve_active_profile(
    eligible: &[&Sourced<ConfigLayer>],
    active_profile_name: Option<&str>,
    selected_by: Option<Source>,
) -> Result<Option<ActiveProfileProvenance>, ConfigError> {
    let resolved = if let Some(name) = active_profile_name {
        let mut definitions = OriginSet::default();
        for layer in eligible {
            if layer.value.profile.contains_key(name) {
                definitions.insert(layer.source);
            }
        }
        if definitions.is_empty() {
            return Err(ConfigError::UnknownProfile {
                name: name.to_string(),
            });
        }
        selected_by.map(|selected_by| ActiveProfileProvenance {
            name: name.to_owned(),
            selected_by,
            definitions,
        })
    } else {
        None
    };
    Ok(resolved)
}

fn system_denies(
    eligible: &[&Sourced<ConfigLayer>],
    active_profile_name: Option<&str>,
) -> Vec<Rule> {
    let mut system_deny: Vec<Rule> = Vec::new();
    for sourced in eligible.iter().filter(|l| l.source == Source::System) {
        if let Some(perm) = &sourced.value.permissions {
            for entry in &perm.deny {
                if !system_deny.contains(entry) {
                    system_deny.push(entry.clone());
                }
            }
        }
        if let Some(profile) = active_profile_name.and_then(|name| sourced.value.profile.get(name))
            && let Some(perm) = &profile.permissions
        {
            for entry in &perm.deny {
                if !system_deny.contains(entry) {
                    system_deny.push(entry.clone());
                }
            }
        }
    }
    system_deny
}

fn system_authority_ceilings(
    eligible: &[&Sourced<ConfigLayer>],
    active_profile_name: Option<&str>,
    mode: Option<Mode>,
    sandbox: Option<Confinement>,
    root_restrictions: Vec<Vec<crate::permit::RootCfg>>,
) -> Result<(Option<Mode>, Vec<Vec<crate::permit::RootCfg>>), ConfigError> {
    let mut max_mode: Option<Mode> = None;
    let mut root_ceilings = root_restrictions;
    for sourced in eligible
        .iter()
        .filter(|layer| layer.source == Source::System)
    {
        if let Some(maximum) = sourced.value.max_mode
            && max_mode.is_none_or(|current| maximum.within(current))
        {
            max_mode = Some(maximum);
        }
        let profile = active_profile_name.and_then(|name| sourced.value.profile.get(name));
        for maximum in sourced
            .value
            .sandbox
            .iter()
            .chain(profile.and_then(|profile| profile.sandbox.as_ref()))
        {
            if !sandbox.unwrap_or_default().within(*maximum) {
                return Err(ConfigError::Invalid {
                    source: Source::System,
                    message: format!(
                        "sandbox {} exceeds the system ceiling {maximum}",
                        sandbox.unwrap_or_default()
                    ),
                });
            }
        }
        if let Some(bounds) = &sourced.value.roots {
            root_ceilings.push(bounds.clone());
        }
        if let Some(bounds) = profile.and_then(|p| p.roots.as_ref()) {
            root_ceilings.push(bounds.clone());
        }
    }
    if let Some(maximum) = max_mode {
        let selected = mode.unwrap_or(Mode::Default);
        if !selected.within(maximum) {
            return Err(ConfigError::Invalid {
                source: Source::System,
                message: format!("mode {selected} exceeds system max_mode {maximum}"),
            });
        }
    }

    Ok((max_mode, root_ceilings))
}

fn merge_exec_controls(
    exec: &mut super::ExecLayer,
    sourced: &Sourced<ConfigLayer>,
    profile: Option<&super::layer::ProfileLayer>,
) {
    for controls in sourced
        .value
        .exec
        .iter()
        .chain(profile.and_then(|profile| profile.exec.as_ref()))
    {
        if controls.env.is_some() {
            exec.env = controls.env;
        }
        if controls.env_allowlist.is_some() {
            exec.env_allowlist = controls.env_allowlist.clone();
        }
    }
}

fn merge_auth(
    auth: &mut std::collections::HashMap<String, AuthProviderLayer>,
    providers: &std::collections::HashMap<String, AuthProviderLayer>,
) {
    for (id, provider) in providers {
        let resolved = auth.entry(id.clone()).or_default();
        if provider.credential_command.is_some() {
            resolved.credential_command = provider.credential_command.clone();
        }
        for (name, value) in &provider.headers {
            resolved
                .headers
                .retain(|existing, _| !existing.eq_ignore_ascii_case(name));
            resolved.headers.insert(name.clone(), value.clone());
        }
    }
}

fn apply_mcp_entries(
    resolved: &mut ResolvedMcpConfig,
    entries: &BTreeMap<McpServerName, McpLayerEntry>,
    provenance: McpConfigProvenance,
) {
    for (name, entry) in entries {
        let entry = match entry {
            McpLayerEntry::Server(server) | McpLayerEntry::Disabled(server) => {
                ResolvedMcpEntry::Configured {
                    server: server.clone(),
                    provenance: provenance.clone(),
                    enabled: matches!(entry, McpLayerEntry::Server(_)),
                }
            }
            McpLayerEntry::Tombstone => ResolvedMcpEntry::Tombstone {
                provenance: provenance.clone(),
            },
        };
        resolved.entries.insert(name.clone(), entry);
    }
}
