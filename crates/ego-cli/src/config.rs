//! Where the config layers of `CLI.md` §7 live, and how the command line
//! becomes one of them.
//!
//! Only the *locations* are here. Parsing is `ConfigLayer::parse` and
//! precedence is `config::resolve`, both pure and both in `ego-core`; this
//! module knows which files to look in and what `EGO_HOME` means, which is
//! process knowledge and belongs to the process.
//!
//! Invalid config is never skipped (§4.5): an unknown key is a hard error
//! from the layer it was written in, with the key and the source named.

use std::path::{Path, PathBuf};

use ego_core::config::{
    AuthProviderLayer, ConfigError, ConfigLayer, Source, Sourced, Trust, resolve,
};

use crate::cli::{Args, UsageError};

/// The system layer: an administrator's fleet defaults and the only source of
/// non-relaxable ceilings.
const SYSTEM_CONFIG: &str = "/etc/ego/config.toml";
/// Overrides `~/.ego` for the user layer (`CLI.md` §7).
const EGO_HOME: &str = "EGO_HOME";
/// Environment overrides, between the flags and the project layers.
const ENV_MODEL: &str = "EGO_MODEL";
const ENV_PROFILE: &str = "EGO_PROFILE";

/// The user layer's directory: `$EGO_HOME`, else `~/.ego`.
pub fn ego_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var(EGO_HOME) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ego"))
}

/// Authentication configuration for `ego auth`.
///
/// The command has no workspace/trust flags, so only the system and user
/// layers are eligible. Parsing and precedence are nevertheless exactly the
/// same [`ConfigLayer`] and [`resolve`] path generation uses; authentication
/// must not grow a second interpretation of `config.toml`.
pub fn auth() -> Result<std::collections::HashMap<String, AuthProviderLayer>, UsageError> {
    let mut found = Vec::new();
    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    if let Some(home) = ego_home()
        && let Some(user) = layer(Source::User, &home.join("config.toml"))?
    {
        found.push(user);
    }
    resolve(&found, Trust::Untrusted, None)
        .map(|effective| effective.auth)
        .map_err(|err| UsageError(err.to_string()))
}

