//! Workspace decisions stored outside the workspace that cannot grant
//! themselves. Executable authority invalidates a decision; instructions do
//! not.
//!
//! See `ARCHITECTURE.md`.
//! `ego-core` owns what each fact means and whether a stored answer still
//! applies. This module owns trust inventory reads and the separate locked
//! documents under `$EGO_HOME`. Records live outside the workspace on purpose:
//! a project file must never be able to write the decision that admits it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

#[cfg(test)]
use ego_core::config::INSTRUCTIONS_FILE;
use ego_core::config::{
    ExecutableDeclaration, ExecutableSurface, ProgramEvidence, TRUST_DIGEST_MAX_BYTES, Trust,
    TrustInventory, TrustLedger, TrustedContent,
};
#[cfg(windows)]
use ego_core::config::{UNCONFINED_CONTRACT_REVISION, UnconfinedLedger};
use ego_tools::instructions::OsInstructionIo;
use ego_tools::privacy::{PrivatePathKind, harden_private_path};
use fd_lock::RwLock;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::output::Diagnostics;

/// Project configuration and the local override beside it: the two files that
/// carry project hooks, project MCP servers and project rules.
const PROJECT_CONFIGS: [&str; 2] = [".ego/config.toml", ".ego/config.local.toml"];
const SKILL_FILE: &str = "SKILL.md";

/// Shared project rules precede repository rules, but scratch fixtures under
/// `~/Gits/.tmp` stay isolated from the human's instruction hierarchy.
pub(crate) fn instruction_discovery_root(root: &Path, home: &Path) -> std::io::Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let home = fs::canonicalize(home)?;
    let shared_path = home.join("Gits");
    let shared = match fs::canonicalize(&shared_path) {
        Ok(shared) => shared,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && fs::symlink_metadata(&shared_path)
                    .is_err_and(|meta_error| meta_error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(root);
        }
        Err(error) => return Err(error),
    };
    let is_scratch = root
        .strip_prefix(&shared)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|component| component.as_os_str() == ".tmp");
    if root.starts_with(&shared) && !is_scratch {
        Ok(shared)
    } else {
        Ok(root)
    }
}

/// The terminal launch resolves the same per-root answer ACP uses.
///
/// `ask` is called at most once, when the workspace has trustable content and
/// no answer for its current executable surface. An explicit launch grant
/// supplies a fresh answer without consulting the seat or a prior refusal.
pub(crate) fn resolve(
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
    explicit_grant: bool,
    ask: impl FnOnce(&str) -> Trust,
) -> Trust {
    resolve_answer(root, home, diagnostics, explicit_grant, |context| {
        Some(ask(context))
    })
}

/// Headless launches may reuse an answer, but never manufacture a refusal.
pub(crate) fn resolve_headless(
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
    explicit_grant: bool,
) -> Trust {
    resolve_answer(root, home, diagnostics, explicit_grant, |_| None)
}

fn resolve_answer(
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
    explicit_grant: bool,
    ask: impl FnOnce(&str) -> Option<Trust>,
) -> Trust {
    let Ok(root) = fs::canonicalize(root) else {
        diagnostics.note("the workspace path could not be resolved; trust was not granted");
        return Trust::Untrusted;
    };
    let root = root.as_path();
    let Some(content) = trustable_content(root) else {
        // No project config, no instructions, no skills: `y` and `n` would
        // open the same session, and a question whose answers do not differ
        // teaches the reader that trust questions do not matter.
        return Trust::Untrusted;
    };

    let surface = executable_surface(root);
    let key = root.display().to_string();
    let store = home.map(TrustStore::new);
    if !explicit_grant && let Some(store) = &store {
        match store.read() {
            Ok(ledger) => {
                if let Some(remembered) = ledger.remembered(&key, &surface) {
                    if ledger.is_legacy(&key)
                        && let Err(error) = store.migrate_legacy(&key, surface.clone(), remembered)
                    {
                        diagnostics.note(&format!(
                            "the old workspace trust record could not be migrated ({error})"
                        ));
                    }
                    return remembered;
                }
            }
            // A ledger nobody can read is not an answer. Asking is the safe
            // degradation; inferring `Trusted` from a broken file is not.
            Err(error) => diagnostics.note(&format!(
                "the remembered workspace trust could not be read ({error}); asking again"
            )),
        }
    }

    let changes = store
        .as_ref()
        .and_then(|store| store.read().ok())
        .and_then(|ledger| {
            ledger
                .prior_surface(&key)
                .map(|old| surface.changes_from(old))
        })
        .filter(|changes| !changes.is_empty());
    let context = match changes {
        Some(changes) => {
            format!("{content}\nExecutable changes since the last answer:\n{changes}\n")
        }
        None => content,
    };
    let decision = if explicit_grant {
        Trust::Trusted
    } else {
        let Some(answer) = ask(&context) else {
            return Trust::Untrusted;
        };
        answer
    };
    if executable_surface(root) != surface {
        diagnostics.note("the workspace executable surface changed while trust was being answered; this answer was not applied");
        return Trust::Untrusted;
    }
    if let Some(store) = &store
        && let Err(error) = store.remember(&key, surface, decision)
    {
        // The person answered, and this session honours that answer. Losing
        // the memory of it costs one more question next launch, which is not
        // worth refusing a session over.
        diagnostics.note(&format!(
            "this trust answer could not be remembered ({error}); \
             this workspace will be asked about again"
        ));
    }
    decision
}

