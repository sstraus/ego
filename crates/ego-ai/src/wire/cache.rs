//! Cache controls and opaque request identities; only declared fields leave ego.
use ego_core::generator::{capabilities::CacheRetention, types::GenRequest};
use serde_json::{Value, json};

/// Hash the encoded Responses input, not the neutral conversation. Re-encoding
/// the parsed JSON gives the full array and each earlier slice one spelling.
/// No prompt bytes are retained in the returned durable metadata.
pub(super) fn responses_request_identity(
    body: &[u8],
    scope: String,
    previous_input_items: Option<usize>,
) -> Option<ego_core::usage::CacheRequestIdentity> {
    let mut frame: Value = serde_json::from_slice(body).ok()?;
    let input = frame.as_object_mut()?.remove("input")?;
    let input = input.as_array()?;
    if input.is_empty() {
        return None;
    }
    fn digest<T: serde::Serialize + ?Sized>(value: &T) -> Option<String> {
        serde_json::to_vec(value)
            .ok()
            .map(|bytes| ego_core::digest::sha256_hex(&bytes))
    }
    let previous_prefix = match previous_input_items
        .filter(|&count| count > 0)
        .and_then(|count| input.get(..count))
    {
        Some(items) => Some(digest(items)?),
        None => None,
    };
    Some(ego_core::usage::CacheRequestIdentity {
        scope,
        frame: digest(&frame)?,
        prefix: digest(input)?,
        previous_prefix,
        input_items: input.len(),
    })
}

pub(super) fn routing_key(req: &GenRequest, provider: &str) -> Option<String> {
    let root = req.cache.as_ref()?.lineage_root.to_string();
    keyed(req, provider, root.as_bytes())
}

/// Affinity shared by every session with the same stable prefix, so the first
/// request of a new session routes to where an earlier session cached it.
pub(super) fn prefix_routing_key(
    req: &GenRequest,
    provider: &str,
    stable_prefix: &[u8],
) -> Option<String> {
    keyed(req, provider, stable_prefix)
}

fn keyed(req: &GenRequest, provider: &str, message: &[u8]) -> Option<String> {
    let cache = req.cache.as_ref()?;
    if cache.retention == CacheRetention::None {
        return None;
    }
    use ego_core::generator::types::VerifierCache;

    let domain = match cache.verifier {
        None => format!("ego/prompt-cache/v1/{provider}"),
        Some(VerifierCache::GoalCheck) => {
            format!("ego/prompt-cache/goal-check/v1/{provider}/{}", req.model)
        }
        Some(VerifierCache::PermissionJudge) => format!(
            "ego/prompt-cache/permission-judge/v1/{provider}/{}",
            req.model
        ),
    };
    let mac = ego_core::digest::hmac_sha256(domain.as_bytes(), message);
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
