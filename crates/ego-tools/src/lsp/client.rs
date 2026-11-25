//! One language server connection: JSON-RPC 2.0 with `Content-Length`
//! framing over the server's stdio (LSP 3.17 "Base Protocol").

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use futures::{AsyncRead, AsyncWrite};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

/// The largest frame a server may send. A reference list of a large code
/// base fits; a broken server cannot make the client allocate without bound.
const MAX_FRAME: usize = 16 * 1024 * 1024;

type Writer = Box<dyn AsyncWrite + Send + Unpin>;
type Reply = oneshot::Sender<Result<Value, String>>;

/// The diagnostics a server last published for one document.
#[derive(Debug, Clone)]
pub(crate) struct Published {
    /// Counts every publish of this connection, so a caller can wait for one
    /// that came after its own change.
    pub(crate) generation: u64,
    pub(crate) items: Vec<Value>,
}

#[derive(Default)]
struct Shared {
    pending: HashMap<i64, Reply>,
    diagnostics: HashMap<String, Published>,
    publishes: u64,
    closed: Option<String>,
}

pub(crate) struct Client {
    writer: Arc<tokio::sync::Mutex<Writer>>,
    shared: Arc<Mutex<Shared>>,
    published: Arc<Notify>,
    next_id: AtomicI64,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // The reader holds the read half; ending it drops the last owner of
        // the process stdio, which terminates the server's process tree.
        self.reader.abort();
    }
}

impl Client {
    pub(crate) fn start(stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static) -> Self {
        let (read, write) = stream.split();
        let writer: Arc<tokio::sync::Mutex<Writer>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(write)));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let published = Arc::new(Notify::new());
        let reader = tokio::spawn(read_loop(
            BufReader::new(read),
            Arc::clone(&writer),
            Arc::clone(&shared),
            Arc::clone(&published),
        ));
        Self {
            writer,
            shared,
            published,
            next_id: AtomicI64::new(1),
            reader,
        }
    }

    /// Send a request and wait for its response.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        {
            let mut shared = self.shared.lock().expect("lsp state");
            if let Some(reason) = &shared.closed {
                return Err(reason.clone());
            }
            shared.pending.insert(id, reply);
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = write_frame(&self.writer, &message).await {
            self.shared.lock().expect("lsp state").pending.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the language server connection closed".into()),
            Err(_) => {
                self.shared.lock().expect("lsp state").pending.remove(&id);
                Err(format!(
                    "the language server did not answer {method} within {} seconds",
                    timeout.as_secs()
                ))
            }
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        if let Some(reason) = &self.shared.lock().expect("lsp state").closed {
            return Err(reason.clone());
        }
        write_frame(
            &self.writer,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    /// Whether the connection ended: the server exited or broke the protocol.
    pub(crate) fn is_closed(&self) -> bool {
        self.shared.lock().expect("lsp state").closed.is_some()
    }

    /// The publish count now; pass it to [`Self::diagnostics_after`].
    pub(crate) fn publishes(&self) -> u64 {
        self.shared.lock().expect("lsp state").publishes
    }

    /// The diagnostics last published for `uri`, if any.
    pub(crate) fn last_published(&self, uri: &str) -> Option<Published> {
        self.shared
            .lock()
            .expect("lsp state")
            .diagnostics
            .get(uri)
            .cloned()
    }

    /// Wait up to `wait` for a publish for `uri` newer than `after`, then a
    /// short quiet period for a follow-up publish (servers often publish the
    /// syntax pass first and the semantic pass soon after).
    pub(crate) async fn diagnostics_after(
        &self,
        uri: &str,
        after: u64,
        wait: Duration,
        settle: Duration,
    ) -> Option<Published> {
        let deadline = tokio::time::Instant::now() + wait;
        let newer = |client: &Self| {
            client
                .last_published(uri)
                .filter(|published| published.generation > after)
        };
        let mut found = None;
        while found.is_none() {
            let notified = self.published.notified();
            found = newer(self);
            if found.is_some() || self.shared.lock().expect("lsp state").closed.is_some() {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return newer(self);
            }
        }
        let mut latest = found?;
        loop {
            let notified = self.published.notified();
            if let Some(next) = newer(self).filter(|next| next.generation > latest.generation) {
                latest = next;
                continue;
            }
            if tokio::time::timeout(settle, notified).await.is_err() {
                return Some(latest);
            }
        }
    }
}

async fn write_frame(writer: &tokio::sync::Mutex<Writer>, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).expect("JSON value serializes");
    let mut writer = writer.lock().await;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let sent = async {
        writer.write_all(header.as_bytes()).await?;
        writer.write_all(&body).await?;
        writer.flush().await
    };
    sent.await
        .map_err(|error| format!("cannot write to the language server: {error}"))
}

