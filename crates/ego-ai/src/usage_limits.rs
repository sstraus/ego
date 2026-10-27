//! Provider-owned parsing of explicitly stated remaining limits.

use std::sync::Arc;

use ego_core::cost::Amount;
use ego_core::event::time::Clock;
use ego_core::permit::HttpHeader;
use ego_core::usage::{ProviderLimit, ProviderLimitKind, ProviderLimitsObservation};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::http::{Http, HttpRequest, Method};
use crate::provider::{ProviderRow, ResolvedCredential};

/// Parse only the selected provider's header family. A compatible wire is not provider identity.
pub(crate) fn response_limits(provider: &str, headers: &[HttpHeader]) -> Vec<ProviderLimit> {
    let prefix = match provider {
        "anthropic" => "anthropic-ratelimit",
        "openai" => "x-ratelimit",
        _ => return Vec::new(),
    };
    let kinds: &[(&str, ProviderLimitKind)] = match provider {
        "anthropic" => &[
            ("requests", ProviderLimitKind::Requests),
            ("tokens", ProviderLimitKind::Tokens),
            ("input-tokens", ProviderLimitKind::InputTokens),
            ("output-tokens", ProviderLimitKind::OutputTokens),
        ],
        _ => &[
            ("requests", ProviderLimitKind::Requests),
            ("tokens", ProviderLimitKind::Tokens),
        ],
    };
    kinds
        .iter()
        .filter_map(|(name, kind)| {
            let field = |part: &str| match provider {
                "anthropic" => format!("{prefix}-{name}-{part}"),
                _ => format!("{prefix}-{part}-{name}"),
            };
            let remaining_name = field("remaining");
            let remaining = count_header(headers, &remaining_name)?;
            Some(ProviderLimit {
                kind: *kind,
                remaining,
                limit: count_header(headers, &field("limit")),
                used: None,
                reset: header(headers, &field("reset")).and_then(safe_reset),
                source: format!("response_header:{remaining_name}"),
            })
        })
        .collect()
}

/// Duplicate fields are ambiguous, so no observation is made for them.
fn header<'a>(headers: &'a [HttpHeader], name: &str) -> Option<&'a str> {
    let mut matches = headers
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case(name));
    let value = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    std::str::from_utf8(value.value()).ok()
}

fn count_header(headers: &[HttpHeader], name: &str) -> Option<Amount> {
    let value = header(headers, name)?.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(|value| Amount::new(u128::from(value), 0).ok())
}

fn safe_reset(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" .:+-/_".contains(&byte)))
    .then(|| value.to_owned())
}

#[derive(Deserialize)]
struct KeyEnvelope {
    data: KeyData,
}
#[derive(Deserialize)]
struct KeyData {
    limit: Option<Box<RawValue>>,
    limit_remaining: Option<Box<RawValue>>,
    usage: Option<Box<RawValue>>,
    limit_reset: Option<String>,
}

fn amount(value: Option<&RawValue>) -> Option<Amount> {
    // Raw JSON preserves decimals without a binary floating-point round trip.
    value?.get().parse().ok()
}

fn key_limits(bytes: &[u8]) -> Result<Vec<ProviderLimit>, String> {
    let envelope: KeyEnvelope = serde_json::from_slice(bytes)
        .map_err(|_| "OpenRouter key response is not a key-limit document".to_owned())?;
    let data = envelope.data;
    let Some(remaining) = amount(data.limit_remaining.as_deref()) else {
        return Ok(Vec::new());
    };
    Ok(vec![ProviderLimit {
        kind: ProviderLimitKind::Usd,
        remaining,
        limit: amount(data.limit.as_deref()),
        used: amount(data.usage.as_deref()),
        reset: data.limit_reset.as_deref().and_then(safe_reset),
        source: "openrouter_key:data.limit_remaining".to_owned(),
    }])
}

/// Query the current OpenRouter credential, never an inferred account balance.
/// The caller must label this separately from historical session observations.
pub async fn openrouter_key_limits(
    row: &ProviderRow,
    credential: &ResolvedCredential,
    http: &Arc<dyn Http>,
    clock: &dyn Clock,
) -> Result<Option<ProviderLimitsObservation>, String> {
    if row.id() != "openrouter" {
        return Ok(None);
    }
    let url = row
        .base()
        .join("key")
        .map_err(|_| "invalid OpenRouter key endpoint".to_owned())?;
    let mut headers = credential.headers();
    for (name, value) in &row.extra_headers {
        headers.retain(|header| !header.name().eq_ignore_ascii_case(name));
        headers.push(crate::http::public_header(name, value.as_bytes().to_vec()));
    }
    let mut response = row
        .http_profile
        .execute(
            http.as_ref(),
            HttpRequest {
                url,
                method: Method::Get,
                headers,
                body: None,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    if response.status != 200 {
        return Err(format!(
            "OpenRouter key endpoint answered {}",
            response.status
        ));
    }
    let observed_at = clock.now();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return Err("OpenRouter key response exceeds 64 KiB".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    let limits = key_limits(&bytes)?;
    Ok((!limits.is_empty()).then(|| ProviderLimitsObservation {
        provider: "openrouter".to_owned(),
        model: None,
        observed_at,
        limits,
    }))
}
