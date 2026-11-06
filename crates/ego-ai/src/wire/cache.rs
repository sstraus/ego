//! Cache metadata is transient; only each provider's declared fields leave ego.
use ego_core::generator::{capabilities::CacheRetention, types::GenRequest};
use serde_json::{Value, json};

pub(super) fn routing_key(req: &GenRequest, provider: &str) -> Option<String> {
    let cache = req.cache.as_ref()?;
    if cache.retention == CacheRetention::None {
        return None;
    }
    let domain = format!("ego/prompt-cache/v1/{provider}");
    let mac =
        ego_core::digest::hmac_sha256(domain.as_bytes(), cache.lineage_root.to_string().as_bytes());
    Some(mac.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn control(req: &GenRequest) -> Option<Value> {
    match req.cache.as_ref()?.retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({"type": "ephemeral"})),
        CacheRetention::Long => Some(json!({"type": "ephemeral", "ttl": "1h"})),
    }
}

/// Mark only a cache-eligible content block, never a replayed thinking block.
pub(super) fn mark_content(content: &mut Value, control: &Value) -> bool {
    if let Some(text) = content.as_str() {
        *content = json!([{"type":"text", "text":text}]);
    }
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if matches!(
            block["type"].as_str(),
            Some("text" | "image" | "image_url" | "tool_use" | "tool_result")
        ) {
            block["cache_control"] = control.clone();
            return true;
        }
    }
    false
}

/// The explicit short/long tiers shared by the documented request controls.
pub(super) fn policy(
    req: &GenRequest,
    long_supported: bool,
    lock_ttl: bool,
) -> ego_core::usage::PromptCachePolicy {
    let seconds = match req.cache.as_ref().map(|cache| cache.retention) {
        Some(CacheRetention::Short) => 300,
        Some(CacheRetention::Long) if long_supported => 3_600,
        Some(CacheRetention::Long) => 300,
        _ => return ego_core::usage::PromptCachePolicy::default(),
    };
    ego_core::usage::PromptCachePolicy {
        ttl_seconds: std::num::NonZeroU64::new(seconds),
        refresh_on_read: true,
        lock_ttl,
    }
}
