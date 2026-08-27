//! Subscription credential storage and operator-provided login programs.
//!
//! The store deliberately has one shape and one lock. Login, refresh and
//! logout all use the same read-modify-write operation, so rotating refresh
//! tokens cannot be lost when two ego processes share an account.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fd_lock::RwLock;
use futures::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use ego_ai::oauth::{self, BuiltInFlow, LoginGrant, OAuthToken, PollOutcome};
use ego_ai::provider::{Credentials, EnvCredentials, MissingCredential, Secret};
use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest, run_secret_child};

const PROGRAM_TIMEOUT: Duration = Duration::from_secs(120);
const PROGRAM_TERMINATION_GRACE: Duration = Duration::from_millis(500);
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "content-type",
    "accept",
    "openai-beta",
    "chatgpt-account-id",
];

/// A stored token whose formatters cannot reveal its value.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredSecret(String);

impl StoredSecret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for StoredSecret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StoredSecret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Debug for StoredSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoredSecret(<redacted>)")
    }
}

impl Serialize for StoredSecret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for StoredSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

/// One entry in `auth.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthEntry {
    OAuth {
        access: StoredSecret,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh: Option<StoredSecret>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires: Option<u64>,
    },
    ApiKey {
        key: StoredSecret,
    },
}

impl AuthEntry {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OAuth { .. } => "oauth",
            Self::ApiKey { .. } => "api_key",
        }
    }

    pub fn expires(&self) -> Option<u64> {
        match self {
            Self::OAuth { expires, .. } => *expires,
            Self::ApiKey { .. } => None,
        }
    }

    fn active_secret(&self) -> &StoredSecret {
        match self {
            Self::OAuth { access, .. } => access,
            Self::ApiKey { key } => key,
        }
    }
}

/// Credentials for a run: OAuth entries come from the locked store, ordinary
/// variables retain the existing resolve-once environment semantics.
#[derive(Debug, Clone)]
pub struct StoreCredentials {
    store: AuthStore,
}

impl StoreCredentials {
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            store: AuthStore::new(home),
        }
    }
}

impl Credentials for StoreCredentials {
    fn secret(&self, name: &str) -> Result<Secret, MissingCredential> {
        let Some(provider) = name.strip_prefix("oauth:") else {
            return EnvCredentials.secret(name);
        };
        let entries = self.store.read().map_err(|_| MissingCredential {
            var: format!("ego auth login {provider}"),
        })?;
        entries
            .get(provider)
            .map(|entry| Secret::from(entry.active_secret().expose()))
            .ok_or_else(|| MissingCredential {
                var: format!("ego auth login {provider}"),
            })
    }
}

#[derive(Debug)]
pub enum AuthError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    CorruptStore {
        path: PathBuf,
        message: String,
    },
    InvalidConfig(String),
    MissingCredential {
        program: String,
        reason: String,
    },
    CredentialCommand {
        program: String,
        failure: CredentialCommandFailure,
    },
    ReloginRequired {
        provider: String,
        reason: String,
    },
    Temporary {
        provider: String,
        status: u16,
        reason: String,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::CorruptStore { path, message } => {
                write!(
                    f,
                    "refusing corrupt credential store {}: {message}",
                    path.display()
                )
            }
            Self::InvalidConfig(message) => f.write_str(message),
            Self::MissingCredential { program, reason } => {
                write!(f, "no credential from {program}: {reason}")
            }
            Self::CredentialCommand { program, failure } => {
                write!(f, "no credential from {program}: {failure}")
            }
            Self::ReloginRequired { provider, reason } => write!(
                f,
                "{provider} login expired; run `ego auth login {provider}`: {reason}"
            ),
            Self::Temporary {
                provider,
                status,
                reason,
            } => write!(
                f,
                "{provider} token endpoint temporarily failed ({status}): {reason}"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

/// Secret-free failure semantics owned by the credential-command consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialCommandFailure {
    Cancelled,
    RunTimeout,
    CommandTimeout,
    Exit(Option<i32>),
    CleanupUncertain {
        cause: Option<Box<CredentialCommandFailure>>,
    },
    Io(String),
}

impl fmt::Display for CredentialCommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::RunTimeout => formatter.write_str("run deadline expired"),
            Self::CommandTimeout => formatter.write_str("timed out"),
            Self::Exit(Some(code)) => write!(formatter, "exited with code {code}"),
            Self::Exit(None) => formatter.write_str("exited by signal"),
            Self::CleanupUncertain { cause } => {
                formatter.write_str("process-tree cleanup is uncertain")?;
                if let Some(cause) = cause {
                    write!(formatter, " after {cause}")?;
                }
                Ok(())
            }
            Self::Io(reason) => formatter.write_str(reason),
        }
    }
}

pub(crate) struct CredentialCommandRequest {
    argv: Vec<OsString>,
    stdin: Option<SecretMaterial>,
    cancel: CancelToken,
    run_deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
struct CredentialCommandScope<'a> {
    commands: &'a dyn CredentialCommandPort,
    cancel: &'a CancelToken,
    run_deadline: Option<Instant>,
}

pub(crate) trait CredentialCommandPort: Send + Sync {
    fn run<'a>(
        &'a self,
        request: CredentialCommandRequest,
    ) -> BoxFuture<'a, Result<SecretMaterial, CredentialCommandFailure>>;
}

#[derive(Debug, Default)]
pub(crate) struct OsCredentialCommand;

