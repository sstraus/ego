//! Adversarial checks of the SigV4 credential and egress boundary.
//! Credentials are AWS's published example values, never an account.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::*;
use crate::support::FakeCredentials;
use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::generator::WireGenerator;
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::ProviderRow;
use ego_core::permit::{OneHopResponse, TransportError};
use futures::future::BoxFuture;

const TOKEN: &str = "FwoGZXIvYXdzEXAMPLETOKEN0123456789abcdef";
const SESSION_DOC: &str = r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE",
    "SecretAccessKey":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    "SessionToken":"FwoGZXIvYXdzEXAMPLETOKEN0123456789abcdef",
    "Expiration":"2099-01-01T00:00:00Z"}"#;

fn sigv4_connections() -> std::collections::BTreeMap<String, ego_core::config::ConnectionConfig> {
    let mut connections = native_connections();
    connections.get_mut("native-account").unwrap().vendor = "bedrock-runtime-sigv4".into();
    connections
}

fn sigv4_instance(base: &str) -> ego_core::config::ProviderInstance {
    let mut instance = native_instance();
    instance.profile = ego_core::config::ProviderProfile::BedrockRuntimeSigv4;
    instance.base_url = base.parse().unwrap();
    instance.models.values_mut().next().unwrap().metering =
        ego_core::budget::MeteringIdentity::Unmetered {
            revision: ego_core::cost::MeteringRevision("test".into()),
        };
    instance
}

fn catalogue(instance: ego_core::config::ProviderInstance) -> Result<Catalogue, String> {
    Catalogue::new([ProviderRow::bedrock_runtime_default()])
        .with_instances(&native_providers(instance), &sigv4_connections())
}

fn chat_instance() -> ego_core::config::ProviderInstance {
    let mut instance = sigv4_instance("https://bedrock-runtime.us-east-1.amazonaws.com/");
    let model = instance.models.values_mut().next().unwrap();
    model.upstream_id = "mistral.mistral-large-3-675b-instruct".into();
    model.contract = serde_json::from_value(serde_json::json!("bedrock-chat")).unwrap();
    model.facts.transport = ego_core::generator::capabilities::Transport::Sse;
    instance
}

fn drive(http: Arc<dyn Http>, doc: &'static str) -> Vec<AiEvent> {
    drive_with(
        sigv4_instance("https://bedrock-runtime.us-east-2.amazonaws.com/"),
        true,
        http,
        doc,
    )
}

fn drive_with(
    instance: ego_core::config::ProviderInstance,
    tools: bool,
    http: Arc<dyn Http>,
    doc: &'static str,
) -> Vec<AiEvent> {
    use ego_core::budget::{BudgetGate, Metering};
    use ego_core::cost::CurrencyCode;
    use ego_core::event::id::GenerationId;
    use ego_core::generator::attempt::GenerationAttempt;
    use ego_core::generator::port::Generator;
    let catalogue = catalogue(instance).unwrap();
    let credential = catalogue
        .row("office-native")
        .unwrap()
        .credential
        .resolve(&FakeCredentials::holding(doc))
        .unwrap();
    let slug = ModelSlug::parse("office-native/chat-looking-alias").unwrap();
    let resolved =
        futures::executor::block_on(catalogue.resolve(&slug, &http, &credential)).unwrap();
    let mut req = request();
    req.model = resolved.model;
    if tools {
        req.tools.push(tool());
    }
    let metering: &Metering = &resolved.metering;
    let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
    let reservation = gate
        .admit(metering, None, &req, &resolved.capabilities)
        .unwrap();
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        metering,
        None,
        &req,
        &resolved.capabilities,
    )
    .unwrap();
    let generator = WireGenerator::new(resolved.row, resolved.wire, http, credential);
    futures::executor::block_on(generator.generate(req, attempt).collect::<Vec<_>>())
}

