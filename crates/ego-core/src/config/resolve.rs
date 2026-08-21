//! Precedence resolution and the trust gate. Pure functions over injected
//! sources — no file I/O (`plans/m0a-contracts.md` Step 12).

use super::error::ConfigError;
use super::layer::{ConfigLayer, Mode, ModelRef, PermCfg};
use super::source::{Source, Sourced, Trust};

/// Ambient-loading posture (`CLI.md` §4.5): whether project-local config,
/// hooks, skills and MCP servers may load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    Bare,
    Ambient,
}

/// The fully resolved config: one layer's worth of values, after precedence
/// and the trust gate have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub model: Option<ModelRef>,
    pub mode: Option<Mode>,
    pub permissions: PermCfg,
    pub posture: Posture,
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

    let eligible: Vec<&Sourced<ConfigLayer>> = ordered
        .into_iter()
        .filter(|l| {
            posture == Posture::Ambient || !matches!(l.source, Source::Project | Source::Local)
        })
        .collect();

    if let Some(name) = active_profile {
        let defined = eligible.iter().any(|l| l.value.profile.contains_key(name));
        if !defined {
            return Err(ConfigError::UnknownProfile {
                name: name.to_string(),
            });
        }
    }

    let mut model = None;
    let mut mode = None;
    let mut allow: Vec<String> = Vec::new();
    let mut deny: Vec<String> = Vec::new();

    for sourced in &eligible {
        let profile = active_profile.and_then(|name| sourced.value.profile.get(name));

        if let Some(m) = profile
            .and_then(|p| p.model.clone())
            .or_else(|| sourced.value.model.clone())
        {
            model = Some(m);
        }
        if let Some(m) = profile.and_then(|p| p.mode).or(sourced.value.mode) {
            mode = Some(m);
        }
        if let Some(perm) = profile
            .and_then(|p| p.permissions.clone())
            .or_else(|| sourced.value.permissions.clone())
        {
            allow = perm.allow;
            deny = perm.deny;
        }
    }

    // The ceiling is gathered independently of the ordinary merge above, so
    // a `[profile.NAME]` override that clears `deny` — or a later layer
    // that overwrites it — can never make a system-layer deny disappear.
    let mut system_deny: Vec<String> = Vec::new();
    for sourced in eligible.iter().filter(|l| l.source == Source::System) {
        if let Some(perm) = &sourced.value.permissions {
            for entry in &perm.deny {
                if !system_deny.contains(entry) {
                    system_deny.push(entry.clone());
                }
            }
        }
        if let Some(profile) = active_profile.and_then(|name| sourced.value.profile.get(name)) {
            if let Some(perm) = &profile.permissions {
                for entry in &perm.deny {
                    if !system_deny.contains(entry) {
                        system_deny.push(entry.clone());
                    }
                }
            }
        }
    }
    for entry in system_deny {
        if !deny.contains(&entry) {
            deny.push(entry);
        }
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
    // `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6 and
    // `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §7.
    //
    // The system-deny ceiling above is a *different* job and stays: it
    // guarantees a `/etc/ego` entry is present in the list at all, which no
    // per-call evaluation can supply if the merge dropped it.

    Ok(Effective {
        model,
        mode,
        permissions: PermCfg { allow, deny },
        posture,
    })
}
