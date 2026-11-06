//! The `lsp` tool's client against a recorded language server.
//!
//! `fixtures/lsp/gopls-0.22.0.jsonl` is every frame of one real exchange
//! with gopls v0.22.0 on `fixtures/lsp/workspace` (see `record.py` there).
//! The replay server reads each frame the client sends, checks it against
//! the recorded client frame, and answers with the recorded server frames,
//! so the client is tested against what a real server sent.

use std::path::Path;
use std::sync::Arc;

use ego_core::code_intel::{
    Answer, CodeIntel, CodeIntelRequest, Diagnostic, Location, Operation, Position,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    CleanupCertainty, DuplexEnd, DuplexHandle, DuplexSession, DuplexSettlement, DuplexSpawner,
    EnvPolicy, SpawnSpec, ToolResult, TransportError, TransportFailure,
};
use ego_tools::lsp::LspPool;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _};

const RECORDING: &str = include_str!("../fixtures/lsp/gopls-0.22.0.jsonl");

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp/workspace");
    for name in ["go.mod", "greet.go", "use.go"] {
        std::fs::copy(fixtures.join(name), dir.path().join(name)).expect("copy fixture");
    }
    dir
}

struct Replay {
    root: std::path::PathBuf,
    spawned: std::sync::Mutex<Vec<SpawnSpec>>,
    /// The client frames the replay server read, in order.
    seen: Arc<std::sync::Mutex<Vec<Value>>>,
    /// The replay server tasks, so a test can end a server.
    servers: std::sync::Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Replay {
    fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.canonicalize().unwrap(),
            spawned: std::sync::Mutex::new(Vec::new()),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            servers: std::sync::Mutex::new(Vec::new()),
        })
    }
}

struct Session(Compat<tokio::io::DuplexStream>);

impl futures::AsyncRead for Session {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl futures::AsyncWrite for Session {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_close(cx)
    }
}

fn settled(end: DuplexEnd) -> BoxFuture<'static, DuplexSettlement> {
    Box::pin(async move {
        DuplexSettlement::new(
            end,
            CleanupCertainty::Complete,
            DeliveryCertainty::DefinitelyUnsent,
            Ok(ToolResult::builtin("settled")),
        )
    })
}

impl DuplexSession for Session {
    fn delivery(&self) -> DeliveryCertainty {
        DeliveryCertainty::DefinitelyUnsent
    }
    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        settled(DuplexEnd::Exited { code: 0 })
    }
    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        settled(DuplexEnd::Interrupted)
    }
}

impl DuplexSpawner for Replay {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        self.spawned.lock().unwrap().push(spec);
        let (client, server) = tokio::io::duplex(1 << 20);
        let root_uri = url::Url::from_directory_path(&self.root)
            .unwrap()
            .to_string()
            .trim_end_matches('/')
            .to_owned();
        let seen = Arc::clone(&self.seen);
        let task = tokio::spawn(serve(server, root_uri, seen));
        self.servers.lock().unwrap().push(task.abort_handle());
        Box::pin(async move { Ok(Box::new(Session(client.compat())) as DuplexHandle) })
    }
}

