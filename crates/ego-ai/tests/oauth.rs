//! Built-in subscription login is a closed, fixture-tested protocol table.

mod support;

use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::oauth::{
    BuiltInFlow, FlowError, LoginGrant, OAuthToken, ProviderAuth, TokenStatus, built_in,
    codex_account_id, provider_auth, provider_base,
};
use ego_ai::provider::ProviderRow;
use ego_core::permit::{OneHopResponse, TransportError};
use futures::future::BoxFuture;
use support::FakeCredentials;

const NOW_MS: u64 = 1_800_000_000_000;

fn oauth_json() -> &'static [u8] {
    br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#
}

#[test]
fn the_closed_set_is_the_union_requested_by_the_implementation_spec() {
    let ids: Vec<&str> = BuiltInFlow::all().iter().map(|flow| flow.id()).collect();
    assert_eq!(
        ids,
        [
            "openai-codex",
            "anthropic",
            "github-copilot",
            "xai-oauth",
            "kimi-coding",
            "minimax-oauth",
            "nous",
            "openrouter",
        ]
    );
}

#[test]
fn every_rotating_flow_builds_a_refresh_request_and_accepts_a_fixture() {
    for flow in BuiltInFlow::all() {
        if flow.id() == "openrouter" || flow.id() == "github-copilot" {
            continue;
        }
        let request = flow.refresh_request("old-refresh").unwrap();
        assert_eq!(request.method, ego_ai::http::Method::Post, "{}", flow.id());
        let body = String::from_utf8(request.body.unwrap()).unwrap();
        assert!(body.contains("refresh_token"), "{}: {body}", flow.id());

        let token = flow
            .read_token(
                TokenStatus::new(200, oauth_json()),
                NOW_MS,
                Some("old-refresh"),
            )
            .unwrap();
        let values = token.into_values();
        assert_eq!(values.access, "access", "{}", flow.id());
        assert_eq!(values.refresh.as_deref(), Some("refresh"), "{}", flow.id());
    }
}