/// Read one layer, or nothing if the file is not there.
///
/// A file that exists and cannot be *read* is an error, not an absence: an
/// unreadable `/etc/ego/config.toml` may be the layer carrying the ceilings,
/// and running as though an administrator had configured nothing is the
/// failure mode §4.5 was written against.
fn layer(source: Source, path: &Path) -> Result<Option<Sourced<ConfigLayer>>, UsageError> {
    match std::fs::read_to_string(path) {
        Ok(text) => ConfigLayer::parse(source, &text)
            .map(Some)
            .map_err(|err: ConfigError| UsageError(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(UsageError(format!(
            "{} could not be read: {err}",
            path.display()
        ))),
    }
}

/// Every layer this invocation is subject to, in no particular order —
/// `resolve` sorts them by rank itself, and depending on this order would be
/// a second, silent precedence rule.
///
/// The project and local layers are gathered even when the posture will drop
/// them. That is not wasted work: `resolve` is what applies the trust gate,
/// and a caller that pre-filtered would be a second implementation of the gate
/// with no test holding it to the first.
pub fn layers(
    workspace: &Path,
    args: &Args,
    trust: Trust,
) -> Result<Vec<Sourced<ConfigLayer>>, UsageError> {
    let mut found = Vec::new();

    if let Some(system) = layer(Source::System, Path::new(SYSTEM_CONFIG))? {
        found.push(system);
    }
    if let Some(home) = ego_home()
        && let Some(user) = layer(Source::User, &home.join("config.toml"))?
    {
        found.push(user);
    }
    // Read only in a trusted workspace. `resolve` would drop them anyway, but
    // "an untrusted workspace is never even looked at" is the stronger claim
    // and the one a headless run depends on — the same rule the instruction
    // loader keeps.
    if trust == Trust::Trusted {
        if let Some(project) = layer(Source::Project, &workspace.join(".ego/config.toml"))? {
            found.push(project);
        }
        if let Some(local) = layer(Source::Local, &workspace.join(".ego/config.local.toml"))? {
            found.push(local);
        }
    }
    if let Some(env) = env_layer()? {
        found.push(env);
    }
    found.push(flags_layer(args)?);

    Ok(found)
}

/// `EGO_MODEL` and friends, as a layer of their own so they take their place
/// in one precedence rule rather than being consulted ad hoc wherever a value
/// is needed.
fn env_layer() -> Result<Option<Sourced<ConfigLayer>>, UsageError> {
    let mut toml = String::new();
    if let Ok(model) = std::env::var(ENV_MODEL) {
        toml.push_str(&format!("model = {}\n", quote(&model)));
    }
    if toml.is_empty() {
        return Ok(None);
    }
    ConfigLayer::parse(Source::Env, &toml)
        .map(Some)
        .map_err(|err| UsageError(format!("the environment: {err}")))
}

/// The command line as a layer.
///
/// `-c K=V` is written into the same TOML the file layers are, so a dotted
/// override is subject to the same strict schema: `-c bogus=1` is refused by
/// name exactly as a config file's unknown key would be, rather than being
/// accepted because it came from a flag.
fn flags_layer(args: &Args) -> Result<Sourced<ConfigLayer>, UsageError> {
    let mut toml = String::new();
    if let Some(model) = &args.model {
        toml.push_str(&format!("model = {}\n", quote(model)));
    }
    if args.bare {
        toml.push_str("bare = true\n");
    }
    // `--budget` is the flags layer of the ceiling, not a value parsed on the
    // side. `2026-08-21-a-run-with-no-ceiling.md` §4: an administrator's
    // system layer is the only source of non-relaxable ceilings, and it can
    // only clamp a figure that went through the same resolution it did.
    if let Some(budget) = &args.budget {
        toml.push_str(&format!("budget = {}\n", quote(budget)));
    }
    if let Some(mode) = args.mode {
        toml.push_str(&format!("mode = {}\n", quote(mode.as_str())));
    }
    // Written as **dotted keys**, not a `[permissions]` table header. A header
    // captures every key after it, and `-c` overrides are appended below —
    // `-c max_turns=3` after a header would land inside `permissions` and be
    // refused as an unknown key, naming the wrong culprit.
    //
    // Emitted only when a rule was actually given. `resolve` replaces a
    // layer's whole `permissions` block, so a `permissions.allow = []` written
    // whenever the flag is absent would make every run erase the deny list a
    // config file set.
    for (key, rules) in [("allow", &args.allow), ("deny", &args.deny)] {
        if rules.is_empty() {
            continue;
        }
        let list = rules.iter().map(|rule| quote(rule)).collect::<Vec<_>>();
        toml.push_str(&format!("permissions.{key} = [{}]\n", list.join(", ")));
    }
    for entry in &args.set {
        let (key, value) = entry.split_once('=').ok_or_else(|| {
            UsageError(format!(
                "`-c {entry}` is not a config override: write it as `-c key=value`, \
                 with the value in TOML — `-c 'model=\"ollama/qwen3\"'`"
            ))
        })?;
        toml.push_str(&format!("{key} = {value}\n"));
    }
    // KNOWN, AND DELIBERATELY LEFT: this prefix doubles. `ConfigError` renders
    // its own origin (`error.rs:27`: "invalid config in {source} layer: …"),
    // so the operator reads "the command line: invalid config in flags layer:
    // …" — two names for one place, neither wrong.
    //
    // Two people have now found this and proposed the same fix: drop one of
    // the two. Do not, in isolation. The flags layer is the only `Source` an
    // operator never *writes* — there is no file to open — so the two halves
    // are not redundant to a reader who does not already know that `flags` and
    // "the command line" are the same layer. The fix worth making is to give
    // `Source`'s `Display` a form that reads correctly in a sentence, which
    // changes every config diagnostic and belongs with `Source::rank` going
    // public (`docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md`
    // "Consequences"), not with whichever caller noticed it.
    ConfigLayer::parse(Source::Flags, &toml)
        .map_err(|err| UsageError(format!("the command line: {err}")))
}

/// The active profile: `--profile` beats `EGO_PROFILE`, per §7's precedence.
pub fn profile(args: &Args) -> Option<String> {
    args.profile
        .clone()
        .or_else(|| std::env::var(ENV_PROFILE).ok())
}

/// A TOML string literal. `toml`'s own serialiser would do this, and reaching
/// for it to quote one string would pull a value model in to answer a lexical
/// question.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
