//! Request bytes for the published prompt-cache contracts; no provider response fixture.
use ego_ai::wire::{
    RequestTarget, anthropic_messages, openai_codex_responses, openai_completions,
    openai_responses, xai_responses,
};
use ego_core::config::{ConfigLayer, Source, Trust, resolve};
use ego_core::event::id::SessionId;
use ego_core::generator::capabilities::CacheRetention;
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, PromptCache, Role, ThinkingLevel,
};
use serde_json::Value;

fn request(model: &str, root: SessionId, live: &str) -> GenRequest {
    GenRequest {
        model: model.to_owned(),
        cache: Some(PromptCache {
            lineage_root: root,
            retention: CacheRetention::Short,
            stable_messages: 4,
        }),
        messages: Conversation::new(vec![
            Message::text(Role::System, "stable identity"),
            Message::text(Role::System, "stable instructions"),
            Message::text(Role::User, "earlier intent"),
            Message::text(Role::Assistant, "earlier answer"),
            Message::text(Role::User, live),
            Message::text(Role::User, "current intent"),
        ])
        .unwrap(),
        tools: Vec::new(),
        thinking: ThinkingLevel::Low,
        max_output_tokens: Some(512),
        parallel_tool_calls: false,
    }
}
fn json(bytes: Vec<u8>) -> Value {
    serde_json::from_slice(&bytes).unwrap()
}
fn markers(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key("cache_control"))
                + object.values().map(markers).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(markers).sum(),
        _ => 0,
    }
}

#[test]
fn codex_consecutive_turns_keep_cache_key_and_session_affinity() {
    // Catches: Codex requests omit the session affinity that ChatGPT uses for cache routing.
    let root = SessionId::new();
    let first = request("gpt-6.1-sol", root, "host rev1");
    let second = request("gpt-6.1-sol", root, "host rev2");
    let base = url::Url::parse("https://chatgpt.com/backend-api/").unwrap();
    let target = RequestTarget::new(&base, &[]);
    let a = openai_codex_responses::build_request(&target, &first).unwrap();
    let b = openai_codex_responses::build_request(&target, &second).unwrap();
    let a_body = json(a.body.unwrap());
    let b_body = json(b.body.unwrap());
    let key = a_body["prompt_cache_key"]
        .as_str()
        .expect("Codex cache key");
    assert_eq!(b_body["prompt_cache_key"], key);
    for headers in [&a.headers, &b.headers] {
        assert!(
            headers
                .iter()
                .any(|header| header.name() == "session-id" && header.value() == key.as_bytes())
        );
    }
    assert_eq!(a_body["instructions"], b_body["instructions"]);
    assert_eq!(
        a_body["input"].as_array().unwrap()[..2],
        b_body["input"].as_array().unwrap()[..2]
    );
    assert_eq!(a_body["store"], false);
    assert!(a_body.get("previous_response_id").is_none());
    let other = request("gpt-6.1-sol", SessionId::new(), "host");
    assert_ne!(
        json(openai_codex_responses::request_body(&other).unwrap())["prompt_cache_key"],
        key
    );
    let mut disabled = first;
    disabled.cache.as_mut().unwrap().retention = CacheRetention::None;
    let disabled = openai_codex_responses::build_request(&target, &disabled).unwrap();
    assert!(
        json(disabled.body.unwrap())
            .get("prompt_cache_key")
            .is_none()
    );
    assert!(
        !disabled
            .headers
            .iter()
            .any(|header| header.name() == "session-id")
    );
}

