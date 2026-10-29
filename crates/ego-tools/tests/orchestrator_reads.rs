//! Real filesystem/permission boundary; TUIC replies replay a recorded response.

use ego_core::config::{Mode, Ruleset};
use ego_core::event::id::SessionId;
use ego_core::mcp::ToolExecution;
use ego_core::orchestrator::{ChildLine, Filter, MAX_CHILD_LINE_BYTES};
use ego_core::permit::{
    CanonPath, Classification, DnsResolution, DuplexHandle, DuplexSpawner, GuardedEffects,
    HttpRequest, NetworkTarget, OneHop, OneHopResponse, PolicyEngine, PrepareCtx, ProcessError,
    ProcessExecutor, Resolution, ResolvedConnect, Resolver, SpawnSpec, ToolResult, TransportError,
    TransportFailure,
};
use ego_tools::fs::OsFs;
use ego_tools::orchestrator::{project_status, tools};
use ego_tools::store::SessionEvidenceStore;
use futures::executor::block_on;
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const SESSIONS: &str = include_str!("fixtures/orchestrator/sessions.json");
const CHILDREN: &str = include_str!("fixtures/orchestrator/children.tsv");

// Catches: raw session metadata or dead/untracked peers are returned to the model.
#[test]
fn recorded_live_children_are_projected_in_tracking_order_with_bounded_rows() {
    assert_eq!(
        serde_json::from_str::<Vec<serde_json::Value>>(SESSIONS)
            .unwrap()
            .len(),
        18
    );
    let rows = project_status(SESSIONS, CHILDREN, &Filter::default()).unwrap();
    let names = [
        "example-worker-a",
        "example-worker-b",
        "example-worker-c",
        "example-worker-d",
        "example-reviewer",
        "example-coordinator",
    ];
    assert_eq!(rows.len(), names.len());
    for (row, name) in rows.iter().zip(names) {
        assert!(row.text().contains(name));
        assert!(row.text().len() <= MAX_CHILD_LINE_BYTES);
        assert!(!row.text().contains("foreground_process"));
    }
    let filtered = project_status(
        SESSIONS,
        CHILDREN,
        &Filter {
            root: Some("/workspace/project".into()),
        },
    )
    .unwrap();
    assert_eq!(filtered.len(), 5);
    assert!(
        !filtered
            .iter()
            .any(|row| row.text().contains("example-worker-a"))
    );
    assert!(project_status("unavailable", CHILDREN, &Filter::default()).is_err());
}

// Catches: byte truncation panics on Unicode or control characters inject extra rows.
#[test]
fn child_rows_stay_single_line_and_utf8_safe_with_long_unicode_names() {
    let row = ChildLine::new("child", &"界\n".repeat(300), "idle", "/repo");
    assert!(row.text().len() <= 500);
    assert!(!row.text().contains('\n'));
    assert!(row.text().ends_with('…'));
}

// Catches: standalone sessions advertise orchestration tools without a transport.
#[test]
fn no_socket_means_no_orchestration_tools() {
    assert!(tools(None, std::path::Path::new("/home/u")).is_empty());
    assert!(
        tools(
            Some(std::ffi::OsStr::new("")),
            std::path::Path::new("/home/u")
        )
        .is_empty()
    );
}