// Catches a canonical path that is not AWS's double-encoded form: every Bedrock
// model ID with a colon (`...-v1:0`) would be signed one way and verified another,
// so the service answers SignatureDoesNotMatch. Oracle: AWS SDKs put `%3A` in the
// URL and sign it; the signature of that request is the one the service computes.
#[test]
fn sigv4_signature_of_a_colon_model_id_is_the_aws_canonical_one() {
    let (http, seen) = crate::support::unasked();
    let _ = drive(http, SESSION_DOC);
    let asked = seen.asked();
    assert_eq!(asked.len(), 1);
    let header = |name: &str| {
        asked[0]
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    let authorization = header("authorization");
    let signed_list = authorization
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_owned();
    let actual = authorization.split("Signature=").nth(1).unwrap().to_owned();
    let date = header("x-amz-date");
    let when = time::PrimitiveDateTime::parse(
        &date,
        &time::format_description::parse_borrowed::<2>(
            "[year][month][day]T[hour][minute][second]Z",
        )
        .unwrap(),
    )
    .unwrap()
    .assume_utc();
    let now = SystemTime::from(when);
    let encoded_path = asked[0].path.replace(':', "%3A");
    let url = format!("https://bedrock-runtime.us-east-2.amazonaws.com{encoded_path}");
    let signed_names: Vec<&str> = signed_list.split(';').collect();
    let headers: Vec<(String, String)> = asked[0]
        .headers
        .iter()
        .filter(|(n, _)| {
            let n = n.to_ascii_lowercase();
            signed_names.contains(&n.as_str()) && n != "host" && !n.starts_with("x-amz-")
        })
        .cloned()
        .collect();
    let credentials = aws_credential_types::Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        Some(TOKEN.into()),
        None,
        "reference",
    );
    let identity = credentials.into();
    let params = aws_sigv4::http_request::SigningSettings::default();
    let params: aws_sigv4::http_request::SigningParams =
        aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region("us-east-2")
            .name("bedrock")
            .time(now)
            .settings(params)
            .build()
            .unwrap()
            .into();
    let body = asked[0].body.clone().unwrap();
    let signable = aws_sigv4::http_request::SignableRequest::new(
        "POST",
        url,
        headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
        aws_sigv4::http_request::SignableBody::Bytes(body.as_bytes()),
    )
    .unwrap();
    let (instructions, _) = aws_sigv4::http_request::sign(signable, &params)
        .unwrap()
        .into_parts();
    let reference = instructions
        .headers()
        .find(|(n, _)| *n == "authorization")
        .unwrap()
        .1
        .split("Signature=")
        .nth(1)
        .unwrap()
        .to_owned();
    assert_eq!(actual, reference, "path as sent: {}", asked[0].path);
}

struct Redirecting(Arc<Mutex<Vec<(String, bool)>>>);

struct RedirectCall(Arc<Mutex<Vec<(String, bool)>>>);

impl Http for Redirecting {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(RedirectCall(Arc::clone(&self.0)))
    }
}

impl HttpCall for RedirectCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, _, _, request) = call.into_parts();
        let signed = request
            .headers()
            .iter()
            .any(|h| h.name().eq_ignore_ascii_case("authorization"));
        let mut seen = self.0.lock().unwrap();
        seen.push((request.path().to_owned(), signed));
        let first = seen.len() == 1;
        Box::pin(async move {
            if first {
                Ok(OneHopResponse::redirect(
                    307,
                    vec![],
                    b"/model/elsewhere/converse".to_vec(),
                ))
            } else {
                Ok(OneHopResponse::response(200, vec![], b"{}".to_vec()))
            }
        })
    }
}

// Catches following a redirect with the old signature: the SigV4 headers would be
// replayed against another path of the AWS origin, or the request re-sent unsigned.
#[test]
fn sigv4_request_is_not_replayed_after_a_redirect() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = drive(Arc::new(Redirecting(Arc::clone(&seen))), SESSION_DOC);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "followed the redirect: {seen:?}");
    assert!(
        events.iter().any(|e| matches!(e, AiEvent::Error { .. })),
        "{events:?}"
    );
}

struct EchoingAuth;

impl Http for EchoingAuth {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(Self)
    }
}

