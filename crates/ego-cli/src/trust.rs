//! Which files a workspace offers to be trusted with, and the answer it was
//! already given.
//!
//! See `docs/decisions/2026-09-05-a-trust-answer-is-remembered-per-root.md`.
//! `ego-core` owns what an inventory *is* and whether a stored answer still
//! applies; this module owns the paths that get probed, the reads that build
//! the inventory, and the locked document under `$EGO_HOME`. The record lives
//! outside the workspace on purpose: a project file must never be able to
//! write the record that admits it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ego_core::config::{
    INSTRUCTIONS_FILE, TRUST_DIGEST_MAX_BYTES, Trust, TrustInventory, TrustLedger, TrustedContent,
};
use fd_lock::RwLock;

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

/// Everything under `root` whose admission the trust answer governs.
fn inventory(root: &Path) -> TrustInventory {
    let mut paths: Vec<PathBuf> = PROJECT_CONFIGS
        .iter()
        .map(|relative| root.join(relative))
        .collect();
    paths.push(root.join(INSTRUCTIONS_FILE));
    paths.extend(skill_files(root));
    TrustInventory::new(paths.iter().filter_map(|path| content(root, path)))
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

/// The remembered answers under one ego home.
///
/// Same shape and same lock as the credential store beside it: one sibling
/// lock held across read-modify-write, a mode-0600 temporary file, `fsync`,
/// then `rename`. Two launches in two workspaces cannot lose each other's
/// answer.
struct TrustStore {
    home: PathBuf,
}

impl TrustStore {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn path(&self) -> PathBuf {
        self.home.join("trust.json")
    }

    /// Read without the lock: `rename` publishes the whole document at once,
    /// so a reader sees either the old file or the new one and never a torn
    /// one. Only the read-modify-write below needs exclusion.
    fn read(&self) -> Result<TrustLedger, String> {
        let path = self.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TrustLedger::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn remember(&self, root: &str, digest: &str, decision: Trust) -> Result<(), String> {
        let mut lock = self.open_lock()?;
        let _guard = lock
            .write()
            .map_err(|error| format!("{}: {error}", self.home.join("trust.lock").display()))?;
        // Re-read under the lock: another launch may have answered about a
        // different workspace since this process read.
        let mut ledger = self.read().unwrap_or_default();
        ledger.remember(root, digest, decision);
        self.write_unlocked(&ledger)
    }

    fn open_lock(&self) -> Result<RwLock<File>, String> {
        fs::create_dir_all(&self.home)
            .map_err(|error| format!("{}: {error}", self.home.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.home, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("{}: {error}", self.home.display()))?;
        }
        let path = self.home.join("trust.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(RwLock::new(file))
    }

    fn write_unlocked(&self, ledger: &TrustLedger) -> Result<(), String> {
        let path = self.path();
        let temporary = self.home.join("trust.json.tmp");
        let bytes = serde_json::to_vec_pretty(ledger)
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
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path).map_err(|error| format!("{}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

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