/// Redacted executable declarations and the programs they run. Kept separate
/// from the full inventory shown to a first-time reviewer.
pub(crate) fn executable_surface(root: &Path) -> ExecutableSurface {
    let Ok(root) = fs::canonicalize(root) else {
        tracing::warn!(path = %root.display(), "the workspace executable surface root could not be resolved");
        return ExecutableSurface::default();
    };
    let root = root.as_path();
    let mut surface = ExecutableSurface::default();
    for path in crate::command::project_files(root) {
        let mut bytes = Vec::new();
        if fs::File::open(&path)
            .and_then(|file| {
                file.take(TRUST_DIGEST_MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .is_ok()
        {
            surface.programs.insert(
                path.to_string_lossy().into_owned(),
                ProgramEvidence {
                    bytes: bytes.len() as u64,
                    digest: ego_core::digest::sha256_hex(&bytes),
                    executable: false,
                    link_target: None,
                },
            );
        }
    }
    let configs = project_configs(root);
    let programs: Vec<_> = configs
        .iter()
        .flat_map(|(_, config)| commands_in(config))
        .filter_map(|command| inside(root, &command))
        .collect();
    for (relative, config) in configs {
        if let Some(servers) = config
            .get("mcp")
            .and_then(|mcp| mcp.get("servers"))
            .and_then(toml::Value::as_table)
        {
            for (name, entry) in servers {
                let command = entry
                    .get("command")
                    .or_else(|| entry.get("url"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default();
                surface.declarations.insert(
                    format!("{relative}:mcp:{name}"),
                    ExecutableDeclaration {
                        config: relative.to_owned(),
                        kind: "MCP server".to_owned(),
                        name: name.to_owned(),
                        command: command.to_owned(),
                        args: args_in(entry),
                        env_keys: env_keys_in(entry),
                        details: details_in(
                            entry,
                            &["transport", "cwd", "allow_insecure_loopback"],
                        ),
                    },
                );
            }
        }
        if let Some(hooks) = config.get("hooks").and_then(toml::Value::as_array) {
            for (index, hook) in hooks.iter().enumerate() {
                let handler = hook.get("handler").unwrap_or(hook);
                let name = format!(
                    "{}#{}",
                    hook.get("event")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("hook"),
                    index + 1
                );
                surface.declarations.insert(
                    format!("{relative}:hook:{name}"),
                    ExecutableDeclaration {
                        config: relative.to_owned(),
                        kind: "hook".to_owned(),
                        name,
                        command: handler
                            .get("command")
                            .and_then(toml::Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        args: args_in(handler),
                        env_keys: env_keys_in(handler),
                        details: details_in(hook, &["matcher", "event"])
                            .into_iter()
                            .chain(details_in(
                                handler,
                                &["type", "if", "shell", "timeout", "prompt"],
                            ))
                            .collect(),
                    },
                );
            }
        }
        if let Some(auth) = config.get("auth").and_then(toml::Value::as_table) {
            for (name, entry) in auth {
                if let Some(command) = entry
                    .get("credential_command")
                    .and_then(toml::Value::as_str)
                {
                    surface.declarations.insert(
                        format!("{relative}:credential:{name}"),
                        ExecutableDeclaration {
                            config: relative.to_owned(),
                            kind: "credential command".to_owned(),
                            name: name.to_owned(),
                            command: command.to_owned(),
                            args: Vec::new(),
                            env_keys: Vec::new(),
                            details: Default::default(),
                        },
                    );
                }
            }
        }
    }
    for path in programs {
        let Ok(link_metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let link_target = link_metadata
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&path).ok())
            .flatten()
            .map(|target| target.to_string_lossy().into_owned());
        if !link_metadata.is_file() && link_target.is_none() {
            continue;
        }
        let Some(relative) = path.strip_prefix(root).ok() else {
            continue;
        };
        let resolved = fs::canonicalize(&path).ok();
        if !resolved.as_ref().is_some_and(|path| path.starts_with(root)) {
            if let Some(target) = link_target {
                surface.programs.insert(
                    relative.to_string_lossy().into_owned(),
                    ProgramEvidence {
                        bytes: 0,
                        digest: format!("{:x}", Sha256::digest(target.as_bytes())),
                        executable: false,
                        link_target: Some(target),
                    },
                );
            }
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];
        let digest = loop {
            match file.read(&mut buffer) {
                Ok(0) => break Some(format!("{:x}", hasher.finalize())),
                Ok(count) => hasher.update(&buffer[..count]),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "an executable program could not be fingerprinted");
                    break None;
                }
            }
        };
        let Some(digest) = digest else { continue };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        surface.programs.insert(
            relative.to_string_lossy().into_owned(),
            ProgramEvidence {
                bytes: metadata.len(),
                digest,
                executable,
                link_target,
            },
        );
    }
    surface
}

pub(crate) fn remembered_decision(
    root: &Path,
    home: Option<&Path>,
    surface: &ExecutableSurface,
) -> Option<Trust> {
    let root = fs::canonicalize(root).ok()?;
    let store = TrustStore::new(home?);
    let key = root.display().to_string();
    let ledger = match store.read() {
        Ok(ledger) => ledger,
        Err(error) => {
            tracing::warn!(%error, "the workspace trust ledger could not be read");
            return None;
        }
    };
    let decision = ledger.remembered(&key, surface)?;
    if ledger.is_legacy(&key)
        && let Err(error) = store.migrate_legacy(&key, surface.clone(), decision)
    {
        tracing::warn!(%error, "the legacy workspace trust record could not be migrated");
    }
    Some(decision)
}

pub(crate) fn executable_changes(
    root: &Path,
    home: Option<&Path>,
    surface: &ExecutableSurface,
) -> Option<String> {
    let root = fs::canonicalize(root).ok()?;
    let ledger = TrustStore::new(home?).read().ok()?;
    let changes = surface.changes_from(ledger.prior_surface(&root.display().to_string())?);
    (!changes.is_empty()).then_some(changes)
}

pub(crate) fn record_answer(
    root: &Path,
    home: Option<&Path>,
    fingerprint: &str,
    decision: Trust,
) -> bool {
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    let surface = executable_surface(&root);
    if surface.fingerprint() != fingerprint {
        return false;
    }
    if let Some(home) = home
        && let Err(error) =
            TrustStore::new(home).remember(&root.display().to_string(), surface, decision)
    {
        tracing::warn!(%error, "the workspace trust answer could not be remembered");
    }
    true
}

fn args_in(entry: &toml::Value) -> Vec<String> {
    entry
        .get("args")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn env_keys_in(entry: &toml::Value) -> Vec<String> {
    let mut keys: Vec<String> = entry
        .get("inherit_env")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect();
    if let Some(env) = entry.get("env").and_then(toml::Value::as_table) {
        keys.extend(env.keys().cloned());
    }
    keys.sort();
    keys.dedup();
    keys
}

fn details_in(entry: &toml::Value, names: &[&str]) -> std::collections::BTreeMap<String, String> {
    names
        .iter()
        .filter_map(|name| {
            entry
                .get(*name)
                .map(|value| ((*name).to_owned(), value.to_string()))
        })
        .collect()
}

/// Resolve the separate host-risk acknowledgement for one canonical workspace.
///
/// A broken document is not an acknowledgement and is never replaced by a
/// fresh answer. The answer still governs this invocation, so a failed write
/// costs another question rather than making an informed interactive start
/// unusable.
#[cfg(windows)]
pub(crate) fn resolve_unconfined(
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
    ask: impl FnOnce() -> bool,
) -> bool {
    let key = root.display().to_string();
    let store = home.map(UnconfinedStore::new);
    if let Some(store) = &store {
        match store.read() {
            Ok(ledger) => {
                if ledger.acknowledged(&key, UNCONFINED_CONTRACT_REVISION) {
                    return true;
                }
            }
            Err(error) => diagnostics.note(&format!(
                "the remembered Windows unconfined acknowledgement could not be read ({error}); \
                 asking again"
            )),
        }
    }

    let accepted = ask();
    if accepted && store.is_none() {
        diagnostics.note(
            "this Windows unconfined acknowledgement could not be remembered because EGO_HOME \
             is unavailable; this workspace will be asked about again",
        );
    }
    if accepted
        && let Some(store) = &store
        && let Err(error) = store.acknowledge(&key)
    {
        diagnostics.note(&format!(
            "this Windows unconfined acknowledgement could not be remembered ({error}); \
             this workspace will be asked about again"
        ));
    }
    accepted
}

/// The digest of what this workspace offers to be trusted with, if anything.
///
/// Public so a test can prove what the answer covers. `None` means an empty
/// inventory: nothing here that a trust answer would change.
pub fn inventory_digest(root: &Path) -> Option<String> {
    let inventory = inventory(root);
    (!inventory.is_empty()).then(|| inventory.digest())
}

/// How many *declared* inventory entries a trust question names before it
/// summarises.
///
/// A seat that lists two hundred skill files is a seat nobody reads, and an
/// unread trust question is the failure this whole module exists to avoid. The
/// digest still covers every entry; only the rendering is bounded.
///
/// The bound applies to declarations only. The programs a trusted config would
/// run are listed in full, however many there are — see [`trustable_content`].
const LISTED_CONTENT: usize = 12;

/// What this workspace offers to be trusted with, rendered for a human.
///
/// `None` is the same empty inventory [`inventory_digest`] reports: nothing
/// here that a trust answer would change, so no seat should be asked.
///
/// Here and not in the seat that asks, because the paths probed are this
/// module's to know — `ego-cli` owns which files an answer is about, and a
/// second lister would be a second answer to that question.
///
/// Rendered in two parts, and that split is the whole point. The inventory is
/// one list sorted by path, so a repository that wants its hook handler unseen
/// only has to name it so it sorts past [`LISTED_CONTENT`] and pad the front
/// with skill files. Truncation would then hide the one entry the question is
/// really about. What a trusted config would *run* is therefore listed in
/// full, separately from what it merely *declares*, and only the declarations
/// are bounded.
pub(crate) fn trustable_content(root: &Path) -> Option<String> {
    let inventory = inventory(root);
    if inventory.is_empty() {
        return None;
    }
    let programs: Vec<String> = referenced_commands(root)
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let (runs, declares): (Vec<&TrustedContent>, Vec<&TrustedContent>) = inventory
        .entries()
        .iter()
        .partition(|entry| programs.iter().any(|program| program == entry.path()));
    let mut rendered = String::from("Project files offered for trust:\n");
    rendered.extend(
        declares
            .iter()
            .take(LISTED_CONTENT)
            .map(|entry| format!("- {}\n", entry.path())),
    );

    if let Some(remaining) = declares
        .len()
        .checked_sub(LISTED_CONTENT)
        .filter(|n| *n > 0)
    {
        rendered.push_str(&format!("- …and {remaining} more\n"));
    }
    if !runs.is_empty() {
        rendered.push_str("\nand runs:\n");
        for entry in runs {
            rendered.push_str(&format!("- {}\n", entry.path()));
        }
    }
    rendered.push_str(
        "\nYes: load project instructions, configuration, skills, hooks and MCP server declarations.\n\
         No: continue without project files.\n",
    );
    Some(rendered)
}

/// Everything under `root` whose admission the trust answer governs.
fn inventory(root: &Path) -> TrustInventory {
    let Ok(root) = fs::canonicalize(root) else {
        tracing::warn!(path = %root.display(), "the workspace trust inventory root could not be resolved");
        return TrustInventory::new(Vec::new());
    };
    let root = root.as_path();
    let mut paths: Vec<PathBuf> = PROJECT_CONFIGS
        .iter()
        .map(|relative| root.join(relative))
        .collect();

    paths.extend(referenced_commands(root));
    let mut entries: Vec<_> = paths
        .iter()
        .filter_map(|path| content(root, path))
        .collect();
    entries.extend(
        crate::command::project_files(root)
            .iter()
            .filter_map(|path| content(path.parent()?, path)),
    );
    entries.extend(skill_files(root).iter().filter_map(|path| {
        // Ancestor skill roots are part of the same explicit trust answer.
        content(path.parent()?, path)
    }));

    let discovery_root = match std::env::var_os("HOME") {
        Some(home) => match instruction_discovery_root(root, &PathBuf::from(home)) {
            Ok(discovery_root) => discovery_root,
            Err(error) => {
                tracing::warn!(%error, "the workspace instruction discovery root could not be resolved");
                return TrustInventory::new(Vec::new());
            }
        },
        None => root.to_path_buf(),
    };
    if let Ok(instructions) = ego_core::config::load(
        &OsInstructionIo,
        &discovery_root,
        root,
        &ego_core::config::Selection::Ambient,
        ego_core::config::Limits::default(),
    ) {
        entries.extend(instructions.loaded.into_iter().map(|file| {
            TrustedContent::regular(
                file.path.to_string_lossy(),
                file.bytes,
                file.text.as_bytes(),
            )
        }));
    }
    TrustInventory::new(entries)
}

/// The workspace files a trusted config would run.
///
/// Three config fields are free strings naming a program: a hook handler, an
/// MCP stdio server, and a credential command. Digesting the config that names
/// one but not the file it names would make the remembered answer a lie — the
/// bytes that execute could be replaced by a `git pull` while the digest, and
/// so the answer, stayed the same.
///
/// Only paths that stay inside the root are inventoried. `/usr/bin/env` is not
/// workspace content, and digesting it would re-ask every workspace on every
/// OS update, which teaches operators to answer without reading.
///
/// A handler that runs through an explicit `shell` is resolved the same way.
/// Its command is a shell fragment rather than a path, but nothing here guesses
/// what a shell would do with it: the fragment is resolved as a path and kept
/// only when that path is a file inside the root. `foo && bar` names no such
/// file and adds nothing; `./scripts/hook.sh` names the file it runs.
fn referenced_commands(root: &Path) -> Vec<PathBuf> {
    project_configs(root)
        .into_iter()
        .map(|(_, config)| config)
        .flat_map(|config| commands_in(&config))
        .filter_map(|command| inside(root, &command))
        .collect()
}

fn project_configs(root: &Path) -> Vec<(&'static str, toml::Table)> {
    PROJECT_CONFIGS
        .into_iter()
        .filter_map(|relative| {
            let text = fs::read_to_string(root.join(relative)).ok()?;
            // A config that does not parse cannot declare a runnable command.
            Some((relative, text.parse().ok()?))
        })
        .collect()
}

/// Every program string one parsed config declares.
fn commands_in(config: &toml::Table) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(hooks) = config.get("hooks").and_then(toml::Value::as_array) {
        for hook in hooks {
            let handler = hook.get("handler");
            // Only the command handler names a program; `http` names a URL and
            // `mcp_tool` names a server already covered by its own entry.
            if handler
                .and_then(|handler| handler.get("type"))
                .and_then(toml::Value::as_str)
                == Some("command")
                && let Some(command) = handler
                    .and_then(|handler| handler.get("command"))
                    .and_then(toml::Value::as_str)
            {
                commands.push(command.to_owned());
            }
        }
    }
    if let Some(servers) = config
        .get("mcp")
        .and_then(|mcp| mcp.get("servers"))
        .and_then(toml::Value::as_table)
    {
        commands.extend(
            servers
                .values()
                .filter_map(|server| server.get("command"))
                .filter_map(toml::Value::as_str)
                .map(str::to_owned),
        );
    }
    if let Some(providers) = config.get("auth").and_then(toml::Value::as_table) {
        commands.extend(
            providers
                .values()
                .filter_map(|provider| provider.get("credential_command"))
                .filter_map(toml::Value::as_str)
                .map(str::to_owned),
        );
    }
    commands
}

/// The path this command names, but only while it stays under `root`.
///
/// Resolved lexically rather than through `canonicalize`, because the target
/// may not exist yet and because following a symlink here would let a link
/// decide what the digest covers. A `..` that would climb above the root ends
/// the walk: whatever it reaches is not this workspace's content.
fn inside(root: &Path, command: &str) -> Option<PathBuf> {
    use std::path::Component;

    let candidate = root.join(command);
    let mut resolved = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved.starts_with(root).then_some(resolved)
}

/// An I/O result, with everything that is not "it is not there" said out loud.
///
/// Absence is the ordinary answer: most workspaces hold none of these paths,
/// and a missing file is exactly the "nothing to trust" this inventory is
/// reporting. Every other failure is a read the inventory drops, and dropping
/// it silently makes an unreadable workspace indistinguishable from an empty
/// one — a permission bit dropped on `.ego/` would leave the seat asked
/// nothing at all about a repository full of content. The answer stays the
/// same, because failing closed is right; it just stops being silent.
fn readable<T>(path: &Path, result: std::io::Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "a workspace trust inventory entry could not be read and is not covered by the answer"
            );
            None
        }
    }
}

