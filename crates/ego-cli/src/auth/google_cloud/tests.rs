use super::*;
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_core::permit::{OneHopResponse, TransportError};

struct Google;
impl Http for Google {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(Google)
    }
}
impl HttpCall for Google {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, route, _, request) = call.into_parts();
        let body = match (route.target().host(), request.path()) {
            ("oauth2.googleapis.com", "/token") => {
                let grant = String::from_utf8_lossy(request.body());
                if grant.contains("refresh_token=fixture-a") {
                    r#"{"access_token":"access-a","expires_in":60}"#
                } else {
                    assert!(grant.contains("refresh_token=fixture-b"));
                    r#"{"access_token":"access-b","expires_in":3600}"#
                }
            }
            ("openidconnect.googleapis.com", "/v1/userinfo") => {
                let auth = request
                    .headers()
                    .iter()
                    .find(|header| header.name() == "authorization")
                    .unwrap();
                match auth.value() {
                    b"Bearer access-a" => r#"{"sub":"10001"}"#,
                    b"Bearer access-b" => r#"{"sub":"10002"}"#,
                    _ => panic!("unexpected fixture access token"),
                }
            }
            _ => panic!("unexpected Google endpoint"),
        };
        Box::pin(async {
            Ok(OneHopResponse::response(
                200,
                vec![],
                body.as_bytes().to_vec(),
            ))
        })
    }
}

// Catches: a named ADC slot is overwritten after switching Google users
// even when both ADC files were issued to the same OAuth client.
#[tokio::test]
async fn vertex_gemini_refresh_refuses_a_different_adc_principal() {
    let home = tempfile::tempdir().unwrap();
    let Some(active) = std::env::var_os("EGO_TEST_VERTEX_ADC_PATH") else {
        let path = home.path().join("active.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "auth::google_cloud::tests::vertex_gemini_refresh_refuses_a_different_adc_principal"])
            .env("EGO_TEST_VERTEX_ADC_PATH", &path)
            .env(CREDENTIALS_VAR, &path)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    };
    let first = home.path().join("first.json");
    let second = home.path().join("second.json");
    for (file, refresh) in [(&first, "fixture-a"), (&second, "fixture-b")] {
        fs::write(
            file,
            serde_json::json!({
                "type": "authorized_user",
                "client_id": "shared-client",
                "client_secret": "fixture-only",
                "refresh_token": refresh
            })
            .to_string(),
        )
        .unwrap();
    }
    fs::copy(first, &active).unwrap();
    let store = AuthStore::new(home.path());
    let id = connection_storage_key("vertex-work");
    store
        .put(&id, token_entry(&Google, true).await.unwrap())
        .unwrap();
    let original = fs::read(store.path()).unwrap();
    fs::copy(second, &active).unwrap();

    let route = ego_ai::provider::VERTEX_GEMINI;
    let result = refresh_connection_at(
        home.path(),
        &id,
        route,
        CredentialSource::BuiltIn(route.into()),
        None,
        &Google,
        &OsCredentialCommand,
        &CancelToken::new(),
        None,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("different Google principal")
    );
    assert_eq!(
        store
            .read_secret_material(&id)
            .unwrap()
            .expose_secret_bytes(),
        b"access-a"
    );
    assert_eq!(fs::read(store.path()).unwrap(), original);
}

#[derive(Clone)]
enum UserInfoReply {
    Transport(ego_core::permit::TransportFailure),
    Response(u16, &'static [u8]),
}

impl Http for UserInfoReply {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(self.clone())
    }
}

impl HttpCall for UserInfoReply {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, route, _, request) = call.into_parts();
        assert_eq!(route.target().host(), "openidconnect.googleapis.com");
        assert_eq!(request.path(), "/v1/userinfo");
        let result = match self {
            Self::Transport(failure) => Err(TransportError::definitely_unsent(failure.clone())),
            Self::Response(status, body) => {
                Ok(OneHopResponse::response(*status, vec![], body.to_vec()))
            }
        };
        Box::pin(async { result })
    }
}

async fn userinfo_refusal(reply: UserInfoReply) -> String {
    let adc = google_adc::parse(
        br#"{"type":"authorized_user","client_id":"fixture","client_secret":"fixture-only","refresh_token":"fixture"}"#,
    ).unwrap();
    vertex_principal_binding(&adc, &reply, b"synthetic-access-token")
        .await
        .unwrap_err()
        .to_string()
}

// Catches: temporary UserInfo failures send the user back through Google login.
#[tokio::test]
async fn vertex_userinfo_transient_failures_allow_retry() {
    use ego_core::permit::TransportFailure;
    for (reply, cause) in [
        (
            UserInfoReply::Transport(TransportFailure::Connect("fixture".into())),
            "connect",
        ),
        (
            UserInfoReply::Transport(TransportFailure::TimedOut),
            "timed out",
        ),
        (UserInfoReply::Response(503, b""), "HTTP 503"),
    ] {
        let message = userinfo_refusal(reply).await;
        assert!(
            message.contains("UserInfo") && message.contains(cause),
            "{message}"
        );
        assert!(
            message.contains("Retrying can help") && !message.contains(GCLOUD_LOGIN),
            "{message}"
        );
    }
}

// Catches: a rejected or insufficiently scoped token produces a blind retry.
#[tokio::test]
async fn vertex_userinfo_access_failures_require_openid_login() {
    for (reply, cause) in [
        (UserInfoReply::Response(401, b""), "HTTP 401"),
        (UserInfoReply::Response(403, b""), "HTTP 403"),
        (UserInfoReply::Response(200, b"{}"), "no usable subject"),
    ] {
        let message = userinfo_refusal(reply).await;
        assert!(
            message.contains("UserInfo") && message.contains(cause),
            "{message}"
        );
        assert!(
            message.contains("Retrying unchanged cannot help"),
            "{message}"
        );
        assert!(
            message.contains(GCLOUD_LOGIN) && message.contains("openid"),
            "{message}"
        );
    }
}
