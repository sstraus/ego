//! Running the real binary against a fake provider.
//!
//! Two things this harness refuses to do, and both are the point.
//!
//! **It never reaches the network.** Every test here points `OLLAMA_HOST` and
//! `OPENAI_BASE_URL` at a loopback socket this process owns. A paid call is not
//! an ordinary gate — a suite that bills someone is a suite people stop
//! running — and a stub also lets a test say "the daemon answered 500", which
//! no real provider will do on request.
//!
//! **It never calls a function in `ego-cli`.** These tests run
//! `CARGO_BIN_EXE_ego` as a child process, because the thing under test is the
//! process: an exit status is not observable from inside the program that
//! produces it, and neither is "stdout carried this and stderr carried that".

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the stub answers a `/v1/chat/completions` request with.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A whole SSE body, written in one go.
    Sse(String),
    /// An SSE body in two parts with a pause between them.
    ///
    /// This is what makes liveness testable from outside: the consumer has a
    /// window in which it has seen `head` and cannot yet have seen the end of
    /// the turn, so "an event reached stdout before the turn that produced it
    /// ended" becomes an observation rather than an assumption.
    Paced {
        head: String,
        tail: String,
        pause: Duration,
    },
    /// An HTTP failure, for the provider-error paths.
    Status(u16),
}

/// What the fake daemon knows.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The names `/api/tags` reports as installed.
    pub installed: Vec<String>,
    /// The context `/api/ps` reports as served. `None` makes the model look
    /// permanently unloaded, which is the `Undiscoverable` path.
    pub context_length: Option<u64>,
    /// Consumed in order, one per generation. The last is repeated once the
    /// list runs out, so a test that does not care how many turns happen does
    /// not have to count them.
    pub completions: Vec<Reply>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            installed: vec![MODEL.to_string()],
            context_length: Some(32_768),
            completions: vec![Reply::Sse(text("It is done."))],
        }
    }
}

/// The model every test asks for unless it is testing what happens when it
/// asks for another.
pub const MODEL: &str = "qwen3";

/// A complete SSE transcript that answers with `content` and stops.
///
/// Shaped after `crates/ego-ai/tests/goldens/stream_text.sse`, which was itself
/// pinned from `openai/openai-openapi` rather than from memory: usage arrives
/// *after* the chunk carrying `finish_reason`, and `[DONE]` last.
pub fn text(content: &str) -> String {
    costing(content, 10, 4)
}

/// The same, reporting the usage a test needs it to.
///
/// The counts are a *parameter* because two of `CLI.md`'s contracts are about
/// them and neither can be reached with a fixed transcript: `usage` in §4.1's
/// object, and §4.5's "budget beats schema-retry" — which needs a turn
/// expensive enough that the next one crosses a ceiling.
pub fn costing(content: &str, prompt_tokens: u64, completion_tokens: u64) -> String {
    let escaped = serde_json::to_string(content).expect("a string is serialisable");
    let total = prompt_tokens + completion_tokens;
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{escaped}}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":{completion_tokens},\"total_tokens\":{total}}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// The same, split so the first delta is deliverable well before the end.
pub fn paced(content: &str, pause: Duration) -> Reply {
    let escaped = serde_json::to_string(content).expect("a string is serialisable");
    Reply::Paced {
        head: format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{escaped}}},\"finish_reason\":null}}]}}\n\n"
        ),
        tail: "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}\n\n\
               data: [DONE]\n\n"
            .to_string(),
        pause,
    }
}

/// An SSE transcript asking for one tool call and stopping.
pub fn tool_call(id: &str, name: &str, arguments: &str) -> String {
    let arguments = serde_json::to_string(arguments).expect("a string is serialisable");
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// One request the stub was sent.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub path: String,
    pub body: String,
}

/// A fake Ollama/OpenAI daemon on loopback.
///
/// The listener is dropped when the `Stub` is, which ends the serving thread on
/// its next `accept`. Tests therefore keep it alive for as long as the child
/// runs, which the borrow checker enforces for them.
pub struct Stub {
    pub address: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    pub fn start(plan: Plan) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback is bindable");
        let address = listener
            .local_addr()
            .expect("a bound listener has an address")
            .to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let thread_seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut generation = 0usize;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                // One request per connection, then closed. Keep-alive would
                // buy nothing here and would make the reply logic guess where
                // one exchange ends.
                if serve(stream, &plan, &thread_seen, &mut generation).is_err() {
                    break;
                }
            }
        });

        Stub { address, seen }
    }

    /// Every request the stub has been sent, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().expect("no writer panics").clone()
    }

    /// The bodies posted to the generation endpoint, parsed.
    pub fn generations(&self) -> Vec<serde_json::Value> {
        self.requests()
            .into_iter()
            .filter(|req| req.path.contains("chat/completions"))
            .map(|req| serde_json::from_str(&req.body).expect("the request body is JSON"))
            .collect()
    }
}