impl CredentialCommandPort for OsCredentialCommand {
    fn run<'a>(
        &'a self,
        request: CredentialCommandRequest,
    ) -> BoxFuture<'a, Result<SecretMaterial, CredentialCommandFailure>> {
        Box::pin(async move {
            let output = run_secret_child(SecretChildRequest {
                argv: request.argv,
                cwd: None,
                stdin: request.stdin,
                cancel: request.cancel,
                run_deadline: request.run_deadline,
                command_timeout: PROGRAM_TIMEOUT,
                termination_grace: PROGRAM_TERMINATION_GRACE,
            })
            .await
            .map_err(credential_child_failure)?;
            Ok(output.into_stdout())
        })
    }
}

fn credential_child_failure(error: SecretChildError) -> CredentialCommandFailure {
    match error {
        SecretChildError::Cancelled => CredentialCommandFailure::Cancelled,
        SecretChildError::RunTimeout => CredentialCommandFailure::RunTimeout,
        SecretChildError::CommandTimeout => CredentialCommandFailure::CommandTimeout,
        SecretChildError::Exit(code) => CredentialCommandFailure::Exit(code),
        SecretChildError::CleanupUncertain { cause } => {
            CredentialCommandFailure::CleanupUncertain {
                cause: cause.map(|cause| Box::new(credential_child_failure(*cause))),
            }
        }
        SecretChildError::InvalidRequest(reason) => {
            CredentialCommandFailure::Io(reason.to_string())
        }
        SecretChildError::Launch(error) | SecretChildError::Io(error) => {
            CredentialCommandFailure::Io(error.to_string())
        }
    }
}

/// The single credential store under an ego home.
#[derive(Debug, Clone)]
pub struct AuthStore {
    home: PathBuf,
}

