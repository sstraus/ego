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
        // xAI: recorded 2026-11-20 (xai-oauth-response.json); same family, no reset field.
        "openai" | "xai" | "xai-oauth" => "x-ratelimit",
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

/// Opaque identity of the credential actually sent, never a configured label.
pub fn credential_fingerprint(headers: &[HttpHeader]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    // The account claim survives access-token rotation when the provider supplies it.
    let account = headers
        .iter()
        .find(|h| h.name().eq_ignore_ascii_case("chatgpt-account-id"));
    for h in headers
        .iter()
        .filter(|h| account.map_or(h.is_secret(), |account| std::ptr::eq(*h, account)))
    {
        digest.update(h.name().to_ascii_lowercase());
        digest.update([0]);
        digest.update(h.value());
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
}

/// Subscription fields are separate from API-key remaining-count limits.
pub fn subscription_response_limits(
    provider: &str,
    credential: &str,
    headers: &[HttpHeader],
    observed_at: u64,
) -> Option<ego_core::quota::RateLimits> {
    use ego_core::quota::{QuotaSource, RateLimitWindow, RateLimits};
    let number = |name: &str, scale: f64| -> Option<serde_json::Number> {
        let value = header(headers, name)?.parse::<f64>().ok()? * scale;
        (value.is_finite() && value >= 0.0)
            .then(|| serde_json::Number::from_f64(value))
            .flatten()
    };
    let integer = |name: &str| header(headers, name)?.parse::<u64>().ok();
    let mut windows = Vec::new();
    let mut plan = None;
    let mut credits = None;
    match provider {
        "openai-codex" => {
            let windows_in = ["primary", "secondary"].map(|part| {
                let prefix = format!("x-codex-{part}");
                CodexWindow {
                    part,
                    minutes: integer(&format!("{prefix}-window-minutes")),
                    used_percentage: number(&format!("{prefix}-used-percent"), 1.0),
                    resets_at: integer(&format!("{prefix}-reset-at")),
                }
            });
            let boolean = |name| {
                header(headers, name).and_then(|s| match s.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                })
            };
            let credits_in = boolean("x-codex-credits-has-credits")
                .zip(boolean("x-codex-credits-unlimited"))
                .map(|(has_credits, unlimited)| {
                    (
                        has_credits,
                        unlimited,
                        header(headers, "x-codex-credits-balance"),
                    )
                });
            (windows, plan, credits) =
                codex_family(windows_in, header(headers, "x-codex-plan-type"), credits_in);
        }
        "anthropic" => {
            // Recorded 2026-11-14 (anthropic-oauth-response.json): `7d-utilization` is a
            // 0..1 fraction and `7d-reset` is Unix seconds. That account stated no `5h` or
            // `overage-utilization` field; they follow the same recorded pattern.
            for (part, seconds, name) in [
                ("5h", Some(18000), "five_hour"),
                ("7d", Some(604800), "seven_day"),
                ("overage", None, "overage"),
            ] {
                let prefix = format!("anthropic-ratelimit-unified-{part}");
                if let Some(used_percentage) = number(&format!("{prefix}-utilization"), 100.0) {
                    windows.push(RateLimitWindow {
                        name: name.into(),
                        used_percentage,
                        resets_at: integer(&format!("{prefix}-reset")),
                        window_seconds: seconds,
                    });
                }
            }
        }
        // xAI states only remaining counts (see `response_limits`), no subscription window.
        _ => return None,
    }
    (!windows.is_empty() || credits.is_some()).then(|| RateLimits {
        provider: provider.into(),
        connection: None,
        credential: credential.into(),
        windows,
        plan,
        credits,
        observed_at,
        source: QuotaSource::Passive,
    })
}

/// One Codex window as either transport states it, before validation.
struct CodexWindow {
    part: &'static str,
    minutes: Option<u64>,
    used_percentage: Option<serde_json::Number>,
    resets_at: Option<u64>,
}

type CodexFamily = (
    Vec<ego_core::quota::RateLimitWindow>,
    Option<String>,
    Option<ego_core::quota::QuotaCredits>,
);

