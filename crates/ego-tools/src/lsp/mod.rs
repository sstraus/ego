//! The language servers of one session, behind [`ego_core::code_intel::CodeIntel`].
//!
//! A server starts at the first query for its language, through the run's
//! guarded duplex spawner, so it runs under the run's confinement like an
//! MCP stdio server. It stays up for the session; dropping the pool drops
//! the connection, which terminates the server's process tree.

mod client;
mod text;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ego_core::code_intel::{
    Answer, CodeIntel, CodeIntelRequest, Diagnostic, Location, MAX_ITEMS, Operation,
};
use ego_core::permit::{CanonPath, DuplexSpawner, EnvPolicy, SpawnSpec};
use futures::future::BoxFuture;
use serde_json::{Value, json};

use client::Client;

const INITIALIZE: Duration = Duration::from_secs(60);
const REQUEST: Duration = Duration::from_secs(30);
const DIAGNOSTICS: Duration = Duration::from_secs(20);
const SETTLE: Duration = Duration::from_millis(500);
/// The largest source file sent to a server.
const MAX_FILE: u64 = 4 * 1024 * 1024;

struct Running {
    client: Arc<Client>,
    /// Document URI to (version, text) as the server last saw it.
    documents: HashMap<String, (i64, String)>,
}

/// The language servers of one session; clones share them.
#[derive(Clone)]
pub struct LspPool {
    inner: Arc<Pool>,
}

struct Pool {
    root: PathBuf,
    environment: EnvPolicy,
    servers: tokio::sync::Mutex<HashMap<&'static [&'static str], Running>>,
}

impl LspPool {
    /// `root` is the workspace; `environment` is the exec tool's.
    pub fn new(root: &Path, environment: EnvPolicy) -> Self {
        Self {
            inner: Arc::new(Pool {
                root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
                environment,
                servers: tokio::sync::Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl CodeIntel for LspPool {
    fn query(
        &self,
        request: CodeIntelRequest,
        spawner: Arc<dyn DuplexSpawner>,
    ) -> BoxFuture<'static, Result<Answer, String>> {
        let pool = Arc::clone(&self.inner);
        Box::pin(async move { pool.answer(request, spawner).await })
    }
}

impl Pool {
    async fn answer(
        &self,
        request: CodeIntelRequest,
        spawner: Arc<dyn DuplexSpawner>,
    ) -> Result<Answer, String> {
        let path = self.resolve(&request.path)?;
        let (server, language) = text::server_for(&path).ok_or_else(|| {
            format!(
                "no language server is known for {}; lsp supports {}",
                request.path,
                text::SUPPORTED
            )
        })?;
        let source = read_source(&path)?;
        let uri = url::Url::from_file_path(&path)
            .map_err(|()| format!("{} has no file URI", path.display()))?
            .to_string();

        let mut servers = self.servers.lock().await;
        // A server that exited (a crash, or its turn's cancellation ended
        // it) starts again at this query instead of failing every later one.
        if servers
            .get(server.argv)
            .is_some_and(|running| running.client.is_closed())
        {
            servers.remove(server.argv);
        }
        if !servers.contains_key(server.argv) {
            let client = self.start(server.argv, spawner).await?;
            servers.insert(
                server.argv,
                Running {
                    client: Arc::new(client),
                    documents: HashMap::new(),
                },
            );
        }
        let running = servers.get_mut(server.argv).expect("inserted above");
        let before = running.client.publishes();
        let synced = sync(running, &uri, language, &source).await;
        if let Err(error) = synced {
            servers.remove(server.argv);
            return Err(error);
        }
        let changed = synced.expect("checked above");
        let client = Arc::clone(&running.client);
        drop(servers);

        match request.operation {
            Operation::Diagnostics => {
                let published = match (changed, client.last_published(&uri)) {
                    (false, Some(published)) => Some(published),
                    _ => {
                        client
                            .diagnostics_after(&uri, before, DIAGNOSTICS, SETTLE)
                            .await
                    }
                };
                Ok(match published {
                    Some(published) => Answer::Diagnostics {
                        published: true,
                        items: published
                            .items
                            .iter()
                            .filter_map(|item| diagnostic(item, &source))
                            .collect(),
                    },
                    None => Answer::Diagnostics {
                        published: false,
                        items: Vec::new(),
                    },
                })
            }
            Operation::Definition | Operation::References => {
                let at = request.position.expect("parse requires a position");
                let line = text::line_of(&source, at.line as usize - 1)
                    .ok_or_else(|| format!("{} has no line {}", request.path, at.line))?;
                let mut params = json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": at.line - 1, "character": text::utf16_offset(line, at.column)},
                });
                let method = if request.operation == Operation::Definition {
                    "textDocument/definition"
                } else {
                    params["context"] = json!({"includeDeclaration": true});
                    "textDocument/references"
                };
                let result = client.request(method, params, REQUEST).await?;
                Ok(Answer::Locations(self.locations(&result)))
            }
        }
    }

    /// The asked file, which must be inside the workspace.
    fn resolve(&self, asked: &str) -> Result<PathBuf, String> {
        let joined = self.root.join(asked);
        let path = joined
            .canonicalize()
            .map_err(|error| format!("{asked}: {error}"))?;
        if !path.starts_with(&self.root) {
            return Err(format!(
                "{asked} is outside the workspace; lsp answers only for workspace files"
            ));
        }
        Ok(path)
    }

    async fn start(
        &self,
        argv: &'static [&'static str],
        spawner: Arc<dyn DuplexSpawner>,
    ) -> Result<Client, String> {
        let spec = SpawnSpec::new(
            argv.iter()
                .map(|part| std::ffi::OsString::from(*part))
                .collect(),
            Some(CanonPath::new(&self.root)),
            None,
            self.environment.clone(),
        )
        .map_err(|error| error.to_string())?;
        let stream = spawner.spawn(spec).await.map_err(|error| {
            format!(
                "the language server {} could not start: {error}. lsp needs the server on PATH and runnable under this session's sandbox; otherwise turn lsp off",
                argv[0]
            )
        })?;
        let client = Client::start(stream);
        let root = url::Url::from_directory_path(&self.root)
            .map_err(|()| "the workspace has no file URI".to_owned())?
            .to_string();
        let root = root.trim_end_matches('/').to_owned();
        client
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root,
                    "workspaceFolders": [{"uri": root, "name": "workspace"}],
                    "capabilities": {
                        "general": {"positionEncodings": ["utf-16"]},
                        "textDocument": {"publishDiagnostics": {}, "definition": {}, "references": {}},
                    },
                }),
                INITIALIZE,
            )
            .await
            .map_err(|error| format!("{} did not initialize: {error}", argv[0]))?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    fn locations(&self, result: &Value) -> Vec<Location> {
        let entries: Vec<&Value> = match result {
            Value::Array(entries) => entries.iter().collect(),
            Value::Object(_) => vec![result],
            _ => Vec::new(),
        };
        let mut files: HashMap<PathBuf, Option<String>> = HashMap::new();
        entries
            .into_iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                // Location, or LocationLink when a server sends links anyway.
                let uri = entry
                    .get("uri")
                    .or_else(|| entry.get("targetUri"))?
                    .as_str()?;
                let start = entry
                    .pointer("/range/start")
                    .or_else(|| entry.pointer("/targetSelectionRange/start"))?;
                let line = start.get("line")?.as_u64()? as u32;
                let offset = start.get("character")?.as_u64()? as u32;
                let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
                let inside = path.starts_with(&self.root);
                // Read source lines only for the entries the answer can show.
                let source = (inside && index < MAX_ITEMS)
                    .then(|| {
                        files
                            .entry(path.clone())
                            .or_insert_with(|| read_source(&path).ok())
                            .clone()
                    })
                    .flatten();
                let source_line = source
                    .as_deref()
                    .and_then(|text| text::line_of(text, line as usize));
                Some(Location {
                    path: if inside {
                        path.strip_prefix(&self.root)
                            .unwrap_or(&path)
                            .display()
                            .to_string()
                    } else {
                        path.display().to_string()
                    },
                    line: line + 1,
                    column: source_line.map_or(offset + 1, |text| text::char_column(text, offset)),
                    text: source_line.map(str::to_owned),
                })
            })
            .collect()
    }
}