/// Read one frame, or `None` at a clean end of stream.
pub(crate) async fn read_frame(
    reader: &mut (impl futures::AsyncBufRead + Unpin),
) -> Result<Option<Value>, String> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|error| format!("cannot read from the language server: {error}"))?;
        if read == 0 {
            return if length.is_none() {
                Ok(None)
            } else {
                Err("the language server closed inside a frame header".into())
            };
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length =
                Some(value.trim().parse::<usize>().map_err(|_| {
                    format!("the language server sent a bad frame header: {header}")
                })?);
        }
    }
    let length = length.ok_or("the language server sent a frame without Content-Length")?;
    if length > MAX_FRAME {
        return Err(format!(
            "the language server sent a frame of {length} bytes, over the {MAX_FRAME} byte limit"
        ));
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| format!("the language server closed inside a frame: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("the language server sent invalid JSON: {error}"))
}

async fn read_loop(
    mut reader: BufReader<futures::io::ReadHalf<impl AsyncRead + AsyncWrite + Send + Unpin>>,
    writer: Arc<tokio::sync::Mutex<Writer>>,
    shared: Arc<Mutex<Shared>>,
    published: Arc<Notify>,
) {
    let reason = loop {
        let message = match read_frame(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break "the language server exited".to_owned(),
            Err(error) => break error,
        };
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            // A response to one of our requests.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let reply = shared.lock().expect("lsp state").pending.remove(&id);
                if let Some(reply) = reply {
                    let result = match message.get("error") {
                        Some(error) => Err(format!(
                            "the language server refused the request: {}",
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("no message")
                        )),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = reply.send(result);
                }
            }
            // A request from the server. Answer so it does not wait: empty
            // configuration, and acceptance of progress and registrations.
            (Some(method), Some(id)) => {
                let result = if method == "workspace/configuration" {
                    let items = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; items])
                } else {
                    Value::Null
                };
                let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if let Err(error) = write_frame(&writer, &answer).await {
                    break error;
                }
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str) else {
                    continue;
                };
                let items = message
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                {
                    let mut shared = shared.lock().expect("lsp state");
                    shared.publishes += 1;
                    let generation = shared.publishes;
                    shared
                        .diagnostics
                        .insert(uri.to_owned(), Published { generation, items });
                }
                published.notify_waiters();
            }
            // Log and progress notifications carry nothing the tool reports.
            _ => {}
        }
    };
    let pending = {
        let mut shared = shared.lock().expect("lsp state");
        shared.closed = Some(reason.clone());
        std::mem::take(&mut shared.pending)
    };
    for (_, reply) in pending {
        let _ = reply.send(Err(reason.clone()));
    }
    published.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: a frame whose header and body arrive in separate reads lost,
    // or the second of two frames in one read dropped, so a response never
    // reaches its request and the tool waits until its timeout.
    #[tokio::test]
    async fn frames_split_across_reads_and_packed_in_one_read_are_all_read() {
        let first = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let second = br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{}}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", first.len()).into_bytes();
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(format!("Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n", second.len()).as_bytes());
        bytes.extend_from_slice(second);
        // One byte per read is the worst split a pipe can produce.
        let reader = futures::io::BufReader::with_capacity(1, futures::io::Cursor::new(bytes));
        let mut reader = reader;
        let one = read_frame(&mut reader).await.unwrap().unwrap();
        let two = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(one["id"], 1);
        assert_eq!(two["method"], "window/logMessage");
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    }

    // Catches: a server announcing a huge frame making the client allocate
    // it before reading a byte.
    #[tokio::test]
    async fn a_frame_over_the_limit_is_refused_before_it_is_read() {
        let bytes = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1).into_bytes();
        let mut reader = futures::io::BufReader::new(futures::io::Cursor::new(bytes));
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(error.contains("over the"), "{error}");
    }
}