async fn read(
    reader: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> Option<Value> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Play the recording: check each client frame, answer with the server's.
async fn serve(
    stream: tokio::io::DuplexStream,
    root_uri: String,
    seen: Arc<std::sync::Mutex<Vec<Value>>>,
) {
    let (read_half, mut write) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut ids = std::collections::HashMap::new();
    for line in RECORDING.lines() {
        let frame: Value =
            serde_json::from_str(&line.replace("file:///workspace", &root_uri)).unwrap();
        let mut message = frame["message"].clone();
        if frame["from"] == "client" {
            if message["method"] == "shutdown" {
                return;
            }
            let Some(actual) = read(&mut reader).await else {
                return;
            };
            assert_eq!(
                actual["method"], message["method"],
                "frame order differs from the recording"
            );
            if let (Some(recorded), Some(id)) = (message["id"].as_i64(), actual.get("id")) {
                ids.insert(recorded, id.clone());
            }
            seen.lock().unwrap().push(actual);
        } else {
            if message.get("method").is_none()
                && let Some(id) = message["id"].as_i64().and_then(|id| ids.get(&id))
            {
                message["id"] = id.clone();
            }
            let body = serde_json::to_vec(&message).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            if write.write_all(header.as_bytes()).await.is_err()
                || write.write_all(&body).await.is_err()
            {
                return;
            }
        }
    }
}

fn request(operation: Operation, path: &str, position: Option<(u32, u32)>) -> CodeIntelRequest {
    CodeIntelRequest {
        operation,
        path: path.into(),
        position: position.map(|(line, column)| Position { line, column }),
    }
}

/// The 1-based character column of `Greeting` on line 4 of `use.go`.
fn greeting_column() -> u32 {
    let line = "\ta := \"héllo😀\"; b := Greeting(a)";
    line[..line.find("Greeting").unwrap()].chars().count() as u32 + 1
}

// Catches: a definition or references position sent in characters instead
// of the UTF-16 units the server expects (the recorded server was asked at
// character 22 after an emoji), a document never opened before the request,
// or result locations not mapped back to workspace paths, lines and source.
#[tokio::test]
async fn diagnostics_definition_and_references_follow_the_recorded_gopls_exchange() {
    let dir = workspace();
    let replay = Replay::new(dir.path());
    let pool = LspPool::new(dir.path(), EnvPolicy::Inherited);
    let spawner: Arc<dyn DuplexSpawner> = replay.clone();

    let diagnostics = pool
        .query(
            request(Operation::Diagnostics, "use.go", None),
            Arc::clone(&spawner),
        )
        .await
        .unwrap();
    assert_eq!(
        diagnostics,
        Answer::Diagnostics {
            published: true,
            items: vec![Diagnostic {
                line: 9,
                column: 9,
                severity: Some("error"),
                source: Some("compiler".into()),
                code: Some("UndeclaredName".into()),
                message: "undefined: Missing".into(),
            }],
        }
    );

    let at = (4, greeting_column());
    let definition = pool
        .query(
            request(Operation::Definition, "use.go", Some(at)),
            Arc::clone(&spawner),
        )
        .await
        .unwrap();
    let declaration = Location {
        path: "greet.go".into(),
        line: 4,
        column: 6,
        text: Some("func Greeting(name string) string {".into()),
    };
    assert_eq!(definition, Answer::Locations(vec![declaration.clone()]));

    let references = pool
        .query(
            request(Operation::References, "use.go", Some(at)),
            Arc::clone(&spawner),
        )
        .await
        .unwrap();
    let Answer::Locations(references) = references else {
        panic!("references answer locations");
    };
    assert_eq!(references[0], declaration);
    assert_eq!(
        (
            references[1].path.as_str(),
            references[1].line,
            references[1].column
        ),
        ("use.go", 4, greeting_column())
    );
    assert_eq!(
        (
            references[2].path.as_str(),
            references[2].line,
            references[2].column
        ),
        ("use.go", 5, 12)
    );

    let seen = replay.seen.lock().unwrap().clone();
    let positions: Vec<&Value> = seen
        .iter()
        .filter(|frame| {
            frame["method"] == "textDocument/definition"
                || frame["method"] == "textDocument/references"
        })
        .map(|frame| &frame["params"]["position"])
        .collect();
    assert_eq!(
        positions,
        vec![&serde_json::json!({"line": 3, "character": 22}); 2]
    );
    let opened = seen
        .iter()
        .find(|frame| frame["method"] == "textDocument/didOpen")
        .expect("the document was opened");
    assert_eq!(
        opened["params"]["textDocument"]["text"],
        std::fs::read_to_string(dir.path().join("use.go")).unwrap()
    );
    // One server for the session: the second and third query reused it.
    assert_eq!(replay.spawned.lock().unwrap().len(), 1);
    let spec = replay.spawned.lock().unwrap()[0].clone();
    assert_eq!(spec.argv(), ["gopls"]);
    assert_eq!(
        spec.cwd().map(|cwd| cwd.as_path().to_path_buf()),
        Some(dir.path().canonicalize().unwrap())
    );
}

// Catches: a server keyed to the spawner's address. The interactive driver
// builds new effect ports, so a new spawner, for every turn
// (driver_runtime.rs "Per turn"), so the language server is started and
// indexes the workspace again at every prompt.
#[tokio::test]
async fn a_later_turns_spawner_reuses_the_running_server() {
    let dir = workspace();
    let pool = LspPool::new(dir.path(), EnvPolicy::Inherited);
    let first = Replay::new(dir.path());
    let second = Replay::new(dir.path());
    pool.query(
        request(Operation::Diagnostics, "use.go", None),
        first.clone(),
    )
    .await
    .unwrap();
    let definition = pool
        .query(
            request(
                Operation::Definition,
                "use.go",
                Some((4, greeting_column())),
            ),
            second.clone(),
        )
        .await;
    assert!(
        matches!(&definition, Ok(Answer::Locations(locations)) if locations.len() == 1),
        "{definition:?}"
    );
    assert_eq!(
        second.spawned.lock().unwrap().len(),
        0,
        "the next turn started the language server again"
    );
}

// Catches: a server that exited (a crash, or the interrupt that ends the
// processes of its turn) kept in the pool, so every later query of an
// unchanged file fails with "the language server exited" for the rest of
// the session instead of starting the server again.
#[tokio::test]
async fn a_server_that_exited_is_started_again_at_the_next_query() {
    let dir = workspace();
    let pool = LspPool::new(dir.path(), EnvPolicy::Inherited);
    let replay = Replay::new(dir.path());
    pool.query(
        request(Operation::Diagnostics, "use.go", None),
        replay.clone(),
    )
    .await
    .unwrap();
    for server in replay.servers.lock().unwrap().iter() {
        server.abort();
    }
    // Let the client's reader see the end of the stream.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let definition = pool
        .query(
            request(
                Operation::Definition,
                "use.go",
                Some((4, greeting_column())),
            ),
            replay.clone(),
        )
        .await;
    assert!(
        matches!(&definition, Ok(Answer::Locations(locations)) if locations.len() == 1),
        "{definition:?}"
    );
    assert_eq!(replay.spawned.lock().unwrap().len(), 2);
}

/// A spawner that never starts anything.
struct Refusing(std::sync::Mutex<usize>);

impl DuplexSpawner for Refusing {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        *self.0.lock().unwrap() += 1;
        Box::pin(async { Err(TransportError::definitely_unsent(TransportFailure::Offline)) })
    }
}