impl HttpCall for EchoingAuth {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, _, _, request) = call.into_parts();
        let header = |name| {
            let value = request
                .headers()
                .iter()
                .find(|header| header.name() == name)
                .unwrap();
            std::str::from_utf8(value.value()).unwrap()
        };
        let token = header("x-amz-security-token");
        assert_eq!(token, TOKEN);
        let authorization = header("authorization");
        let signature = authorization.rsplit_once(", Signature=").unwrap().1;
        let access_key = authorization
            .split_once("Credential=")
            .unwrap()
            .1
            .split('/')
            .next()
            .unwrap();
        let secret_key = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let message = format!(
            "The security token {token} included in the request is invalid. auth={authorization} sig={signature} id={access_key} key={secret_key}"
        );
        let body = json!({"error":{"message":message, "type":"invalid_request_error"}}).to_string();
        Box::pin(async move { Ok(OneHopResponse::response(403, vec![], body.into_bytes())) })
    }
}

// Catches provider diagnostics that reflect the whole signed header or its
// parts in the error delivered to the host and the attempt receipt.
#[test]
fn sigv4_session_token_echoed_by_the_provider_is_masked() {
    let events = drive_with(chat_instance(), false, Arc::new(EchoingAuth), SESSION_DOC);
    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::Error { message, .. } => Some(message),
            AiEvent::ProviderFailure { cause, .. } => Some(cause),
            _ => None,
        })
        .collect();
    assert_eq!(diagnostics.len(), 2, "{events:?}");
    for diagnostic in diagnostics {
        assert!(!diagnostic.contains(TOKEN), "{diagnostic}");
        assert!(diagnostic.contains(
            "The security token [REDACTED] included in the request is invalid. auth=[REDACTED] sig=[REDACTED] id=[REDACTED] key=[REDACTED]"
        ), "{diagnostic}");
    }
}

// Catches accepting expired, token-without-expiry or malformed credential-process
// documents, and echoing their secret values in the refusal.
#[test]
fn sigv4_refuses_unusable_credential_documents_without_echoing_them() {
    let row = catalogue(sigv4_instance(
        "https://bedrock-runtime.us-east-2.amazonaws.com/",
    ))
    .unwrap()
    .row("office-native")
    .unwrap()
    .clone();
    for doc in [
        r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRETVALUE1234567890","SessionToken":"TOKENVALUE1234567890","Expiration":"2001-01-01T00:00:00Z"}"#,
        r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRETVALUE1234567890","Expiration":"2001-01-01T00:00:00Z"}"#,
        r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRETVALUE1234567890","SessionToken":"TOKENVALUE1234567890"}"#,
        r#"{"Version":2,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRETVALUE1234567890"}"#,
        r#"{"Version":1,"AccessKeyId":"AKID EXAMPLE","SecretAccessKey":"SECRETVALUE1234567890"}"#,
        r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRET VALUE"}"#,
        r#"{"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"SECRETVALUE1234567890","Expiration":"not a time"}"#,
        "SECRETVALUE1234567890",
    ] {
        let error = match row.credential.resolve(&FakeCredentials::holding(doc)) {
            Ok(_) => panic!("accepted {doc}"),
            Err(error) => error,
        };
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("SECRETVALUE") && !shown.contains("TOKENVALUE"),
            "{shown}"
        );
    }
}

// Catches admitting Chat for another model or region than the reviewed binding,
// so signed prompts would go to an unreviewed deployment.
#[test]
fn bedrock_chat_binding_is_pinned_to_mistral_large_3_in_us_east_1() {
    for (base, upstream) in [
        (
            "https://bedrock-runtime.us-east-2.amazonaws.com/",
            "mistral.mistral-large-3-675b-instruct",
        ),
        (
            "https://bedrock-runtime.us-east-1.amazonaws.com/",
            "mistral.other-model",
        ),
        (
            "https://bedrock-runtime.us-east-1.amazonaws.com/declared/",
            "mistral.mistral-large-3-675b-instruct",
        ),
    ] {
        let mut instance = sigv4_instance(base);
        let model = instance.models.values_mut().next().unwrap();
        model.upstream_id = upstream.into();
        model.contract = serde_json::from_value(serde_json::json!("bedrock-chat")).unwrap();
        model.facts.transport = ego_core::generator::capabilities::Transport::Sse;
        assert!(catalogue(instance).is_err(), "{base} {upstream}");
    }
}