fn serve(
    mut stream: TcpStream,
    plan: &Plan,
    seen: &Arc<Mutex<Vec<Recorded>>>,
    generation: &mut usize,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    seen.lock().expect("no writer panics").push(Recorded {
        path: path.clone(),
        body,
    });

    match path.as_str() {
        "/api/tags" => {
            let models: Vec<serde_json::Value> = plan
                .installed
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "capabilities": ["completion", "tools"],
                    })
                })
                .collect();
            json(&mut stream, &serde_json::json!({ "models": models }))
        }
        "/api/ps" => {
            let models: Vec<serde_json::Value> = plan
                .context_length
                .map(|context| serde_json::json!({ "name": MODEL, "context_length": context }))
                .into_iter()
                .collect();
            json(&mut stream, &serde_json::json!({ "models": models }))
        }
        "/api/chat" => json(&mut stream, &serde_json::json!({ "done_reason": "load" })),
        _ => {
            let reply = plan
                .completions
                .get(*generation)
                .or_else(|| plan.completions.last())
                .cloned()
                .unwrap_or(Reply::Status(500));
            *generation += 1;
            sse(&mut stream, reply)
        }
    }
}

fn json(stream: &mut TcpStream, value: &serde_json::Value) -> std::io::Result<()> {
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// An SSE response, delimited by the connection closing.
///
/// No `content-length`, so the client reads until EOF — which is what lets the
/// paced variant stop mid-stream and resume without knowing the total up front.
fn sse(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Status(status) => {
            let body = format!("the stub was told to answer {status}");
            write!(
                stream,
                "HTTP/1.1 {status} Stub\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()
        }
        Reply::Sse(body) => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            )?;
            stream.flush()
        }
        Reply::Paced { head, tail, pause } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{head}"
            )?;
            stream.flush()?;
            std::thread::sleep(pause);
            write!(stream, "{tail}")?;
            stream.flush()
        }
    }
}

/// A home and a workspace two invocations can share.
///
/// Needed because `--session NAME` is only meaningful across runs: attaching to
/// a named session in a *different* workspace is a different session by design,
/// so a test that gives each run its own directories cannot observe a resume at
/// all.
pub struct Places {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Places {
    pub fn new() -> Self {
        Places {
            home: tempfile::tempdir().expect("a temp dir"),
            workspace: tempfile::tempdir().expect("a temp dir"),
        }
    }

    pub fn ego(&self, stub: &Stub) -> Ego {
        Ego::at(self.home.path(), self.workspace.path(), Some(stub))
    }
}

/// An invocation of the real binary, in a workspace of its own.
pub struct Ego {
    home: PathBuf,
    workspace: PathBuf,
    /// Kept alive for as long as the invocation is described, so a builder
    /// that owns its directories does not delete them before it runs.
    keep: Vec<tempfile::TempDir>,
    /// Which of `CLI.md` §2's commands this invocation runs. `run` unless a
    /// test says otherwise, because almost every test here is about a turn.
    subcommand: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    stdin: Option<String>,
}

/// What a finished invocation left behind.
#[derive(Debug)]
pub struct Finished {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Finished {
    /// The single object `--output json` writes.
    pub fn report(&self) -> serde_json::Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|err| panic!("stdout was not one JSON object ({err}): {}", self.stdout))
    }

    /// The events `--output jsonl` wrote, one per line.
    pub fn events(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|err| panic!("`{line}` is not JSON: {err}"))
            })
            .collect()
    }

    /// The `type` of every event, in order.
    pub fn kinds(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| {
                event
                    .get("type")
                    .and_then(|kind| kind.as_str())
                    .map(str::to_string)
            })
            .collect()
    }
}

