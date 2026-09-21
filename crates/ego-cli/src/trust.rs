//! Workspace decisions stored outside the workspace that cannot grant
//! themselves.
//!
//! See `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.
//! `ego-core` owns what each fact means and whether a stored answer still
//! applies. This module owns trust inventory reads and the separate locked
//! documents under `$EGO_HOME`. Records live outside the workspace on purpose:
//! a project file must never be able to write the decision that admits it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use ego_core::config::{
    INSTRUCTIONS_FILE, TRUST_DIGEST_MAX_BYTES, Trust, TrustInventory, TrustLedger, TrustedContent,
};
#[cfg(windows)]
use ego_core::config::{UNCONFINED_CONTRACT_REVISION, UnconfinedLedger};
use ego_tools::privacy::{PrivatePathKind, harden_private_path};
use fd_lock::RwLock;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::output::Diagnostics;

/// Project configuration and the local override beside it: the two files that
/// carry project hooks, project MCP servers and project rules.
const PROJECT_CONFIGS: [&str; 2] = [".ego/config.toml", ".ego/config.local.toml"];
/// One directory per project skill, each holding one `SKILL.md`.
const PROJECT_SKILLS: &str = ".ego/skills";
const SKILL_FILE: &str = "SKILL.md";

/// The one place a launch decides what a workspace's posture is.
///
/// `ask` is called at most once, and only when this workspace has content the
/// answer would change and no still-valid answer on record.
pub(crate) fn resolve(
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
    ask: impl FnOnce() -> Trust,
) -> Trust {
    let inventory = inventory(root);
    if inventory.is_empty() {
        // No project config, no instructions, no skills: `y` and `n` would
        // open the same session, and a question whose answers do not differ
        // teaches the reader that trust questions do not matter.
        return Trust::Untrusted;
    }

    let digest = inventory.digest();
    let key = root.display().to_string();
    let store = home.map(TrustStore::new);
    if let Some(store) = &store {
        match store.read() {
            Ok(ledger) => {
                if let Some(remembered) = ledger.remembered(&key, &digest) {
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

    let decision = ask();
    if let Some(store) = &store
        && let Err(error) = store.remember(&key, &digest, decision)
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
        .filter_map(|path| path.strip_prefix(root).ok())
        .map(|relative| relative.to_string_lossy().into_owned())
        .collect();
    let (runs, declares): (Vec<&TrustedContent>, Vec<&TrustedContent>) = inventory
        .entries()
        .iter()
        .partition(|entry| programs.iter().any(|program| program == entry.path()));
    let mut rendered: String = declares
        .iter()
        .take(LISTED_CONTENT)
        .map(|entry| format!("- {}\n", entry.path()))
        .collect();
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
    Some(rendered)
}

/// Everything under `root` whose admission the trust answer governs.
fn inventory(root: &Path) -> TrustInventory {
    let mut paths: Vec<PathBuf> = PROJECT_CONFIGS
        .iter()
        .map(|relative| root.join(relative))
        .collect();
    paths.push(root.join(INSTRUCTIONS_FILE));
    paths.extend(skill_files(root));
    paths.extend(referenced_commands(root));
    TrustInventory::new(paths.iter().filter_map(|path| content(root, path)))
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
    PROJECT_CONFIGS
        .iter()
        .filter_map(|relative| fs::read_to_string(root.join(relative)).ok())
        // A config that does not parse declares nothing this build would run:
        // it fails the load that comes later, so it names no command to cover.
        .filter_map(|text| text.parse::<toml::Table>().ok())
        .flat_map(|config| commands_in(&config))
        .filter_map(|command| inside(root, &command))
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

/// `<root>/.ego/skills/*/SKILL.md`, exactly as the skill loader finds them.
fn skill_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root.join(PROJECT_SKILLS)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(SKILL_FILE))
        .collect()
}

/// One entry, or nothing if this path is not something ego would admit.
fn content(root: &Path, path: &Path) -> Option<TrustedContent> {
    // `symlink_metadata`, so a symlink at the final component is refused
    // rather than followed — the instruction loader's rule, for the
    // instruction loader's reason, applied one step earlier. A workspace
    // whose only content is a symlink is therefore never asked about, and so
    // is never read.
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    let mut prefix = Vec::new();
    File::open(path)
        .ok()?
        .take(TRUST_DIGEST_MAX_BYTES as u64)
        .read_to_end(&mut prefix)
        .ok()?;
    Some(TrustedContent::regular(
        relative.to_string_lossy(),
        metadata.len(),
        &prefix,
    ))
}

/// One typed JSON document under an ego home.
///
/// The sibling lock is held across read-modify-write. Publication uses a
/// mode-0600 temporary file, `fsync`, then `rename`, so concurrent launches do
/// not lose each other's records and readers never observe a torn document.
struct LockedJsonStore<T> {
    home: PathBuf,
    name: &'static str,
    document: PhantomData<T>,
}

impl<T> LockedJsonStore<T>
where
    T: Default + DeserializeOwned + Serialize,
{
    fn new(home: &Path, name: &'static str) -> Self {
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
    fn read(&self) -> Result<T, String> {
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

    fn update(&self, change: impl FnOnce(&mut T)) -> Result<(), String> {
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

    fn remember(&self, root: &str, digest: &str, decision: Trust) -> Result<(), String> {
        self.0
            .update(|ledger| ledger.remember(root, digest, decision))
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
                || {
                    self.asked.set(self.asked.get() + 1);
                    answer
                },
            )
        }
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
    fn an_answer_is_remembered_until_the_content_changes() {
        let fixture = Fixture::new();
        fixture.write("AGENTS.md", "first\n");

        assert_eq!(fixture.resolve(Trust::Trusted), Trust::Trusted);
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Trusted);
        assert_eq!(fixture.asked.get(), 1, "the second launch asked nothing");

        fixture.write("AGENTS.md", "second\n");
        assert_eq!(fixture.resolve(Trust::Untrusted), Trust::Untrusted);
        assert_eq!(fixture.asked.get(), 2, "an edit is a new question");
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
                resolve(fixture.workspace.path(), None, &Silent, || {
                    fixture.asked.set(fixture.asked.get() + 1);
                    Trust::Trusted
                }),
                Trust::Trusted
            );
        }
        assert_eq!(fixture.asked.get(), 2);
    }
}