#[test]
fn anthropic_breakpoints_exclude_host_updates_and_current_user_intent() {
    // Catches: host data merged into system or marked as the stable cache boundary.
    let root = SessionId::new();
    let first = json(
        anthropic_messages::request_body(&request("claude-opus-5-5", root, "host rev1")).unwrap(),
    );
    let second = json(
        anthropic_messages::request_body(&request("claude-opus-5-5", root, "host rev2")).unwrap(),
    );
    assert_eq!(first["system"], second["system"]);
    assert_eq!(
        first["messages"].as_array().unwrap()[..2],
        second["messages"].as_array().unwrap()[..2]
    );
    assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        first["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(markers(&first), 2);
    assert_eq!(markers(&first["messages"][2]), 0);
    assert_eq!(markers(&first["messages"][3]), 0);
    assert_eq!(first["messages"][3]["content"][0]["text"], "current intent");
}

#[test]
fn openrouter_claude_marks_only_the_normalized_system_and_stable_history() {
    // Catches: routed Claude missing explicit breakpoints, or unrelated endpoints receiving them.
    let req = request("anthropic/claude-opus-5-5", SessionId::new(), "host data");
    let body = json(openai_completions::openrouter_request_body(&req).unwrap());
    assert_eq!(
        body["messages"][0]["content"][1]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        body["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(markers(&body), 2);
    assert_eq!(markers(&body["messages"][3]), 0);
    assert_eq!(markers(&body["messages"][4]), 0);
    let generic = json(openai_completions::request_body(&req).unwrap());
    assert_eq!(markers(&generic), 0);
    assert!(generic.get("session_id").is_none());
    let mut unrelated = req;
    unrelated.model = "openai/gpt-5.6".to_owned();
    assert_eq!(
        markers(&json(
            openai_completions::openrouter_request_body(&unrelated).unwrap()
        )),
        0
    );
}

#[test]
fn one_hour_cache_writes_require_explicit_user_config() {
    // Catches: a default or project config silently selecting the one-hour write premium.
    let mut req = request("claude-opus-5-5", SessionId::new(), "host");
    let default = resolve(&[], Trust::Trusted, None).unwrap();
    assert_eq!(default.cache, CacheRetention::Short);
    assert!(
        json(anthropic_messages::request_body(&req).unwrap())["system"][0]["cache_control"]
            .get("ttl")
            .is_none()
    );
    for source in [Source::Project, Source::Local, Source::System] {
        assert!(ConfigLayer::parse(source, "cache = 'long'").is_err());
    }
    let user = ConfigLayer::parse(Source::User, "cache = 'long'").unwrap();
    req.cache.as_mut().unwrap().retention = resolve(&[user], Trust::Trusted, None).unwrap().cache;
    let direct = json(anthropic_messages::request_body(&req).unwrap());
    assert_eq!(direct["system"][0]["cache_control"]["ttl"], "1h");
    req.model = "anthropic/claude-opus-5-5".to_owned();
    assert_eq!(
        json(openai_completions::openrouter_request_body(&req).unwrap())["messages"][0]["content"]
            [1]["cache_control"]["ttl"],
        "1h"
    );
    req.cache.as_mut().unwrap().retention = CacheRetention::None;
    assert_eq!(
        markers(&json(anthropic_messages::request_body(&req).unwrap())),
        0
    );
    assert!(
        json(openai_completions::openrouter_request_body(&req).unwrap())
            .get("session_id")
            .is_none()
    );
}

#[test]
fn routing_keys_follow_lineage_roots_and_are_separated_between_providers() {
    // Catches: child-specific affinity, raw session IDs, or cross-provider correlation.
    let root = SessionId::new();
    let source = request("model", root, "host");
    let fork = request("model", root, "other host");
    let unrelated = request("model", SessionId::new(), "host");
    let mut keys = Vec::new();
    let encoders: [fn(&GenRequest) -> Value; 2] = [
        |req| json(openai_responses::request_body(req).unwrap()),
        |req| json(xai_responses::request_body(req).unwrap()),
    ];
    for encode in encoders {
        let a = encode(&source);
        let b = encode(&fork);
        let c = encode(&unrelated);
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
        assert_ne!(a["prompt_cache_key"], c["prompt_cache_key"]);
        assert_ne!(a["prompt_cache_key"], root.to_string());
        keys.push(a["prompt_cache_key"].clone());
    }
    let a = json(openai_completions::openrouter_request_body(&source).unwrap());
    let b = json(openai_completions::openrouter_request_body(&fork).unwrap());
    let c = json(openai_completions::openrouter_request_body(&unrelated).unwrap());
    assert_eq!(a["session_id"], b["session_id"]);
    assert_ne!(a["session_id"], c["session_id"]);
    assert_ne!(a["session_id"], root.to_string());
    assert_eq!(a["session_id"].as_str().unwrap().len(), 64);
    keys.push(a["session_id"].clone());
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert_ne!(keys[1], keys[2]);
}

#[test]
fn openrouter_tool_only_history_does_not_gain_a_null_content_field() {
    // Catches: marker lookup mutating an assistant tool-call-only request to content:null.
    let mut req = request("anthropic/claude-opus-5-5", SessionId::new(), "host");
    req.messages = Conversation::new(vec![
        Message::text(Role::System, "stable system"),
        Message::text(Role::User, "earlier intent"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "read1".to_owned(),
                name: "read".to_owned(),
                args: "{}".to_owned(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "read1".to_owned(),
                output: "file bytes".to_owned(),
                is_error: false,
            }],
        },
        Message::text(Role::User, "host"),
        Message::text(Role::User, "current intent"),
    ])
    .unwrap();
    req.cache.as_mut().unwrap().stable_messages = 3;
    let body = json(openai_completions::openrouter_request_body(&req).unwrap());
    assert!(body["messages"][2].get("content").is_none());
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(markers(&body), 2);
}

#[test]
fn kimi_messages_uses_top_level_writes_and_lineage_affinity_not_ignored_breakpoints() {
    // Catches: Kimi inherits Claude block markers, so it never writes a reusable cache.
    let root = SessionId::new();
    for retention in [
        CacheRetention::Short,
        CacheRetention::Long,
        CacheRetention::None,
    ] {
        let mut req = request("kimi-k3", root, "host rev1");
        req.cache.as_mut().unwrap().retention = retention;
        let a = json(ego_ai::wire::kimi_anthropic::request_body(&req).unwrap());
        let enabled = retention != CacheRetention::None;
        assert_eq!(a.get("cache_control").is_some(), enabled);
        assert_eq!(a["metadata"].get("user_id").is_some(), enabled);
        assert_eq!(markers(&a["system"]), 0);
        assert_eq!(markers(&a["messages"]), 0);
        if enabled {
            assert_eq!(a["cache_control"]["type"], "ephemeral");
            assert_eq!(
                a["cache_control"]["ttl"],
                if retention == CacheRetention::Long {
                    "1h"
                } else {
                    "5m"
                }
            );
            req.messages = request("kimi-k3", root, "host rev2").messages;
            let b = json(ego_ai::wire::kimi_anthropic::request_body(&req).unwrap());
            assert_eq!(a["system"], b["system"]);
            assert_eq!(
                a["messages"].as_array().unwrap()[..2],
                b["messages"].as_array().unwrap()[..2]
            );
            assert_eq!(a["metadata"]["user_id"], b["metadata"]["user_id"]);
            req.cache.as_mut().unwrap().lineage_root = SessionId::new();
            let c = json(ego_ai::wire::kimi_anthropic::request_body(&req).unwrap());
            assert_ne!(a["metadata"]["user_id"], c["metadata"]["user_id"]);
        }
    }
}

#[test]
fn kimi_chat_preserves_lineage_affinity_and_requested_cache_ttl() {
    // Catches: Kimi Chat omits its session cache key and silently ignores long retention.
    let root = SessionId::new();
    for retention in [
        CacheRetention::Short,
        CacheRetention::Long,
        CacheRetention::None,
    ] {
        let mut req = request("kimi-k3", root, "host rev1");
        req.cache.as_mut().unwrap().retention = retention;
        let a = json(ego_ai::wire::kimi_openai::request_body(&req).unwrap());
        if retention == CacheRetention::None {
            assert!(a.get("prompt_cache_key").is_none());
            assert!(a.get("prompt_cache_options").is_none());
            continue;
        }
        let key = a["prompt_cache_key"].as_str().expect("Kimi cache key");
        assert_ne!(key, root.to_string());
        assert_eq!(a["prompt_cache_options"]["mode"], "implicit");
        assert_eq!(
            a["prompt_cache_options"]["ttl"],
            if retention == CacheRetention::Long {
                "1h"
            } else {
                "5m"
            }
        );
        req.messages = request("kimi-k3", root, "host rev2").messages;
        let b = json(ego_ai::wire::kimi_openai::request_body(&req).unwrap());
        assert_eq!(
            a["messages"].as_array().unwrap()[..4],
            b["messages"].as_array().unwrap()[..4]
        );
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
        req.cache.as_mut().unwrap().lineage_root = SessionId::new();
        let c = json(ego_ai::wire::kimi_openai::request_body(&req).unwrap());
        assert_ne!(a["prompt_cache_key"], c["prompt_cache_key"]);
    }
}

#[test]
fn minimax_long_preference_never_exports_an_unsupported_one_hour_ttl() {
    // Catches: inheriting Claude's one-hour TTL on MiniMax's five-minute-only cache.
    use ego_ai::wire::{WireFormat, minimax_anthropic::MiniMaxAnthropic};
    let base = url::Url::parse("https://api.minimax.io/anthropic/").unwrap();
    let target = RequestTarget::new(&base, &[]);
    let root = SessionId::new();
    for retention in [
        CacheRetention::Short,
        CacheRetention::Long,
        CacheRetention::None,
    ] {
        let mut req = request("MiniMax-M2.7", root, "host rev1");
        req.cache.as_mut().unwrap().retention = retention;
        let a = json(
            MiniMaxAnthropic
                .build_request(&target, &req)
                .unwrap()
                .body
                .unwrap(),
        );
        if retention == CacheRetention::None {
            assert_eq!(markers(&a), 0);
            continue;
        }
        assert_eq!(
            a["system"][0]["cache_control"],
            serde_json::json!({"type":"ephemeral"})
        );
        assert_eq!(
            a["messages"][1]["content"][0]["cache_control"],
            serde_json::json!({"type":"ephemeral"})
        );
        assert_eq!(markers(&a), 2);
        assert_eq!(markers(&a["messages"][2]), 0);
        assert_eq!(markers(&a["messages"][3]), 0);
        req.messages = request("MiniMax-M2.7", root, "host rev2").messages;
        let b = json(
            MiniMaxAnthropic
                .build_request(&target, &req)
                .unwrap()
                .body
                .unwrap(),
        );
        assert_eq!(a["system"], b["system"]);
        assert_eq!(
            a["messages"].as_array().unwrap()[..2],
            b["messages"].as_array().unwrap()[..2]
        );
    }
}