impl AuthStore {
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            home: home.as_ref().to_path_buf(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.home.join("auth.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.home.join("auth.lock")
    }

    fn prepare(&self) -> Result<(), AuthError> {
        fs::create_dir_all(&self.home).map_err(|source| AuthError::Io {
            path: self.home.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.home, fs::Permissions::from_mode(0o700)).map_err(
                |source| AuthError::Io {
                    path: self.home.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    fn open_lock(&self) -> Result<RwLock<File>, AuthError> {
        self.prepare()?;
        let path = self.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| AuthError::Io { path, source })?;
        Ok(RwLock::new(file))
    }

    fn read_unlocked(&self) -> Result<BTreeMap<String, AuthEntry>, AuthError> {
        let path = self.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(source) => return Err(AuthError::Io { path, source }),
        };
        serde_json::from_slice(&bytes).map_err(|err| AuthError::CorruptStore {
            path,
            message: err.to_string(),
        })
    }

    pub fn read(&self) -> Result<BTreeMap<String, AuthEntry>, AuthError> {
        let mut lock = self.open_lock()?;
        let _guard = lock.write().map_err(|source| AuthError::Io {
            path: self.lock_path(),
            source,
        })?;
        self.read_unlocked()
    }

    pub fn modify<T>(
        &self,
        update: impl FnOnce(&mut BTreeMap<String, AuthEntry>) -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        let mut lock = self.open_lock()?;
        let _guard = lock.write().map_err(|source| AuthError::Io {
            path: self.lock_path(),
            source,
        })?;
        let mut entries = self.read_unlocked()?;
        let answer = update(&mut entries)?;
        self.write_unlocked(&entries)?;
        Ok(answer)
    }

    pub fn put(&self, id: &str, entry: AuthEntry) -> Result<(), AuthError> {
        self.modify(|entries| {
            entries.insert(id.to_string(), entry);
            Ok(())
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool, AuthError> {
        self.modify(|entries| Ok(entries.remove(id).is_some()))
    }

    fn write_unlocked(&self, entries: &BTreeMap<String, AuthEntry>) -> Result<(), AuthError> {
        let path = self.path();
        let tmp = self.home.join("auth.json.tmp");
        let bytes = serde_json::to_vec_pretty(entries).expect("auth entries are serialisable");
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(|source| AuthError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| AuthError::Io {
                path: tmp.clone(),
                source,
            })?;
        fs::rename(&tmp, &path).map_err(|source| AuthError::Io {
            path: path.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|source| AuthError::Io { path, source })?;
        }
        Ok(())
    }
}

/// Non-secret authentication configuration.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub auth: HashMap<String, ProviderAuthConfig>,
}

/// Execute the credential-management command and return protocol-clean text.
pub async fn execute(
    args: &crate::cli::AuthArgs,
    cancel: &CancelToken,
) -> Result<String, AuthError> {
    let commands = OsCredentialCommand;
    let home = ego_home()?;
    let store = AuthStore::new(&home);
    match args.action {
        crate::cli::AuthAction::Status => status(&store, args.provider.as_deref()),
        crate::cli::AuthAction::Logout => {
            let Some(id) = args.provider.as_deref() else {
                return Err(AuthError::InvalidConfig(
                    "ego auth logout requires PROVIDER".to_string(),
                ));
            };
            let removed = store.remove(id)?;
            Ok(if removed {
                format!("logged out {id}\n")
            } else {
                format!("no stored credential for {id}\n")
            })
        }
        crate::cli::AuthAction::Login => {
            let Some(id) = args.provider.as_deref() else {
                return Ok(known_and_discovered(&home));
            };
            login(&store, &home, id, args.device, cancel, &commands).await?;
            Ok(format!("logged in {id}\n"))
        }
    }
}

fn ego_home() -> Result<PathBuf, AuthError> {
    optional_ego_home().ok_or_else(|| {
        AuthError::InvalidConfig("no home directory for credentials: set EGO_HOME".to_string())
    })
}

fn optional_ego_home() -> Option<PathBuf> {
    std::env::var_os("EGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ego")))
}

fn status(store: &AuthStore, selected: Option<&str>) -> Result<String, AuthError> {
    let entries = store.read()?;
    let mut text = String::new();
    for (id, entry) in entries {
        if selected.is_some_and(|wanted| wanted != id) {
            continue;
        }
        text.push_str(&id);
        text.push('\t');
        text.push_str(entry.kind());
        if let Some(expires) = entry.expires() {
            text.push_str("\texpires=");
            text.push_str(&expires.to_string());
        }
        text.push('\n');
    }
    Ok(text)
}

async fn login(
    store: &AuthStore,
    home: &Path,
    id: &str,
    device: bool,
    cancel: &CancelToken,
    commands: &dyn CredentialCommandPort,
) -> Result<(), AuthError> {
    let config = crate::config::auth().map_err(|err| AuthError::InvalidConfig(err.to_string()))?;
    let command = config
        .get(id)
        .and_then(|provider| provider.credential_command.as_deref());
    let plugin = discover_plugin(id, home, None);
    let source = select_source(id, command, plugin, oauth::built_in(id).is_some(), None);
    let entry = match source {
        CredentialSource::Command(program) => {
            run_configured_command_scoped(&program, "login", None, commands, cancel, None).await?
        }
        CredentialSource::Plugin(program) => {
            run_program_scoped(&program, "login", None, commands, cancel, None).await?
        }
        CredentialSource::BuiltIn(provider) => {
            let scope = crate::provider_http::CommandHttpScope::online(cancel.clone())
                .await
                .map_err(|reason| AuthError::MissingCredential {
                    program: format!("ego built-in {provider} login"),
                    reason,
                })?;
            built_in_login(
                &provider,
                std::io::stdin().is_terminal(),
                device,
                scope.http().as_ref(),
            )
            .await?
        }
        CredentialSource::Environment(var) => {
            return Err(AuthError::MissingCredential {
                program: var,
                reason: "environment credentials are resolved at generation time".to_string(),
            });
        }
        CredentialSource::Missing(provider) => {
            return Err(AuthError::MissingCredential {
                program: provider.clone(),
                reason: format!(
                    "configure auth.{provider}.credential_command or install ego-auth-{provider}"
                ),
            });
        }
    };
    ensure_store_may_mutate(cancel, None)?;
    store.put(id, entry)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginMethod {
    Device,
    Browser,
}

fn login_methods(flow: BuiltInFlow, tty: bool, force_device: bool) -> Vec<LoginMethod> {
    let supports_device = matches!(
        flow,
        BuiltInFlow::OpenAiCodex
            | BuiltInFlow::GitHubCopilot
            | BuiltInFlow::Xai
            | BuiltInFlow::KimiCoding
            | BuiltInFlow::Qwen
            | BuiltInFlow::MiniMax
            | BuiltInFlow::Nous
    );
    let supports_browser = matches!(
        flow,
        BuiltInFlow::OpenAiCodex | BuiltInFlow::Anthropic | BuiltInFlow::OpenRouter
    );
    if force_device {
        return supports_device
            .then_some(LoginMethod::Device)
            .into_iter()
            .collect();
    }
    if tty && supports_device && supports_browser {
        vec![LoginMethod::Device, LoginMethod::Browser]
    } else if supports_device && (!tty || !supports_browser) {
        vec![LoginMethod::Device]
    } else if supports_browser {
        vec![LoginMethod::Browser]
    } else {
        Vec::new()
    }
}

fn choose_login_method(methods: &[LoginMethod]) -> Result<LoginMethod, AuthError> {
    match methods {
        [method] => Ok(*method),
        [LoginMethod::Device, LoginMethod::Browser] => {
            eprintln!("Choose login method: [1] device code (default), [2] browser callback");
            match read_line()?.to_ascii_lowercase().as_str() {
                "" | "1" | "device" => Ok(LoginMethod::Device),
                "2" | "browser" => Ok(LoginMethod::Browser),
                other => Err(AuthError::InvalidConfig(format!(
                    "unknown login method `{other}`: enter 1 or 2"
                ))),
            }
        }
        _ => Err(AuthError::InvalidConfig(
            "the selected provider does not support the requested login method".to_string(),
        )),
    }
}

async fn built_in_login(
    id: &str,
    tty: bool,
    force_device: bool,
    http: &dyn ego_ai::http::Http,
) -> Result<AuthEntry, AuthError> {
    let flow = oauth::built_in(id).ok_or_else(|| AuthError::MissingCredential {
        program: format!("ego built-in {id} login"),
        reason: "unknown built-in provider".to_string(),
    })?;
    let method = choose_login_method(&login_methods(flow, tty, force_device))?;
    let token = match (flow, method) {
        (BuiltInFlow::MiniMax, _) => minimax_login(flow, http).await?,
        (_, LoginMethod::Browser) => browser_login(flow, http).await?,
        (_, LoginMethod::Device) => device_login(flow, http).await?,
    };
    token_entry(flow, token)
}

async fn device_login(
    flow: BuiltInFlow,
    http: &dyn ego_ai::http::Http,
) -> Result<OAuthToken, AuthError> {
    let verifier = verifier();
    let start = if flow == BuiltInFlow::Qwen {
        flow.device_authorization_request_with_pkce(&oauth::pkce_challenge(&verifier))
    } else {
        flow.device_authorization_request()
    }
    .map_err(flow_error)?;
    let response = oauth::execute(http, flow, start)
        .await
        .map_err(flow_error)?;
    let device = flow
        .read_device_authorization(response)
        .map_err(flow_error)?;
    eprintln!(
        "Open {} and enter code {}",
        device.verification_uri, device.user_code
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval_seconds.max(1);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let request = if flow == BuiltInFlow::Qwen {
            flow.device_poll_request_with_pkce(&device.device_code, &verifier)
        } else {
            flow.device_poll_request(&device.device_code, Some(&device.user_code))
        }
        .map_err(flow_error)?;
        let response = oauth::execute(http, flow, request)
            .await
            .map_err(flow_error)?;
        match flow
            .read_poll(response, now_ms(), None)
            .map_err(flow_error)?
        {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = interval.saturating_add(5),
            PollOutcome::Token(token) => return Ok(token),
            PollOutcome::GitHubToken(github) => {
                let request = flow.copilot_exchange_request(&github).map_err(flow_error)?;
                let response = oauth::execute(http, flow, request)
                    .await
                    .map_err(flow_error)?;
                return flow
                    .read_token(response, now_ms(), Some(&github))
                    .map_err(flow_error);
            }
            PollOutcome::CodexAuthorization { code, verifier } => {
                let request = flow
                    .login_request(LoginGrant::AuthorizationCode {
                        code: &code,
                        verifier: &verifier,
                        redirect_uri: "https://auth.openai.com/deviceauth/callback",
                        state: None,
                    })
                    .map_err(flow_error)?;
                let response = oauth::execute(http, flow, request)
                    .await
                    .map_err(flow_error)?;
                return flow
                    .read_token(response, now_ms(), None)
                    .map_err(flow_error);
            }
        }
    }
    Err(AuthError::MissingCredential {
        program: format!("ego built-in {} login", flow.id()),
        reason: "device authorization expired".to_string(),
    })
}

async fn browser_login(
    flow: BuiltInFlow,
    http: &dyn ego_ai::http::Http,
) -> Result<OAuthToken, AuthError> {
    let verifier = verifier();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let (redirect, port, callback_path) = match flow {
        BuiltInFlow::OpenAiCodex => (
            "http://localhost:1455/auth/callback",
            1455,
            "/auth/callback",
        ),
        BuiltInFlow::Anthropic => ("http://localhost:53692/callback", 53692, "/callback"),
        BuiltInFlow::OpenRouter => (
            "http://127.0.0.1:1456/oauth/callback",
            1456,
            "/oauth/callback",
        ),
        _ => unreachable!("only browser flows call this function"),
    };
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|source| AuthError::Io {
            path: PathBuf::from(format!("127.0.0.1:{port}")),
            source,
        })?;
    let url = flow
        .authorization_url(redirect, &oauth::pkce_challenge(&verifier), &state)
        .map_err(flow_error)?;
    eprintln!("Open this URL to authenticate:\n{url}");
    eprintln!(
        "Waiting for the loopback callback; paste the code or redirect URL if it cannot arrive:"
    );
    let (sender, pasted) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_line());
    });
    let pasted = async {
        pasted.await.map_err(|_| AuthError::Io {
            path: PathBuf::from("stdin"),
            source: std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "manual authorization input closed",
            ),
        })?
    };
    let code = wait_for_browser_code(listener, callback_path, &state, pasted).await?;
    let request = flow
        .login_request(LoginGrant::AuthorizationCode {
            code: &code,
            verifier: &verifier,
            redirect_uri: redirect,
            state: Some(&state),
        })
        .map_err(flow_error)?;
    let response = oauth::execute(http, flow, request)
        .await
        .map_err(flow_error)?;
    flow.read_token(response, now_ms(), None)
        .map_err(flow_error)
}