/// Open the document, or send its new text when it changed on disk.
/// Returns whether the server saw a change.
async fn sync(
    running: &mut Running,
    uri: &str,
    language: &str,
    source: &str,
) -> Result<bool, String> {
    match running.documents.get(uri) {
        Some((_, known)) if known == source => Ok(false),
        Some((version, _)) => {
            let version = version + 1;
            running
                .client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": [{"text": source}],
                    }),
                )
                .await?;
            running
                .documents
                .insert(uri.to_owned(), (version, source.to_owned()));
            Ok(true)
        }
        None => {
            running
                .client
                .notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language, "version": 1, "text": source}}),
                )
                .await?;
            running
                .documents
                .insert(uri.to_owned(), (1, source.to_owned()));
            Ok(true)
        }
    }
}

fn read_source(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    if size > MAX_FILE {
        return Err(format!(
            "{} has {size} bytes; lsp sends files of at most {MAX_FILE} bytes",
            path.display()
        ));
    }
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn diagnostic(item: &Value, source: &str) -> Option<Diagnostic> {
    let start = item.pointer("/range/start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let offset = start.get("character")?.as_u64()? as u32;
    let column = text::line_of(source, line as usize)
        .map_or(offset + 1, |text| text::char_column(text, offset));
    Some(Diagnostic {
        line: line + 1,
        column,
        severity: match item.get("severity").and_then(Value::as_u64) {
            Some(1) => Some("error"),
            Some(2) => Some("warning"),
            Some(3) => Some("information"),
            Some(4) => Some("hint"),
            _ => None,
        },
        source: item
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        code: match item.get("code") {
            Some(Value::String(code)) => Some(code.clone()),
            Some(Value::Number(code)) => Some(code.to_string()),
            _ => None,
        },
        message: item.get("message")?.as_str()?.to_owned(),
    })
}