#[test]
fn invalid_grant_requires_login_but_rate_limiting_does_not() {
    for flow in BuiltInFlow::all() {
        let invalid = flow
            .read_token(
                TokenStatus::new(400, br#"{"error":"invalid_grant"}"#),
                NOW_MS,
                Some("refresh"),
            )
            .unwrap_err();
        assert!(
            matches!(invalid, FlowError::ReloginRequired { .. }),
            "{}: {invalid:?}",
            flow.id()
        );

        let limited = flow
            .read_token(
                TokenStatus::new(429, br#"{"error":"rate_limited"}"#),
                NOW_MS,
                Some("refresh"),
            )
            .unwrap_err();
        assert!(
            matches!(limited, FlowError::Temporary { status: 429, .. }),
            "{}: {limited:?}",
            flow.id()
        );

        let invalid_poll = flow
            .read_poll(
                TokenStatus::new(400, br#"{"error":"invalid_grant"}"#),
                NOW_MS,
                Some("refresh"),
            )
            .unwrap_err();
        assert!(matches!(invalid_poll, FlowError::ReloginRequired { .. }));

        let limited_poll = flow
            .read_poll(
                TokenStatus::new(429, br#"{"error":"rate_limited"}"#),
                NOW_MS,
                Some("refresh"),
            )
            .unwrap_err();
        assert!(matches!(
            limited_poll,
            FlowError::Temporary { status: 429, .. }
        ));
    }
}

#[test]
fn codex_device_code_is_custom_and_then_exchanges_at_oauth_token() {
    let flow = built_in("openai-codex").unwrap();
    let start = flow.device_authorization_request().unwrap();
    assert_eq!(
        start.url.as_str(),
        "https://auth.openai.com/api/accounts/deviceauth/usercode"
    );
    assert_eq!(
        start.body.unwrap(),
        br#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
    );

    let poll = flow
        .device_poll_request("device-auth", Some("USER-CODE"))
        .unwrap();
    assert_eq!(
        poll.url.as_str(),
        "https://auth.openai.com/api/accounts/deviceauth/token"
    );
    let exchange = flow
        .login_request(LoginGrant::AuthorizationCode {
            code: "authorization-code",
            verifier: "verifier",
            redirect_uri: "https://auth.openai.com/deviceauth/callback",
            state: None,
        })
        .unwrap();
    assert_eq!(exchange.url.as_str(), "https://auth.openai.com/oauth/token");
}

#[test]
fn codex_and_anthropic_browser_login_are_pkce_flows() {
    for (id, redirect, host) in [
        (
            "openai-codex",
            "http://localhost:1455/auth/callback",
            "auth.openai.com",
        ),
        ("anthropic", "http://localhost:53692/callback", "claude.ai"),
    ] {
        let url = built_in(id)
            .unwrap()
            .authorization_url(redirect, "challenge", "state")
            .unwrap();
        assert_eq!(url.host_str(), Some(host), "{id}");
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(
            query.get("code_challenge").map(|v| v.as_ref()),
            Some("challenge")
        );
        assert_eq!(
            query.get("code_challenge_method").map(|v| v.as_ref()),
            Some("S256")
        );
        assert_eq!(query.get("state").map(|v| v.as_ref()), Some("state"));
    }
}

#[test]
fn rfc_device_flows_name_the_provider_endpoints_and_clients() {
    let expected = [
        (
            "github-copilot",
            "https://github.com/login/device/code",
            "Iv1.b507a08c87ecfe98",
        ),
        (
            "xai-oauth",
            "https://auth.x.ai/oauth2/device/code",
            "b1a00492-073a-47ea-816f-4c329264a828",
        ),
        (
            "kimi-coding",
            "https://auth.kimi.com/api/oauth/device_authorization",
            "17e5f671-d194-4dfb-9706-5516cb48c098",
        ),
        (
            "nous",
            "https://portal.nousresearch.com/api/oauth/device/code",
            "hermes-cli",
        ),
    ];
    for (id, endpoint, client) in expected {
        let request = built_in(id)
            .unwrap()
            .device_authorization_request()
            .unwrap();
        assert_eq!(request.url.as_str(), endpoint, "{id}");
        assert!(
            String::from_utf8(request.body.unwrap())
                .unwrap()
                .contains(client),
            "{id}"
        );
    }
}

#[test]
fn device_authorization_fixtures_supply_every_value_the_cli_polls_with() {
    for id in ["github-copilot", "xai-oauth", "kimi-coding", "nous"] {
        let device = built_in(id)
            .unwrap()
            .read_device_authorization(TokenStatus::new(
                200,
                br#"{"device_code":"device","user_code":"CODE","verification_uri":"https://verify.example/","interval":3,"expires_in":600}"#,
            ))
            .unwrap();
        assert_eq!(device.device_code, "device", "{id}");
        assert_eq!(device.verification_uri, "https://verify.example/", "{id}");
        assert_eq!(device.interval_seconds, 3, "{id}");
    }

    let codex = built_in("openai-codex")
        .unwrap()
        .read_device_authorization(TokenStatus::new(
            200,
            br#"{"device_auth_id":"device","user_code":"CODE"}"#,
        ))
        .unwrap();
    assert_eq!(
        codex.verification_uri,
        "https://auth.openai.com/codex/device"
    );
}

#[test]
fn pkce_and_user_code_flows_post_their_vendor_contracts() {
    let authorization = LoginGrant::AuthorizationCode {
        code: "code",
        verifier: "verifier",
        redirect_uri: "http://localhost/callback",
        state: Some("state"),
    };
    for (id, endpoint) in [
        ("openai-codex", "https://auth.openai.com/oauth/token"),
        ("anthropic", "https://platform.claude.com/v1/oauth/token"),
    ] {
        let request = built_in(id)
            .unwrap()
            .login_request(authorization.clone())
            .unwrap();
        assert_eq!(request.url.as_str(), endpoint, "{id}");
        let body = String::from_utf8(request.body.unwrap()).unwrap();
        assert!(body.contains("code"), "{id}: {body}");
        assert!(body.contains("verifier"), "{id}: {body}");
    }

    let minimax = built_in("minimax-oauth")
        .unwrap()
        .login_request(LoginGrant::UserCode {
            user_code: "MINIMAX-CODE",
            verifier: "verifier",
        })
        .unwrap();
    assert_eq!(minimax.url.as_str(), "https://api.minimax.io/oauth/token");
    let body = String::from_utf8(minimax.body.unwrap()).unwrap();
    assert!(body.contains("MINIMAX-CODE"));
    assert!(body.contains("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Auser_code"));
    let user_code = built_in("minimax-oauth")
        .unwrap()
        .read_minimax_user_code(
            TokenStatus::new(
                200,
                br#"{"user_code":"MINIMAX-CODE","verification_uri":"https://verify.minimax.example/","expired_in":600,"interval":2000,"state":"state"}"#,
            ),
            "state",
        )
        .unwrap();
    assert_eq!(user_code.user_code, "MINIMAX-CODE");
}

#[test]
fn copilot_exchanges_the_github_token_and_keeps_it_for_refresh() {
    let flow = built_in("github-copilot").unwrap();
    let request = flow.copilot_exchange_request("gho_refresh").unwrap();
    assert_eq!(
        request.url.as_str(),
        "https://api.github.com/copilot_internal/v2/token"
    );
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name() == "authorization"
                && header.value() == b"Bearer gho_refresh")
    );

    let token = flow
        .read_token(
            TokenStatus::new(
                200,
                br#"{"token":"copilot-access","expires_at":1800003600}"#,
            ),
            NOW_MS,
            Some("gho_refresh"),
        )
        .unwrap();
    assert_eq!(token.into_values().refresh.as_deref(), Some("gho_refresh"));
}

#[test]
fn openrouter_exchange_mints_an_api_key_without_a_refresh_token() {
    let flow = built_in("openrouter").unwrap();
    let url = flow
        .authorization_url("http://127.0.0.1/callback", "challenge", "unused")
        .unwrap();
    assert_eq!(url.host_str(), Some("openrouter.ai"));
    assert!(url.as_str().contains("code_challenge=challenge"));
    let request = flow
        .login_request(LoginGrant::AuthorizationCode {
            code: "code",
            verifier: "verifier",
            redirect_uri: "http://127.0.0.1/callback",
            state: None,
        })
        .unwrap();
    assert_eq!(
        request.url.as_str(),
        "https://openrouter.ai/api/v1/auth/keys"
    );
    let token = flow
        .read_token(
            TokenStatus::new(200, br#"{"key":"sk-or-key"}"#),
            NOW_MS,
            None,
        )
        .unwrap();
    assert!(matches!(token, OAuthToken::ApiKey { .. }));
}

#[test]
fn codex_requires_the_chatgpt_account_claim() {
    let payload = base64_url(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1"}}"#);
    let token = format!("head.{payload}.sig");
    assert_eq!(codex_account_id(&token).unwrap(), "acct_1");

    let missing = format!("head.{}.sig", base64_url(br#"{}"#));
    assert!(matches!(
        codex_account_id(&missing),
        Err(FlowError::MissingAccountId)
    ));
}

#[test]
fn anthropic_oat_and_api_keys_have_distinct_presentations() {
    let oat = provider_auth("anthropic", "sk-ant-oat01-secret").unwrap();
    assert!(matches!(&oat, ProviderAuth::Headers(_)));
    let headers = oat.headers();
    assert!(headers.iter().any(|header| {
        header.name() == "authorization" && header.value() == b"Bearer sk-ant-oat01-secret"
    }));
    // The presentation is the Bearer and the beta that admits it, and nothing
    // else: the `claude-cli` user-agent, `x-app: cli` and the
    // `claude-code-20250219` beta that used to travel with it were Claude
    // Code's identity, and `tests/vendor_client_identity.rs` is where their
    // absence is enforced across every request ego builds.
    assert!(
        headers.iter().any(
            |header| header.name() == "anthropic-beta" && header.value() == b"oauth-2025-04-20"
        )
    );
    assert!(!headers.iter().any(|header| header.name() == "user-agent"));
    assert!(!headers.iter().any(|header| header.name() == "x-api-key"));

    let api_headers = provider_auth("anthropic", "sk-ant-api03-secret")
        .unwrap()
        .headers();
    assert_eq!(api_headers.len(), 1);
    assert_eq!(api_headers[0].name(), "x-api-key");
    assert_eq!(api_headers[0].value(), b"sk-ant-api03-secret");
}

#[test]
fn subscription_rows_have_distinct_ids_and_endpoints() {
    let rows = [
        ProviderRow::openai_codex_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
    ];
    let actual: Vec<_> = rows
        .iter()
        .map(|row| (row.id(), row.base().as_str()))
        .collect();
    assert_eq!(
        actual,
        [
            ("openai-codex", "https://chatgpt.com/backend-api/"),
            (
                "github-copilot",
                "https://api.individual.githubcopilot.com/"
            ),
            ("xai-oauth", "https://api.x.ai/v1/"),
            ("kimi-coding", "https://api.kimi.com/coding/v1/"),
            ("minimax-oauth", "https://api.minimax.io/anthropic/v1/"),
            ("nous", "https://inference-api.nousresearch.com/v1/"),
        ]
    );
    assert!(
        rows[0]
            .extra_headers
            .contains(&("originator".into(), "ego".into()))
    );
    // The Copilot row declares one identity header and it is ego's own. The
    // four Copilot Chat / VS Code values it used to carry are an operator's
    // `[auth.github-copilot.headers]` overlay now, not a built-in.
    let copilot = &rows[1].extra_headers;
    assert_eq!(copilot.len(), 1, "{copilot:?}");
    assert_eq!(copilot[0].0, "user-agent");
    assert!(copilot[0].1.starts_with("ego/"), "{}", copilot[0].1);
    for row in &rows {
        let resolver = format!("{:?}", row.resolver);
        if row.id() == "kimi-coding" {
            assert_eq!(resolver, "KimiManaged", "{}: {resolver}", row.id());
            continue;
        }
        assert!(
            resolver.contains("SubscriptionSupplied"),
            "{}: {resolver}",
            row.id()
        );
        assert!(
            resolver.contains(&format!("{}-subscription/v1", row.id())),
            "{}: {resolver}",
            row.id()
        );
    }
}

#[test]
fn every_selectable_subscription_row_names_login_when_all_sources_are_missing() {
    let rows = [
        ProviderRow::openai_codex_default(),
        ProviderRow::github_copilot_default(),
        ProviderRow::xai_oauth_default(),
        ProviderRow::kimi_coding_default(),
        ProviderRow::minimax_oauth_default(),
        ProviderRow::nous_default(),
        ProviderRow::anthropic_default(),
        ProviderRow::openrouter_default(),
    ];
    let credentials = FakeCredentials::empty();

    for row in rows {
        let error = row.credential.resolve(&credentials).unwrap_err();
        assert_eq!(error.var, format!("ego auth login {}", row.id()));
        assert!(
            error
                .to_string()
                .contains(&format!("`ego auth login {}`", row.id()))
        );
    }
}

#[test]
fn codex_request_auth_is_derived_from_the_jwt() {
    let payload = base64_url(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_1"}}"#);
    let token = format!("head.{payload}.sig");
    let headers = provider_auth("openai-codex", &token).unwrap().headers();
    assert!(headers.iter().any(|header| header.name() == "authorization"
        && header.value() == format!("Bearer {token}").as_bytes()));
    assert!(
        headers
            .iter()
            .any(|header| header.name() == "chatgpt-account-id" && header.value() == b"acct_1")
    );
    assert!(headers.iter().any(
        |header| header.name() == "openai-beta" && header.value() == b"responses=experimental"
    ));
}

#[test]
fn copilot_proxy_endpoint_is_credential_derived() {
    assert_eq!(
        provider_base(
            "github-copilot",
            "tid=1;proxy-ep=proxy.enterprise.example;exp=1800003600"
        )
        .unwrap()
        .unwrap()
        .as_str(),
        "https://api.enterprise.example/"
    );
}

#[tokio::test]
async fn token_exchange_runs_only_through_the_injected_fixture_transport() {
    let http = FixtureHttp;
    let flow = built_in("xai-oauth").unwrap();
    let response = ego_ai::oauth::execute(
        &http,
        flow,
        flow.device_poll_request("device", None).unwrap(),
    )
    .await
    .unwrap();
    let values = flow
        .read_token(response, NOW_MS, None)
        .unwrap()
        .into_values();
    assert_eq!(values.access, "access");
    assert_eq!(values.refresh.as_deref(), Some("refresh"));
}

struct FixtureHttp;

impl Http for FixtureHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(FixtureCall)
    }
}

struct FixtureCall;

impl HttpCall for FixtureCall {
    fn execute(
        &mut self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(async {
            Ok(OneHopResponse::response(
                200,
                Vec::new(),
                oauth_json().to_vec(),
            ))
        })
    }
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