/// Inventory the same project roots as the loader before asking for trust.
fn skill_files(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut canonical = std::collections::BTreeSet::new();
    for skills in crate::capability_roots::discover_capability_roots(
        root,
        true,
        Path::new("/etc/ego"),
        Path::new(""),
        None,
        "skills",
    )
    .into_iter()
    .filter(|entry| entry.source == ego_core::event::skill::SkillSourceAuthority::Project)
    {
        let Some(entries) = readable(&skills.path, fs::read_dir(&skills.path)) else {
            continue;
        };
        for path in entries
            .filter_map(|entry| readable(&skills.path, entry))
            .map(|entry| entry.path().join(SKILL_FILE))
        {
            if let Ok(resolved) = fs::canonicalize(&path)
                && canonical.insert(resolved)
            {
                paths.push(path);
            }
        }
    }
    paths
}

/// One entry, or nothing if this path is not something ego would admit.
fn content(root: &Path, path: &Path) -> Option<TrustedContent> {
    // `symlink_metadata`, so a symlink at the final component is refused
    // rather than followed — the instruction loader's rule, for the
    // instruction loader's reason, applied one step earlier. A workspace
    // whose only content is a symlink is therefore never asked about, and so
    // is never read.
    let metadata = readable(path, fs::symlink_metadata(path))?;
    if !metadata.is_file() {
        return None;
    }
    path.strip_prefix(root).ok()?;
    let mut prefix = Vec::new();
    readable(
        path,
        readable(path, File::open(path))?
            .take(TRUST_DIGEST_MAX_BYTES as u64)
            .read_to_end(&mut prefix),
    )?;
    Some(TrustedContent::regular(
        path.to_string_lossy(),
        metadata.len(),
        &prefix,
    ))
}