async fn wait_for_browser_code<F>(
    listener: TcpListener,
    callback_path: &str,
    expected_state: &str,
    pasted: F,
) -> Result<String, AuthError>
where
    F: Future<Output = Result<String, AuthError>>,
{
    tokio::pin!(pasted);
    tokio::select! {
        callback = read_loopback_callback(listener, callback_path, expected_state) => callback,
        input = &mut pasted => validated_authorization_code(&input?, expected_state, false),
    }
}

async fn read_loopback_callback(
    listener: TcpListener,
    callback_path: &str,
    expected_state: &str,
) -> Result<String, AuthError> {
    let (mut stream, _) = listener.accept().await.map_err(|source| AuthError::Io {
        path: PathBuf::from("OAuth loopback callback"),
        source,
    })?;
    let mut request = Vec::with_capacity(1024);
    while request.len() < 8192 && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let mut chunk = [0_u8; 1024];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|source| AuthError::Io {
                path: PathBuf::from("OAuth loopback callback"),
                source,
            })?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    let request = std::str::from_utf8(&request)
        .map_err(|err| AuthError::InvalidConfig(format!("OAuth callback was not UTF-8: {err}")))?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| {
            AuthError::InvalidConfig("OAuth callback had no request target".to_string())
        })?;
    let parsed = url::Url::parse(&format!("http://localhost{target}"))
        .map_err(|err| AuthError::InvalidConfig(format!("invalid OAuth callback: {err}")))?;
    let result = if parsed.path() == callback_path {
        validated_authorization_code(parsed.as_str(), expected_state, true)
    } else {
        Err(AuthError::InvalidConfig(format!(
            "OAuth callback used unexpected path `{}`",
            parsed.path()
        )))
    };
    let (status, message) = if result.is_ok() {
        (
            "200 OK",
            "Authentication complete. You can close this window.\n",
        )
    } else {
        ("400 Bad Request", "Authentication callback was refused.\n")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|source| AuthError::Io {
            path: PathBuf::from("OAuth loopback callback"),
            source,
        })?;
    result
}

fn validated_authorization_code(
    input: &str,
    expected_state: &str,
    require_state: bool,
) -> Result<String, AuthError> {
    let (code, returned_state) =
        authorization_code(input).ok_or_else(|| AuthError::MissingCredential {
            program: "built-in OAuth".to_string(),
            reason: "no authorization code was supplied".to_string(),
        })?;
    if (require_state && returned_state.is_none())
        || returned_state
            .as_deref()
            .is_some_and(|returned| returned != expected_state)
    {
        return Err(AuthError::MissingCredential {
            program: "built-in OAuth".to_string(),
            reason: "authorization state mismatch".to_string(),
        });
    }
    Ok(code)
}

