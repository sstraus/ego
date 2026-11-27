//! contract for the OS adapter's opaque whole-document editor session.

use std::fs;
use std::io;
#[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    {
        for path in [first.candidate_path(), second.candidate_path()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

fn provider_instance_fixture(upstream: &str) -> ego_core::config::ProviderInstance {
    use ego_core::generator::capabilities::*;
    ego_core::config::ProviderInstance {
        profile: "openai-compatible".parse().unwrap(),
        base_url: "https://office.example/prefix/v1".parse().unwrap(),
        credential_ref: Some("office".parse().unwrap()),
        models: std::collections::BTreeMap::from([(
            "assistant".parse().unwrap(),
            ego_core::config::ProviderInstanceModel {
                upstream_id: upstream.into(),
                contract: ego_core::config::ProviderModelContract::ChatStandard,
                metering: ego_core::budget::MeteringIdentity::Currency {
                    settlement: ego_core::budget::SettlementSource::Quote,
                },
                facts: ModelCapabilities {
                    schema_version: ModelCapabilities::SCHEMA_VERSION,
                    max_context_tokens: Some(32000),
                    max_output_tokens: Some(2048),
                    max_tools: Some(8),
                    supports_parallel_tool_calls: false,
                    reasoning_carryover: ReasoningCarryover::Unsupported,
                    reasoning_efforts: ReasoningEffortSupport::Unknown,
                    default_reasoning_effort: None,
                    reasoning_state_dialect: None,
                    cancel: CancelSemantics::Cooperative,
                    cache: CacheRetention::None,
                    transport: Transport::Sse,
                    image_input: ImageInputSupport::Unknown,
                    audio_input: AudioInputSupport::Unknown,
                },
            },
        )]),
    }
}

// Catches save/reload collapse and stale endpoint/auth overwrites inside the atomic adapter.
#[test]
fn provider_instances_atomic_save_reload_compare_and_remove_preserve_other_authority() {
    use ego_core::config::{ConfigLayer, Source, UserConfigMutation, UserConfigStore};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "# Keep this comment\nmodel = 'openai/existing'\n").unwrap();
    let store = store(&path);
    let office = provider_instance_fixture("deployment-a");
    let home = provider_instance_fixture("deployment-b");
    for (id, spec) in [("office", &office), ("home", &home)] {
        assert!(
            store
                .mutate(UserConfigMutation::ProviderInstance {
                    id: id.parse().unwrap(),
                    expected: None,
                    replacement: Some(spec.clone())
                })
                .unwrap()
                .changed
        );
    }
    let text = fs::read_to_string(&path).unwrap();
    let parsed = ConfigLayer::parse(Source::User, &text).unwrap();
    assert_eq!(
        parsed.value.providers.instances[&"office".parse().unwrap()],
        office
    );
    assert_eq!(
        parsed.value.providers.instances[&"home".parse().unwrap()],
        home
    );
    assert!(text.contains("# Keep this comment"));
    assert_eq!(parsed.value.model.unwrap().0, "openai/existing");
    assert!(
        !store
            .mutate(UserConfigMutation::ProviderInstance {
                id: "office".parse().unwrap(),
                expected: Some(office.clone()),
                replacement: Some(office.clone())
            })
            .unwrap()
            .changed
    );
    let mut newer = office.clone();
    newer.base_url = "https://new.example/v1".parse().unwrap();
    store
        .mutate(UserConfigMutation::ProviderInstance {
            id: "office".parse().unwrap(),
            expected: Some(office.clone()),
            replacement: Some(newer.clone()),
        })
        .unwrap();
    let before_stale = fs::read(&path).unwrap();
    assert!(
        store
            .mutate(UserConfigMutation::ProviderInstance {
                id: "office".parse().unwrap(),
                expected: Some(office),
                replacement: Some(home.clone())
            })
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before_stale);
    store
        .mutate(UserConfigMutation::ProviderInstance {
            id: "office".parse().unwrap(),
            expected: Some(newer),
            replacement: None,
        })
        .unwrap();
    let parsed = ConfigLayer::parse(Source::User, &fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        !parsed
            .value
            .providers
            .instances
            .contains_key(&"office".parse().unwrap())
    );
    assert_eq!(
        parsed.value.providers.instances[&"home".parse().unwrap()],
        home
    );
}

// Catches an unsupported profile/model contract reaching durable publication.
#[test]
fn provider_instances_invalid_candidate_never_replaces_user_document() {
    use ego_core::config::{ProviderModelContract, UserConfigMutation, UserConfigStore};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "# unchanged\nmodel = 'openai/existing'\n").unwrap();
    let before = fs::read(&path).unwrap();
    let mut invalid = provider_instance_fixture("deployment-a");
    invalid
        .models
        .get_mut(&"assistant".parse().unwrap())
        .unwrap()
        .contract = ProviderModelContract::Messages;
    assert!(
        store(&path)
            .mutate(UserConfigMutation::ProviderInstance {
                id: "office".parse().unwrap(),
                expected: None,
                replacement: Some(invalid)
            })
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}
