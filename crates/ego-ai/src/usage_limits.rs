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

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: accepting another provider's fields, ambiguous duplicates, or malformed counters.
    // These are adversarial parser inputs, not a fabricated provider response fixture.
    #[test]
    fn usage_limits_header_parser_rejects_cross_provider_and_ambiguous_inputs() {
        let header =
            |name: &str, value: &str| HttpHeader::new(name, value.as_bytes().to_vec()).unwrap();
        for (provider, remaining, limit) in [
            (
                "openai",
                "X-RateLimit-Remaining-Tokens",
                "x-ratelimit-limit-tokens",
            ),
            (
                "anthropic",
                "anthropic-ratelimit-tokens-remaining",
                "anthropic-ratelimit-tokens-limit",
            ),
        ] {
            let fields = [header(remaining, "0"), header(limit, "100")];
            let limits = response_limits(provider, &fields);
            assert_eq!(limits[0].remaining, Amount::ZERO);
            assert_eq!(limits[0].limit.unwrap().to_string(), "100");
            assert!(response_limits("openrouter", &fields).is_empty());
            let other = if provider == "openai" {
                "anthropic"
            } else {
                "openai"
            };
            assert!(response_limits(other, &fields).is_empty());
            for invalid in ["-1", "1.2", "NaN", "18446744073709551616", ""] {
                assert!(
                    response_limits(provider, &[header(remaining, invalid)]).is_empty(),
                    "{invalid}"
                );
            }
            assert!(
                response_limits(provider, &[header(remaining, "5"), header(remaining, "6")])
                    .is_empty()
            );
            assert!(response_limits(provider, &[header(limit, "100")]).is_empty());
        }
    }

    // Catches: computing limit minus lifetime usage instead of reading the stated remaining value.
    #[test]
    fn usage_limits_recorded_key_uses_stated_remaining_and_preserves_absence() {
        let recorded = include_bytes!("../tests/fixtures/usage_limits/openrouter-key.json");
        let limits = key_limits(recorded).unwrap();
        assert_eq!(limits[0].remaining.to_string(), "73.123456789");
        assert_eq!(limits[0].limit.unwrap().to_string(), "80");
        assert_eq!(limits[0].used.unwrap().to_string(), "123.987654321");
        let original: serde_json::Value = serde_json::from_slice(recorded).unwrap();
        for replacement in [
            serde_json::Value::Null,
            serde_json::json!(-1),
            serde_json::json!("42"),
        ] {
            let mut mutated = original.clone();
            mutated["data"]["limit_remaining"] = replacement;
            assert!(
                key_limits(&serde_json::to_vec(&mutated).unwrap())
                    .unwrap()
                    .is_empty()
            );
        }
        let mut absent = original;
        absent["data"]
            .as_object_mut()
            .unwrap()
            .remove("limit_remaining");
        assert!(
            key_limits(&serde_json::to_vec(&absent).unwrap())
                .unwrap()
                .is_empty()
        );
    }
}