async fn minimax_login(
    flow: BuiltInFlow,
    http: &dyn ego_ai::http::Http,
) -> Result<OAuthToken, AuthError> {
    let verifier = verifier();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let request = flow
        .minimax_user_code_request(&oauth::pkce_challenge(&verifier), &state)
        .map_err(flow_error)?;
    let response = oauth::execute(http, flow, request)
        .await
        .map_err(flow_error)?;
    let code = flow
        .read_minimax_user_code(response, &state)
        .map_err(flow_error)?;
    eprintln!(
        "Open {} and enter code {}",
        code.verification_uri, code.user_code
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in.max(1));
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(code.interval_ms.max(2_000))).await;
        let request = flow
            .login_request(LoginGrant::UserCode {
                user_code: &code.user_code,
                verifier: &verifier,
            })
            .map_err(flow_error)?;
        let response = oauth::execute(http, flow, request)
            .await
            .map_err(flow_error)?;
        match flow
            .read_poll(response, now_ms(), None)
            .map_err(flow_error)?
        {
            PollOutcome::Pending | PollOutcome::SlowDown => {}
            PollOutcome::Token(token) => return Ok(token),
            other => {
                return Err(AuthError::MissingCredential {
                    program: "ego built-in minimax-oauth login".to_string(),
                    reason: format!("unexpected token response: {other:?}"),
                });
            }
        }
    }
    Err(AuthError::MissingCredential {
        program: "ego built-in minimax-oauth login".to_string(),
        reason: "user-code authorization expired".to_string(),
    })
}

fn token_entry(flow: BuiltInFlow, token: OAuthToken) -> Result<AuthEntry, AuthError> {
    let values = token.into_values();
    if flow == BuiltInFlow::OpenAiCodex {
        oauth::codex_account_id(&values.access).map_err(flow_error)?;
    }
    Ok(if values.api_key {
        AuthEntry::ApiKey {
            key: values.access.into(),
        }
    } else {
        AuthEntry::OAuth {
            access: values.access.into(),
            refresh: values.refresh.map(Into::into),
            expires: values.expires,
        }
    })
}

fn verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn read_line() -> Result<String, AuthError> {
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|source| AuthError::Io {
            path: PathBuf::from("stdin"),
            source,
        })?;
    Ok(input.trim().to_string())
}

fn authorization_code(input: &str) -> Option<(String, Option<String>)> {
    if let Ok(url) = url::Url::parse(input) {
        let mut code = None;
        let mut state = None;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                _ => {}
            }
        }
        return code.map(|code| (code, state));
    }
    input
        .split_once('#')
        .map(|(code, state)| (code.to_string(), Some(state.to_string())))
        .or_else(|| (!input.trim().is_empty()).then(|| (input.trim().to_string(), None)))
}

