//! Dedicated PTY composition: production CLI, isolated fixture credentials.
//! No environment flag can install this backend in the production executable.

#[cfg(debug_assertions)]
#[path = "../../src/main.rs"]
mod process;

#[cfg(debug_assertions)]
mod credentials {
    use ego_tools::credentials::{CredentialBackend, CredentialBackendError};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    pub struct FixtureStore(pub PathBuf);

    impl FixtureStore {
        fn path(&self, service: &str, user: &str) -> PathBuf {
            self.0.join(format!(
                "{:x}",
                Sha256::digest(format!("{service}\0{user}").as_bytes())
            ))
        }
    }

    impl CredentialBackend for FixtureStore {
        fn get(
            &self,
            service: &str,
            user: &str,
        ) -> Result<Option<Vec<u8>>, CredentialBackendError> {
            if user.starts_with("mcp/oauth/v1/")
                && std::env::var_os("EGO_FIXTURE_BLOCK_MCP_READ").is_some()
            {
                // Model an OS authorization dialog that nobody can answer.
                if let Some(marker) = std::env::var_os("EGO_FIXTURE_NATIVE_READ_MARKER") {
                    std::fs::write(marker, b"native OAuth credential read entered").unwrap();
                }
                loop {
                    std::thread::park();
                }
            }
            match std::fs::read(self.path(service, user)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(CredentialBackendError::Unavailable),
            }
        }

        fn put(
            &self,
            service: &str,
            user: &str,
            value: Vec<u8>,
        ) -> Result<(), CredentialBackendError> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(self.path(service, user))
                .map_err(|_| CredentialBackendError::Unavailable)?;
            file.write_all(&value)
                .map_err(|_| CredentialBackendError::Unavailable)
        }

        fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialBackendError> {
            match std::fs::remove_file(self.path(service, user)) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(CredentialBackendError::Unavailable),
            }
        }
    }
}

#[cfg(debug_assertions)]
mod login {
    use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
    use ego_core::permit::{OneHopResponse, TransportError};
    use futures::future::BoxFuture;

    pub struct FixtureHttp;
    struct FixtureCall;

    impl Http for FixtureHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            Box::new(FixtureCall)
        }
    }

    impl HttpCall for FixtureCall {
        fn execute(
            &mut self,
            call: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            let (_, route, _, request) = call.into_parts();
            Box::pin(async move {
                assert_eq!(route.scheme(), ego_core::permit::HttpScheme::Https);
                assert_eq!(route.target().port(), 443);
                assert_eq!(request.method(), "POST");
                // Schema fixtures from ego-ai/tests/it/oauth.rs; these prove
                // seat/store orchestration, not live provider compatibility.
                let body = match (route.target().host(), request.path()) {
                    ("platform.claude.com", "/v1/oauth/token") => {
                        let body: serde_json::Value =
                            serde_json::from_slice(request.body()).unwrap();
                        assert_eq!(body["code"], "fixture-code");
                        assert_eq!(body["redirect_uri"], "http://localhost:53692/callback");
                        assert!(body["code_verifier"].as_str().unwrap().len() >= 43);
                        br#"{"access_token":"sk-ant-oatTestOnly0405","refresh_token":"fixture-refresh","expires_in":3600}"#.to_vec()
                    }
                    ("github.com", "/login/device/code") => {
                        br#"{"device_code":"device","user_code":"CODE","verification_uri":"https://verify.example/","interval":1,"expires_in":600}"#.to_vec()
                    }
                    ("github.com", "/login/oauth/access_token") => {
                        eprintln!("fixture device poll pending");
                        return std::future::pending().await;
                    }
                    _ => panic!("the login fixture refuses unknown request URLs"),
                };
                Ok(OneHopResponse::response(200, Vec::new(), body))
            })
        }
    }
}

#[cfg(debug_assertions)]
fn main() -> std::process::ExitCode {
    let root = std::path::PathBuf::from(std::env::var_os("EGO_HOME").expect("fixture EGO_HOME"))
        .join("fixture-credentials");
    std::fs::create_dir_all(&root).expect("fixture credential directory");
    assert!(
        ego_tools::credentials::install_test_credential_backend(std::sync::Arc::new(
            credentials::FixtureStore(root)
        ),)
        .is_ok()
    );
    assert!(
        ego_cli::auth::install_test_login_http(std::sync::Arc::new(login::FixtureHttp)).is_ok()
    );
    process::main()
}

// A release build, even with all features, contains neither the fixture store
// nor the programmatic override. It cannot start this test process.
#[cfg(not(debug_assertions))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(125)
}
