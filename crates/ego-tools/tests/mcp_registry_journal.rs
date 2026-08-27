//! RED contract for the append-only MCP registry journal adapter.
//!
//! The port belongs to `ego-core`; this suite crosses the final filesystem
//! adapter and reopens it to prove that replay does not infer settlement.

use std::io::{self, Write};
use std::path::Path;

use ego_core::turn::{
    McpRegistryEvent, McpRegistryJournal, McpRegistryJournalErrorKind, McpRegistryJournalOperation,
};
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{AtomicReplacePublication, OsIo, StoreIo};

fn journal(root: &std::path::Path) -> JsonlMcpRegistryJournal {
    JsonlMcpRegistryJournal::new(root.join("mcp"), Box::new(OsIo))
}

struct FailingFsyncIo;

impl StoreIo for FailingFsyncIo {
    fn append_line(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        OsIo.append_line(path, line)
    }

    fn fsync(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::other("injected journal durability failure"))
    }

    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        OsIo.read(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        OsIo.exists(path)
    }

    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OsIo.replace_atomic(path, bytes)
    }

    fn replace_atomic_publication(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> io::Result<AtomicReplacePublication> {
        OsIo.replace_atomic_publication(path, bytes)
    }

    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        OsIo.truncate(path, len)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        OsIo.remove_file(path)
    }

    fn list_dirs(&self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        OsIo.list_dirs(path)
    }
}

#[test]
fn replay_preserves_all_machine_event_kinds_and_their_sanitized_fields() {
    let home = tempfile::tempdir().expect("temp EGO_HOME");
    let writer = journal(home.path());
    writer
        .append(McpRegistryEvent::RefreshRequested {
            request_id: "request-1".to_owned(),
            trigger: "reload".to_owned(),
            server_scope: Some("example".to_owned()),
            config_revision: 7,
        })
        .expect("refresh request");
    writer
        .append(McpRegistryEvent::SnapshotPublished {
            generation: "generation-2".to_owned(),
            predecessor: Some("generation-1".to_owned()),
            servers: vec!["example".to_owned()],
            descriptor_digests: vec!["sha256:descriptor".to_owned()],
        })
        .expect("publication");
    writer
        .append(McpRegistryEvent::RefreshFailed {
            request_id: "request-1".to_owned(),
            cause: "invalid descriptor".to_owned(),
            retained_generation: Some("generation-1".to_owned()),
        })
        .expect("retained failure");
    writer
        .append(McpRegistryEvent::HealthChanged {
            server: "example".to_owned(),
            prior: "ready".to_owned(),
            current: "degraded".to_owned(),
        })
        .expect("health transition");
    writer
        .append(McpRegistryEvent::AuthorizationRequired {
            server: "example".to_owned(),
            issuer: "https://issuer.example".to_owned(),
            protected_resource: "https://mcp.example".to_owned(),
            scopes: vec!["tools".to_owned()],
            request_correlation: "request-1".to_owned(),
        })
        .expect("authorization state");

    let events = journal(home.path()).replay().expect("replay");
    assert_eq!(events.len(), 5);
    let mut kinds = [false; 5];
    for event in events {
        match event {
            McpRegistryEvent::RefreshRequested {
                request_id,
                trigger,
                server_scope,
                config_revision,
            } => {
                kinds[0] = true;
                assert_eq!(request_id, "request-1");
                assert_eq!(trigger, "reload");
                assert_eq!(server_scope.as_deref(), Some("example"));
                assert_eq!(config_revision, 7);
            }
            McpRegistryEvent::SnapshotPublished {
                generation,
                predecessor,
                servers,
                descriptor_digests,
            } => {
                kinds[1] = true;
                assert_eq!(generation, "generation-2");
                assert_eq!(predecessor.as_deref(), Some("generation-1"));
                assert_eq!(servers, ["example"]);
                assert_eq!(descriptor_digests, ["sha256:descriptor"]);
            }
            McpRegistryEvent::RefreshFailed {
                request_id,
                cause,
                retained_generation,
            } => {
                kinds[2] = true;
                assert_eq!(request_id, "request-1");
                assert_eq!(cause, "invalid descriptor");
                assert_eq!(retained_generation.as_deref(), Some("generation-1"));
            }
            McpRegistryEvent::HealthChanged {
                server,
                prior,
                current,
            } => {
                kinds[3] = true;
                assert_eq!(server, "example");
                assert_eq!(prior, "ready");
                assert_eq!(current, "degraded");
            }
            McpRegistryEvent::AuthorizationRequired {
                server,
                issuer,
                protected_resource,
                scopes,
                request_correlation,
            } => {
                kinds[4] = true;
                assert_eq!(server, "example");
                assert_eq!(issuer, "https://issuer.example");
                assert_eq!(protected_resource, "https://mcp.example");
                assert_eq!(scopes, ["tools"]);
                assert_eq!(request_correlation, "request-1");
            }
        }
    }
    assert_eq!(kinds, [true; 5]);

    let raw =
        std::fs::read_to_string(home.path().join("mcp/registry.jsonl")).expect("journal bytes");
    for forbidden_key in [
        "\"secret\"",
        "\"challenge\"",
        "\"access_token\"",
        "\"refresh_token\"",
    ] {
        assert!(
            !raw.contains(forbidden_key),
            "journal contains forbidden field {forbidden_key}"
        );
    }
}