impl Ego {
    /// A run against `stub`, in an empty workspace, with an empty home.
    pub fn against(stub: &Stub) -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        let workspace = tempfile::tempdir().expect("a temp dir");
        let mut ego = Ego::at(home.path(), workspace.path(), Some(stub));
        ego.keep = vec![home, workspace];
        ego
    }

    /// A run with no provider at all, for the paths that refuse before one is
    /// reached.
    pub fn nowhere() -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        let workspace = tempfile::tempdir().expect("a temp dir");
        let mut ego = Ego::at(home.path(), workspace.path(), None);
        ego.keep = vec![home, workspace];
        ego
    }

    /// A run in directories somebody else owns.
    pub fn at(home: &Path, workspace: &Path, stub: Option<&Stub>) -> Self {
        let mut env = HashMap::new();
        if let Some(stub) = stub {
            env.insert("OLLAMA_HOST".to_string(), stub.address.clone());
            env.insert(
                "OPENAI_BASE_URL".to_string(),
                format!("http://{}", stub.address),
            );
        }
        if stub.is_none() {
            // Never the machine's own daemon. A run this harness builds must
            // not be able to reach a provider nobody in the test asked for:
            // port 1 is reserved, so the connection is refused rather than
            // quietly answered by whatever the developer has installed.
            env.insert("OLLAMA_HOST".to_string(), "127.0.0.1:1".to_string());
        }
        Ego {
            home: home.to_path_buf(),
            workspace: workspace.to_path_buf(),
            keep: Vec::new(),
            subcommand: "run".to_string(),
            args: Vec::new(),
            env,
            stdin: None,
        }
    }

    /// Run `ego <name>` rather than `ego run`.
    pub fn subcommand(mut self, name: &str) -> Self {
        self.subcommand = name.to_string();
        self
    }

    /// Run `ego` with no subcommand at all — the conversational surface.
    ///
    /// Not `subcommand("")`: an empty argument is a *first argument*, and
    /// `ego ""` is a run with an empty prompt rather than a session. The
    /// distinction is the whole of what this method exists to make.
    pub fn conversing(mut self) -> Self {
        self.subcommand.clear();
        self
    }

    pub fn arg(mut self, arg: &str) -> Self {
        self.args.push(arg.to_string());
        self
    }

    pub fn args<const N: usize>(mut self, args: [&str; N]) -> Self {
        self.args.extend(args.iter().map(|arg| arg.to_string()));
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    pub fn stdin(mut self, text: &str) -> Self {
        self.stdin = Some(text.to_string());
        self
    }

    /// Write a file into the workspace and return its path.
    pub fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.workspace.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a temp dir is writable");
        }
        std::fs::write(&path, contents).expect("a temp dir is writable");
        path
    }

    /// Write a file into `$EGO_HOME` and return its path.
    pub fn home_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.join(name);
        std::fs::write(&path, contents).expect("a temp dir is writable");
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
        // Cleared and rebuilt: a developer with `EGO_MODEL` set would otherwise
        // get different answers from the same test, and a suite whose result
        // depends on the shell that started it proves nothing.
        command.env_clear();
        command.env("HOME", &self.home);
        command.env("EGO_HOME", &self.home);
        command.env("PATH", "/usr/bin:/bin");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command.current_dir(&self.workspace);
        if !self.subcommand.is_empty() {
            command.arg(&self.subcommand);
        }
        command.args(&self.args);
        command
    }

    /// Run to completion.
    pub fn run(self) -> Finished {
        let mut command = self.command();
        command.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().expect("the binary was built");
        if let Some(text) = &self.stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin was piped")
                .write_all(text.as_bytes())
                .expect("the child is reading");
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().expect("the child ran");
        Finished {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Start the binary and hand back the child, for the tests that have to
    /// observe it while it is still running.
    pub fn spawn(self) -> Running {
        let mut command = self.command();
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let child = command.spawn().expect("the binary was built");
        Running {
            child,
            _keep: self.keep,
        }
    }
}

/// A binary that has not finished yet.
pub struct Running {
    pub child: Child,
    _keep: Vec<tempfile::TempDir>,
}

impl Running {
    pub fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("stdout was piped"))
    }

    /// Whether the process is still running *now*.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn finish(mut self) -> Finished {
        let mut stderr = String::new();
        if let Some(mut handle) = self.child.stderr.take() {
            let _ = handle.read_to_string(&mut stderr);
        }
        let status = self.child.wait().expect("the child ran");
        Finished {
            code: status.code(),
            stdout: String::new(),
            stderr,
        }
    }
}