/// The Codex quota family: HTTP response headers and the WebSocket
/// `codex.rate_limits` event state the same fields under different names.
fn codex_family(
    windows_in: [CodexWindow; 2],
    plan: Option<&str>,
    credits: Option<(bool, bool, Option<&str>)>,
) -> CodexFamily {
    use ego_core::quota::{QuotaCredits, RateLimitWindow, window_name};
    let mut windows: Vec<RateLimitWindow> = Vec::new();
    for window in windows_in {
        let Some(seconds) = window
            .minutes
            .and_then(|v| v.checked_mul(60))
            .filter(|v| *v > 0)
        else {
            continue;
        };
        if let Some(used_percentage) = window.used_percentage {
            let mut name = window_name(seconds, window.part);
            if windows.iter().any(|w| w.name == name) {
                name = format!("{name}_{}", window.part);
            }
            windows.push(RateLimitWindow {
                name,
                used_percentage,
                resets_at: window.resets_at,
                window_seconds: Some(seconds),
            });
        }
    }
    let plan = plan
        .filter(|v| {
            v.len() <= 64
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .map(str::to_owned);
    let credits = credits.map(|(has_credits, unlimited, balance)| QuotaCredits {
        has_credits,
        unlimited,
        balance: balance
            .filter(|v| v.len() <= 64 && v.parse::<f64>().is_ok_and(|v| v.is_finite() && v >= 0.0))
            .map(str::to_owned),
    });
    (windows, plan, credits)
}

/// Quota stated inside a generation stream rather than in response headers.
/// Codex WebSocket generations send no quota headers on the handshake; they
/// send one `codex.rate_limits` event instead.
pub fn subscription_event_limits(
    provider: &str,
    credential: &str,
    event: &serde_json::Value,
    observed_at: u64,
) -> Option<ego_core::quota::RateLimits> {
    use ego_core::quota::{QuotaSource, RateLimits};
    if provider != "openai-codex" || event["type"] != "codex.rate_limits" {
        return None;
    }
    let limits = &event["rate_limits"];
    let windows_in = ["primary", "secondary"].map(|part| {
        let window = &limits[part];
        CodexWindow {
            part,
            minutes: window["window_minutes"].as_u64(),
            used_percentage: window["used_percent"]
                .as_number()
                .filter(|n| n.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0))
                .cloned(),
            resets_at: window["reset_at"].as_u64(),
        }
    });
    let credits = &event["credits"];
    let credits_in = credits["has_credits"]
        .as_bool()
        .zip(credits["unlimited"].as_bool())
        .map(|(has_credits, unlimited)| (has_credits, unlimited, credits["balance"].as_str()));
    let (windows, plan, credits) =
        codex_family(windows_in, event["plan_type"].as_str(), credits_in);
    (!windows.is_empty() || credits.is_some()).then(|| RateLimits {
        provider: provider.into(),
        connection: None,
        credential: credential.into(),
        windows,
        plan,
        credits,
        observed_at,
        source: QuotaSource::Passive,
    })
}

/// Buffered response from a fixed, non-generating subscription endpoint.
pub struct SubscriptionUsageResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

/// Read using ego's own access token. This function never logs in or refreshes.
pub async fn subscription_usage(
    provider: &str,
    access: &str,
    http: &Arc<dyn Http>,
) -> Result<SubscriptionUsageResponse, String> {
    let endpoint = match provider {
        "anthropic" => "https://api.anthropic.com/api/oauth/usage",
        "openai-codex" => "https://chatgpt.com/backend-api/wham/usage",
        // DEFERRED (2026-11-12): record xAI billing with a current access credential first.
        _ => return Err("no recorded subscription usage endpoint".into()),
    };
    let url = url::Url::parse(endpoint).expect("fixed usage endpoint");
    let profile = crate::http::AdmittedHttpProfile::from_urls(
        "subscription-usage",
        std::slice::from_ref(&url),
    )
    .map_err(|error| error.to_string())?;
    let mut headers = crate::oauth::provider_auth(provider, access)
        .map_err(|_| "invalid subscription credential".to_owned())?
        .headers();
    headers.push(crate::http::public_header(
        "accept",
        b"application/json".to_vec(),
    ));
    let mut response = profile
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
        .map_err(|_| "subscription usage transport failed".to_owned())?;
    let retry_after = header(&response.headers, "retry-after").map(str::to_owned);
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(|_| "subscription usage response failed".to_owned())?;
        if body.len().saturating_add(chunk.len()) > 256 * 1024 {
            return Err("subscription usage body exceeds 256 KiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(SubscriptionUsageResponse {
        status: response.status,
        retry_after,
        body,
    })
}

#[cfg(test)]
mod subscription_tests {
    use super::*;

    /// Synthetic values in the recorded shape: the recordings keep names and
    /// type classes only, never live values.
    fn fill(skeleton: &serde_json::Value, key: &str) -> serde_json::Value {
        use serde_json::{Value, json};
        match skeleton {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), fill(value, key)))
                    .collect(),
            ),
            Value::String(class) => match (class.as_str(), key) {
                ("int", "used_percent") => json!(37),
                ("int", "window_minutes") => json!(10080),
                ("int", "reset_at") => json!(1_795_000_000_u64),
                ("int", _) => json!(100),
                ("bool", "has_credits" | "allowed") => json!(true),
                ("bool", _) => json!(false),
                ("string", "type") => json!("codex.rate_limits"),
                ("string", "plan_type") => json!("pro"),
                ("string", "balance") => json!("12.5"),
                ("null", _) => Value::Null,
                (other, key) => panic!("unknown recorded class {other} for {key}"),
            },
            other => panic!("unexpected skeleton node {other}"),
        }
    }

    // Catches: reading the WebSocket event with the header-family names or nesting,
    // naming a window by position instead of duration, or crediting another provider.
    #[test]
    fn codex_websocket_event_from_recorded_skeleton() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/subscription_quota/openai-codex-websocket.json"
        ))
        .unwrap();
        let mut event = fill(&fixture["event_skeletons"]["codex.rate_limits"], "");
        let limits = subscription_event_limits("openai-codex", "test", &event, 1).unwrap();
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].name, "seven_day");
        assert_eq!(limits.windows[0].window_seconds, Some(604_800));
        assert_eq!(limits.windows[0].used_percentage.as_f64(), Some(37.0));
        assert_eq!(limits.windows[0].resets_at, Some(1_795_000_000));
        assert_eq!(limits.plan.as_deref(), Some("pro"));
        let credits = limits.credits.unwrap();
        assert!(credits.has_credits && !credits.unlimited);
        assert_eq!(credits.balance.as_deref(), Some("12.5"));
        assert!(subscription_event_limits("openai", "test", &event, 1).is_none());
        event["rate_limits"]["primary"]["window_minutes"] = 300.into();
        let limits = subscription_event_limits("openai-codex", "test", &event, 1).unwrap();
        assert_eq!(limits.windows[0].name, "five_hour");
        event["type"] = "codex.response.metadata".into();
        assert!(subscription_event_limits("openai-codex", "test", &event, 1).is_none());
    }

    // Catches: dropping the recorded xAI x-ratelimit family, or reporting it as a
    // subscription window although xAI states no reset or window length.
    #[test]
    fn xai_ratelimit_headers_from_recorded_response() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/subscription_quota/xai-oauth-response.json"
        ))
        .unwrap();
        let headers: Vec<_> = fixture["quota_header_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| {
                let name = name.as_str().unwrap();
                let value = if name.contains("-limit-") {
                    "1000"
                } else {
                    "750"
                };
                crate::http::public_header(name, value.as_bytes().to_vec())
            })
            .collect();
        for provider in ["xai", "xai-oauth"] {
            let limits = response_limits(provider, &headers);
            let kinds: Vec<_> = limits.iter().map(|limit| limit.kind).collect();
            assert_eq!(
                kinds,
                [ProviderLimitKind::Requests, ProviderLimitKind::Tokens],
                "{provider}"
            );
            for limit in &limits {
                assert_eq!(limit.remaining, Amount::new(750, 0).unwrap());
                assert_eq!(limit.limit, Some(Amount::new(1000, 0).unwrap()));
                assert_eq!(limit.reset, None);
            }
            assert!(subscription_response_limits(provider, "test", &headers, 1).is_none());
        }
    }

    // Catches: treating the recorded 0..1 utilization fraction as a percentage,
    // or parsing unified field names that the live response does not use.
    #[test]
    fn anthropic_unified_headers_from_recorded_response() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/subscription_quota/anthropic-oauth-response.json"
        ))
        .unwrap();
        let headers: Vec<_> = fixture["quota_header_shapes"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, shape)| {
                let value = match shape.as_str().unwrap() {
                    "decimal_0_1" => "0.37",
                    "unix_seconds" => "1795000000",
                    "lowercase_token" => "allowed",
                    other => panic!("unknown recorded shape {other}"),
                };
                crate::http::public_header(name, value.as_bytes().to_vec())
            })
            .collect();
        let limits = subscription_response_limits("anthropic", "test", &headers, 1).unwrap();
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].name, "seven_day");
        assert_eq!(limits.windows[0].used_percentage.as_f64(), Some(37.0));
        assert_eq!(limits.windows[0].resets_at, Some(1_795_000_000));
        assert_eq!(limits.windows[0].window_seconds, Some(604_800));
    }
    // Catches: assuming primary means five-hour, or parsing token-like non-quota headers.
    #[test]
    fn subscription_headers_from_recorded_response() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/subscription_quota/openai-codex-response.json"
        ))
        .unwrap();
        let headers: Vec<_> = fixture["quota_headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| {
                crate::http::public_header(name, value.as_str().unwrap().as_bytes().to_vec())
            })
            .collect();
        let limits = subscription_response_limits("openai-codex", "test", &headers, 1).unwrap();
        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].name, "seven_day");
        assert_eq!(limits.windows[0].window_seconds, Some(604800));
        assert_eq!(limits.windows[0].used_percentage.as_f64(), Some(14.0));
        assert_eq!(
            limits.credits.unwrap().balance.as_deref(),
            Some("8418.4673090000")
        );
        assert!(subscription_response_limits("other", "test", &headers, 1).is_none());
        let mut duplicate = headers.clone();
        duplicate.push(crate::http::public_header(
            "x-codex-primary-used-percent",
            b"99".to_vec(),
        ));
        assert!(
            subscription_response_limits("openai-codex", "test", &duplicate, 1)
                .unwrap()
                .windows
                .is_empty()
        );
    }
}