fn flow_error(error: oauth::FlowError) -> AuthError {
    match error {
        oauth::FlowError::ReloginRequired { provider, message } => AuthError::ReloginRequired {
            provider,
            reason: message,
        },
        oauth::FlowError::Temporary {
            provider,
            status,
            message,
        } => AuthError::Temporary {
            provider,
            status,
            reason: message,
        },
        error => AuthError::MissingCredential {
            program: "built-in OAuth".to_string(),
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
fn apply_refresh_failure(
    store: &AuthStore,
    id: &str,
    error: oauth::FlowError,
) -> Result<(), AuthError> {
    let mut lock = store.open_lock()?;
    let _guard = lock.write().map_err(|source| AuthError::Io {
        path: store.lock_path(),
        source,
    })?;
    let mut entries = store.read_unlocked()?;
    apply_refresh_result_unlocked(store, &mut entries, id, Err(flow_error(error)))
}

fn apply_refresh_result_unlocked(
    store: &AuthStore,
    entries: &mut BTreeMap<String, AuthEntry>,
    id: &str,
    result: Result<AuthEntry, AuthError>,
) -> Result<(), AuthError> {
    match result {
        Ok(refreshed) => {
            entries.insert(id.to_string(), refreshed);
            store.write_unlocked(entries)
        }
        Err(error @ AuthError::ReloginRequired { .. }) => {
            entries.remove(id);
            store.write_unlocked(entries)?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// Prepare the single credential source used to assemble a provider request.
///
/// Refresh and the subsequent credential read use the same home and locked
/// store. Non-OAuth rows still delegate to their environment variable through
/// [`StoreCredentials`], preserving the existing API-key path.
pub async fn request_credentials(
    id: &str,
    command: Option<&str>,
    http: &dyn ego_ai::http::Http,
) -> Result<Arc<dyn Credentials>, AuthError> {
    let commands = OsCredentialCommand;
    let cancel = CancelToken::new();
    request_credentials_scoped(id, command, http, &commands, &cancel, None).await
}

pub(crate) async fn request_credentials_scoped(
    id: &str,
    command: Option<&str>,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<Arc<dyn Credentials>, AuthError> {
    let home = optional_ego_home();
    request_credentials_at_scoped(
        home.as_deref(),
        id,
        command,
        http,
        commands,
        cancel,
        run_deadline,
    )
    .await
}

#[cfg(test)]
async fn request_credentials_at(
    home: Option<&Path>,
    id: &str,
    command: Option<&str>,
    http: &dyn ego_ai::http::Http,
) -> Result<Arc<dyn Credentials>, AuthError> {
    let commands = OsCredentialCommand;
    let cancel = CancelToken::new();
    request_credentials_at_scoped(home, id, command, http, &commands, &cancel, None).await
}

async fn request_credentials_at_scoped(
    home: Option<&Path>,
    id: &str,
    command: Option<&str>,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<Arc<dyn Credentials>, AuthError> {
    refresh_for_run_at(home, id, command, http, commands, cancel, run_deadline).await?;
    Ok(match home {
        Some(home) => Arc::new(StoreCredentials::new(home)),
        None => Arc::new(EnvCredentials),
    })
}

/// Refresh a stored credential before request assembly, holding the same
/// exclusive lock across the network exchange and the rotating-token write.
async fn refresh_for_run_at(
    home: Option<&Path>,
    id: &str,
    command: Option<&str>,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<(), AuthError> {
    if oauth::built_in(id).is_none() && command.is_none() {
        return Ok(());
    }
    let Some(home) = home else {
        return Ok(());
    };
    let store = AuthStore::new(home);
    if !store.path().exists() {
        return Ok(());
    }
    let mut lock = store.open_lock()?;
    let _guard = lock.write().map_err(|source| AuthError::Io {
        path: store.lock_path(),
        source,
    })?;
    let mut entries = store.read_unlocked()?;
    let Some(current) = entries.get(id).cloned() else {
        return Ok(());
    };
    let expires = current.expires();
    if expires.is_none() || expires.is_some_and(|at| at > now_ms().saturating_add(300_000)) {
        return Ok(());
    }

    let plugin = discover_plugin(id, home, None);
    let source = select_source(id, command, plugin, oauth::built_in(id).is_some(), None);
    let refreshed = match source {
        CredentialSource::Command(program) => {
            run_configured_command_scoped(
                &program,
                "refresh",
                Some(&current),
                commands,
                cancel,
                run_deadline,
            )
            .await
        }
        CredentialSource::Plugin(program) => {
            run_program_scoped(
                &program,
                "refresh",
                Some(&current),
                commands,
                cancel,
                run_deadline,
            )
            .await
        }
        CredentialSource::BuiltIn(_) => refresh_built_in(id, &current, http).await,
        CredentialSource::Environment(_) | CredentialSource::Missing(_) => {
            return Err(AuthError::MissingCredential {
                program: format!("ego auth login {id}"),
                reason: "the stored credential expired and has no refresh implementation"
                    .to_string(),
            });
        }
    };
    ensure_store_may_mutate(cancel, run_deadline)?;
    apply_refresh_result_unlocked(&store, &mut entries, id, refreshed)
}

fn ensure_store_may_mutate(
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<(), AuthError> {
    if run_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        cancel.cancel(CancelCause::Timeout);
    }
    match cancel.cause() {
        Some(CancelCause::Timeout) => Err(AuthError::CredentialCommand {
            program: "credential refresh".to_string(),
            failure: CredentialCommandFailure::RunTimeout,
        }),
        Some(_) => Err(AuthError::CredentialCommand {
            program: "credential refresh".to_string(),
            failure: CredentialCommandFailure::Cancelled,
        }),
        None => Ok(()),
    }
}

async fn refresh_built_in(
    id: &str,
    current: &AuthEntry,
    http: &dyn ego_ai::http::Http,
) -> Result<AuthEntry, AuthError> {
    let flow = oauth::built_in(id).ok_or_else(|| AuthError::MissingCredential {
        program: format!("ego auth login {id}"),
        reason: "unknown built-in provider".to_string(),
    })?;
    let AuthEntry::OAuth {
        refresh: Some(refresh),
        ..
    } = current
    else {
        return Err(AuthError::MissingCredential {
            program: format!("ego auth login {id}"),
            reason: "the stored credential has no refresh token".to_string(),
        });
    };
    let request = if flow == BuiltInFlow::GitHubCopilot {
        flow.copilot_exchange_request(refresh.expose())
    } else {
        flow.refresh_request(refresh.expose())
    }
    .map_err(flow_error)?;
    let response = oauth::execute(http, flow, request)
        .await
        .map_err(flow_error)?;
    let token = flow
        .read_token(response, now_ms(), Some(refresh.expose()))
        .map_err(flow_error)?;
    token_entry(flow, token)
}

fn known_and_discovered(home: &Path) -> String {
    let mut ids: Vec<String> = BuiltInFlow::all()
        .iter()
        .map(|flow| flow.id().to_string())
        .collect();
    let plugin_dir = home.join("plugins/auth");
    if let Ok(entries) = fs::read_dir(plugin_dir) {
        for entry in entries.flatten() {
            if executable(&entry.path()) {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids.into_iter().map(|id| format!("{id}\n")).collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuthConfig {
    pub credential_command: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl AuthConfig {
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        let sourced = ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, text)
            .map_err(|err| AuthError::InvalidConfig(err.to_string()))?;
        let effective =
            ego_core::config::resolve(&[sourced], ego_core::config::Trust::Untrusted, None)
                .map_err(|err| AuthError::InvalidConfig(err.to_string()))?;
        let config = Self {
            auth: effective
                .auth
                .into_iter()
                .map(|(id, provider)| {
                    (
                        id,
                        ProviderAuthConfig {
                            credential_command: provider.credential_command,
                            headers: provider.headers.into_iter().collect(),
                        },
                    )
                })
                .collect(),
        };
        for (id, provider) in &config.auth {
            for name in provider.headers.keys() {
                if forbidden_header(name) {
                    return Err(AuthError::InvalidConfig(format!(
                        "auth.{id}.headers refuses forbidden header `{name}`"
                    )));
                }
            }
        }
        Ok(config)
    }
}

fn forbidden_header(name: &str) -> bool {
    FORBIDDEN_HEADERS
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
}

/// Apply operator header data by case-insensitive name. Empty removes.
pub fn overlay_headers(
    defaults: &[(String, String)],
    overlay: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, AuthError> {
    for name in overlay.keys() {
        if forbidden_header(name) {
            return Err(AuthError::InvalidConfig(format!(
                "header overlay refuses forbidden header `{name}`"
            )));
        }
    }
    let mut result = defaults.to_vec();
    for (name, value) in overlay {
        result.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        if !value.is_empty() {
            result.push((name.clone(), value.clone()));
        }
    }
    Ok(result)
}

/// The source selected before a login or token resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Command(String),
    Plugin(PathBuf),
    BuiltIn(String),
    Environment(String),
    Missing(String),
}

pub fn select_source(
    id: &str,
    command: Option<&str>,
    plugin: Option<PathBuf>,
    built_in: bool,
    environment: Option<&str>,
) -> CredentialSource {
    if let Some(command) = command {
        CredentialSource::Command(command.to_string())
    } else if let Some(plugin) = plugin {
        CredentialSource::Plugin(plugin)
    } else if built_in {
        CredentialSource::BuiltIn(id.to_string())
    } else if let Some(var) = environment {
        CredentialSource::Environment(var.to_string())
    } else {
        CredentialSource::Missing(id.to_string())
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// Discover `$EGO_HOME/plugins/auth/<id>`, then `ego-auth-<id>` on PATH.
pub fn discover_plugin(id: &str, home: &Path, path: Option<&Path>) -> Option<PathBuf> {
    let local = home.join("plugins").join("auth").join(id);
    if executable(&local) {
        return Some(local);
    }
    let name = format!("ego-auth-{id}");
    let search = path
        .map(|path| path.as_os_str().to_os_string())
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(&name))
        .find(|candidate| executable(candidate))
}

/// Run the two-verb credential program without ever logging stdout.
pub async fn run_program(
    program: &Path,
    verb: &str,
    current: Option<&AuthEntry>,
) -> Result<AuthEntry, AuthError> {
    let commands = OsCredentialCommand;
    run_program_scoped(program, verb, current, &commands, &CancelToken::new(), None).await
}

async fn run_configured_command_scoped(
    command: &str,
    verb: &str,
    current: Option<&AuthEntry>,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<AuthEntry, AuthError> {
    let mut words = shell_words::split(command).map_err(|err| AuthError::MissingCredential {
        program: command.to_string(),
        reason: format!("cannot parse command: {err}"),
    })?;
    if words.is_empty() {
        return Err(AuthError::MissingCredential {
            program: command.to_string(),
            reason: "command is empty".to_string(),
        });
    }
    let program = PathBuf::from(words.remove(0));
    run_program_parts(
        &program,
        &words,
        command,
        verb,
        current,
        CredentialCommandScope {
            commands,
            cancel,
            run_deadline,
        },
    )
    .await
}

async fn run_program_scoped(
    program: &Path,
    verb: &str,
    current: Option<&AuthEntry>,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<AuthEntry, AuthError> {
    run_program_parts(
        program,
        &[],
        &program.display().to_string(),
        verb,
        current,
        CredentialCommandScope {
            commands,
            cancel,
            run_deadline,
        },
    )
    .await
}

async fn run_program_parts(
    program: &Path,
    arguments: &[String],
    label: &str,
    verb: &str,
    current: Option<&AuthEntry>,
    scope: CredentialCommandScope<'_>,
) -> Result<AuthEntry, AuthError> {
    let mut argv = Vec::with_capacity(arguments.len() + 2);
    argv.push(program.as_os_str().to_os_string());
    argv.extend(arguments.iter().map(OsString::from));
    argv.push(OsString::from(verb));
    let stdin = current.map(|entry| {
        SecretMaterial::new(serde_json::to_vec(entry).expect("an auth entry serialises"))
    });
    let stdout = scope
        .commands
        .run(CredentialCommandRequest {
            argv,
            stdin,
            cancel: scope.cancel.clone(),
            run_deadline: scope.run_deadline,
        })
        .await
        .map_err(|failure| match failure {
            CredentialCommandFailure::Exit(code) => missing_label(
                label,
                code.map_or_else(
                    || "exited by signal".to_string(),
                    |code| format!("exited with code {code}"),
                ),
            ),
            CredentialCommandFailure::Io(reason) => missing_label(label, reason),
            failure => AuthError::CredentialCommand {
                program: label.to_string(),
                failure,
            },
        })?;
    parse_program_output(label, stdout.expose_secret_bytes())
}

fn parse_program_output(program: &str, stdout: &[u8]) -> Result<AuthEntry, AuthError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|err| missing_label(program, err.to_string()))?
        .trim();
    if text.is_empty() {
        return Err(missing_label(program, "printed an empty credential"));
    }
    #[derive(Deserialize)]
    struct Blob {
        access: StoredSecret,
        #[serde(default)]
        refresh: Option<StoredSecret>,
        #[serde(default)]
        expires: Option<u64>,
    }
    match serde_json::from_str::<Blob>(text) {
        Ok(blob) => Ok(AuthEntry::OAuth {
            access: blob.access,
            refresh: blob.refresh,
            expires: blob.expires,
        }),
        Err(_) if text.starts_with('{') => {
            Err(missing_label(program, "printed invalid credential JSON"))
        }
        Err(_) => Ok(AuthEntry::ApiKey { key: text.into() }),
    }
}

fn missing_label(program: &str, reason: impl Into<String>) -> AuthError {
    AuthError::MissingCredential {
        program: program.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::future;

    use ego_ai::oauth::{BuiltInFlow, FlowError};
    use ego_ai::provider::ProviderRow;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        AuthEntry, AuthError, AuthStore, LoginMethod, StoredSecret, apply_refresh_failure,
        authorization_code, login_methods, request_credentials_at, wait_for_browser_code,
    };

    struct UnexpectedHttp;

    impl ego_ai::http::Http for UnexpectedHttp {
        fn begin_call(&self) -> Box<dyn ego_ai::http::HttpCall> {
            panic!("credential tests must not perform an HTTP exchange")
        }
    }

    #[tokio::test]
    async fn request_credentials_read_the_token_written_by_login() {
        let root = tempfile::tempdir().unwrap();
        AuthStore::new(root.path())
            .put(
                "nous",
                AuthEntry::OAuth {
                    access: StoredSecret::from("login-access"),
                    refresh: Some(StoredSecret::from("login-refresh")),
                    expires: Some(4_102_444_800_000),
                },
            )
            .unwrap();

        let credentials = request_credentials_at(Some(root.path()), "nous", None, &UnexpectedHttp)
            .await
            .unwrap();
        let credential = ProviderRow::nous_default()
            .credential
            .resolve(credentials.as_ref())
            .unwrap();

        assert!(credential.headers().iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer login-access"
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_credentials_refresh_the_same_store_before_reading_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let store = AuthStore::new(root.path());
        store
            .put(
                "nous",
                AuthEntry::OAuth {
                    access: StoredSecret::from("expired-access"),
                    refresh: Some(StoredSecret::from("rotating-refresh")),
                    expires: Some(1),
                },
            )
            .unwrap();
        let command = root.path().join("refresh-token");
        std::fs::write(
            &command,
            "#!/bin/sh\n[ \"$1\" = refresh ] || exit 9\ncat >/dev/null\nprintf '%s\\n' '{\"access\":\"refreshed-access\",\"refresh\":\"refreshed-refresh\",\"expires\":4102444800000}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

        let credentials = request_credentials_at(
            Some(root.path()),
            "nous",
            Some(command.to_str().unwrap()),
            &UnexpectedHttp,
        )
        .await
        .unwrap();
        let credential = ProviderRow::nous_default()
            .credential
            .resolve(credentials.as_ref())
            .unwrap();

        assert!(credential.headers().iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer refreshed-access"
        }));
        assert_eq!(
            store.read().unwrap().get("nous").unwrap(),
            &AuthEntry::OAuth {
                access: StoredSecret::from("refreshed-access"),
                refresh: Some(StoredSecret::from("refreshed-refresh")),
                expires: Some(4_102_444_800_000),
            }
        );
    }

    #[test]
    fn pasted_redirect_keeps_state_for_csrf_validation() {
        assert_eq!(
            authorization_code("http://localhost/callback?code=granted&state=expected"),
            Some(("granted".to_string(), Some("expected".to_string())))
        );
        assert_eq!(
            authorization_code("granted#expected"),
            Some(("granted".to_string(), Some("expected".to_string())))
        );
    }

    #[test]
    fn tty_codex_offers_device_default_and_browser_while_headless_is_device_only() {
        assert_eq!(
            login_methods(BuiltInFlow::OpenAiCodex, true, false),
            vec![LoginMethod::Device, LoginMethod::Browser]
        );
        assert_eq!(
            login_methods(BuiltInFlow::OpenAiCodex, false, false),
            vec![LoginMethod::Device]
        );
        assert_eq!(
            login_methods(BuiltInFlow::OpenAiCodex, true, true),
            vec![LoginMethod::Device]
        );
        assert_eq!(
            login_methods(BuiltInFlow::Anthropic, true, false),
            vec![LoginMethod::Browser]
        );
    }

    #[tokio::test]
    async fn loopback_callback_can_win_the_race_against_manual_paste() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"GET /auth/callback?code=from-loopback&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        });

        let code = wait_for_browser_code(
            listener,
            "/auth/callback",
            "expected",
            future::pending::<Result<String, AuthError>>(),
        )
        .await
        .unwrap();

        assert_eq!(code, "from-loopback");
        assert!(client.await.unwrap().starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn manual_paste_can_win_the_race_against_loopback() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let pasted = future::ready(Ok(
            "http://localhost/auth/callback?code=from-paste&state=expected".to_string(),
        ));

        let code = wait_for_browser_code(listener, "/auth/callback", "expected", pasted)
            .await
            .unwrap();

        assert_eq!(code, "from-paste");
    }

    #[tokio::test]
    async fn loopback_callback_refuses_a_missing_or_mismatched_state() {
        for query in ["code=missing", "code=wrong&state=unexpected"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let request = format!("GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let client = tokio::spawn(async move {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
                response
            });

            let error = wait_for_browser_code(
                listener,
                "/auth/callback",
                "expected",
                future::pending::<Result<String, AuthError>>(),
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("state mismatch"), "{error}");
            assert!(client.await.unwrap().starts_with("HTTP/1.1 400"));
        }
    }

    #[test]
    fn invalid_grant_removes_dead_state_but_429_preserves_retryable_state() {
        let root = tempfile::tempdir().unwrap();
        let store = AuthStore::new(root.path());
        let entry = AuthEntry::OAuth {
            access: StoredSecret::from("access"),
            refresh: Some(StoredSecret::from("refresh")),
            expires: Some(1),
        };
        store.put("openai-codex", entry.clone()).unwrap();

        let invalid = FlowError::ReloginRequired {
            provider: "openai-codex".to_string(),
            message: "invalid_grant".to_string(),
        };
        let err = apply_refresh_failure(&store, "openai-codex", invalid).unwrap_err();
        assert!(matches!(err, AuthError::ReloginRequired { .. }));
        assert!(!store.read().unwrap().contains_key("openai-codex"));

        store.put("openai-codex", entry).unwrap();
        let limited = FlowError::Temporary {
            provider: "openai-codex".to_string(),
            status: 429,
            message: "rate_limited".to_string(),
        };
        let err = apply_refresh_failure(&store, "openai-codex", limited).unwrap_err();
        assert!(matches!(err, AuthError::Temporary { status: 429, .. }));
        assert!(store.read().unwrap().contains_key("openai-codex"));
    }
}
