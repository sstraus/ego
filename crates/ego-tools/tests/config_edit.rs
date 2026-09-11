//! contract for the OS adapter's opaque whole-document editor session.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_tools::config::OsUserConfigStore;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

fn store(path: &Path) -> OsUserConfigStore<Arc<OsIo>> {
    OsUserConfigStore::new(Arc::new(OsIo), path)
}

struct PhaseIo {
    inner: OsIo,
    publication: Option<AtomicReplacePublication>,
}

impl StoreIo for PhaseIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> io::Result<()> {
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_atomic(path, bytes)
    }
    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        if let Some(publication) = &self.publication {
            if let AtomicReplacePublication::PublishedDurabilityUncertain { .. } = publication {
                let _ = self.inner.replace_atomic(path, bytes);
            }
            return Ok(publication.clone());
        }
        Err(io::Error::other("pre-publication replacement failure"))
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.truncate(path, len)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn list_dirs(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list_dirs(path)
    }
}

#[test]
fn one_store_prepares_candidate_and_publishes_only_after_opaque_cas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original =
        "# preserved\nmodel = \"ollama/qwen3\"\n[profile.ci]\nmodel = \"ollama/gemma3\"\n";
    fs::write(&path, original).unwrap();
    let authority = store(&path);
    let edit = authority.prepare_edit().unwrap();
    let candidate_path = edit.candidate_path().to_owned();
    assert_eq!(fs::read(&candidate_path).unwrap(), original.as_bytes());
    assert_eq!(
        fs::metadata(&candidate_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let edited =
        "# preserved\nmodel = \"ollama/llama3\"\n[profile.ci]\nmodel = \"ollama/gemma3\"\n";
    fs::write(&candidate_path, edited).unwrap();
    let result = authority.publish_edit(edit).unwrap();
    assert!(result.changed);
    assert!(matches!(
        result.publication,
        ego_core::config::UserConfigPublication::PublishedDurably
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    assert!(!candidate_path.exists());
}

#[test]
fn simultaneous_prepares_are_distinct_private_sibling_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "model = \"ollama/qwen3\"\n").unwrap();
    let authority = store(&path);
    let first = authority.prepare_edit().unwrap();
    let second = authority.prepare_edit().unwrap();
    assert_ne!(first.candidate_path(), second.candidate_path());
    for path in [first.candidate_path(), second.candidate_path()] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    authority.discard_edit(first).unwrap();
    authority.discard_edit(second).unwrap();
}

#[test]
fn unchanged_is_not_published_and_invalid_or_stale_candidates_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "model = \"ollama/qwen3\"\n").unwrap();
    let authority = store(&path);
    let unchanged = authority.prepare_edit().unwrap();
    let unchanged_path = unchanged.candidate_path().to_owned();
    fs::write(&unchanged_path, "model = \"ollama/qwen3\"\n").unwrap();
    let result = authority.publish_edit(unchanged).unwrap();
    assert!(!result.changed);
    assert!(matches!(
        result.publication,
        ego_core::config::UserConfigPublication::NotPublished
    ));
    assert!(!unchanged_path.exists());

    let invalid = authority.prepare_edit().unwrap();
    let invalid_path = invalid.candidate_path().to_owned();
    fs::write(&invalid_path, "model = [not valid TOML").unwrap();
    assert!(authority.publish_edit(invalid).is_err());
    assert!(invalid_path.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = \"ollama/qwen3\"\n"
    );

    let stale = authority.prepare_edit().unwrap();
    let stale_path = stale.candidate_path().to_owned();
    fs::write(&stale_path, "model = \"ollama/llama3\"\n").unwrap();
    fs::write(&path, "model = \"ollama/gemma3\"\n").unwrap();
    assert!(authority.publish_edit(stale).is_err());
    assert!(stale_path.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = \"ollama/gemma3\"\n"
    );
}

#[test]
fn discard_edit_removes_candidate_without_creating_a_second_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "model = \"ollama/qwen3\"\n").unwrap();
    let authority = store(&path);
    let edit = authority.prepare_edit().unwrap();
    let candidate_path = edit.candidate_path().to_owned();
    authority.discard_edit(edit).unwrap();
    assert!(!candidate_path.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = \"ollama/qwen3\"\n"
    );
}

#[test]
fn uncertain_publication_keeps_recovery_while_prepublication_error_keeps_old_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "model = \"ollama/qwen3\"\n").unwrap();
    let uncertain = OsUserConfigStore::new(
        Arc::new(PhaseIo {
            inner: OsIo,
            publication: Some(AtomicReplacePublication::PublishedDurabilityUncertain {
                diagnostic: "directory barrier failed".to_owned(),
            }),
        }),
        &path,
    );
    let edit = uncertain.prepare_edit().unwrap();
    let candidate = edit.candidate_path().to_owned();
    fs::write(&candidate, "model = \"ollama/llama3\"\n").unwrap();
    let result = uncertain.publish_edit(edit).unwrap();
    assert!(result.changed);
    assert!(matches!(
        result.publication,
        ego_core::config::UserConfigPublication::PublishedDurabilityUncertain { .. }
    ));
    assert!(candidate.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = \"ollama/llama3\"\n"
    );

    let failing = OsUserConfigStore::new(
        Arc::new(PhaseIo {
            inner: OsIo,
            publication: None,
        }),
        &path,
    );
    let edit = failing.prepare_edit().unwrap();
    let candidate = edit.candidate_path().to_owned();
    fs::write(&candidate, "model = \"ollama/gemma3\"\n").unwrap();
    assert!(failing.publish_edit(edit).is_err());
    assert!(candidate.exists());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = \"ollama/llama3\"\n"
    );
}
