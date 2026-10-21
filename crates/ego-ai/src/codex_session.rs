//! Codex continuation, pinned to Pi 4954d760504c11df9b2cbdc040346107b6a8153d,
//! packages/ai/src/api/openai-codex-responses.ts:1426-1478.
//! Retain only successful connection-local state; durable reasoning uses core history.
use std::sync::Arc;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{TransportError, TransportFailure};
use futures::{StreamExt, lock::Mutex, stream};
use serde_json::{Value, json};

use crate::http::{AdmittedHttpProfile, Http, HttpError, HttpRequest, HttpResponse, HttpWebSocket};

#[derive(Default)]
pub(crate) struct Session {
    socket: Option<Box<dyn HttpWebSocket>>,
    continuation: Option<Continuation>,
    session_id: Option<ego_core::event::id::SessionId>,
}

impl Session {
    pub(crate) fn release_idle_connection(&mut self, session: ego_core::event::id::SessionId) {
        if self.session_id == Some(session) {
            self.socket = None;
            self.continuation = None;
        }
    }

    fn bind_session(&mut self, session_id: Option<ego_core::event::id::SessionId>) {
        if session_id.is_none() || self.session_id != session_id {
            self.socket = None;
            self.continuation = None;
        }
        self.session_id = session_id;
    }
}

struct Continuation {
    body: Value,
    baseline: Vec<Value>,
    response_id: String,
}

fn without_input(mut body: Value) -> Value {
    body.as_object_mut()
        .expect("Codex body object")
        .remove("input");
    body.as_object_mut().unwrap().remove("previous_response_id");
    body
}

impl Continuation {
    fn request(&self, body: &Value) -> Value {
        let mut next = body.clone();
        if without_input(body.clone()) == without_input(self.body.clone())
            && let Some(input) = body["input"].as_array()
            && input.starts_with(&self.baseline)
        {
            next["input"] = json!(input[self.baseline.len()..]);
            next["previous_response_id"] = json!(self.response_id);
        }
        next
    }
}

fn normalize_output(output: &[Value]) -> Option<Vec<Value>> {
    let mut items = Vec::new();
    for item in output {
        match item["type"].as_str()? {
            "reasoning" => {
                if item["encrypted_content"].is_string() {
                    items.push(json!({"type":"reasoning", "id":item["id"], "encrypted_content":item["encrypted_content"], "summary":item.get("summary").cloned().unwrap_or(json!([]))}));
                }
            }
            "message" => {
                let text = item["content"].as_array()?.iter().filter_map(|block| block["text"].as_str()).collect::<String>();
                if !text.is_empty() { items.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})); }
            }
            "function_call" => items.push(json!({"type":"function_call","call_id":item["call_id"],"name":item["name"],"arguments":item["arguments"]})),
            _ => return None,
        }
    }
    Some(items)
}

// Allowlist routing metadata only: never hash or log authorization/account headers.
fn fingerprint(request: &HttpRequest, body: &Value, transport: &str) {
    if !tracing::enabled!(target: "ego_ai::codex_session", tracing::Level::DEBUG) {
        return;
    }
    let mut settings = body.clone();
    settings
        .as_object_mut()
        .expect("Codex body object")
        .remove("input");
    let mut prefix = Vec::new();
    let prefix_digests: Vec<_> = body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            prefix.extend(serde_json::to_vec(item).expect("Codex JSON serializes"));
            ego_core::digest::sha256_hex(&prefix)
        })
        .collect();
    let routing: Vec<_> = request
        .headers
        .iter()
        .filter(|header| {
            matches!(
                header.name(),
                "session-id" | "x-client-request-id" | "openai-beta"
            )
        })
        .map(|header| (header.name(), ego_core::digest::sha256_hex(header.value())))
        .collect();
    tracing::debug!(
        target: "ego_ai::codex_session",
        transport,
        request_digest = %ego_core::digest::sha256_hex(&serde_json::to_vec(body).expect("Codex JSON serializes")),
        settings_digest = %ego_core::digest::sha256_hex(&serde_json::to_vec(&settings).expect("Codex JSON serializes")),
        prompt_cache_key_digest = ?body["prompt_cache_key"].as_str().map(|key| ego_core::digest::sha256_hex(key.as_bytes())),
        routing_header_digests = ?routing,
        input_prefix_digests = ?prefix_digests,
        "Codex transport fingerprint"
    );
}