// Catches: the model reaching a file outside the workspace through the
// server (the server reads whatever it is opened on), or a file with no
// known server starting some process anyway.
#[tokio::test]
async fn files_outside_the_workspace_or_without_a_server_start_nothing() {
    let dir = workspace();
    let outside = tempfile::NamedTempFile::with_suffix(".go").unwrap();
    let pool = LspPool::new(dir.path(), EnvPolicy::Inherited);
    let spawner = Arc::new(Refusing(std::sync::Mutex::new(0)));

    for (path, expected) in [
        (
            outside.path().to_str().unwrap().to_owned(),
            "outside the workspace",
        ),
        (
            "../".to_owned() + outside.path().file_name().unwrap().to_str().unwrap(),
            "outside the workspace",
        ),
        ("go.mod".to_owned(), "no language server is known"),
    ] {
        let error = pool
            .query(
                request(Operation::Diagnostics, &path, None),
                spawner.clone(),
            )
            .await
            .unwrap_err();
        assert!(error.contains(expected), "{path}: {error}");
    }
    assert_eq!(
        *spawner.0.lock().unwrap(),
        0,
        "a refused file started a server"
    );
}

// Catches: a server start refused by the run's confinement (here the
// offline posture) reported as an empty answer instead of the refusal.
#[tokio::test]
async fn a_confinement_refusal_of_the_server_start_reaches_the_model() {
    let dir = workspace();
    let pool = LspPool::new(dir.path(), EnvPolicy::Inherited);
    let spawner = Arc::new(Refusing(std::sync::Mutex::new(0)));
    let error = pool
        .query(
            request(Operation::Diagnostics, "use.go", None),
            spawner.clone(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("gopls could not start"), "{error}");
    assert_eq!(*spawner.0.lock().unwrap(), 1);
}

/// Real gopls through the run's guarded spawner, once confined to the
/// workspace and once unconfined. Needs gopls on PATH outside the workspace
/// and outside the system directories, so it runs only on request:
/// `cargo nextest run -p ego-tools --run-ignored only -E 'test(live_gopls)'`.
// Catches: a language server that escapes the run's confinement (it starts
// although the sandbox refuses its executable), or a pool that works only
// with the in-memory replay and not with a real server.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs gopls on PATH outside the system directories"]
async fn live_gopls_is_confined_like_a_command_and_answers_when_admitted() {
    use ego_core::event::id::SessionId;
    use ego_core::permit::{CanonPath, Confinement, RunConfinement};
    use ego_core::turn::CancelToken;
    use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts};

    for level in [Confinement::Workspace, Confinement::Off] {
        let dir = workspace();
        let root = dir.path().canonicalize().unwrap();
        let confinement = RunConfinement::new(level, false, CanonPath::new(&root)).unwrap();
        let evidence_root = tempfile::tempdir().unwrap();
        let evidence = ego_tools::store::SessionEvidenceStore::open(
            evidence_root.path(),
            SessionId::new(),
            64 * 1024 * 1024,
        )
        .unwrap();
        let config = ExecRunConfig::new(
            confinement,
            evidence,
            CancelToken::new(),
            ExecLimits::default(),
            HttpCallTimeout::default(),
            None,
            Arc::new(ego_tools::http::SystemResolver::new()),
        );
        let (ports, _, _) = RunEffectPorts::for_run(config).await.unwrap();
        let pool = LspPool::new(&root, EnvPolicy::Inherited);
        let spawner = ports.duplex_handle();

        let diagnostics = pool
            .query(
                request(Operation::Diagnostics, "use.go", None),
                Arc::clone(&spawner),
            )
            .await;
        if level == Confinement::Workspace {
            let error = diagnostics.unwrap_err();
            assert!(
                error.contains("sandbox denies execution") && error.contains("exec_paths"),
                "{error}"
            );
            continue;
        }
        assert!(
            matches!(&diagnostics, Ok(Answer::Diagnostics { published: true, items }) if items.iter().any(|item| item.message == "undefined: Missing")),
            "{diagnostics:?}"
        );
        let definition = pool
            .query(
                request(
                    Operation::Definition,
                    "use.go",
                    Some((4, greeting_column())),
                ),
                Arc::clone(&spawner),
            )
            .await;
        assert_eq!(
            definition.unwrap(),
            Answer::Locations(vec![Location {
                path: "greet.go".into(),
                line: 4,
                column: 6,
                text: Some("func Greeting(name string) string {".into()),
            }])
        );
    }
}