#[test]
fn replay_keeps_a_requested_refresh_without_inventing_publication() {
    let home = tempfile::tempdir().expect("temp EGO_HOME");
    journal(home.path())
        .append(McpRegistryEvent::RefreshRequested {
            request_id: "crashed-request".to_owned(),
            trigger: "notification".to_owned(),
            server_scope: None,
            config_revision: 8,
        })
        .expect("refresh request");

    let replay = journal(home.path()).replay().expect("replay");
    assert_eq!(replay.len(), 1);
    assert!(matches!(
        &replay[0],
        McpRegistryEvent::RefreshRequested {
            request_id,
            trigger,
            server_scope: None,
            config_revision: 8,
        } if request_id == "crashed-request" && trigger == "notification"
    ));
    assert!(
        !replay
            .iter()
            .any(|event| matches!(event, McpRegistryEvent::SnapshotPublished { .. }))
    );
}

#[test]
fn replay_rejects_an_unknown_committed_record_version() {
    let home = tempfile::tempdir().expect("temp EGO_HOME");
    let mcp = home.path().join("mcp");
    let path = mcp.join("registry.jsonl");
    let record = br#"{"version":2,"event":{"kind":"refresh_requested","request_id":"version-request","trigger":"test","server_scope":null,"config_revision":1}}"#;
    OsIo.append_line(&path, record)
        .expect("committed unknown version");

    let error = journal(home.path())
        .replay()
        .expect_err("unknown version must fail closed");
    assert_eq!(error.operation(), McpRegistryJournalOperation::Replay);
    assert_eq!(
        error.kind(),
        McpRegistryJournalErrorKind::UnsupportedVersion
    );
}

#[test]
fn append_reports_durability_uncertain_after_the_record_was_written() {
    let home = tempfile::tempdir().expect("temp EGO_HOME");
    let mcp = home.path().join("mcp");
    let event = McpRegistryEvent::RefreshRequested {
        request_id: "durability-request".to_owned(),
        trigger: "test".to_owned(),
        server_scope: None,
        config_revision: 2,
    };
    let error = JsonlMcpRegistryJournal::new(mcp.clone(), Box::new(FailingFsyncIo))
        .append(event)
        .expect_err("fsync failure must be reported");

    assert_eq!(error.operation(), McpRegistryJournalOperation::Append);
    assert_eq!(
        error.kind(),
        McpRegistryJournalErrorKind::DurabilityUncertain
    );
    let bytes = std::fs::read(mcp.join("registry.jsonl")).expect("appended journal record");
    assert!(
        bytes.ends_with(b"\n"),
        "append must complete before fsync fails"
    );
    assert!(!bytes.is_empty(), "append must complete before fsync fails");
    assert!(
        std::str::from_utf8(&bytes)
            .expect("journal record is UTF-8")
            .contains("durability-request"),
        "the requested event must be written before fsync fails"
    );
}

#[test]
fn append_repairs_a_torn_tail_before_publishing_the_next_event() {
    let home = tempfile::tempdir().expect("temp EGO_HOME");
    let mcp = home.path().join("mcp");
    let prior = McpRegistryEvent::RefreshRequested {
        request_id: "prior-request".to_owned(),
        trigger: "initial".to_owned(),
        server_scope: None,
        config_revision: 3,
    };
    let next = McpRegistryEvent::HealthChanged {
        server: "example".to_owned(),
        prior: "degraded".to_owned(),
        current: "ready".to_owned(),
    };
    journal(home.path())
        .append(prior.clone())
        .expect("prior event");

    let path = mcp.join("registry.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal for torn tail");
    file.write_all(
        br#"{"version":1,"event":{"kind":"refresh_requested","request_id":"torn-request"}"#,
    )
    .expect("torn tail");
    drop(file);

    journal(home.path())
        .append(next.clone())
        .expect("repair and append");
    let replay = journal(home.path())
        .replay()
        .expect("replay repaired journal");
    assert_eq!(replay, vec![prior, next]);
    assert!(!replay.iter().any(|event| {
        matches!(
            event,
            McpRegistryEvent::RefreshRequested { request_id, .. }
                if request_id == "torn-request"
        )
    }));
}