fn invalid() -> HttpError {
    HttpError::transport(TransportError::definitely_unsent(
        TransportFailure::AuthorityMismatch,
    ))
}

pub(crate) async fn execute(
    session: Arc<Mutex<Session>>,
    http: Arc<dyn Http>,
    profile: AdmittedHttpProfile,
    mut request: HttpRequest,
    session_id: Option<ego_core::event::id::SessionId>,
) -> Result<HttpResponse, HttpError> {
    let mut owner = session.lock_owned().await;
    owner.bind_session(session_id);
    let body: Value = serde_json::from_slice(request.body.as_deref().unwrap_or_default())
        .map_err(|_| invalid())?;
    fingerprint(&request, &body, "websocket_attempt");
    let fallback = HttpRequest {
        url: request.url.clone(),
        method: request.method,
        headers: request.headers.clone(),
        body: request.body.clone(),
    };
    request
        .headers
        .retain(|header| !matches!(header.name(), "content-type" | "accept" | "openai-beta"));
    request.headers.push(crate::http::public_header(
        "openai-beta",
        b"responses_websockets=2026-02-06".to_vec(),
    ));
    // Taking the connection makes cancellation/drop fail closed: no partial response can be reused.
    let socket_reused = owner.socket.is_some();
    let mut socket = match owner.socket.take() {
        Some(socket) => socket,
        None => {
            owner.continuation = None;
            let prepared = profile
                .prepare(
                    request.url.clone(),
                    request.to_core().map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?;
            match http.open_websocket(prepared).await {
                Ok(socket) => socket,
                Err(error)
                    if error.delivery() == DeliveryCertainty::DefinitelyUnsent
                        && matches!(error.failure(), TransportFailure::Connect(_)) =>
                {
                    fingerprint(&fallback, &body, "http");
                    drop(owner);
                    return profile.execute(http.as_ref(), fallback).await;
                }
                Err(error) => return Err(HttpError::transport(error)),
            }
        }
    };
    let mut outgoing = owner
        .continuation
        .take()
        .map_or_else(|| body.clone(), |prior| prior.request(&body));
    // Diagnostic evidence contains hashes only, never prompt text or transport headers.
    if tracing::enabled!(target: "ego_ai::codex_session", tracing::Level::DEBUG) {
        let mut prefix = Vec::new();
        let prefix_digests: Vec<_> = body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                prefix.extend(serde_json::to_vec(item).expect("Codex JSON serializes"));
                ego_core::digest::sha256_hex(&prefix)
            })
            .collect();
        let mut settings = body.clone();
        settings
            .as_object_mut()
            .expect("Codex request object")
            .remove("input");
        tracing::debug!(
            target: "ego_ai::codex_session",
            socket_reused,
            settings_digest = %ego_core::digest::sha256_hex(
                &serde_json::to_vec(&settings).expect("Codex JSON serializes")
            ),
            input_prefix_digests = ?prefix_digests,
            outgoing_digest = %ego_core::digest::sha256_hex(
                &serde_json::to_vec(&outgoing).expect("Codex JSON serializes")
            ),
            "Codex request fingerprint evidence"
        );
    }
    tracing::debug!(
        target: "ego_ai::codex_session",
        incremental = outgoing.get("previous_response_id").is_some(),
        full_input_items = body["input"].as_array().map_or(0, Vec::len),
        sent_input_items = outgoing["input"].as_array().map_or(0, Vec::len),
        "sending Codex WebSocket generation"
    );
    outgoing["type"] = json!("response.create");
    request.body = Some(serde_json::to_vec(&outgoing).expect("Codex JSON serializes"));
    let prepared = profile
        .prepare(
            request.url.clone(),
            request.to_core().map_err(|_| invalid())?,
        )
        .map_err(|_| invalid())?;
    fingerprint(&request, &outgoing, "websocket");
    socket.send(prepared).await.map_err(HttpError::transport)?;
    let chunks = stream::unfold(Some((owner, socket, body)), |state| async move {
        let (mut owner, mut socket, body) = state?;
        match socket.receive().await {
            Ok(Some(bytes)) => {
                let event: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(_) => {
                        return Some((
                            Err(HttpError::transport(TransportError::possibly_sent(
                                TransportFailure::ResponseBody(
                                    "invalid Codex WebSocket JSON".into(),
                                ),
                            ))),
                            None,
                        ));
                    }
                };
                let mut sse = b"data: ".to_vec();
                sse.extend_from_slice(&bytes);
                sse.extend_from_slice(b"\n\n");
                let terminal = matches!(
                    event["type"].as_str(),
                    Some("response.completed" | "response.failed" | "error")
                );
                if terminal {
                    if event["type"] == "response.completed"
                        && event["response"]["status"] == "completed"
                        && let Some(id) = event["response"]["id"].as_str()
                        && let Some(output) = event["response"]["output"]
                            .as_array()
                            .and_then(|items| normalize_output(items))
                    {
                        let mut baseline = body["input"].as_array().cloned().unwrap_or_default();
                        baseline.extend(output);
                        owner.continuation = Some(Continuation {
                            body,
                            baseline,
                            response_id: id.into(),
                        });
                        owner.socket = Some(socket);
                    }
                    Some((Ok(bytes::Bytes::from(sse)), None))
                } else {
                    Some((Ok(bytes::Bytes::from(sse)), Some((owner, socket, body))))
                }
            }
            Ok(None) => None,
            Err(error) => Some((Err(HttpError::transport(error)), None)),
        }
    });
    Ok(HttpResponse {
        status: 200,
        headers: vec![],
        body: chunks.boxed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        json!({"model":"codex", "prompt_cache_key":"lineage-a", "input":[{"role":"user","content":"read"}], "store":false})
    }
    fn prior() -> Continuation {
        let body = body();
        let mut baseline = body["input"].as_array().unwrap().clone();
        baseline
            .push(json!({"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}));
        Continuation {
            body,
            baseline,
            response_id: "resp_1".into(),
        }
    }

    #[test]
    fn second_tool_result_sends_suffix_instead_of_full_history() {
        let prior = prior();
        let result = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
        let mut next = body();
        next["input"] = json!([prior.baseline.clone(), vec![result.clone()]].concat());
        let sent = prior.request(&next);
        assert_eq!(sent["previous_response_id"], "resp_1");
        assert_eq!(sent["input"], json!([result]));
    }

    #[test]
    fn fork_compaction_model_and_settings_changes_never_reuse_continuation() {
        let prior = prior();
        let mut next = body();
        next["input"] = json!(prior.baseline);
        for (key, value) in [
            ("prompt_cache_key", json!("fork")),
            ("model", json!("other")),
            ("store", json!(true)),
            ("input", json!([{"role":"user","content":"checkpoint"}])),
        ] {
            let mut changed = next.clone();
            changed[key] = value;
            assert_eq!(
                prior.request(&changed),
                changed,
                "must send full request after {key} changes"
            );
        }
    }
    #[test]
    fn fork_identity_invalidates_continuation_even_with_identical_cache_root_and_history() {
        let parent = ego_core::event::id::SessionId::new();
        let fork = ego_core::event::id::SessionId::new();
        let mut session = Session {
            session_id: Some(parent),
            continuation: Some(prior()),
            socket: None,
        };
        session.bind_session(Some(parent));
        assert!(session.continuation.is_some());
        session.bind_session(Some(fork));
        assert!(
            session.continuation.is_none(),
            "fork must not inherit the parent's response ID"
        );
        session.continuation = Some(prior());
        session.bind_session(None);
        assert!(
            session.continuation.is_none(),
            "unidentified requests must not reuse state"
        );
        assert!(
            Session::default().continuation.is_none(),
            "resume starts without connection-scoped state"
        );
    }
}