struct RecordedTuic;
impl ProcessExecutor for RecordedTuic {
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(async move {
            if spec.argv()[0] == "tuic" {
                // Recorded boundary permits exactly session/list, never any inbox/wait.
                assert_eq!(
                    spec.argv()
                        .iter()
                        .map(|s| s.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["tuic", "mcp", "session", r#"{"action":"list"}"#]
                );
                return Ok(ToolResult::builtin(SESSIONS));
            }
            assert_eq!(spec.argv()[0], "find");
            let output = std::process::Command::new(&spec.argv()[0])
                .args(&spec.argv()[1..])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(ToolResult::builtin(
                String::from_utf8(output.stdout).unwrap(),
            ))
        })
    }
}

struct NoNetwork;
impl Resolver for NoNetwork {
    fn resolve(
        &self,
        _: NetworkTarget,
        _: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(async { Err(TransportError::definitely_unsent(TransportFailure::Offline)) })
    }
}
impl OneHop for NoNetwork {
    fn begin_call(&self) -> Instant {
        Instant::now() + Duration::from_secs(60)
    }
    fn execute(
        &self,
        _: ResolvedConnect,
        _: HttpRequest,
        _: Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(async { Err(TransportError::definitely_unsent(TransportFailure::Offline)) })
    }
}
impl DuplexSpawner for NoNetwork {
    fn spawn(&self, _: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        Box::pin(async { Err(TransportError::definitely_unsent(TransportFailure::Offline)) })
    }
}

// Catches: brief mutates tracking/stamp, steals mail, or drifts from coord-resume's child/unread set.
#[test]
fn brief_uses_guarded_reads_and_matches_the_reference_child_and_unread_sets() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("Gits/.tmp/coordinator");
    let results = home.path().join("Gits/.tmp/results");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&results).unwrap();
    let children = state.join("children.tsv");
    std::fs::write(&children, CHILDREN).unwrap();
    let stamp = results.join(".watch-stamp");
    let old = results.join("old.md");
    let new = results.join("new.md");
    for (file, second) in [(&old, 1), (&stamp, 2), (&new, 3)] {
        std::fs::write(file, "RESULT recorded by example-coordinator").unwrap();
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(second)),
            )
            .unwrap();
    }
    let stamp_before = std::fs::metadata(&stamp).unwrap().modified().unwrap();
    let registry = tools(
        Some(std::ffi::OsStr::new("/recorded/tuic.sock")),
        home.path(),
    );
    let tool = registry
        .iter()
        .find(|tool| tool.descriptor().name == "orchestrator_brief")
        .unwrap();
    let root = CanonPath::new(std::fs::canonicalize(home.path()).unwrap());
    let ctx = PrepareCtx {
        cwd: root.clone(),
        resolver: &OsFs,
    };
    let call = tool.prepare(json!({}), &ctx).unwrap();
    let policy =
        PolicyEngine::new(root, Mode::Default, Ruleset::default()).with_coordinator_profile(true);
    let authorized = match policy.classify(call) {
        Classification::Settled(Ok(authorized)) => authorized,
        Classification::Ask(pending) => match block_on(policy.settle(pending)) {
            Resolution::Settled(Ok(authorized)) => authorized,
            _ => panic!("read-only coordinator brief must not ask"),
        },
        _ => panic!("unexpected denial"),
    };
    let (call, permit) = authorized.into_parts();
    // Catches: admitting both reads also authorizes an unprepared third process.
    let unprepared = SpawnSpec::new(
        vec![
            "tuic".into(),
            "mcp".into(),
            "agent".into(),
            r#"{"action":"inbox"}"#.into(),
        ],
        None,
        None,
        ego_core::permit::EnvPolicy::Inherited,
    )
    .unwrap();
    assert!(permit.allows_spawn(&unprepared).is_err());
    let evidence = SessionEvidenceStore::open(home.path(), SessionId::new(), 1024 * 1024).unwrap();
    let io = GuardedEffects::new(
        permit,
        &OsFs,
        &RecordedTuic,
        &evidence,
        Arc::new(NoNetwork),
        Arc::new(NoNetwork),
        Arc::new(NoNetwork),
    );
    let ToolExecution::Complete(answer) = block_on(tool.execute(call, &io)).unwrap() else {
        panic!("unexpected question");
    };
    let answer = answer.model_text();
    for name in [
        "example-worker-a",
        "example-worker-b",
        "example-worker-c",
        "example-worker-d",
        "example-reviewer",
        "example-coordinator",
    ] {
        assert!(answer.contains(name));
    }
    assert!(answer.contains(new.to_str().unwrap()));
    assert!(!answer.contains(old.to_str().unwrap()));
    assert!(!answer.contains("example-stale-a"));
    assert_eq!(std::fs::read_to_string(children).unwrap(), CHILDREN);
    assert_eq!(
        std::fs::metadata(stamp).unwrap().modified().unwrap(),
        stamp_before
    );
}