/// One typed JSON document under an ego home.
///
/// The sibling lock is held across read-modify-write. Publication uses a
/// mode-0600 temporary file, `fsync`, then `rename`, so concurrent launches do
/// not lose each other's records and readers never observe a torn document.
pub(crate) struct LockedJsonStore<T> {
    home: PathBuf,
    name: &'static str,
    document: PhantomData<T>,
}

impl<T> LockedJsonStore<T>
where
    T: Default + DeserializeOwned + Serialize,
{
    pub(crate) fn new(home: &Path, name: &'static str) -> Self {
        Self {
            home: home.to_path_buf(),
            name,
            document: PhantomData,
        }
    }

    fn path(&self) -> PathBuf {
        self.home.join(format!("{}.json", self.name))
    }

    /// Read without the lock: `rename` publishes the whole document at once,
    /// so a reader sees either the old file or the new one and never a torn
    /// one. Only the read-modify-write below needs exclusion.
    pub(crate) fn read(&self) -> Result<T, String> {
        let path = self.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(T::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub(crate) fn update(&self, change: impl FnOnce(&mut T)) -> Result<(), String> {
        let mut lock = self.open_lock()?;
        let _guard = lock
            .write()
            .map_err(|error| format!("{}: {error}", self.lock_path().display()))?;
        // Re-read under the lock: another launch may have answered about a
        // different workspace since this process read. An unreadable document
        // must not be silently replaced with an empty one.
        let mut document = self.read()?;
        change(&mut document);
        self.write_unlocked(&document)
    }

    fn lock_path(&self) -> PathBuf {
        self.home.join(format!("{}.lock", self.name))
    }

    fn open_lock(&self) -> Result<RwLock<File>, String> {
        fs::create_dir_all(&self.home)
            .map_err(|error| format!("{}: {error}", self.home.display()))?;
        harden_private_path(&self.home, PrivatePathKind::Directory)
            .map_err(|error| format!("{}: {error}", self.home.display()))?;
        let path = self.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(RwLock::new(file))
    }

    fn write_unlocked(&self, document: &T) -> Result<(), String> {
        let path = self.path();
        let temporary = self.home.join(format!("{}.json.tmp", self.name));
        let bytes = serde_json::to_vec_pretty(document)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        harden_private_path(&temporary, PrivatePathKind::File)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        harden_private_path(&path, PrivatePathKind::File)
            .map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// Project-trust answers under one ego home.
struct TrustStore(LockedJsonStore<TrustLedger>);

impl TrustStore {
    fn new(home: &Path) -> Self {
        Self(LockedJsonStore::new(home, "trust"))
    }

    fn read(&self) -> Result<TrustLedger, String> {
        self.0.read()
    }

    fn remember(
        &self,
        root: &str,
        surface: ExecutableSurface,
        decision: Trust,
    ) -> Result<(), String> {
        self.0
            .update(|ledger| ledger.remember(root, surface, decision))
    }

    fn migrate_legacy(
        &self,
        root: &str,
        surface: ExecutableSurface,
        decision: Trust,
    ) -> Result<(), String> {
        self.0.update(|ledger| {
            if ledger.is_legacy(root) && ledger.remembered(root, &surface) == Some(decision) {
                ledger.remember(root, surface, decision);
            }
        })
    }
}

/// Windows unconfined-execution acknowledgements under one ego home.
#[cfg(windows)]
struct UnconfinedStore(LockedJsonStore<UnconfinedLedger>);

#[cfg(windows)]
impl UnconfinedStore {
    fn new(home: &Path) -> Self {
        Self(LockedJsonStore::new(home, "unconfined"))
    }

    fn read(&self) -> Result<UnconfinedLedger, String> {
        self.0.read()
    }

    fn acknowledge(&self, root: &str) -> Result<(), String> {
        self.0
            .update(|ledger| ledger.acknowledge(root, UNCONFINED_CONTRACT_REVISION))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// `inside` is the containment check itself, and no test that goes through
    /// `inventory_digest` can prove it: `content` refuses a path outside the
    /// root too, so the digest comes out identical either way. Calling it
    /// directly is what makes the guard falsifiable rather than merely asserted.
    #[test]
    fn inside_keeps_only_what_stays_under_the_root() {
        let root = Path::new("/workspace");
        assert_eq!(
            inside(root, "./scripts/hook.sh"),
            Some(PathBuf::from("/workspace/scripts/hook.sh"))
        );
        assert_eq!(inside(root, "../escape.sh"), None);
        assert_eq!(inside(root, "scripts/../../escape.sh"), None);
        // Absolute, so `root.join` discards the root entirely.
        assert_eq!(inside(root, "/usr/bin/env"), None);
        // Climbing back down to the root is not an escape.
        assert_eq!(
            inside(root, "scripts/../hook.sh"),
            Some(PathBuf::from("/workspace/hook.sh"))
        );
    }

    /// The rendering is the whole consent screen an ACP host sees, and the
    /// inventory it reads from is sorted by path — a path the repository
    /// chooses. Truncating one list would therefore let a repository push the
    /// program its hook runs off the end behind a wall of skill files, and a
    /// host would grant execution to something it was never shown.
    #[test]
    fn a_program_a_trusted_config_runs_is_listed_even_past_the_summary_bound() {
        let root = tempfile::tempdir().expect("a temp dir");
        fs::create_dir_all(root.path().join(".ego")).expect("the config directory");
        // Sorts after every skill file and after `AGENTS.md`, so a single
        // truncated list would cut it.
        fs::write(root.path().join("zzz-run.sh"), "#!/bin/sh\n").expect("the script");
        fs::write(
            root.path().join(".ego/config.toml"),
            "[[hooks]]\nevent = \"pre_tool_use\"\n[hooks.handler]\ntype = \"command\"\n\
             command = \"./zzz-run.sh\"\n",
        )
        .expect("the project config");
        fs::write(root.path().join(INSTRUCTIONS_FILE), "read me\n").expect("the instructions");
        for index in 0..LISTED_CONTENT {
            let skill = root.path().join(format!(".ego/skills/a{index:02}"));
            fs::create_dir_all(&skill).expect("the skill directory");
            fs::write(skill.join(SKILL_FILE), "a skill\n").expect("the skill");
        }

        let rendered = trustable_content(root.path()).expect("a non-empty inventory");

        assert!(
            rendered.contains("zzz-run.sh"),
            "the program a trusted config runs was not named: {rendered}"
        );
        assert!(
            rendered.contains("…and"),
            "the declarations were not summarised, so this proves nothing: {rendered}"
        );
    }

    struct Silent;

    impl Diagnostics for Silent {
        fn note(&self, _message: &str) {}
    }

    /// One workspace and one ego home, and a counter for how often the seat
    /// was consulted.
    struct Fixture {
        workspace: tempfile::TempDir,
        home: tempfile::TempDir,
        asked: Cell<usize>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                workspace: tempfile::tempdir().expect("a temp dir"),
                home: tempfile::tempdir().expect("a temp dir"),
                asked: Cell::new(0),
            }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.workspace.path().join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("a temp dir is writable");
            fs::write(path, contents).expect("a temp dir is writable");
        }

        fn resolve(&self, answer: Trust) -> Trust {
            resolve(
                self.workspace.path(),
                Some(self.home.path()),
                &Silent,
                false,
                |_| {
                    self.asked.set(self.asked.get() + 1);
                    answer
                },
            )
        }
    }

    // Catches: an absent headless answer records a refusal, or stale executable
    // authority is reused after a project command changes.
    #[test]
    fn headless_absence_and_executable_changes_never_write_an_answer() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "# instructions");
        let resolve = |grant| {
            resolve_headless(
                fixture.workspace.path(),
                Some(fixture.home.path()),
                &Silent,
                grant,
            )
        };
        assert_eq!(resolve(false), Trust::Untrusted);
        assert!(!fixture.home.path().join("trust.json").exists());
        assert_eq!(resolve(true), Trust::Trusted);
        assert_eq!(resolve(false), Trust::Trusted);
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.new]\ncommand = \"new-server\"\n",
        );
        let before = fs::read(fixture.home.path().join("trust.json")).unwrap();
        assert_eq!(resolve(false), Trust::Untrusted);
        assert_eq!(
            fs::read(fixture.home.path().join("trust.json")).unwrap(),
            before
        );
        assert_eq!(resolve(true), Trust::Trusted);
    }

    #[test]
    fn a_workspace_with_nothing_to_trust_is_untrusted_without_a_question() {
        let fixture = Fixture::new();

        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 0);
        assert!(!fixture.home.path().join("trust.json").exists());
    }

    #[test]
    fn each_kind_of_trusted_content_is_enough_to_be_asked_about() {
        for relative in [
            "AGENTS.md",
            ".ego/config.toml",
            ".ego/config.local.toml",
            ".ego/skills/alpha/SKILL.md",
        ] {
            let fixture = Fixture::new();
            fixture.write(relative, "content\n");

            assert_eq!(
                fixture.resolve(Trust::Trusted),
                Trust::Trusted,
                "{relative}"
            );
            assert_eq!(fixture.asked.get(), 1, "{relative}");
        }
    }

    #[test]
    fn instruction_edits_do_not_revoke_a_workspace_answer() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "first\n");

        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(fixture.asked.get(), 1, "the second launch asked nothing");

        fixture.write("AGENTS.md", "second\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(
            fixture.asked.get(),
            1,
            "instructions do not add an executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_uses_one_canonical_trust_key_and_loads_its_instructions() {
        let tree = tempfile::tempdir().expect("fixture root");
        let workspace = tree.path().join("workspace");
        let alias = tree.path().join("workspace-link");
        let home = tree.path().join("ego-home");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&home).expect("ego home");
        fs::write(workspace.join("AGENTS.md"), "project rules\n").expect("instructions");
        std::os::unix::fs::symlink(&workspace, &alias).expect("workspace alias");

        let asked = Cell::new(0);
        assert_eq!(
            resolve(&alias, Some(&home), &Silent, false, |_: &str| {
                asked.set(asked.get() + 1);
                Trust::Trusted
            }),
            Trust::Trusted
        );
        assert_eq!(asked.get(), 1, "the aliased instructions were offered");
        assert_eq!(
            resolve(&workspace, Some(&home), &Silent, false, |_: &str| {
                asked.set(asked.get() + 1);
                Trust::Trusted
            }),
            Trust::Trusted
        );
        assert_eq!(asked.get(), 1, "both paths share one remembered answer");

        let ledger: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("trust.json")).expect("trust ledger"))
                .expect("JSON ledger");
        let roots = ledger["workspaces"].as_object().expect("workspace entries");
        assert_eq!(roots.len(), 1);
        let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        assert!(roots.contains_key(&canonical_workspace.display().to_string()));

        fs::create_dir_all(workspace.join(".ego")).expect("project config directory");
        fs::write(
            workspace.join(".ego/config.toml"),
            "[mcp.servers.new]\ntransport = 'stdio'\ncommand = '/usr/bin/env'\n",
        )
        .expect("new executable declaration");
        assert_eq!(
            resolve(&alias, Some(&home), &Silent, false, |_| {
                asked.set(asked.get() + 1);
                Trust::Untrusted
            }),
            Trust::Untrusted,
            "new executable authority requires a fresh answer through the alias"
        );
        assert_eq!(asked.get(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_home_discovers_shared_instructions() {
        let tree = tempfile::tempdir().expect("fixture root");
        let home = tree.path().join("home-real");
        let alias = tree.path().join("home-link");
        let shared = home.join("Gits");
        let workspace = shared.join("personal/ego");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(shared.join("AGENTS.md"), "shared rules\n").expect("shared instructions");
        fs::write(workspace.join("AGENTS.md"), "project rules\n").expect("project instructions");
        std::os::unix::fs::symlink(&home, &alias).expect("home alias");

        let workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        let shared = fs::canonicalize(&shared).expect("canonical shared root");
        let discovery_root =
            instruction_discovery_root(&workspace, &alias).expect("canonical discovery root");
        let loaded = ego_core::config::load(
            &OsInstructionIo,
            &discovery_root,
            &workspace,
            &ego_core::config::Selection::Ambient,
            ego_core::config::Limits::default(),
        )
        .expect("workspace within discovery root");
        let texts: Vec<_> = loaded
            .loaded
            .iter()
            .map(|file| file.text.as_str())
            .collect();
        assert_eq!(texts, ["shared rules\n", "project rules\n"]);
        assert_eq!(discovery_root, shared);

        let outside = tree.path().join("outside");
        fs::create_dir_all(&outside).expect("sibling workspace");
        assert_eq!(
            instruction_discovery_root(&outside, &alias).expect("canonical outside root"),
            fs::canonicalize(outside).expect("canonical sibling root")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_shared_gits_root_discovers_its_canonical_tree() {
        let tree = tempfile::tempdir().expect("fixture root");
        let home = tree.path().join("home");
        let shared = tree.path().join("shared-real");
        let workspace = shared.join("personal/ego");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(shared.join("AGENTS.md"), "shared rules\n").expect("shared instructions");
        std::os::unix::fs::symlink(&shared, home.join("Gits")).expect("shared root alias");

        let workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        let shared = fs::canonicalize(&shared).expect("canonical shared root");
        let discovery_root =
            instruction_discovery_root(&workspace, &home).expect("canonical discovery root");
        let loaded = ego_core::config::load(
            &OsInstructionIo,
            &discovery_root,
            &workspace,
            &ego_core::config::Selection::Ambient,
            ego_core::config::Limits::default(),
        )
        .expect("workspace inside shared root");
        let texts: Vec<_> = loaded
            .loaded
            .iter()
            .map(|file| file.text.as_str())
            .collect();
        assert_eq!(texts, ["shared rules\n"]);
        assert_eq!(discovery_root, shared);
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_workspace_root_cannot_fall_back_to_raw_path_and_grant_trust() {
        let tree = tempfile::tempdir().expect("fixture root");
        let missing = tree.path().join("missing");
        let dangling = tree.path().join("dangling");
        std::os::unix::fs::symlink(&missing, &dangling).expect("dangling workspace link");
        for root in [&missing, &dangling] {
            let asked = Cell::new(0);
            assert_eq!(
                resolve(root, Some(tree.path()), &Silent, false, |_| {
                    asked.set(asked.get() + 1);
                    Trust::Trusted
                }),
                Trust::Untrusted,
                "{} must not grant trust after path resolution fails",
                root.display()
            );
            assert_eq!(asked.get(), 0, "no question can grant an unresolved root");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_workspace_root_cannot_fall_back_to_raw_path_for_inventory() {
        let tree = tempfile::tempdir().expect("fixture root");
        let missing = tree.path().join("missing");
        let dangling = tree.path().join("dangling");
        std::os::unix::fs::symlink(&missing, &dangling).expect("dangling workspace link");
        for root in [&missing, &dangling] {
            assert!(inventory(root).is_empty(), "{}", root.display());
            let surface = executable_surface(root);
            assert!(surface.declarations.is_empty(), "{}", root.display());
            assert!(surface.programs.is_empty(), "{}", root.display());
        }
    }

    #[test]
    fn editing_an_imported_instruction_keeps_the_trust_answer() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "@details.md\n");
        fixture.write("details.md", "first\n");
        assert_eq!(
            inventory(fixture.workspace.path()).entries().len(),
            2,
            "both the containing file and its import form the trust offer"
        );

        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(fixture.asked.get(), 1);

        fixture.write("details.md", "second\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(
            fixture.asked.get(),
            1,
            "imported instructions cannot add an executable"
        );
    }

    #[test]
    fn editing_a_project_skill_keeps_the_trust_answer() {
        let fixture = Fixture::new();
        fixture.write(".ego/skills/local/SKILL.md", "first\n");
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        fixture.write(".ego/skills/local/SKILL.md", "second\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(fixture.asked.get(), 1);
    }

    #[test]
    fn adding_a_project_mcp_server_requires_a_new_answer() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "rules\n");
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.new]\ntype = 'stdio'\ncommand = '/usr/bin/env'\n",
        );
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 2);
    }

    #[test]
    fn command_only_workspace_requires_trust_again_when_shell_source_changes() {
        let fixture = Fixture::new();
        fixture.write(".claude/commands/review.md", "Review !`true`\n");
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        fixture.write(".claude/commands/review.md", "Review !`false`\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 2);
    }

    #[test]
    fn changing_a_referenced_hook_program_requires_a_new_answer() {
        let fixture = Fixture::new();
        fixture.write("scripts/hook.sh", "#!/bin/sh\ntrue\n");
        fixture.write(".ego/config.toml", "[[hooks]]\nevent = 'pre_tool_use'\n[hooks.handler]\ntype = 'command'\ncommand = './scripts/hook.sh'\n");
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        fixture.write("scripts/hook.sh", "#!/bin/sh\nfalse\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 2);
    }

    #[test]
    fn a_same_size_program_edit_after_one_mebibyte_requires_a_new_answer() {
        let fixture = Fixture::new();
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.local]\ntransport='stdio'\ncommand='./server'\n",
        );
        let mut bytes = vec![b'a'; TRUST_DIGEST_MAX_BYTES + 1];
        fixture.write("server", &String::from_utf8(bytes.clone()).expect("ASCII"));
        let before = executable_surface(fixture.workspace.path()).fingerprint();
        bytes[TRUST_DIGEST_MAX_BYTES] = b'b';
        fixture.write("server", &String::from_utf8(bytes).expect("ASCII"));
        assert_ne!(
            before,
            executable_surface(fixture.workspace.path()).fingerprint()
        );
    }

    #[cfg(unix)]
    #[test]
    fn changing_only_a_referenced_programs_executable_bit_requires_a_new_answer() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.local]\ntransport='stdio'\ncommand='./server'\n",
        );
        fixture.write("server", "#!/bin/sh\n");
        let path = fixture.workspace.path().join("server");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        let before = executable_surface(fixture.workspace.path()).fingerprint();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("permissions");
        assert_ne!(
            before,
            executable_surface(fixture.workspace.path()).fingerprint()
        );
    }

    #[cfg(unix)]
    #[test]
    fn editing_an_in_workspace_symlinked_program_requires_a_new_answer() {
        let fixture = Fixture::new();
        fixture.write(
            ".ego/config.toml",
            "[mcp.servers.local]\ntransport='stdio'\ncommand='./link.sh'\n",
        );
        fixture.write("target.sh", "#!/bin/sh\necho old\n");
        std::os::unix::fs::symlink("target.sh", fixture.workspace.path().join("link.sh"))
            .expect("link");
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        fixture.write("target.sh", "#!/bin/sh\necho new\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 2);
    }

    #[test]
    fn legacy_digest_only_approval_is_migrated_without_a_question() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "changed since approval\n");
        let root = fs::canonicalize(fixture.workspace.path())
            .expect("canonical workspace")
            .display()
            .to_string();
        fs::write(
            fixture.home.path().join("trust.json"),
            serde_json::json!({
                "v": 1,
                "workspaces": {root.clone(): {"decision": "trusted", "digest": "old-content-digest"}}
            })
            .to_string(),
        )
        .expect("legacy ledger");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(fixture.asked.get(), 0);
        let stored: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.home.path().join("trust.json")).expect("ledger"),
        )
        .expect("json");
        assert!(
            stored["workspaces"][root]
                .get("executable_surface")
                .is_some()
        );
        assert_eq!(stored["v"], 2);
    }

    #[test]
    fn stale_legacy_migration_cannot_replace_a_newer_refusal() {
        let fixture = Fixture::new();
        let root = fixture.workspace.path().display().to_string();
        fs::write(
            fixture.home.path().join("trust.json"),
            serde_json::json!({
                "v": 1,
                "workspaces": {root.clone(): {"decision": "trusted", "digest": "old"}}
            })
            .to_string(),
        )
        .expect("legacy ledger");
        let store = TrustStore::new(fixture.home.path());
        let surface = ExecutableSurface::default();
        store
            .remember(&root, surface.clone(), Trust::Untrusted)
            .expect("new refusal");
        store
            .migrate_legacy(&root, surface.clone(), Trust::Trusted)
            .expect("stale migration");
        assert_eq!(
            store.read().expect("ledger").remembered(&root, &surface),
            Some(Trust::Untrusted)
        );
    }

    #[test]
    fn a_refusal_is_remembered_too() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "first\n");

        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 1, "`no` is not re-asked every launch");
    }

    #[test]
    fn a_symlinked_instruction_file_is_not_content_to_be_trusted_with() {
        let fixture = Fixture::new();
        fixture.write("elsewhere.md", "content\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            fixture.workspace.path().join("elsewhere.md"),
            fixture.workspace.path().join("AGENTS.md"),
        )
        .expect("a temp dir is writable");

        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 0);
    }

    #[test]
    fn a_ledger_nobody_can_read_asks_rather_than_trusting() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "first\n");
        fs::write(fixture.home.path().join("trust.json"), "{ not json")
            .expect("a temp dir is writable");

        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 1);
    }

    #[test]
    fn a_workspace_without_a_home_is_asked_every_launch_and_records_nothing() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "first\n");

        for _ in 0..2 {
            assert_eq!(
                resolve(fixture.workspace.path(), None, &Silent, false, |_| {
                    fixture.asked.set(fixture.asked.get() + 1);
                    Trust::Trusted
                }),
                Trust::Trusted
            );
        }
        assert_eq!(fixture.asked.get(), 2);
    }
}

#[cfg(test)]
mod regression_case {
    use super::*;

    #[test]
    fn portable_only_workspace_prompts_and_ancestor_changes_invalidate_trust() {
        // Catches: portable skills bypass the prompt or reuse stale approval.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        let skill = root.join(".agents/skills/foo/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "first").unwrap();
        let before = inventory(&nested);
        assert!(!before.is_empty());
        fs::write(&skill, "second").unwrap();
        assert_ne!(before.digest(), inventory(&nested).digest());
    }
}
