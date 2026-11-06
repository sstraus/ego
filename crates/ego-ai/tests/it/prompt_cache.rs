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
        web_search: None,
        model: model.to_owned(),
        cache: Some(PromptCache {
            session_id: root,
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
    for headers in [&a.headers, &b.headers] {
        assert!(
            headers
                .iter()
                .any(|header| header.name() == "x-client-request-id"
                    && header.value() == key.as_bytes()),
            "Codex request correlation must use the same opaque lineage identity"
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

// Catches: a fingerprint of the entire request treating a volatile user
// message as a changed cache prefix, or missing Anthropic's real breakpoints.
#[test]
fn anthropic_continuity_uses_the_actual_marked_request_prefix() {
    use ego_ai::wire::{WireFormat, anthropic_messages::AnthropicMessages};
    let root = SessionId::new();
    let a = request("claude-opus-5-5", root, "host rev1");
    let b = request("claude-opus-5-5", root, "host rev2");
    let fingerprint = |req: &GenRequest| {
        AnthropicMessages.cache_prefix(&anthropic_messages::request_body(req).unwrap())
    };
    assert!(fingerprint(&a).is_some());
    assert_eq!(fingerprint(&a), fingerprint(&b));
    let mut changed = b;
    changed.messages = Conversation::new(vec![
        Message::text(Role::System, "changed identity"),
        Message::text(Role::System, "stable instructions"),
        Message::text(Role::User, "earlier intent"),
        Message::text(Role::Assistant, "earlier answer"),
        Message::text(Role::User, "host rev2"),
        Message::text(Role::User, "current intent"),
    ])
    .unwrap();
    assert_ne!(fingerprint(&a), fingerprint(&changed));
    changed.cache.as_mut().unwrap().retention = CacheRetention::None;
    assert_eq!(fingerprint(&changed), None);
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
fn one_hour_cache_writes_require_an_owner_or_trusted_project_config() {
    // Catches: a default or untrusted project config silently selecting the one-hour write premium.
    let mut req = request("claude-opus-5-5", SessionId::new(), "host");
    let default = resolve(&[], Trust::Trusted, None).unwrap();
    assert_eq!(default.cache, CacheRetention::Short);
    assert!(
        json(anthropic_messages::request_body(&req).unwrap())["system"][0]["cache_control"]
            .get("ttl")
            .is_none()
    );
    for source in [Source::Project, Source::Local] {
        let layer = ConfigLayer::parse(source, "cache = 'long'").unwrap();
        let untrusted = resolve(std::slice::from_ref(&layer), Trust::Untrusted, None).unwrap();
        assert_eq!(untrusted.cache, CacheRetention::Short, "{source:?}");
        let trusted = resolve(&[layer], Trust::Trusted, None).unwrap();
        assert_eq!(trusted.cache, CacheRetention::Long, "{source:?}");
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

// Catches: unsupported current models receive the older 24h retention field,
// or an explicit long preference is ignored for a documented supported model.
#[test]
fn public_openai_long_retention_is_limited_to_documented_models() {
    for (model, expected) in [
        ("gpt-5.5", true),
        ("gpt-4.1", true),
        ("gpt-5.6", false),
        ("gpt-6.1-sol", false),
        ("custom-model", false),
    ] {
        let mut req = request(model, SessionId::new(), "host");
        for retention in [
            CacheRetention::Short,
            CacheRetention::Long,
            CacheRetention::None,
        ] {
            req.cache.as_mut().unwrap().retention = retention;
            let body = json(openai_responses::request_body(&req).unwrap());
            assert_eq!(
                body.get("prompt_cache_retention").and_then(Value::as_str),
                (expected && retention == CacheRetention::Long).then_some("24h"),
                "{model} {retention:?}"
            );
        }
    }
}

// Catches: the displayed tier diverging from the mapping's sent TTL or leaking to unrelated rows.
#[test]
fn cache_countdown_policy_follows_each_mapping_and_keeps_managed_ttl_unknown() {
    use ego_ai::wire::{
        WireFormat,
        anthropic_messages::AnthropicMessages,
        kimi_anthropic::KimiAnthropic,
        kimi_openai::KimiOpenAi,
        minimax_anthropic::MiniMaxAnthropic,
        openai_codex_responses::OpenAiCodexResponses,
        openai_completions::{OpenAiCompletions, OpenRouterCompletions},
        openai_responses::OpenAiResponses,
        xai_responses::XaiResponses,
    };
    // Wire, model, expected TTL seconds, refresh on read, locked TTL.
    type Mapping<'a> = (&'a dyn WireFormat, &'a str, Option<u64>, bool, bool);
    let mappings: &[Mapping] = &[
        (&AnthropicMessages, "model", Some(3_600), true, false),
        (&KimiAnthropic, "model", Some(3_600), true, true),
        (&KimiOpenAi, "model", Some(3_600), true, true),
        (&MiniMaxAnthropic, "model", Some(300), false, false),
        (
            &OpenRouterCompletions,
            "anthropic/model",
            Some(3_600),
            true,
            false,
        ),
        (&OpenRouterCompletions, "other/model", None, false, false),
        (&OpenAiCompletions, "anthropic/model", None, false, false),
        (&OpenAiResponses, "gpt-5.5", Some(86_400), false, false),
        (&OpenAiResponses, "gpt-6.1-sol", None, false, false),
        (&OpenAiCodexResponses, "gpt-6.1-sol", None, false, false),
        (&XaiResponses, "model", None, false, false),
    ];
    for (wire, model, expected, refresh, lock) in mappings {
        let mut req = request(model, SessionId::new(), "host");
        req.cache.as_mut().unwrap().retention = CacheRetention::Long;
        let policy = wire.prompt_cache_policy(&req);
        assert_eq!(
            policy.ttl_seconds.map(std::num::NonZeroU64::get),
            *expected,
            "{model} {wire:?}"
        );
        assert_eq!(policy.refresh_on_read, *refresh, "{model} {wire:?}");
        assert_eq!(policy.lock_ttl, *lock, "{model} {wire:?}");
        req.cache.as_mut().unwrap().retention = CacheRetention::None;
        assert!(
            wire.prompt_cache_policy(&req).ttl_seconds.is_none(),
            "disabled {wire:?}"
        );
    }
}

use crate::support;

// Catches: a profile estimate replacing an explicit wire TTL or being sent as a provider control.
#[test]
fn cache_countdown_profile_estimate_only_fills_unknown_wire_lifetimes() {
    use ego_ai::generator::WireGenerator;
    use ego_ai::provider::ProviderRow;
    use ego_ai::wire::{
        WireFormat, anthropic_messages::AnthropicMessages, openai_responses::OpenAiResponses,
    };
    use ego_core::generator::port::Generator;
    for (wire, expected) in [
        (&OpenAiResponses as &dyn WireFormat, 900),
        (&AnthropicMessages as &dyn WireFormat, 300),
    ] {
        let (http, server) = support::unasked();
        let generator =
            WireGenerator::new(ProviderRow::openai(), wire, http, support::credential())
                .with_cache_ttl_seconds(std::num::NonZeroU64::new(900));
        let mut req = request("model", SessionId::new(), "host");
        assert_eq!(
            generator
                .prompt_cache_policy(&req)
                .ttl_seconds
                .map(std::num::NonZeroU64::get),
            Some(expected)
        );
        req.cache.as_mut().unwrap().retention = CacheRetention::None;
        assert!(generator.prompt_cache_policy(&req).ttl_seconds.is_none());
        assert!(
            server.asked().is_empty(),
            "countdown metadata does not make requests"
        );
    }
}
