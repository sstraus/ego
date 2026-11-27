//! Subscription credential storage and operator-provided login programs.
//!
//! The store deliberately has one shape and one lock. Login, refresh and
//! logout all use the same read-modify-write operation, so rotating refresh
//! tokens cannot be lost when two ego processes share an account.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use ego_ai::oauth::{self, BuiltInFlow, LoginGrant, LoginHeaders, OAuthToken, PollOutcome};
use ego_ai::provider::{Credentials, EnvCredentials, MissingCredential, Secret};
use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest, run_secret_child};
use ego_tools::privacy::{PrivatePathKind, harden_private_path};

mod key_input;
pub(crate) mod startup;

/// Default per-call limit; `auth.PROVIDER.credential_timeout_secs` overrides it.
const PROGRAM_TIMEOUT: Duration = Duration::from_secs(120);
const PROGRAM_TERMINATION_GRACE: Duration = Duration::from_millis(500);

#[cfg(all(feature = "test-support", debug_assertions))]
static TEST_LOGIN_HTTP: std::sync::OnceLock<Arc<dyn ego_ai::http::Http>> =
    std::sync::OnceLock::new();

/// Install the dedicated PTY fixture's closed OAuth transport.
#[cfg(all(feature = "test-support", debug_assertions))]
pub fn install_test_login_http(
    http: Arc<dyn ego_ai::http::Http>,
) -> Result<(), Arc<dyn ego_ai::http::Http>> {
    TEST_LOGIN_HTTP.set(http)
}

const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "content-type",
    "accept",
    "openai-beta",
    "chatgpt-account-id",
];

/// Established auth-store name for the single core-owned secret allocation.
pub type StoredSecret = SecretMaterial;

/// One entry in `auth.json`.
#[derive(Debug)]
pub enum AuthEntry {
    OAuth {
        access: StoredSecret,
        refresh: Option<StoredSecret>,
        expires: Option<u64>,
        scope_binding: Option<String>,
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

    /// Valid API-key presence, without conflating OAuth material with a key.
    pub(crate) fn has_api_key(&self) -> bool {
        matches!(self, Self::ApiKey { key }
            if key_input::valid(key.expose_secret_bytes()))
    }

    /// OAuth source presence, not token validity or entitlement.
    pub(crate) fn has_oauth(&self) -> bool {
        matches!(self, Self::OAuth { .. })
    }

    fn into_active_secret(self) -> StoredSecret {
        match self {
            Self::OAuth { access, .. } => access,
            Self::ApiKey { key } => key,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AuthEntryWire {
    OAuth {
        access: String,
        #[serde(default)]
        refresh: Option<String>,
        #[serde(default)]
        expires: Option<u64>,
        #[serde(default)]
        scope_binding: Option<String>,
    },
    ApiKey {
        key: String,
    },
}

impl From<AuthEntryWire> for AuthEntry {
    fn from(entry: AuthEntryWire) -> Self {
        match entry {
            AuthEntryWire::OAuth {
                access,
                refresh,
                expires,
                scope_binding,
            } => Self::OAuth {
                access: access.into(),
                refresh: refresh.map(Into::into),
                expires,
                scope_binding,
            },
            AuthEntryWire::ApiKey { key } => Self::ApiKey { key: key.into() },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AuthEntryRef<'a> {
    OAuth {
        access: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope_binding: Option<&'a str>,
    },
    ApiKey {
        key: &'a str,
    },
}

fn secret_text(secret: &SecretMaterial) -> Result<&str, &'static str> {
    std::str::from_utf8(secret.expose_secret_bytes())
        .map_err(|_| "credential material is not UTF-8")
}

fn encode_entry(entry: &AuthEntry) -> Result<AuthEntryRef<'_>, &'static str> {
    match entry {
        AuthEntry::OAuth {
            access,
            refresh,
            expires,
            scope_binding,
        } => Ok(AuthEntryRef::OAuth {
            access: secret_text(access)?,
            refresh: refresh.as_ref().map(secret_text).transpose()?,
            expires: *expires,
            scope_binding: scope_binding.as_deref(),
        }),
        AuthEntry::ApiKey { key } => Ok(AuthEntryRef::ApiKey {
            key: secret_text(key)?,
        }),
    }
}

const CONNECTION_STORAGE_PREFIX: &str = "connection:";

/// Named IDs cannot collide with legacy provider slots, even when the visible
/// user-config key is itself a provider name.
pub(crate) fn connection_storage_key(id: &str) -> String {
    format!("{CONNECTION_STORAGE_PREFIX}{id}")
}

pub(crate) fn connection_id_from_storage_key(key: &str) -> Option<&str> {
    key.strip_prefix(CONNECTION_STORAGE_PREFIX)
}

/// A named account never consults another saved account or an ambient token.
struct ConnectionCredentials {
    store: AuthStore,
    id: String,
    route: String,
    key_source: Option<(String, KeySource)>,
    context_vars: Vec<String>,
    scope_binding: Option<String>,
}

enum KeySource {
    Environment(String),
    Store,
}

impl Credentials for ConnectionCredentials {
    fn secret(&self, name: &str) -> Result<Secret, MissingCredential> {
        let missing = || MissingCredential {
            var: format!("connection {}: {name}", self.id),
        };
        if let Some((target, source)) = &self.key_source {
            return if name == target {
                match source {
                    KeySource::Environment(source) => {
                        EnvCredentials.secret(source).and_then(|secret| {
                            if secret.expose_secret_bytes().len()
                                >= ego_core::secrets::MIN_ENV_SECRET_LEN
                            {
                                Ok(secret)
                            } else {
                                Err(missing())
                            }
                        })
                    }
                    KeySource::Store => {
                        let entry = self
                            .store
                            .read()
                            .map_err(|_| missing())?
                            .remove(&connection_storage_key(&self.id))
                            .ok_or_else(missing)?;
                        if entry.has_api_key() {
                            Ok(entry.into_active_secret())
                        } else {
                            Err(missing())
                        }
                    }
                }
            } else {
                Err(missing())
            };
        }
        // Cloud project/location are declared request context, not credential fallbacks.
        if self.context_vars.iter().any(|var| var == name) {
            return EnvCredentials.secret(name);
        }
        if name != format!("oauth:{}", self.route) {
            return Err(missing());
        }
        match self
            .store
            .read()
            .map_err(|_| missing())?
            .remove(&connection_storage_key(&self.id))
        {
            Some(AuthEntry::OAuth {
                access,
                scope_binding,
                expires,
                ..
            }) if scope_binding == self.scope_binding
                && (scope_binding.is_none() || expires.is_some_and(|at| at > now_ms())) =>
            {
                Ok(access)
            }
            _ => Err(missing()),
        }
    }
}

/// Bind the credential slot and selected refresh source, not secret values or display text.
pub(crate) fn connection_binding(
    id: &str,
    account: &ego_core::config::ConnectionConfig,
    source: Option<&CredentialSource>,
) -> String {
    let refresh = source.map(|source| match source {
        CredentialSource::Command(program) => ("command", program.as_bytes().to_vec()),
        CredentialSource::Plugin(program) => {
            ("plugin", program.as_os_str().as_encoded_bytes().to_vec())
        }
        _ => ("default", Vec::new()),
    });
    let bytes = serde_json::to_vec(&(
        2u8,
        id,
        &account.vendor,
        account.access,
        &account.key_env,
        refresh,
    ))
    .expect("credential source fields serialize");
    if let Some(resource) = &account.azure_resource {
        // Keep existing non-Azure durable bindings stable.
        return ego_core::digest::sha256_hex(
            &serde_json::to_vec(&(bytes, resource)).expect("Azure resource serializes"),
        );
    }
    ego_core::digest::sha256_hex(&bytes)
}

pub(crate) fn connection_refresh_source(
    route: &str,
    command: Option<&str>,
    home: Option<&Path>,
) -> CredentialSource {
    select_source(
        route,
        command,
        home.and_then(|home| discover_plugin(route, home, None)),
        oauth::built_in(route).is_some() || google_cloud::is_google_cloud(route),
        None,
    )
}

pub(crate) const CHANGED_CONNECTION_BINDING: &str =
    "durable task has no matching credential binding; start a new task";

#[expect(
    clippy::too_many_arguments,
    reason = "account identity and provider strategy stay separate from request lifetime authority"
)]
pub(crate) async fn connection_credentials(
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
    route: &str,
    strategy: &ego_ai::provider::CredentialStrategy,
    auth: &HashMap<String, ego_core::config::AuthProviderLayer>,
    expected_binding: Option<&str>,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    deadline: Option<Instant>,
) -> Result<Arc<dyn Credentials>, AuthError> {
    use ego_ai::provider::CredentialStrategy;
    use ego_core::config::ConnectionAccess;
    let oauth = connection.access == ConnectionAccess::Oauth;
    let source_route = connection_route(connection);
    if oauth && oauth_strategy_route(strategy) != Some(source_route.as_str()) {
        return Err(AuthError::InvalidConfig(format!(
            "connection {id} requires OAuth route {source_route}"
        )));
    }
    let home = ego_home()?;
    let provider = auth.get(&source_route);
    let refresh = oauth.then(|| {
        let command = provider.and_then(|config| config.credential_command.as_deref());
        connection_refresh_source(&source_route, command, Some(&home))
    });
    if let Some(expected) = expected_binding
        && ((!oauth && route != source_route)
            || connection_binding(id, connection, refresh.as_ref()) != expected)
    {
        return Err(AuthError::InvalidConfig(CHANGED_CONNECTION_BINDING.into()));
    }
    let (key_source, scope_binding) = match refresh {
        Some(source) => {
            let source = if source_route == ego_core::config::AZURE_OPENAI {
                connection.validate(id).map_err(AuthError::InvalidConfig)?;
                let command = provider.and_then(|config| config.credential_command.as_deref());
                let scoped = azure::scoped_command(connection, command)?;
                CredentialSource::Command(scoped)
            } else {
                source
            };
            let scope_binding = refresh_connection_at(
                &home,
                &connection_storage_key(id),
                &source_route,
                source,
                provider,
                http,
                commands,
                cancel,
                deadline,
            )
            .await
            .map_err(|mut error| {
                if let AuthError::ReloginRequired { connection, .. } = &mut error {
                    *connection = Some(id.to_owned());
                }
                error
            })?;
            (None, scope_binding)
        }
        None => {
            let target = strategy.environment_var().ok_or_else(|| {
                AuthError::InvalidConfig(format!("connection {id} requires a key route"))
            })?;
            let source = connection
                .key_env
                .clone()
                .map_or(KeySource::Store, KeySource::Environment);
            (Some((target.to_owned(), source)), None)
        }
    };
    Ok(Arc::new(ConnectionCredentials {
        store: AuthStore::new(home),
        id: id.into(),
        route: if oauth { source_route } else { route.into() },
        key_source,
        scope_binding,
        context_vars: match strategy {
            CredentialStrategy::GoogleCloud {
                project_var,
                location_var,
                ..
            } => {
                vec![project_var.clone(), location_var.clone()]
            }
            _ => Vec::new(),
        },
    }))
}

pub(crate) fn oauth_connection_supported(strategy: &ego_ai::provider::CredentialStrategy) -> bool {
    oauth_strategy_route(strategy).is_some()
}

fn oauth_strategy_route(strategy: &ego_ai::provider::CredentialStrategy) -> Option<&str> {
    use ego_ai::provider::CredentialStrategy;
    match strategy {
        CredentialStrategy::OAuth { provider }
        | CredentialStrategy::OAuthOrEnv { provider, .. }
        | CredentialStrategy::Anthropic { provider, .. }
        | CredentialStrategy::GoogleCloud { provider, .. } => Some(provider),
        _ => None,
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
        connection: Option<String>,
        cause: oauth::TokenRefusal,
    },
    Temporary {
        provider: String,
        status: u16,
        cause: oauth::TokenRefusal,
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
                write!(f, "could not get login details from '{program}': {reason}")
            }
            Self::CredentialCommand { program, failure } => {
                write!(f, "could not get login details from '{program}': {failure}")
            }
            Self::ReloginRequired {
                provider,
                connection,
                cause,
            } => {
                if connection.is_some() {
                    write!(
                        f,
                        "{provider} login expired ({cause}); {}",
                        renewal_hint(provider)
                    )
                } else {
                    write!(
                        f,
                        "{provider} legacy login expired ({cause}); {} To repair an existing saved account instead, {}",
                        legacy_login_remedy(provider),
                        renewal_hint(provider)
                    )
                }
            }
            Self::Temporary {
                provider,
                status,
                cause,
            } => write!(
                f,
                "{provider} token endpoint temporarily failed ({status}): {cause}"
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
    /// `stderr` is the helper's masked, bounded diagnostic; empty when it printed none.
    Exit {
        code: Option<i32>,
        stderr: String,
    },
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
            Self::Exit { code, stderr } => {
                match code {
                    Some(code) => write!(formatter, "exited with code {code}")?,
                    None => formatter.write_str("exited by signal")?,
                }
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
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
    timeout: Duration,
}

#[derive(Clone, Copy)]
struct CredentialCommandScope<'a> {
    commands: &'a dyn CredentialCommandPort,
    cancel: &'a CancelToken,
    run_deadline: Option<Instant>,
    timeout: Duration,
}

/// The owner's `credential_timeout_secs` for a provider, else the default.
fn credential_timeout(config: Option<&ego_core::config::AuthProviderLayer>) -> Duration {
    config
        .and_then(|provider| provider.credential_timeout_secs)
        .map_or(PROGRAM_TIMEOUT, Duration::from_secs)
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
                command_timeout: request.timeout,
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
        SecretChildError::Exit { code, stderr } => CredentialCommandFailure::Exit { code, stderr },
        SecretChildError::CleanupUncertain { cause } => {
            CredentialCommandFailure::CleanupUncertain {
                cause: cause.map(|cause| Box::new(credential_child_failure(*cause))),
            }
        }
        SecretChildError::InvalidRequest(reason) => {
            CredentialCommandFailure::Io(reason.to_string())
        }
        SecretChildError::ProtocolFraming
        | SecretChildError::ProtocolEarlyEof
        | SecretChildError::ProtocolOutputLimit => CredentialCommandFailure::Io(error.to_string()),
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
        harden_private_path(&self.home, PrivatePathKind::Directory).map_err(|source| {
            AuthError::Io {
                path: self.home.clone(),
                source,
            }
        })?;
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
        let bytes = SecretMaterial::new(bytes);
        serde_json::from_slice::<BTreeMap<String, AuthEntryWire>>(bytes.expose_secret_bytes())
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|(id, entry)| (id, entry.into()))
                    .collect()
            })
            .map_err(|err| AuthError::CorruptStore {
                path,
                message: err.to_string(),
            })
    }

    /// Read the store without bringing any of it into existence.
    ///
    /// [`read`](Self::read) creates the home directory and the lock file in
    /// order to take a write lock, which is right for a command about to log
    /// in and wrong for `ego doctor`, whose whole contract is that observing
    /// changes nothing. So: no store, no entries — an installation nobody has
    /// authenticated has none, and saying so is a fact rather than a failure.
    ///
    /// The lock is taken shared, and only if it already exists. A store with
    /// no lock beside it is one no writer has ever held, so reading it
    /// unguarded races with nothing.
    pub fn read_observed(&self) -> Result<BTreeMap<String, AuthEntry>, AuthError> {
        if !self.path().exists() {
            return Ok(BTreeMap::new());
        }
        let path = self.lock_path();
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            return self.read_unlocked();
        };
        let lock = RwLock::new(file);
        let _guard = lock
            .read()
            .map_err(|source| AuthError::Io { path, source })?;
        self.read_unlocked()
    }

    /// Provider identities present in the observed store, without credentials.
    ///
    /// Startup selection needs to distinguish a saved OAuth identity from no
    /// configured source. It does not need, and must not receive, the access or
    /// refresh material. [`read_observed`](Self::read_observed) also guarantees
    /// this projection creates no home, lock, or store and refreshes nothing.
    pub(crate) fn observed_provider_ids(&self) -> Result<BTreeSet<String>, AuthError> {
        let connections = connection_layer_at(&self.home)?.connections;
        Ok(self
            .read_observed()?
            .into_iter()
            .filter(|(_, entry)| entry.has_oauth())
            .filter_map(|(key, _)| match connection_id_from_storage_key(&key) {
                Some(id) => connections
                    .get(id)
                    .filter(|connection| {
                        connection.access == ego_core::config::ConnectionAccess::Oauth
                    })
                    .map(connection_route),
                None => None,
            })
            .collect())
    }

    pub fn read(&self) -> Result<BTreeMap<String, AuthEntry>, AuthError> {
        let mut lock = self.open_lock()?;
        let _guard = lock.write().map_err(|source| AuthError::Io {
            path: self.lock_path(),
            source,
        })?;
        self.read_unlocked()
    }

    pub fn read_secret_material(&self, id: &str) -> Result<SecretMaterial, AuthError> {
        let mut lock = self.open_lock()?;
        let _guard = lock.write().map_err(|source| AuthError::Io {
            path: self.lock_path(),
            source,
        })?;
        let mut entries = self.read_unlocked()?;
        entries
            .remove(id)
            .map(AuthEntry::into_active_secret)
            .ok_or_else(|| AuthError::MissingCredential {
                program: format!("ego auth login {id}"),
                reason: "no stored credential".to_string(),
            })
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
        let encoded: Result<BTreeMap<&str, AuthEntryRef<'_>>, _> = entries
            .iter()
            .map(|(id, entry)| encode_entry(entry).map(|entry| (id.as_str(), entry)))
            .collect();
        let encoded = encoded.map_err(|message| AuthError::CorruptStore {
            path: path.clone(),
            message: message.to_string(),
        })?;
        let bytes = SecretMaterial::new(serde_json::to_vec_pretty(&encoded).map_err(|err| {
            AuthError::CorruptStore {
                path: path.clone(),
                message: err.to_string(),
            }
        })?);
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
        harden_private_path(&tmp, PrivatePathKind::File).map_err(|source| AuthError::Io {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(bytes.expose_secret_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| AuthError::Io {
                path: tmp.clone(),
                source,
            })?;
        fs::rename(&tmp, &path).map_err(|source| AuthError::Io {
            path: path.clone(),
            source,
        })?;
        harden_private_path(&path, PrivatePathKind::File)
            .map_err(|source| AuthError::Io { path, source })?;
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
    let config = connection_layer_at(&home)?;
    if args.action == crate::cli::AuthAction::Logout
        && let Some(id) = args.connection.as_deref()
        && !config.connections.contains_key(id)
    {
        let removed = store.remove(&connection_storage_key(id))?;
        return Ok(if removed {
            format!("logged out {id}\n")
        } else {
            format!("no stored credential for {id}\n")
        });
    }
    let config = if args.connection.is_none() && !config.connections.is_empty() {
        numbered_connection_layer_at(&home, config)?
    } else {
        config
    };
    let vendor = args.provider.as_deref().map(connection_vendor);
    let selected = if let Some(account) = args.account {
        let id = account_id(
            &config.account_numbers,
            &config.connections,
            vendor,
            account.get(),
        )
        .map_err(AuthError::InvalidConfig)?;
        config
            .connections
            .get_key_value(id)
            .map(|(id, config)| (id.as_str(), config))
    } else if args.connection.is_some()
        || (vendor.is_some()
            && (args.action != crate::cli::AuthAction::Login || args.renew)
            && !matches!(
                args.action,
                crate::cli::AuthAction::Status
                    | crate::cli::AuthAction::StatusJson
                    | crate::cli::AuthAction::Usage { .. }
            ))
    {
        ego_core::config::select_connection(&config.connections, args.connection.as_deref(), vendor)
            .map_err(AuthError::InvalidConfig)?
    } else {
        None
    };
    if args.renew && selected.is_none() {
        return Err(AuthError::InvalidConfig(
            "no saved account to renew; specify a provider with a saved account".into(),
        ));
    }
    match args.action {
        crate::cli::AuthAction::Usage { json } => {
            let key = selected.map(|(id, _)| connection_storage_key(id));
            crate::auth_usage::execute(&home, key.as_deref(), vendor, json, cancel)
                .await
                .map_err(AuthError::InvalidConfig)
        }
        crate::cli::AuthAction::StatusJson => {
            let selected_key = selected.map(|(id, _)| connection_storage_key(id));
            status_json(&store, &config.connections, selected_key.as_deref(), vendor)
        }
        crate::cli::AuthAction::Status => {
            if selected.is_none() {
                let stored = store.read()?;
                let mut output = String::new();
                let mut accounts: Vec<_> = config
                    .connections
                    .iter()
                    .filter(|(_, entry)| vendor.is_none_or(|vendor| entry.vendor == vendor))
                    .collect();
                accounts.sort_by_key(|(id, entry)| {
                    (&entry.vendor, account_number(&config, &entry.vendor, id))
                });
                for (id, connection) in accounts {
                    let label = account_label(&config, &connection.vendor, id);
                    output.push_str(&format!(
                        "{label}: {} ({:?})\n",
                        connection.name, connection.access
                    ));
                    if connection.access == ego_core::config::ConnectionAccess::Oauth {
                        output.push_str(&connection_status(&store, id, &label)?);
                    } else {
                        output.push_str(&key_status(&store, id, connection));
                    }
                }
                for key in stored.keys() {
                    if let Some(id) = connection_id_from_storage_key(key)
                        && vendor.is_none()
                        && !config.connections.contains_key(id)
                    {
                        output.push_str(&format!(
                            "orphan [{id}]: remove with ego auth --connection {id} logout\n"
                        ));
                    }
                }
                if output.is_empty() {
                    crate::output::note("no saved accounts; see 'ego auth login --help'");
                }
                return Ok(output);
            }
            let text = match selected {
                Some((id, connection))
                    if connection.access == ego_core::config::ConnectionAccess::Oauth =>
                {
                    connection_status(&store, id, &account_label(&config, &connection.vendor, id))?
                }
                Some((id, connection)) => key_status(&store, id, connection),
                None => unreachable!("unselected account status is handled above"),
            };
            Ok(match selected {
                Some((id, connection)) => format!(
                    "{}: {} ({:?})\n{text}",
                    account_label(&config, &connection.vendor, id),
                    connection.name,
                    connection.access
                ),
                None => text,
            })
        }
        crate::cli::AuthAction::Remove => {
            use ego_core::config::{UserConfigMutation, UserConfigStore};
            let (id, connection) = selected
                .ok_or_else(|| AuthError::InvalidConfig("no saved account to remove".into()))?;
            ensure_store_may_mutate(cancel, None)?;
            let label = account_label(&config, &connection.vendor, id);
            // Use the auth lock before the config lock, like credential
            // publication. A late renewal cannot resurrect a retired slot.
            store.modify(|entries| {
                ego_tools::config::OsUserConfigStore::new(
                    Arc::new(ego_tools::store::OsIo),
                    home.join("config.toml"),
                )
                .mutate(UserConfigMutation::RemoveConnection {
                    id: id.into(),
                    expected: connection.clone(),
                })
                .map_err(|error| AuthError::InvalidConfig(error.to_string()))?;
                entries.remove(&connection_storage_key(id));
                Ok(())
            })?;
            Ok(format!("removed {label}\n"))
        }
        crate::cli::AuthAction::Logout => {
            let Some((id, connection)) = selected else {
                return Err(AuthError::InvalidConfig(
                    "no saved account to log out; list saved accounts with 'ego auth status'"
                        .to_string(),
                ));
            };
            if connection.access == ego_core::config::ConnectionAccess::Key
                && let Some(source) = connection.key_env.as_deref()
            {
                return Err(AuthError::InvalidConfig(format!(
                    "{} uses key_env {source}; remove that environment variable to revoke access",
                    account_label(&config, &connection.vendor, id),
                )));
            }
            let removed = store.remove(&connection_storage_key(id))?;
            let id = account_label(&config, &connection.vendor, id);
            Ok(if removed {
                format!("logged out {id}\n")
            } else {
                format!("no stored credential for {id}\n")
            })
        }
        crate::cli::AuthAction::Login => {
            let target_route = selected
                .map(|(id, _)| id)
                .map(|id| {
                    config
                        .connections
                        .get(id)
                        .map(connection_route)
                        .ok_or_else(|| AuthError::InvalidConfig(format!("unknown connection {id}")))
                })
                .transpose()?;
            let Some(id) = args.provider.as_deref().or(target_route.as_deref()) else {
                // The bare rows are the contract a script reads, so the
                // sentence explaining them goes to the diagnostic channel
                // exactly as `status` sends its empty-listing note there. A
                // listing that arrives as eight unlabelled words reads as a
                // command that did nothing.
                crate::output::note(
                    "providers with interactive login are listed below; see \
                     'ego auth login --help' for sign-in instructions",
                );
                let config = crate::config::auth()
                    .map_err(|err| AuthError::InvalidConfig(err.to_string()))?;
                return Ok(known_and_discovered(&home, &config));
            };
            let mut request = args.clone();
            request.connection = selected.map(|(id, _)| id.to_owned());
            let connection = login_from_cli(&store, &home, id, &request, cancel, &commands).await?;
            let config = connection_layer_at(&home)?;
            Ok(format!(
                "logged in {}\n",
                account_label(&config, connection_vendor(id), &connection)
            ))
        }
    }
}

/// Authenticate using this process's interactive owner, never global stdin.
/// The next generation reads AuthStore for every request, so no runtime
/// credential snapshot needs invalidation after the atomic store write.
pub(crate) async fn session_login(
    provider: Option<&str>,
    seat: Option<&crate::repl::Seat>,
    diagnostics: &dyn crate::output::Diagnostics,
) -> Result<String, AuthError> {
    use ego_core::question::{
        QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest,
    };

    let config =
        crate::config::auth().map_err(|error| AuthError::InvalidConfig(error.to_string()))?;
    let Some(id) = provider else {
        let mut table = vec![vec!["Provider".into()]];
        table.extend(
            BuiltInFlow::all()
                .iter()
                .filter(|flow| {
                    oauth_login_enabled(flow.id(), &config)
                        && ensure_oauth_login_flow(flow.id()).is_ok()
                })
                .map(|flow| vec![flow.id().into()]),
        );
        return Ok(format!(
            "Built-in OAuth providers\n{}\n/login PROVIDER — authorize a provider",
            crate::render::table::plain_table(table).join("\n")
        ));
    };
    ensure_login_enabled(id, &config)?;
    google_cloud::validate_context(id)?;
    ensure_oauth_login_flow(id)?;
    let flow = oauth::built_in(id).ok_or_else(|| {
        AuthError::InvalidConfig(format!(
            "{id} is not a built-in OAuth provider; use /login to list supported identities"
        ))
    })?;
    if id == "openai-codex"
        && EnvCredentials
            .secret(ego_ai::provider::OPENAI_CODEX_ACCESS_TOKEN_ENV)
            .is_ok()
    {
        return Err(AuthError::InvalidConfig(
            "openai-codex uses OPENAI_CODEX_ACCESS_TOKEN in this process; unset it before /login to use a stored credential".into(),
        ));
    }
    let seat = seat
        .ok_or_else(|| AuthError::InvalidConfig("login requires an interactive terminal".into()))?;
    let home = ego_home()?;
    let source = select_source(
        id,
        config
            .get(id)
            .and_then(|entry| entry.credential_command.as_deref()),
        discover_plugin(id, &home, None),
        true,
        None,
    );
    if !matches!(source, CredentialSource::BuiltIn(_)) {
        return Err(AuthError::InvalidConfig(format!(
            "{id} uses a configured credential source; /login supports built-in OAuth only"
        )));
    }
    let methods = login_methods(flow, true, false);
    let method = if methods.len() == 2 {
        match seat
            .select_host(QuestionRequest::new(
                "Choose an OAuth login method (Esc cancels)",
                QuestionMode::Single,
                vec![
                    QuestionOption::new("device", "Authorize with a device code"),
                    QuestionOption::new("browser", "Authorize with a loopback browser callback"),
                ],
                false,
            ))
            .await
        {
            QuestionOutcome::Answered(QuestionAnswer::Single(value)) if value == "device" => {
                LoginMethod::Device
            }
            QuestionOutcome::Answered(QuestionAnswer::Single(value)) if value == "browser" => {
                LoginMethod::Browser
            }
            _ => return Err(AuthError::InvalidConfig("login cancelled".into())),
        }
    } else {
        *methods
            .first()
            .ok_or_else(|| AuthError::InvalidConfig("no supported login method".into()))?
    };
    let headers = LoginHeaders::new(
        config
            .get(id)
            .map(|entry| entry.headers.clone())
            .unwrap_or_default(),
    );
    let cancel = CancelToken::new();
    let mut interruption = seat.setup_cancellation();
    let scope = crate::provider_http::CommandHttpScope::online(cancel.clone())
        .await
        .map_err(AuthError::InvalidConfig)?;
    let http = scope.http();
    #[cfg(all(feature = "test-support", debug_assertions))]
    let http = TEST_LOGIN_HTTP.get().unwrap_or(http);
    let output = |text: &str| diagnostics.note(text);
    let login = async {
        let token = match (flow, method) {
            (BuiltInFlow::MiniMax, _) => {
                minimax_login(flow, http.as_ref(), &headers, &output).await?
            }
            (_, LoginMethod::Device) => {
                device_login(flow, http.as_ref(), &headers, &output).await?
            }
            (_, LoginMethod::Browser) => {
                browser_login_with_input(
                    flow,
                    http.as_ref(),
                    &headers,
                    &output,
                    false,
                    std::future::pending::<Result<String, AuthError>>,
                )
                .await?
            }
        };
        token_entry(flow, token)
    };
    let entry = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(1200), login) =>
            result.map_err(|_| AuthError::InvalidConfig("login timed out".into()))??,
        () = interruption.cancelled() => {
            cancel.cancel(CancelCause::User);
            return Err(AuthError::InvalidConfig("login cancelled".into()));
        }
    };
    ensure_store_may_mutate(&cancel, None)?;
    let connection = publish_login(&AuthStore::new(&home), &home, id, None, entry)?;
    Ok(format!(
        "Logged in {id} as connection {connection}; select it with --connection {connection}"
    ))
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

/// A local presence inventory, not an authentication or entitlement check.
fn status_json(
    store: &AuthStore,
    connections: &std::collections::BTreeMap<String, ego_core::config::ConnectionConfig>,
    selected_key: Option<&str>,
    vendor: Option<&str>,
) -> Result<String, AuthError> {
    use crate::onboarding::EnvironmentPresence;
    #[derive(serde::Serialize)]
    struct Presence {
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        present: bool,
    }
    #[derive(serde::Serialize)]
    struct Inventory {
        v: u8,
        credentials: Vec<Presence>,
    }
    let stored = store.read_observed()?;
    let mut records = std::collections::BTreeMap::new();
    for (id, connection) in connections {
        let key = connection_storage_key(id);
        if selected_key.is_some_and(|wanted| wanted != key)
            || vendor.is_some_and(|wanted| wanted != connection.vendor)
        {
            continue;
        }
        let present = match connection.access {
            ego_core::config::ConnectionAccess::Key => match connection.key_env.as_deref() {
                Some(name) => crate::onboarding::ProcessEnvironment.has_nonblank(name),
                None => stored.get(&key).is_some_and(AuthEntry::has_api_key),
            },
            ego_core::config::ConnectionAccess::Oauth => {
                stored.get(&key).is_some_and(AuthEntry::has_oauth)
            }
        };
        records.insert(
            id.clone(),
            Presence {
                provider: Some(connection.vendor.clone()),
                connection: Some(id.clone()),
                present,
            },
        );
    }
    for key in stored.keys() {
        if selected_key.is_some_and(|wanted| wanted != key) {
            continue;
        }
        match connection_id_from_storage_key(key) {
            Some(id) if vendor.is_none() && !connections.contains_key(id) => {
                records.insert(
                    id.to_owned(),
                    Presence {
                        provider: None,
                        connection: Some(id.to_owned()),
                        present: true,
                    },
                );
            }
            _ => {}
        }
    }
    serde_json::to_string(&Inventory {
        v: 1,
        credentials: records.into_values().collect(),
    })
    .map(|json| format!("{json}\n"))
    .map_err(|error| {
        AuthError::InvalidConfig(format!(
            "credential presence could not be rendered: {error}"
        ))
    })
}

pub(crate) fn key_connection_present(
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
    store: Option<&AuthStore>,
    environment: &dyn crate::onboarding::EnvironmentPresence,
) -> bool {
    match connection.key_env.as_deref() {
        Some(source) => environment.has_nonblank(source),
        None => store
            .and_then(|store| store.read_observed().ok())
            .is_some_and(|entries| {
                entries
                    .get(&connection_storage_key(id))
                    .is_some_and(AuthEntry::has_api_key)
            }),
    }
}

fn key_status(
    store: &AuthStore,
    id: &str,
    connection: &ego_core::config::ConnectionConfig,
) -> String {
    match connection.key_env.as_deref() {
        Some(source) => format!("key source: {source}\n"),
        None => format!(
            "key source: ego auth store ({})\n",
            if key_connection_present(
                id,
                connection,
                Some(store),
                &crate::onboarding::ProcessEnvironment
            ) {
                "present"
            } else {
                "missing"
            }
        ),
    }
}

fn connection_status(store: &AuthStore, id: &str, label: &str) -> Result<String, AuthError> {
    let key = connection_storage_key(id);
    status(store, Some(&key)).map(|text| text.replace(&key, label))
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
    // Silence here is indistinguishable from a broken command, and the rows
    // are stdout's contract, so the explanation goes to the diagnostic
    // channel like every other empty listing.
    if text.is_empty() {
        crate::output::note(&crate::output::empty_listing(
            "no provider credentials are stored",
            "ego auth login",
        ));
    }
    Ok(text)
}

pub(crate) fn connection_route(connection: &ego_core::config::ConnectionConfig) -> String {
    use ego_core::config::ConnectionAccess;
    match (connection.vendor.as_str(), connection.access) {
        ("openai", ConnectionAccess::Oauth) => "openai-codex".into(),
        ("xai", ConnectionAccess::Oauth) => "xai-oauth".into(),
        ("minimax", ConnectionAccess::Oauth) => "minimax-oauth".into(),
        (vendor, _) => vendor.into(),
    }
}

pub(crate) fn renewal_hint(route: &str) -> String {
    format!(
        "see 'ego auth status {route}', then run 'ego auth login {route} --renew' with --account N if needed"
    )
}

pub(crate) fn connection_vendor(route: &str) -> &str {
    match route {
        "openai-codex" => "openai",
        "xai-oauth" => "xai",
        "minimax-oauth" => "minimax",
        route => route,
    }
}

pub(crate) fn connection_layer_at(home: &Path) -> Result<ego_core::config::ConfigLayer, AuthError> {
    let path = home.join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(AuthError::Io { path, source }),
    };
    ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, &text)
        .map(|layer| layer.value)
        .map_err(|error| AuthError::InvalidConfig(error.to_string()))
}

/// Read-only admission candidates for built-in stored API-key routes.
/// Configuration and the exact namespaced key must both exist.
pub(crate) fn saved_key_accounts_at(home: &Path) -> Result<Vec<SavedKeyAccount>, AuthError> {
    let config = connection_layer_at(home)?;
    let entries = AuthStore::new(home).read_observed()?;
    Ok(config
        .connections
        .iter()
        .filter(|(id, connection)| {
            connection.access == ego_core::config::ConnectionAccess::Key
                && connection.key_env.is_none()
                && connection.validate(id).is_ok()
                && entries
                    .get(&connection_storage_key(id))
                    .is_some_and(AuthEntry::has_api_key)
        })
        .map(|(id, connection)| SavedKeyAccount {
            id: id.clone(),
            number: account_number(&config, &connection.vendor, id),
            route: connection_route(connection),
            config: connection.clone(),
        })
        .collect())
}

#[derive(Debug, Clone)]
pub(crate) struct SavedKeyAccount {
    pub(crate) id: String,
    pub(crate) number: Option<u32>,
    pub(crate) route: String,
    pub(crate) config: ego_core::config::ConnectionConfig,
}

fn account_number(config: &ego_core::config::ConfigLayer, vendor: &str, id: &str) -> Option<u32> {
    config.account_numbers.get(vendor)?.get(id).copied()
}

pub(crate) fn account_label(
    config: &ego_core::config::ConfigLayer,
    vendor: &str,
    id: &str,
) -> String {
    account_number(config, vendor, id)
        .map_or_else(|| vendor.into(), |n| format!("{vendor} account {n}"))
}

pub(crate) fn account_id<'a>(
    numbers: &'a ego_core::config::AccountNumbers,
    connections: &std::collections::BTreeMap<String, ego_core::config::ConnectionConfig>,
    vendor: Option<&str>,
    account: u32,
) -> Result<&'a str, String> {
    let vendor = vendor.ok_or("--account requires a provider")?;
    let id = numbers.get(vendor).and_then(|assignments| {
        assignments.iter().find(|(id, number)| {
            **number == account
                && connections
                    .get(*id)
                    .is_some_and(|connection| connection.vendor == vendor)
        })
    });
    id.map(|(id, _)| id.as_str()).ok_or_else(|| {
        format!(
            "no {vendor} account {account}; list saved accounts with 'ego auth status {vendor}'"
        )
    })
}

fn numbered_connection_layer_at(
    home: &Path,
    config: ego_core::config::ConfigLayer,
) -> Result<ego_core::config::ConfigLayer, AuthError> {
    use ego_core::config::{UserConfigMutation, UserConfigStore};
    if config
        .connections
        .iter()
        .all(|(id, connection)| account_number(&config, &connection.vendor, id).is_some())
    {
        return Ok(config);
    }
    ego_tools::config::OsUserConfigStore::new(
        Arc::new(ego_tools::store::OsIo),
        home.join("config.toml"),
    )
    .mutate(UserConfigMutation::NumberAccounts)
    .map_err(|error| AuthError::InvalidConfig(error.to_string()))?;
    connection_layer_at(home)
}

fn publish_login(
    store: &AuthStore,
    home: &Path,
    route: &str,
    target: Option<&str>,
    entry: AuthEntry,
) -> Result<String, AuthError> {
    if !matches!(entry, AuthEntry::OAuth { .. }) {
        return Err(AuthError::InvalidConfig(
            "OAuth login requires OAuth credentials; register an API key through explicit API-key login instead".into(),
        ));
    }
    google_cloud::validate_context(route)?;
    publish_credential(
        store,
        home,
        route,
        target,
        ego_core::config::ConnectionAccess::Oauth,
        entry,
    )
}

fn validate_login_target(
    config: &ego_core::config::ConfigLayer,
    route: &str,
    target: Option<&str>,
    access: ego_core::config::ConnectionAccess,
) -> Result<(), AuthError> {
    if let Some(id) = target {
        let connection = config.connections.get(id).ok_or_else(|| {
            AuthError::InvalidConfig("selected account is no longer saved".into())
        })?;
        if connection.access != access || connection_route(connection) != route {
            return Err(AuthError::InvalidConfig(format!(
                "selected account does not use {} route {route}",
                if access == ego_core::config::ConnectionAccess::Oauth {
                    "OAuth"
                } else {
                    "API-key"
                }
            )));
        }
        if access == ego_core::config::ConnectionAccess::Key && connection.key_env.is_some() {
            return Err(AuthError::InvalidConfig("the selected account declares an environment key source; login cannot replace that source".into()));
        }
    }
    Ok(())
}

/// The source-specific callers establish credential kind before publication.
fn publish_credential(
    store: &AuthStore,
    home: &Path,
    route: &str,
    target: Option<&str>,
    access: ego_core::config::ConnectionAccess,
    entry: AuthEntry,
) -> Result<String, AuthError> {
    use ego_core::config::{ConnectionConfig, UserConfigMutation, UserConfigStore};
    if let Some(id) = target {
        store.modify(|entries| {
            let config = connection_layer_at(home)?;
            validate_login_target(&config, route, target, access)?;
            entries.insert(connection_storage_key(id), entry);
            Ok(())
        })?;
        return Ok(id.into());
    }
    let id = format!("conn_{}", uuid::Uuid::new_v4().simple());
    let vendor = connection_vendor(route);
    let connection = ConnectionConfig {
        name: vendor.into(),
        vendor: vendor.into(),
        access,
        key_env: None,
        azure_resource: None,
    };
    store.put(&connection_storage_key(&id), entry)?;
    let config_store = ego_tools::config::OsUserConfigStore::new(
        Arc::new(ego_tools::store::OsIo),
        home.join("config.toml"),
    );
    if let Err(error) = config_store.mutate(UserConfigMutation::InsertConnection {
        id: id.clone(),
        connection,
    }) {
        store.remove(&connection_storage_key(&id))?;
        return Err(AuthError::InvalidConfig(error.to_string()));
    }
    Ok(id)
}

/// Direct key registration is selected by the CLI operation, never inferred
/// from unclassified credential-program output.
async fn login_from_cli(
    store: &AuthStore,
    home: &Path,
    id: &str,
    args: &crate::cli::AuthArgs,
    cancel: &CancelToken,
    commands: &dyn CredentialCommandPort,
) -> Result<String, AuthError> {
    use crate::cli::AuthLoginMethod;
    use ego_core::config::ConnectionAccess;
    let auth =
        crate::config::auth().map_err(|error| AuthError::InvalidConfig(error.to_string()))?;
    let row = crate::model::rows(&auth)
        .map_err(|error| AuthError::InvalidConfig(error.to_string()))?
        .into_iter()
        .find(|row| row.id() == id);
    let key_available = row
        .as_ref()
        .is_some_and(|row| row.credential.environment_var().is_some());
    let oauth_available = row
        .as_ref()
        .is_none_or(|row| oauth_connection_supported(&row.credential))
        && ensure_oauth_login_flow(id).is_ok()
        && oauth_login_enabled(id, &auth);
    let metadata = connection_layer_at(home)?;
    let target_method = args
        .connection
        .as_deref()
        .map(|target| {
            let connection = metadata.connections.get(target).ok_or_else(|| {
                AuthError::InvalidConfig("selected account is no longer saved".into())
            })?;
            Ok::<_, AuthError>(match connection.access {
                ConnectionAccess::Key => AuthLoginMethod::ApiKey,
                ConnectionAccess::Oauth => AuthLoginMethod::Oauth,
            })
        })
        .transpose()?;
    let requested = args
        .method
        .or(target_method)
        .or(args.device.then_some(AuthLoginMethod::Oauth));
    let method = match requested {
        Some(method) => method,
        None if key_available && oauth_available => {
            if !std::io::stdin().is_terminal() {
                return Err(AuthError::InvalidConfig(
                    "this provider offers API key and OAuth; choose --method api-key or --method oauth".into(),
                ));
            }
            key_input::method(cancel).await?
        }
        None if key_available => AuthLoginMethod::ApiKey,
        None => AuthLoginMethod::Oauth,
    };
    if method == AuthLoginMethod::ApiKey {
        if !key_available || args.device {
            return Err(AuthError::InvalidConfig(format!(
                "provider {id} does not offer the selected API-key login"
            )));
        }
        validate_login_target(
            &metadata,
            id,
            args.connection.as_deref(),
            ConnectionAccess::Key,
        )?;
        ensure_store_may_mutate(cancel, None)?;
        let key = key_input::read(id, cancel).await?;
        ensure_store_may_mutate(cancel, None)?;
        return publish_credential(
            store,
            home,
            id,
            args.connection.as_deref(),
            ConnectionAccess::Key,
            AuthEntry::ApiKey { key },
        );
    }
    if !oauth_available {
        return Err(AuthError::InvalidConfig(format!(
            "OAuth login for {id} is disabled; set auth.{id}.oauth_login_enabled=true in your user config to enable it"
        )));
    }
    login(
        store,
        home,
        id,
        args.connection.as_deref(),
        args.device,
        cancel,
        commands,
    )
    .await
}

async fn login(
    store: &AuthStore,
    home: &Path,
    id: &str,
    connection_id: Option<&str>,
    device: bool,
    cancel: &CancelToken,
    commands: &dyn CredentialCommandPort,
) -> Result<String, AuthError> {
    let config = crate::config::auth().map_err(|err| AuthError::InvalidConfig(err.to_string()))?;
    ensure_login_enabled(id, &config)?;
    if let Some(connection_id) = connection_id {
        let metadata = connection_layer_at(home)?;
        let connection = metadata.connections.get(connection_id).ok_or_else(|| {
            AuthError::InvalidConfig("selected account is no longer saved".into())
        })?;
        if connection.access != ego_core::config::ConnectionAccess::Oauth
            || connection_route(connection) != id
        {
            return Err(AuthError::InvalidConfig(format!(
                "selected account does not use OAuth route {id}"
            )));
        }
    }
    let command = config
        .get(id)
        .and_then(|provider| provider.credential_command.as_deref());
    if id == ego_core::config::AZURE_OPENAI {
        let target = connection_id.ok_or_else(|| AuthError::InvalidConfig(
            "configure an azure-openai OAuth connection with azure_resource, then select it with --account N".into()
        ))?;
        let metadata = connection_layer_at(home)?;
        let account = metadata.connections.get(target).ok_or_else(|| {
            AuthError::InvalidConfig("selected Azure account is no longer saved".into())
        })?;
        validate_login_target(
            &metadata,
            id,
            Some(target),
            ego_core::config::ConnectionAccess::Oauth,
        )?;
        account.validate(target).map_err(AuthError::InvalidConfig)?;
        let command = azure::scoped_command(account, command)?;
        let entry = run_configured_command_scoped(
            &command,
            "login",
            None,
            commands,
            cancel,
            None,
            credential_timeout(config.get(id)),
        )
        .await?;
        let entry = azure::bind_entry(entry, azure::binding(&command))?;
        ensure_store_may_mutate(cancel, None)?;
        return publish_login(store, home, id, Some(target), entry);
    }
    // The same declared table the generation path applies. A login is the
    // request that establishes the credential rather than spending it, so a
    // vendor that gates it on a client it recognises would otherwise be
    // unreachable — and ego does not answer that by claiming to be one.
    let headers = LoginHeaders::new(
        config
            .get(id)
            .map(|provider| provider.headers.clone())
            .unwrap_or_default(),
    );
    let timeout = credential_timeout(config.get(id));
    let plugin = discover_plugin(id, home, None);
    let environment = environment_var(id);
    let source = select_source(
        id,
        command,
        plugin,
        oauth::built_in(id).is_some() || google_cloud::is_google_cloud(id),
        environment.as_deref(),
    );
    let entry = match source {
        CredentialSource::Command(program) => {
            run_configured_command_scoped(&program, "login", None, commands, cancel, None, timeout)
                .await?
        }
        CredentialSource::Plugin(program) => {
            run_program_scoped(&program, "login", None, commands, cancel, None, timeout).await?
        }
        CredentialSource::BuiltIn(provider) => {
            ensure_oauth_login_flow(&provider)?;
            let scope = crate::provider_http::CommandHttpScope::online(cancel.clone())
                .await
                .map_err(|reason| AuthError::MissingCredential {
                    program: format!("ego built-in {provider} login"),
                    reason,
                })?;
            if google_cloud::is_google_cloud(&provider) {
                google_cloud::token_entry(
                    scope.http().as_ref(),
                    provider == ego_ai::provider::VERTEX_GEMINI,
                )
                .await?
            } else {
                built_in_login(
                    &provider,
                    std::io::stdin().is_terminal(),
                    device,
                    scope.http().as_ref(),
                    &headers,
                )
                .await?
            }
        }
        CredentialSource::Environment(var) => {
            return Err(AuthError::MissingCredential {
                program: id.to_string(),
                reason: format!(
                    "it reads its key from {var} rather than through a login; \
                     set that variable in the environment ego runs in"
                ),
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
    publish_login(store, home, id, connection_id, entry)
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
    headers: &LoginHeaders,
) -> Result<AuthEntry, AuthError> {
    let flow = oauth::built_in(id).ok_or_else(|| AuthError::MissingCredential {
        program: format!("ego built-in {id} login"),
        reason: "unknown built-in provider".to_string(),
    })?;
    let method = choose_login_method(&login_methods(flow, tty, force_device))?;
    let output = |text: &str| eprintln!("{text}");
    let token = match (flow, method) {
        (BuiltInFlow::MiniMax, _) => minimax_login(flow, http, headers, &output).await?,
        (_, LoginMethod::Browser) => browser_login(flow, http, headers).await?,
        (_, LoginMethod::Device) => device_login(flow, http, headers, &output).await?,
    };
    token_entry(flow, token)
}

async fn device_login(
    flow: BuiltInFlow,
    http: &dyn ego_ai::http::Http,
    headers: &LoginHeaders,
    output: &dyn Fn(&str),
) -> Result<OAuthToken, AuthError> {
    let start = flow
        .device_authorization_request()
        .map_err(login_flow_error)?;
    let response = oauth::execute(http, flow, start, headers)
        .await
        .map_err(login_flow_error)?;
    let device = flow
        .read_device_authorization(response)
        .map_err(login_flow_error)?;
    output(&format!(
        "Open {} and enter code {}",
        device.verification_uri, device.user_code
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval_seconds.max(1);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let request = flow
            .device_poll_request(&device.device_code, Some(&device.user_code))
            .map_err(login_flow_error)?;
        let response = oauth::execute(http, flow, request, headers)
            .await
            .map_err(login_flow_error)?;
        match flow
            .read_poll(response, now_ms(), None)
            .map_err(login_flow_error)?
        {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = interval.saturating_add(5),
            PollOutcome::Token(token) => return Ok(token),
            PollOutcome::GitHubToken(github) => {
                let request = flow
                    .copilot_exchange_request(&github)
                    .map_err(login_flow_error)?;
                let response = oauth::execute(http, flow, request, headers)
                    .await
                    .map_err(login_flow_error)?;
                return flow
                    .read_token(response, now_ms(), Some(&github))
                    .map_err(login_flow_error);
            }
            PollOutcome::CodexAuthorization { code, verifier } => {
                let request = flow
                    .login_request(LoginGrant::AuthorizationCode {
                        code: &code,
                        verifier: &verifier,
                        redirect_uri: "https://auth.openai.com/deviceauth/callback",
                        state: None,
                    })
                    .map_err(login_flow_error)?;
                let response = oauth::execute(http, flow, request, headers)
                    .await
                    .map_err(login_flow_error)?;
                return flow
                    .read_token(response, now_ms(), None)
                    .map_err(login_flow_error);
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
    headers: &LoginHeaders,
) -> Result<OAuthToken, AuthError> {
    browser_login_with_input(
        flow,
        http,
        headers,
        &|text| eprintln!("{text}"),
        true,
        || async {
            tokio::task::spawn_blocking(read_line)
                .await
                .map_err(|error| AuthError::InvalidConfig(error.to_string()))?
        },
    )
    .await
}

async fn browser_login_with_input<F, Fut>(
    flow: BuiltInFlow,
    http: &dyn ego_ai::http::Http,
    headers: &LoginHeaders,
    output: &dyn Fn(&str),
    manual_input: bool,
    input: F,
) -> Result<OAuthToken, AuthError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String, AuthError>>,
{
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
        .map_err(login_flow_error)?;
    output(&format!("Open this URL to authenticate:\n{url}"));
    if manual_input {
        output(
            "Waiting for the loopback callback; paste the code or redirect URL if it cannot arrive:",
        );
    } else {
        output("Waiting for the loopback callback on this host; press Ctrl-C to cancel.");
    }
    // This provider's authorization URL has no state parameter. Its PKCE
    // verifier still binds the code exchange; other providers require state.
    let require_state = flow != BuiltInFlow::OpenRouter;
    let code =
        wait_for_browser_code(listener, callback_path, &state, require_state, input()).await?;
    let request = flow
        .login_request(LoginGrant::AuthorizationCode {
            code: &code,
            verifier: &verifier,
            redirect_uri: redirect,
            state: Some(&state),
        })
        .map_err(login_flow_error)?;
    let response = oauth::execute(http, flow, request, headers)
        .await
        .map_err(login_flow_error)?;
    flow.read_token(response, now_ms(), None)
        .map_err(login_flow_error)
}

async fn wait_for_browser_code<F>(
    listener: TcpListener,
    callback_path: &str,
    expected_state: &str,
    require_state: bool,
    pasted: F,
) -> Result<String, AuthError>
where
    F: Future<Output = Result<String, AuthError>>,
{
    tokio::pin!(pasted);
    tokio::select! {
        callback = read_loopback_callback(listener, callback_path, expected_state, require_state) => callback,
        input = &mut pasted => validated_authorization_code(&input?, expected_state, false),
    }
}

async fn read_loopback_callback(
    listener: TcpListener,
    callback_path: &str,
    expected_state: &str,
    require_state: bool,
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
        validated_authorization_code(parsed.as_str(), expected_state, require_state)
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
    headers: &LoginHeaders,
    output: &dyn Fn(&str),
) -> Result<OAuthToken, AuthError> {
    let verifier = verifier();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let request = flow
        .minimax_user_code_request(&oauth::pkce_challenge(&verifier), &state)
        .map_err(login_flow_error)?;
    let response = oauth::execute(http, flow, request, headers)
        .await
        .map_err(login_flow_error)?;
    let code = flow
        .read_minimax_user_code(response, &state)
        .map_err(login_flow_error)?;
    output(&format!(
        "Open {} and enter code {}",
        code.verification_uri, code.user_code
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in.max(1));
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(code.interval_ms.max(2_000))).await;
        let request = flow
            .login_request(LoginGrant::UserCode {
                user_code: &code.user_code,
                verifier: &verifier,
            })
            .map_err(login_flow_error)?;
        let response = oauth::execute(http, flow, request, headers)
            .await
            .map_err(login_flow_error)?;
        match flow
            .read_poll(response, now_ms(), None)
            .map_err(login_flow_error)?
        {
            PollOutcome::Pending | PollOutcome::SlowDown => {}
            PollOutcome::Token(token) => return Ok(token),
            other => {
                return Err(AuthError::MissingCredential {
                    program: "ego built-in minimax-oauth login".to_string(),
                    // `other.kind()` and never `{other:?}`: the variants that
                    // reach here carry an authorization code, a PKCE verifier
                    // or a bearer token, and this string is shown to an
                    // operator. Naming the shape is the whole diagnostic — an
                    // operator cannot act on the bytes of a token they were
                    // not supposed to receive, and neither can anyone reading
                    // over their shoulder.
                    reason: format!(
                        "the authorization server answered with {}, which this \
                         login flow cannot use",
                        other.kind()
                    ),
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
        oauth::codex_account_id(&values.access).map_err(login_flow_error)?;
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
            scope_binding: None,
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

fn login_flow_error(error: oauth::FlowError) -> AuthError {
    match error {
        oauth::FlowError::ReloginRequired { provider, cause } => AuthError::MissingCredential {
            program: format!("ego built-in {provider} login"),
            reason: cause.to_string(),
        },
        error => flow_error(error),
    }
}

pub(crate) fn legacy_login_remedy(provider: &str) -> String {
    format!(
        "'ego auth login {provider}' creates a new named connection, not a repair of this legacy identity. Existing unbound sessions cannot resume with that connection."
    )
}

fn flow_error(error: oauth::FlowError) -> AuthError {
    match error {
        oauth::FlowError::ReloginRequired { provider, cause } => AuthError::ReloginRequired {
            provider,
            connection: None,
            cause,
        },
        oauth::FlowError::Temporary {
            provider,
            status,
            cause,
        } => AuthError::Temporary {
            provider,
            status,
            cause,
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
            if connection_id_from_storage_key(id).is_some()
                && !matches!(refreshed, AuthEntry::OAuth { .. })
            {
                return Err(AuthError::InvalidConfig(
                    "named connection refresh requires OAuth credentials; configure a key connection with key_env instead".into(),
                ));
            }
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
/// A saved login must have an admitted named connection. Without one, only
/// the provider route's independent environment or no-auth source is used.
pub async fn request_credentials(
    id: &str,
    http: &dyn ego_ai::http::Http,
) -> Result<Arc<dyn Credentials>, AuthError> {
    let commands = OsCredentialCommand;
    let cancel = CancelToken::new();
    request_credentials_scoped(id, http, &commands, &cancel, None).await
}

pub(crate) async fn request_credentials_scoped(
    id: &str,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<Arc<dyn Credentials>, AuthError> {
    let config = crate::config::user_effective()
        .map_err(|error| AuthError::InvalidConfig(error.to_string()))?;
    if !config.connections.is_empty() {
        let selected = ego_core::config::select_connection(
            &config.connections,
            config.connection.as_deref(),
            Some(connection_vendor(id)),
        )
        .map_err(AuthError::InvalidConfig)?;
        if let Some((connection_id, connection)) = selected {
            if connection_route(connection) != id {
                return Err(AuthError::InvalidConfig(format!(
                    "selected account requires route {}",
                    connection_route(connection)
                )));
            }
            let strategy = crate::model::rows(&config.auth)
                .map_err(|error| AuthError::InvalidConfig(error.to_string()))?
                .into_iter()
                .find(|row| row.id() == id)
                .ok_or_else(|| AuthError::InvalidConfig(format!("unknown route {id}")))?
                .credential;
            return connection_credentials(
                connection_id,
                connection,
                id,
                &strategy,
                &config.auth,
                None,
                http,
                commands,
                cancel,
                run_deadline,
            )
            .await;
        }
    }
    Ok(Arc::new(EnvCredentials))
}

#[expect(
    clippy::too_many_arguments,
    reason = "storage identity and protocol route have separate authority under one refresh lifetime"
)]
async fn refresh_connection_at(
    home: &Path,
    storage_id: &str,
    id: &str,
    source: CredentialSource,
    provider: Option<&ego_core::config::AuthProviderLayer>,
    http: &dyn ego_ai::http::Http,
    commands: &dyn CredentialCommandPort,
    cancel: &CancelToken,
    run_deadline: Option<Instant>,
) -> Result<Option<String>, AuthError> {
    let azure_binding = if id == ego_core::config::AZURE_OPENAI {
        let CredentialSource::Command(command) = &source else {
            return Err(azure::missing_command());
        };
        Some(azure::binding(command))
    } else {
        None
    };
    let google_cloud = google_cloud::is_google_cloud(id);
    let vertex_adc =
        id == ego_ai::provider::VERTEX_GEMINI && matches!(source, CredentialSource::BuiltIn(_));
    if oauth::built_in(id).is_none()
        && !google_cloud
        && !matches!(source, CredentialSource::Command(_))
    {
        return Ok(None);
    }
    let connection = connection_id_from_storage_key(storage_id).unwrap_or(storage_id);
    let missing_entry = || {
        if vertex_adc {
            Err(google_cloud::missing_principal_binding(connection))
        } else {
            Ok(azure_binding.clone())
        }
    };
    let store = AuthStore::new(home);
    if !store.path().exists() {
        return missing_entry();
    }
    let mut lock = store.open_lock()?;
    let _guard = lock.write().map_err(|source| AuthError::Io {
        path: store.lock_path(),
        source,
    })?;
    let mut entries = store.read_unlocked()?;
    let Some(current) = entries.remove(storage_id) else {
        return missing_entry();
    };
    let binding = match &current {
        AuthEntry::OAuth { scope_binding, .. } => scope_binding.as_deref(),
        AuthEntry::ApiKey { .. } => None,
    };
    if vertex_adc {
        if !google_cloud::has_principal_binding(binding) {
            return Err(google_cloud::missing_principal_binding(connection));
        }
    } else if id == ego_ai::provider::VERTEX_GEMINI && google_cloud::has_principal_binding(binding)
    {
        return Err(AuthError::InvalidConfig(format!(
            "cannot refresh Vertex Gemini connection {connection}: the credential source changed; \
             retrying unchanged cannot help. Restore the built-in ADC source or use a separate connection for the new source"
        )));
    } else if binding != azure_binding.as_deref() {
        return Err(azure::changed_binding());
    }
    let expires = current.expires();
    if expires.is_none() || expires.is_some_and(|at| at > now_ms().saturating_add(300_000)) {
        return Ok(binding.map(str::to_owned));
    }

    let timeout = credential_timeout(provider);
    let headers = LoginHeaders::new(
        provider
            .map(|config| config.headers.clone())
            .unwrap_or_default(),
    );
    let refreshed = match source {
        CredentialSource::Command(program) => {
            run_configured_command_scoped(
                &program,
                "refresh",
                Some(&current),
                commands,
                cancel,
                run_deadline,
                timeout,
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
                timeout,
            )
            .await
        }
        CredentialSource::BuiltIn(_) if google_cloud => {
            google_cloud::token_entry(http, vertex_adc).await
        }
        CredentialSource::BuiltIn(_) => refresh_built_in(id, &current, http, &headers).await,
        CredentialSource::Environment(_) | CredentialSource::Missing(_) => {
            return Err(AuthError::MissingCredential {
                program: format!("ego auth login {id}"),
                reason: "the stored credential expired and has no refresh implementation"
                    .to_string(),
            });
        }
    };
    let refreshed = match azure_binding {
        Some(binding) => refreshed.and_then(|entry| azure::bind_entry(entry, binding)),
        None => refreshed,
    };
    let binding = binding.map(str::to_owned);
    let refreshed = refreshed.and_then(|entry| {
        if vertex_adc
            && !matches!(
                &entry,
                AuthEntry::OAuth { scope_binding, .. } if scope_binding == &binding
            )
        {
            return Err(google_cloud::changed_principal(connection));
        }
        Ok(entry)
    });
    ensure_store_may_mutate(cancel, run_deadline)?;
    apply_refresh_result_unlocked(&store, &mut entries, storage_id, refreshed)?;
    Ok(binding)
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
    headers: &LoginHeaders,
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
    let refresh = secret_text(refresh).map_err(|reason| AuthError::MissingCredential {
        program: format!("ego auth login {id}"),
        reason: reason.to_string(),
    })?;
    let request = if flow == BuiltInFlow::GitHubCopilot {
        flow.copilot_exchange_request(refresh)
    } else {
        flow.refresh_request(refresh)
    }
    .map_err(flow_error)?;
    let response = oauth::execute(http, flow, request, headers)
        .await
        .map_err(flow_error)?;
    let token = flow
        .read_token(response, now_ms(), Some(refresh))
        .map_err(flow_error)?;
    token_entry(flow, token)
}

/// The variable the row for `id` reads a key from, if this build has that row
/// and it reads one.
///
/// Read from the provider table rather than from a second list here, so a row
/// that changes which variable it names cannot leave this message naming the
/// old one. An unknown id yields `None` and the caller reports it as missing,
/// which is the true answer for a provider this build does not have.
fn environment_var(id: &str) -> Option<String> {
    crate::model::rows(&Default::default())
        .ok()?
        .into_iter()
        .find(|row| row.id() == id)?
        .credential
        .environment_var()
        .map(str::to_owned)
}

fn oauth_login_enabled(
    id: &str,
    config: &HashMap<String, ego_core::config::AuthProviderLayer>,
) -> bool {
    config
        .get(id)
        .and_then(|provider| provider.oauth_login_enabled)
        .unwrap_or(id != "anthropic")
}

fn ensure_oauth_login_flow(id: &str) -> Result<(), AuthError> {
    if id == "openrouter" {
        return Err(AuthError::InvalidConfig(
            "this login flow issues an API key, not OAuth; configure a key connection with key_env instead".into(),
        ));
    }
    Ok(())
}

fn ensure_login_enabled(
    id: &str,
    config: &HashMap<String, ego_core::config::AuthProviderLayer>,
) -> Result<(), AuthError> {
    if oauth_login_enabled(id, config) {
        Ok(())
    } else {
        Err(AuthError::InvalidConfig(format!(
            "OAuth login for {id} is disabled; set auth.{id}.oauth_login_enabled=true in your user config to enable it"
        )))
    }
}

fn known_and_discovered(
    home: &Path,
    config: &HashMap<String, ego_core::config::AuthProviderLayer>,
) -> String {
    let mut ids: Vec<String> = BuiltInFlow::all()
        .iter()
        .filter(|flow| ensure_oauth_login_flow(flow.id()).is_ok())
        .map(|flow| flow.id().to_string())
        .collect();
    ids.extend(
        crate::model::rows(&Default::default())
            .unwrap_or_default()
            .into_iter()
            .filter(|row| {
                row.credential.environment_var().is_some()
                    || matches!(
                        row.credential,
                        ego_ai::provider::CredentialStrategy::GoogleCloud { .. }
                    )
            })
            .map(|row| row.id().to_string()),
    );
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
    ids.into_iter()
        .filter(|id| environment_var(id).is_some() || oauth_login_enabled(id, config))
        .map(|id| format!("{id}\n"))
        .collect()
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

mod azure;
mod credential_program;
mod google_cloud;
pub use credential_program::run_program;
use credential_program::{run_configured_command_scoped, run_program_scoped};

#[cfg(test)]
mod tests {
    mod named_oauth_route;

    use std::future;

    use ego_ai::oauth::{BuiltInFlow, FlowError, TokenRefusal, TokenStatus};
    use ego_ai::provider::ProviderRow;
    use ego_core::turn::CancelToken;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        AuthEntry, AuthError, AuthStore, LoginMethod, StoredSecret, apply_refresh_failure,
        authorization_code, login_methods, request_credentials, status, token_entry,
        wait_for_browser_code,
    };

    fn account_entry(value: &str) -> AuthEntry {
        AuthEntry::OAuth {
            access: StoredSecret::new(value.as_bytes().to_vec()),
            refresh: Some(StoredSecret::new(format!("refresh-{value}").into_bytes())),
            expires: None,
            scope_binding: None,
        }
    }

    // Catches: interrupted new-login publication loses an existing account or
    // leaves its committed credential invisible or impossible to remove explicitly.
    #[tokio::test]
    async fn named_connections_interrupted_login_preserves_accounts_and_exposes_cleanup() {
        const STAGE: &str = "EGO_INTERRUPTED_LOGIN_STAGE";
        const TEST: &str = "auth::tests::named_connections_interrupted_login_preserves_accounts_and_exposes_cleanup";
        if let Some(stage) = std::env::var_os(STAGE) {
            let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
            let store = AuthStore::new(&home);
            if stage == "publish" {
                super::publish_login(
                    &store,
                    &home,
                    "anthropic",
                    None,
                    account_entry("interrupted-new-account"),
                )
                .unwrap();
                panic!("publication must remain blocked by the parent's config lock");
            }
            assert_eq!(stage, "recover");
            let entries = store.read_observed().unwrap();
            let orphan_key = entries.keys().find(|key| *key != "anthropic").unwrap();
            let orphan = orphan_key.strip_prefix("connection:").unwrap();
            let cancel = CancelToken::new();
            let status = super::execute(
                &crate::cli::AuthArgs {
                    account: None,
                    renew: false,
                    action: crate::cli::AuthAction::Status,
                    connection: None,
                    provider: None,
                    device: false,
                    method: None,
                },
                &cancel,
            )
            .await
            .unwrap();
            assert!(status.contains(&format!("orphan [{orphan}]")));
            assert!(!status.contains("interrupted-new-account"));
            assert!(!status.contains("existing-private-account"));
            let logout = super::execute(
                &crate::cli::AuthArgs {
                    account: None,
                    renew: false,
                    action: crate::cli::AuthAction::Logout,
                    connection: Some(orphan.into()),
                    provider: None,
                    device: false,
                    method: None,
                },
                &cancel,
            )
            .await
            .unwrap();
            assert!(logout.contains(&format!("logged out {orphan}")));
            assert!(store.read_secret_material(orphan_key).is_err());
            assert_eq!(
                store
                    .read_secret_material("anthropic")
                    .unwrap()
                    .expose_secret_bytes(),
                b"existing-private-account"
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        store
            .put("anthropic", account_entry("existing-private-account"))
            .unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.path().join("config.toml.lock"))
            .unwrap();
        let mut lock = fd_lock::RwLock::new(file);
        let guard = lock.write().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(STAGE, "publish")
            .env("EGO_HOME", home.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // This is a state checkpoint, not a timing guess: the held config lock
        // prevents metadata publication after the observed secret-file commit.
        let orphan_key = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "publisher exited before checkpoint"
                );
                if let Some(key) = store
                    .read_observed()
                    .unwrap()
                    .keys()
                    .find(|key| *key != "anthropic")
                {
                    break key.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("publisher did not commit the new credential");
        assert!(!home.path().join("config.toml").exists());
        child.start_kill().unwrap();
        let killed = child.wait_with_output().await.unwrap();
        assert!(!killed.status.success());
        drop(guard);
        assert_eq!(
            store
                .read_secret_material(&orphan_key)
                .unwrap()
                .expose_secret_bytes(),
            b"interrupted-new-account"
        );
        let recovered = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(STAGE, "recover")
            .env("EGO_HOME", home.path())
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            recovered.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&recovered.stdout),
            String::from_utf8_lossy(&recovered.stderr)
        );
    }

    // Catches: a saved key reads another account, accepts OAuth, or falls back to an environment slot.
    #[test]
    fn stored_key_resolver_is_exact_and_kind_checked() {
        use ego_ai::provider::Credentials as _;
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        store
            .put(
                "connection:other",
                AuthEntry::ApiKey {
                    key: "synthetic-other-account".into(),
                },
            )
            .unwrap();
        store
            .put(
                "gemini",
                AuthEntry::ApiKey {
                    key: "synthetic-legacy-account".into(),
                },
            )
            .unwrap();
        let credentials = super::ConnectionCredentials {
            store: store.clone(),
            id: "selected".into(),
            route: "gemini".into(),
            key_source: Some(("GEMINI_API_KEY".into(), super::KeySource::Store)),
            context_vars: vec![],
            scope_binding: None,
        };
        assert!(credentials.secret("GEMINI_API_KEY").is_err());
        store
            .put(
                "connection:selected",
                account_entry("synthetic-oauth-not-key"),
            )
            .unwrap();
        assert!(credentials.secret("GEMINI_API_KEY").is_err());
        store
            .put(
                "connection:selected",
                AuthEntry::ApiKey {
                    key: "synthetic-exact-account".into(),
                },
            )
            .unwrap();
        assert!(
            credentials
                .secret("GEMINI_API_KEY")
                .unwrap()
                .expose_secret_bytes()
                == b"synthetic-exact-account"
        );
        assert!(credentials.secret("OTHER_API_KEY").is_err());
        let oauth_only = super::ConnectionCredentials {
            store: store.clone(),
            id: "selected".into(),
            route: "openai-codex".into(),
            key_source: None,
            context_vars: vec![],
            scope_binding: None,
        };
        assert!(oauth_only.secret("oauth:openai-codex").is_err());
        let env_only = super::ConnectionCredentials {
            store,
            id: "selected".into(),
            route: "gemini".into(),
            key_source: Some((
                "GEMINI_API_KEY".into(),
                super::KeySource::Environment("EGO_TEST_ABSENT_KEY_1165".into()),
            )),
            context_vars: vec![],
            scope_binding: None,
        };
        assert!(env_only.secret("GEMINI_API_KEY").is_err());
    }

    // Catches: key material returned by a login program is saved as an unusable
    // OAuth connection and mutates owner configuration before the mismatch is found.
    #[test]
    fn named_connections_oauth_login_refuses_key_material_before_publication() {
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        let result = super::publish_login(
            &store,
            home.path(),
            "anthropic",
            None,
            AuthEntry::ApiKey {
                key: StoredSecret::from("key-not-an-oauth-token"),
            },
        );
        assert!(result.is_err());
        assert!(store.read_observed().unwrap().is_empty());
        assert!(!home.path().join("config.toml").exists());
    }

    // Catches: a key-minting built-in starts network/browser effects before refusing unsupported persistence.
    #[tokio::test]
    async fn named_connections_key_minting_login_refuses_before_network() {
        const CHILD: &str = "EGO_KEY_LOGIN_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "auth::tests::named_connections_key_minting_login_refuses_before_network",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("EGO_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let result = super::login(
            &AuthStore::new(&home),
            &home,
            "openrouter",
            None,
            false,
            &CancelToken::new(),
            &super::OsCredentialCommand,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("issues an API key")
        );
        assert!(AuthStore::new(&home).read_observed().unwrap().is_empty());
        assert!(
            super::known_and_discovered(&home, &Default::default())
                .lines()
                .any(|id| id == "openrouter")
        );
    }

    // Catches: route/vendor hard-coding overwrites another account, rename rekeys
    // secrets, or logout removes a different vendor's selected account.
    // Synthetic OAuth entries exercise local publication, not remote login validity.
    #[test]
    fn named_connections_login_rename_logout_keep_accounts_isolated() {
        use ego_ai::provider::Credentials;
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        let cases = [
            (
                "anthropic",
                "anthropic",
                "oauth:anthropic",
                "account-a",
                "account-b",
            ),
            (
                "openai",
                "openai-codex",
                "oauth:openai-codex",
                "openai-a",
                "openai-b",
            ),
            ("xai", "xai-oauth", "oauth:xai-oauth", "xai-a", "xai-b"),
        ];
        let mut accounts = Vec::new();
        for (vendor, route, credential, token_a, token_b) in cases {
            let a = super::publish_login(&store, home.path(), route, None, account_entry(token_a))
                .unwrap();
            let b = super::publish_login(&store, home.path(), route, None, account_entry(token_b))
                .unwrap();
            accounts.push((vendor, route, credential, token_a, token_b, a, b));
        }
        // All six identities coexist before any relogin or logout.
        let config = super::connection_layer_at(home.path()).unwrap();
        assert_eq!(config.connections.len(), 6);
        for (vendor, route, credential, token_a, token_b, a, b) in &accounts {
            assert_ne!(a, b, "{vendor}");
            assert_eq!(config.account_numbers[*vendor][a], 1);
            assert_eq!(config.account_numbers[*vendor][b], 2);
            for id in [a, b] {
                let selected = ego_core::config::select_connection(
                    &config.connections,
                    Some(id),
                    Some(vendor),
                )
                .unwrap()
                .unwrap();
                assert_eq!(selected.0, id, "{vendor}");
                assert_eq!(super::connection_route(selected.1), *route);
            }
            let path = home.path().join("config.toml");
            let text = std::fs::read_to_string(&path).unwrap();
            let original = format!("[connections.{a}]\nname = \"{vendor}\"");
            let edited = format!("[connections.{a}]\nname = \"Renamed\"");
            std::fs::write(&path, text.replace(&original, &edited)).unwrap();
            let renamed = super::connection_layer_at(home.path()).unwrap();
            assert_eq!(renamed.connections[a].name, "Renamed");
            assert_eq!(super::connection_route(&renamed.connections[a]), *route);
            let source = super::ConnectionCredentials {
                store: AuthStore::new(home.path()),
                id: a.clone(),
                route: (*route).into(),
                key_source: None,
                context_vars: Vec::new(),
                scope_binding: None,
            };
            assert_eq!(
                source.secret(credential).unwrap().expose_secret_bytes(),
                token_a.as_bytes()
            );
            assert!(source.secret("ANTHROPIC_API_KEY").is_err());
            let wrong_route = if *vendor == "openai" {
                "anthropic"
            } else {
                "openai-codex"
            };
            assert!(source.secret(&format!("oauth:{wrong_route}")).is_err());
            assert!(
                super::publish_login(
                    &store,
                    home.path(),
                    wrong_route,
                    Some(a),
                    account_entry("wrong"),
                )
                .is_err()
            );
            assert_eq!(
                source.secret(credential).unwrap().expose_secret_bytes(),
                token_a.as_bytes()
            );
            assert_eq!(
                store
                    .read_secret_material(&super::connection_storage_key(b))
                    .unwrap()
                    .expose_secret_bytes(),
                token_b.as_bytes()
            );
            super::publish_login(
                &store,
                home.path(),
                route,
                Some(b),
                account_entry(&format!("updated-{vendor}")),
            )
            .unwrap();
            store.remove(&super::connection_storage_key(a)).unwrap();
            assert!(source.secret(credential).is_err());
            assert_eq!(
                store
                    .read_secret_material(&super::connection_storage_key(b))
                    .unwrap()
                    .expose_secret_bytes(),
                format!("updated-{vendor}").as_bytes()
            );
        }
        // Later vendors' updates must not remove earlier vendors' surviving B.
        for (vendor, route, credential, _, _, _, b) in &accounts {
            let source = super::ConnectionCredentials {
                store: AuthStore::new(home.path()),
                id: b.clone(),
                route: (*route).into(),
                key_source: None,
                context_vars: Vec::new(),
                scope_binding: None,
            };
            assert_eq!(
                source.secret(credential).unwrap().expose_secret_bytes(),
                format!("updated-{vendor}").as_bytes(),
                "{vendor}"
            );
        }
    }

    // Catches: a named ID equal to a provider silently adopts, refreshes or
    // deletes its retired provider slot.
    #[tokio::test]
    async fn named_connections_never_adopt_provider_slots() {
        use ego_ai::provider::ProviderRow;
        const CHILD: &str = "EGO_TEST_NAMED_NAMESPACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(home.path().join("config.toml"), "connection='anthropic'\n[connections.anthropic]\nname='Personal'\nvendor='anthropic'\naccess='oauth'\n").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "auth::tests::named_connections_never_adopt_provider_slots",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("EGO_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let store = AuthStore::new(&home);
        let mut expired = account_entry("legacy-account");
        if let AuthEntry::OAuth { expires, .. } = &mut expired {
            *expires = Some(1);
        }
        store.put("anthropic", expired).unwrap();
        let metadata = super::connection_layer_at(&home).unwrap();
        let cancel = ego_core::turn::CancelToken::new();
        let source = super::connection_credentials(
            "anthropic",
            &metadata.connections["anthropic"],
            "anthropic",
            &ProviderRow::anthropic_default().credential,
            &Default::default(),
            None,
            &UnexpectedHttp,
            &super::OsCredentialCommand,
            &cancel,
            None,
        )
        .await
        .unwrap();
        assert!(source.secret("oauth:anthropic").is_err());
        let args = |action| crate::cli::AuthArgs {
            account: None,
            renew: false,
            connection: Some("anthropic".into()),
            action,
            provider: Some("anthropic".into()),
            device: false,
            method: None,
        };
        let logout = super::execute(&args(crate::cli::AuthAction::Logout), &cancel)
            .await
            .unwrap();
        assert!(logout.contains("no stored credential"));
        assert_eq!(
            store
                .read_secret_material("anthropic")
                .unwrap()
                .expose_secret_bytes(),
            b"legacy-account"
        );
        let usage: serde_json::Value = serde_json::from_str(
            &crate::auth_usage::execute(&home, Some("connection:anthropic"), None, true, &cancel)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(usage["rate_limits"].as_array().unwrap().is_empty());
        assert_eq!(usage["errors"][0]["state"], "auth_invalid");
        assert!(
            usage["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("ego auth login anthropic --renew")
        );
        // Expired named usage must retain the repair ID too, without any network refresh.
        let key = super::connection_storage_key("anthropic");
        store
            .put(
                &key,
                AuthEntry::OAuth {
                    access: StoredSecret::from("expired-test-token"),
                    refresh: None,
                    expires: Some(1),
                    scope_binding: None,
                },
            )
            .unwrap();
        let usage: serde_json::Value = serde_json::from_str(
            &crate::auth_usage::execute(&home, Some(&key), None, true, &cancel)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(usage["errors"][0]["state"], "auth_invalid");
        assert!(
            usage["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("ego auth login anthropic --renew")
        );
        store.remove(&key).unwrap();
        assert!(source.secret("oauth:anthropic").is_err());
        assert!(store.read_secret_material("anthropic").is_ok());
        // A key-env identity has no revocable saved login. Do not claim success
        // or remove an orphan OAuth slot when its environment key remains active.
        std::fs::write(home.join("config.toml"), "[connections.key]\nname='Key'\nvendor='anthropic'\naccess='key'\nkey_env='NAMED_NAMESPACE_KEY'\n").unwrap();
        let key_slot = super::connection_storage_key("key");
        store.put(&key_slot, account_entry("orphan-oauth")).unwrap();
        let key_args = crate::cli::AuthArgs {
            account: None,
            renew: false,
            connection: Some("key".into()),
            action: crate::cli::AuthAction::Logout,
            provider: None,
            device: false,
            method: None,
        };
        let refused = super::execute(&key_args, &cancel).await.unwrap_err();
        assert!(refused.to_string().contains("environment variable"));
        assert_eq!(
            store
                .read_secret_material(&key_slot)
                .unwrap()
                .expose_secret_bytes(),
            b"orphan-oauth"
        );
    }

    // Catches: a provider-only destructive command silently picks a named default, including a same-spelled ID.
    #[tokio::test]
    async fn named_connections_provider_commands_scope_accounts_without_touching_legacy() {
        const CHILD: &str = "EGO_PROVIDER_AUTH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "auth::tests::named_connections_provider_commands_scope_accounts_without_touching_legacy",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("EGO_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let store = AuthStore::new(&home);
        let cancel = CancelToken::new();
        for vendor in ["anthropic", "openai"] {
            std::fs::write(home.join("config.toml"), format!("connection='anthropic'\n[connections.anthropic]\nname='Named'\nvendor='{vendor}'\naccess='oauth'\n")).unwrap();
            store
                .put("anthropic", account_entry("legacy-private-token"))
                .unwrap();
            store
                .put(
                    &super::connection_storage_key("anthropic"),
                    account_entry("named-private-token"),
                )
                .unwrap();
            let mut args = crate::cli::AuthArgs {
                account: None,
                renew: false,
                action: crate::cli::AuthAction::Status,
                connection: None,
                provider: Some("anthropic".into()),
                device: false,
                method: None,
            };
            let status = super::execute(&args, &cancel).await.unwrap();
            assert_eq!(
                status.contains("Named (Oauth)"),
                vendor == "anthropic",
                "{status}"
            );
            assert!(!status.contains("[anthropic]"));
            let bare = crate::cli::AuthArgs {
                account: None,
                renew: false,
                action: crate::cli::AuthAction::Status,
                connection: None,
                provider: None,
                device: false,
                method: None,
            };
            let all = super::execute(&bare, &cancel).await.unwrap();
            assert!(!all.contains("legacy anthropic"));
            let orphan_key = super::connection_storage_key("orphan");
            store
                .put(&orphan_key, account_entry("orphan-private-token"))
                .unwrap();
            let all = super::execute(&bare, &cancel).await.unwrap();
            assert!(all.contains("orphan [orphan]"));
            let orphan_logout = crate::cli::AuthArgs {
                account: None,
                renew: false,
                action: crate::cli::AuthAction::Logout,
                connection: Some("orphan".into()),
                provider: None,
                device: false,
                method: None,
            };
            assert!(
                super::execute(&orphan_logout, &cancel)
                    .await
                    .unwrap()
                    .contains("logged out orphan")
            );
            assert!(store.read_secret_material(&orphan_key).is_err());
            assert!(store.read_secret_material("anthropic").is_ok());
            assert_eq!(
                store
                    .read_secret_material(&super::connection_storage_key("anthropic"))
                    .unwrap()
                    .expose_secret_bytes(),
                b"named-private-token"
            );
            args.action = crate::cli::AuthAction::Status;
            args.connection = Some("anthropic".into());
            args.provider = None;
            assert!(
                super::execute(&args, &cancel)
                    .await
                    .unwrap()
                    .contains("Named (Oauth)")
            );
            args.connection = None;
            args.provider = Some("anthropic".into());
            args.action = crate::cli::AuthAction::Logout;
            let logout = super::execute(&args, &cancel).await;
            assert_eq!(logout.is_ok(), vendor == "anthropic");
            assert!(store.read_secret_material("anthropic").is_ok());
            assert_eq!(
                store
                    .read_secret_material(&super::connection_storage_key("anthropic"))
                    .is_ok(),
                vendor != "anthropic"
            );
        }
    }

    // Catches: machine status collapses named/legacy identities, leaks human metadata, or guesses missing sources.
    #[tokio::test]
    async fn named_connections_status_json_preserves_settings_identity_and_presence() {
        const CHILD: &str = "EGO_AUTH_STATUS_CONTRACT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "auth::tests::named_connections_status_json_preserves_settings_identity_and_presence", "--nocapture"])
                .env(CHILD, "1").env("EGO_HOME", home.path())
                .env("CONTRACT_API_KEY", "synthetic-private-key")
                .env("CONTRACT_EMPTY_API_KEY", "")
                .env("CONTRACT_BLANK_API_KEY", "   ")
                .env_remove("CONTRACT_ABSENT_API_KEY")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            // Forward only the expected captured machine fixture from the isolated process.
            for payload in String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| {
                    line.split_once("AUTH_PRESENCE_CONTRACT_FIXTURE=")
                        .map(|(_, payload)| payload)
                })
            {
                println!("AUTH_PRESENCE_CONTRACT_FIXTURE={payload}");
            }
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let long_id = format!("long_{}", "x".repeat(80));
        std::fs::write(home.join("config.toml"), format!("connection='anthropic'\n[connections.account_a]\nname='Hidden label'\nvendor='anthropic'\naccess='oauth'\n[connections.account_b]\nname='Hidden label'\nvendor='openai'\naccess='oauth'\n[connections.anthropic]\nname='Hidden label'\nvendor='anthropic'\naccess='oauth'\n[connections.key_absent]\nname='Hidden label'\nvendor='openai'\naccess='key'\nkey_env='CONTRACT_ABSENT_API_KEY'\n[connections.key_blank]\nname='Hidden label'\nvendor='openai'\naccess='key'\nkey_env='CONTRACT_BLANK_API_KEY'\n[connections.key_empty]\nname='Hidden label'\nvendor='openai'\naccess='key'\nkey_env='CONTRACT_EMPTY_API_KEY'\n[connections.key_present]\nname='Hidden label'\nvendor='openai'\naccess='key'\nkey_env='CONTRACT_API_KEY'\n[connections.{long_id}]\nname='Hidden label'\nvendor='anthropic'\naccess='oauth'\n")).unwrap();
        let store = AuthStore::new(&home);
        for key in [
            "anthropic".to_owned(),
            "openai-codex".to_owned(),
            super::connection_storage_key("account_a"),
            super::connection_storage_key("anthropic"),
            super::connection_storage_key("key_absent"),
            super::connection_storage_key(&long_id),
            super::connection_storage_key("orphan"),
        ] {
            store
                .put(
                    &key,
                    AuthEntry::OAuth {
                        access: "synthetic-private-token".to_owned().into(),
                        refresh: Some("synthetic-private-refresh".to_owned().into()),
                        expires: Some(0),
                        scope_binding: None,
                    },
                )
                .unwrap();
        }
        // Catches: an OAuth account is reported present after a key-to-OAuth
        // metadata edit even though generation refuses its stored ApiKey.
        store
            .put(
                "connection:account_b",
                AuthEntry::ApiKey {
                    key: "synthetic-private-wrong-kind-key".into(),
                },
            )
            .unwrap();
        let before = std::fs::read(home.join("auth.json")).unwrap();
        // Catches: setup/startup discovery reports a route ready solely because
        // its OAuth metadata points at a stored ApiKey (account_b).
        assert_eq!(
            store.observed_provider_ids().unwrap(),
            std::collections::BTreeSet::from(["anthropic".to_owned()])
        );
        let mut args = crate::cli::AuthArgs {
            account: None,
            renew: false,
            action: crate::cli::AuthAction::StatusJson,
            connection: None,
            provider: None,
            device: false,
            method: None,
        };
        let cancel = CancelToken::new();
        let raw = super::execute(&args, &cancel).await.unwrap();
        for secret_metadata in [
            "synthetic-private",
            "Hidden label",
            "CONTRACT_",
            "expires",
            "key source",
            "legacy ",
        ] {
            assert!(
                !raw.contains(secret_metadata),
                "machine output must contain presence only"
            );
        }
        let actual: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let expected = serde_json::json!({"v":1,"credentials":[
            {"provider":"anthropic","connection":"account_a","present":true},
            {"provider":"openai","connection":"account_b","present":false},
            {"provider":"anthropic","connection":"anthropic","present":true},
            {"provider":"openai","connection":"key_absent","present":false},
            {"provider":"openai","connection":"key_blank","present":false},
            {"provider":"openai","connection":"key_empty","present":false},
            {"provider":"openai","connection":"key_present","present":true},
            {"provider":"anthropic","connection":long_id,"present":true},
            {"connection":"orphan","present":true}
        ]});
        assert_eq!(
            actual, expected,
            "full IDs and namespace identity must survive deterministic inventory order"
        );
        print!("AUTH_PRESENCE_CONTRACT_FIXTURE={raw}");
        assert_eq!(
            before,
            std::fs::read(home.join("auth.json")).unwrap(),
            "presence reads must not mutate credentials"
        );
        for (connection, provider, expected_rows) in [
            (
                None,
                Some("anthropic"),
                serde_json::json!([
                    {"provider":"anthropic","connection":"account_a","present":true},
                    {"provider":"anthropic","connection":"anthropic","present":true},
                    {"provider":"anthropic","connection":long_id,"present":true}
                ]),
            ),
            (
                Some("anthropic"),
                None,
                serde_json::json!([{"provider":"anthropic","connection":"anthropic","present":true}]),
            ),
            (
                Some("account_b"),
                None,
                serde_json::json!([{"provider":"openai","connection":"account_b","present":false}]),
            ),
            (
                Some(long_id.as_str()),
                None,
                serde_json::json!([{"provider":"anthropic","connection":long_id,"present":true}]),
            ),
            (None, Some("account_b"), serde_json::json!([])),
            (None, Some("missing"), serde_json::json!([])),
        ] {
            args.connection = connection.map(str::to_owned);
            args.provider = provider.map(str::to_owned);
            let output: serde_json::Value =
                serde_json::from_str(&super::execute(&args, &cancel).await.unwrap()).unwrap();
            assert_eq!(
                output,
                serde_json::json!({"v":1,"credentials":expected_rows})
            );
        }
        args.connection = Some("orphan".into());
        args.provider = None;
        assert!(
            super::execute(&args, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown connection")
        );
        args.connection = Some("anthropic".into());
        args.provider = Some("openai-codex".into());
        assert!(
            super::execute(&args, &cancel).await.is_err(),
            "existing vendor selection validation stays intact"
        );
        args.action = crate::cli::AuthAction::Status;
        args.connection = None;
        args.provider = None;
        let human = super::execute(&args, &cancel).await.unwrap();
        assert!(
            human.contains("openai account 5: Hidden label (Key)\nkey source: CONTRACT_API_KEY\n")
                && !human.contains("key_present")
        );
        assert!(!human.contains("legacy anthropic\toauth\texpires=0\n"));
    }

    // Catches: credentials accepted or refreshed under a changed Azure resource or helper.
    #[tokio::test]
    async fn azure_entra_refresh_keeps_resource_and_source_binding() {
        use ego_ai::provider::CredentialStrategy;
        use ego_core::config::AZURE_OPENAI;

        const TEST: &str = "auth::tests::azure_entra_refresh_keeps_resource_and_source_binding";
        const CHILD: &str = "EGO_TEST_AZURE_SCOPE_CHILD";
        const RESOURCE: &str = "https://tenant.openai.azure.com/openai/v1/";
        let program = std::env::current_exe().unwrap();
        let command = shell_words::join([program.to_str().unwrap()]);
        let config = format!(
            r#"
[connections.azure]
name = "Azure"
vendor = "azure-openai"
access = "oauth"
azure_resource = "{RESOURCE}"
[auth.azure-openai]
credential_command = {}
"#,
            serde_json::to_string(&command).unwrap()
        );
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(home.path().join("config.toml"), config).unwrap();
            let output = tokio::process::Command::new(&program)
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("EGO_HOME", home.path())
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("ran Azure scope scenario"));
            return;
        }
        struct Helper;
        impl super::CredentialCommandPort for Helper {
            fn run<'a>(
                &'a self,
                request: super::CredentialCommandRequest,
            ) -> futures::future::BoxFuture<'a, Result<StoredSecret, super::CredentialCommandFailure>>
            {
                Box::pin(async move {
                    let verb = request.argv.last().unwrap();
                    assert_eq!(
                        request.argv[1..5],
                        [
                            "--resource",
                            RESOURCE,
                            "--scope",
                            "https://ai.azure.com/.default"
                        ]
                        .map(std::ffi::OsString::from),
                    );
                    let (access, expires) = if verb == "login" {
                        assert!(request.stdin.is_none());
                        // Valid now but within the existing refresh window.
                        ("initial-test-token", super::now_ms() + 180_000)
                    } else {
                        assert_eq!(verb, "refresh");
                        let input: serde_json::Value =
                            serde_json::from_slice(request.stdin.unwrap().expose_secret_bytes())
                                .unwrap();
                        assert_eq!(input["refresh"], "test-renewal-state");
                        ("rotated-test-token", 4_102_444_800_000)
                    };
                    Ok(StoredSecret::from(
                        serde_json::json!({
                            "access": access, "refresh": "test-renewal-state", "expires": expires
                        })
                        .to_string(),
                    ))
                })
            }
        }
        let home = super::ego_home().unwrap();
        let store = AuthStore::new(&home);
        let cancel = CancelToken::new();
        super::login(
            &store,
            &home,
            AZURE_OPENAI,
            Some("azure"),
            false,
            &cancel,
            &Helper,
        )
        .await
        .unwrap();
        let account = super::connection_layer_at(&home)
            .unwrap()
            .connections
            .remove("azure")
            .unwrap();
        let strategy = CredentialStrategy::OAuth {
            provider: AZURE_OPENAI.into(),
        };
        let auth_for = |command: &str| {
            std::collections::HashMap::from([(
                AZURE_OPENAI.into(),
                ego_core::config::AuthProviderLayer {
                    credential_command: Some(command.into()),
                    ..Default::default()
                },
            )])
        };
        let load = |account, auth| {
            super::connection_credentials(
                "azure",
                account,
                AZURE_OPENAI,
                &strategy,
                auth,
                None,
                &UnexpectedHttp,
                &Helper,
                &cancel,
                None,
            )
        };
        let mut changed = account.clone();
        changed.azure_resource = Some("https://other.openai.azure.com/openai/v1/".parse().unwrap());
        let auth = auth_for(&command);
        let changed_auth = auth_for(&format!("{command} changed"));
        for (account, auth) in [(&changed, &auth), (&account, &changed_auth)] {
            let error = load(account, auth)
                .await
                .err()
                .expect("changed binding must refuse");
            assert!(error.to_string().contains("renew the selected account"));
        }
        let credentials = load(&account, &auth).await.unwrap();
        assert_eq!(
            credentials
                .secret("oauth:azure-openai")
                .unwrap()
                .expose_secret_bytes(),
            b"rotated-test-token"
        );
        println!("ran Azure scope scenario");
    }

    // Catches: refresh reads or replaces the provider-keyed account instead of the selected storage ID.
    #[tokio::test]
    async fn named_connections_refresh_reads_and_rotates_only_the_selected_account() {
        struct RefreshCommand;
        impl super::CredentialCommandPort for RefreshCommand {
            fn run<'a>(
                &'a self,
                request: super::CredentialCommandRequest,
            ) -> futures::future::BoxFuture<'a, Result<StoredSecret, super::CredentialCommandFailure>>
            {
                Box::pin(async move {
                    assert_eq!(request.argv.last().unwrap(), "refresh");
                    let input: serde_json::Value =
                        serde_json::from_slice(request.stdin.unwrap().expose_secret_bytes())
                            .unwrap();
                    assert_eq!(input["refresh"], "refresh-account-b");
                    Ok(StoredSecret::from(
                        r#"{"access":"new-b","refresh":"rotated-b","expires":4102444800000}"#,
                    ))
                })
            }
        }
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        store
            .put(
                &super::connection_storage_key("account_a"),
                account_entry("account-a"),
            )
            .unwrap();
        store
            .put("anthropic", account_entry("legacy-account"))
            .unwrap();
        store
            .put(
                &super::connection_storage_key("account_b"),
                AuthEntry::OAuth {
                    access: StoredSecret::from("account-b"),
                    refresh: Some(StoredSecret::from("refresh-account-b")),
                    expires: Some(1),
                    scope_binding: None,
                },
            )
            .unwrap();
        super::refresh_connection_at(
            home.path(),
            &super::connection_storage_key("account_b"),
            "anthropic",
            super::CredentialSource::Command("owned-refresh".into()),
            None,
            &UnexpectedHttp,
            &RefreshCommand,
            &ego_core::turn::CancelToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            store
                .read_secret_material(&super::connection_storage_key("account_b"))
                .unwrap()
                .expose_secret_bytes(),
            b"new-b"
        );
        assert_eq!(
            store
                .read_secret_material(&super::connection_storage_key("account_a"))
                .unwrap()
                .expose_secret_bytes(),
            b"account-a"
        );
        assert_eq!(
            store
                .read_secret_material("anthropic")
                .unwrap()
                .expose_secret_bytes(),
            b"legacy-account"
        );
    }

    // Catches: a key-returning refresh replaces a named OAuth slot, or its refusal
    // changes another account. Legacy credential-program key output remains valid.
    #[tokio::test]
    async fn named_connections_refresh_refuses_key_material_without_persistence() {
        struct KeyRefreshCommand;
        impl super::CredentialCommandPort for KeyRefreshCommand {
            fn run<'a>(
                &'a self,
                request: super::CredentialCommandRequest,
            ) -> futures::future::BoxFuture<'a, Result<StoredSecret, super::CredentialCommandFailure>>
            {
                Box::pin(async move {
                    assert_eq!(request.argv.last().unwrap(), "refresh");
                    let input: serde_json::Value =
                        serde_json::from_slice(request.stdin.unwrap().expose_secret_bytes())
                            .unwrap();
                    assert_eq!(input["refresh"], "original-refresh");
                    Ok(StoredSecret::from("replacement-api-key"))
                })
            }
        }
        for (storage_id, named) in [("connection:account_b", true), ("anthropic", false)] {
            let home = tempfile::tempdir().unwrap();
            let store = AuthStore::new(home.path());
            store
                .put("connection:account_a", account_entry("other-account"))
                .unwrap();
            store
                .put("openai-codex", account_entry("legacy-other-account"))
                .unwrap();
            store
                .put(
                    storage_id,
                    AuthEntry::OAuth {
                        access: StoredSecret::from("original-access"),
                        refresh: Some(StoredSecret::from("original-refresh")),
                        expires: Some(1),
                        scope_binding: None,
                    },
                )
                .unwrap();
            let before = std::fs::read(store.path()).unwrap();
            let result = super::refresh_connection_at(
                home.path(),
                storage_id,
                "anthropic",
                super::CredentialSource::Command("owned-refresh".into()),
                None,
                &UnexpectedHttp,
                &KeyRefreshCommand,
                &CancelToken::new(),
                None,
            )
            .await;
            if named {
                let error = result.unwrap_err();
                assert!(error.to_string().contains("requires OAuth credentials"));
                assert!(!error.to_string().contains("replacement-api-key"));
                assert_eq!(std::fs::read(store.path()).unwrap(), before);
            } else {
                result.unwrap();
                assert_eq!(
                    store
                        .read_secret_material(storage_id)
                        .unwrap()
                        .expose_secret_bytes(),
                    b"replacement-api-key"
                );
            }
            for (id, expected) in [
                ("connection:account_a", b"other-account".as_slice()),
                ("openai-codex", b"legacy-other-account".as_slice()),
            ] {
                assert_eq!(
                    store
                        .read_secret_material(id)
                        .unwrap()
                        .expose_secret_bytes(),
                    expected
                );
            }
        }
    }

    // Catches: failed temporary-file publication loses an existing account during
    // named update or logout.
    #[test]
    fn named_connections_failed_persistence_preserves_committed_accounts() {
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        store
            .put("connection:account_a", account_entry("account-a"))
            .unwrap();
        store
            .put("connection:account_b", account_entry("account-b"))
            .unwrap();
        store.put("anthropic", account_entry("legacy")).unwrap();
        let before = std::fs::read(store.path()).unwrap();
        std::fs::create_dir(home.path().join("auth.json.tmp")).unwrap();
        let results = [
            store.put("connection:account_b", account_entry("replacement")),
            store.remove("connection:account_a").map(|_| ()),
        ];
        for result in results {
            assert!(result.is_err());
        }
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        let reopened = AuthStore::new(home.path());
        assert_eq!(reopened.read().unwrap().len(), 3);
        for (id, expected) in [
            ("connection:account_a", b"account-a".as_slice()),
            ("connection:account_b", b"account-b".as_slice()),
            ("anthropic", b"legacy".as_slice()),
        ] {
            assert_eq!(
                reopened
                    .read_secret_material(id)
                    .unwrap()
                    .expose_secret_bytes(),
                expected
            );
        }
        assert!(
            reopened
                .read_secret_material("connection:migrated")
                .is_err()
        );
    }

    // Catches: a failed InsertConnection after credential publication leaves
    // a new unregistered key or damages a previously committed account.
    #[test]
    fn new_key_publication_rolls_back_when_account_metadata_cannot_be_written() {
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        store
            .put(
                "connection:existing",
                AuthEntry::ApiKey {
                    key: "synthetic-existing-key".into(),
                },
            )
            .unwrap();
        let before = std::fs::read(store.path()).unwrap();
        // Only metadata publication fails: the independent auth store is writable.
        std::fs::create_dir(home.path().join("config.toml")).unwrap();
        let result = super::publish_credential(
            &store,
            home.path(),
            "gemini",
            None,
            ego_core::config::ConnectionAccess::Key,
            AuthEntry::ApiKey {
                key: "synthetic-new-key".into(),
            },
        );
        assert!(matches!(result, Err(AuthError::InvalidConfig(_))));
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        let entries = AuthStore::new(home.path()).read_observed().unwrap();
        assert_eq!(entries.len(), 1, "failed publication left an orphan");
        assert!(entries.contains_key("connection:existing"));
        assert!(home.path().join("config.toml").is_dir());
    }

    // Catches: independent concurrent logins lose an account in either atomic store.
    #[test]
    fn named_connections_concurrent_login_preserves_both_identity_records() {
        let home = tempfile::tempdir().unwrap();
        let ids: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = ["account-a", "account-b"]
                .into_iter()
                .map(|value| {
                    let home = home.path();
                    scope.spawn(move || {
                        super::publish_login(
                            &AuthStore::new(home),
                            home,
                            "anthropic",
                            None,
                            account_entry(value),
                        )
                        .unwrap()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        let config = super::connection_layer_at(home.path()).unwrap();
        let store = AuthStore::new(home.path());
        assert_eq!(config.connections.len(), 2);
        assert_eq!(store.read().unwrap().len(), 2);
        for id in ids {
            assert!(config.connections.contains_key(&id));
            assert!(
                store
                    .read_secret_material(&super::connection_storage_key(&id))
                    .is_ok()
            );
        }
    }

    // Catches: listings disagree with direct login, a filter implicitly enables
    // Anthropic, or disabled login reaches a configured credential command.
    #[tokio::test]
    async fn oauth_login_availability_gates_cli_and_session_before_effects() {
        const CHILD: &str = "EGO_LOGIN_AVAILABILITY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for (provider, setting, enabled) in [
                ("anthropic", "", false),
                ("anthropic", "oauth_login_enabled = true\n", true),
                ("anthropic", "oauth_login_enabled = false\n", false),
                (
                    "anthropic",
                    if cfg!(windows) {
                        "[auth.anthropic.protocol_filter]\ncommand = 'C:/filters/filter.exe'\n"
                    } else {
                        "[auth.anthropic.protocol_filter]\ncommand = '/filters/filter'\n"
                    },
                    false,
                ),
                ("openai-codex", "", true),
                ("openai-codex", "oauth_login_enabled = false\n", false),
            ] {
                let root = tempfile::tempdir().unwrap();
                // A missing executable is an effect sentinel: admission must
                // refuse disabled login rather than try to launch this command.
                std::fs::write(
                    root.path().join("config.toml"),
                    format!(
                        "[auth.{provider}]\ncredential_command = '/nonexistent/ego-login-sentinel'\n{setting}"
                    ),
                )
                .unwrap();
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "auth::tests::oauth_login_availability_gates_cli_and_session_before_effects",
                        "--nocapture",
                    ])
                    .env(CHILD, provider)
                    .env("EGO_HOME", root.path())
                    .env("EGO_LOGIN_EXPECT_ENABLED", enabled.to_string())
                    .env_remove(ego_ai::provider::OPENAI_CODEX_ACCESS_TOKEN_ENV)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{provider} {setting:?}: stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let provider = std::env::var(CHILD).unwrap();
        let enabled = std::env::var("EGO_LOGIN_EXPECT_ENABLED").unwrap() == "true";
        let cancel = ego_core::turn::CancelToken::new();
        let args = |selected: Option<&str>, action| crate::cli::AuthArgs {
            account: None,
            renew: false,
            action,
            connection: None,
            provider: selected.map(str::to_string),
            device: false,
            method: None,
        };
        let listing = super::execute(&args(None, crate::cli::AuthAction::Login), &cancel)
            .await
            .unwrap();
        assert_eq!(
            listing.lines().any(|id| id == provider),
            enabled || provider == "anthropic",
            "{listing}"
        );
        let session_listing = super::session_login(None, None, &crate::output::Stderr)
            .await
            .unwrap();
        assert_eq!(
            session_listing.split_whitespace().any(|id| id == provider),
            enabled
        );
        let mut oauth_args = args(Some(&provider), crate::cli::AuthAction::Login);
        oauth_args.method = Some(crate::cli::AuthLoginMethod::Oauth);
        let cli_error = super::execute(&oauth_args, &cancel)
            .await
            .unwrap_err()
            .to_string();
        let session_error = super::session_login(Some(&provider), None, &crate::output::Stderr)
            .await
            .unwrap_err()
            .to_string();
        if enabled {
            assert!(cli_error.contains("ego-login-sentinel"), "{cli_error}");
            assert_eq!(session_error, "login requires an interactive terminal");
        } else {
            for error in [cli_error, session_error] {
                assert!(error.contains("is disabled"), "{error}");
                assert!(error.contains(&format!("auth.{provider}.oauth_login_enabled=true")));
            }
        }
        // An old provider slot must neither appear in status nor be removed by logout.
        let store = AuthStore::new(super::ego_home().unwrap());
        store
            .put(&provider, account_entry("saved-test-access"))
            .unwrap();
        let status = super::execute(
            &args(Some(&provider), crate::cli::AuthAction::Status),
            &cancel,
        )
        .await
        .unwrap();
        assert!(status.is_empty());
        assert!(
            super::execute(
                &args(Some(&provider), crate::cli::AuthAction::Logout),
                &cancel
            )
            .await
            .is_err()
        );
        assert!(store.read_secret_material(&provider).is_ok());
    }

    struct UnexpectedHttp;

    struct LoginFixtureHttp;

    impl ego_ai::http::Http for LoginFixtureHttp {
        fn begin_call(&self) -> Box<dyn ego_ai::http::HttpCall> {
            Box::new(LoginFixtureCall)
        }
    }

    struct LoginFixtureCall;

    impl ego_ai::http::HttpCall for LoginFixtureCall {
        fn execute(
            &mut self,
            _call: ego_ai::http::PreparedHttpCall,
        ) -> futures::future::BoxFuture<
            'static,
            Result<ego_core::permit::OneHopResponse, ego_core::permit::TransportError>,
        > {
            Box::pin(async {
                Ok(ego_core::permit::OneHopResponse::response(
                    200,
                    Vec::new(),
                    br#"{"key":"sk-or-fixture"}"#.to_vec(),
                ))
            })
        }
    }

    impl ego_ai::http::Http for UnexpectedHttp {
        fn begin_call(&self) -> Box<dyn ego_ai::http::HttpCall> {
            panic!("credential tests must not perform an HTTP exchange")
        }
    }

    // Catches: external task credentials read or refresh the stored login.
    #[tokio::test]
    async fn external_token_is_used_and_never_refreshed() {
        const TOKEN: &str = "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig";
        if std::env::var_os("EGO_EXTERNAL_TOKEN_TEST_CHILD").is_none() {
            for token in [TOKEN, "malformed-external-token"] {
                let home = tempfile::tempdir().unwrap();
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "auth::tests::external_token_is_used_and_never_refreshed",
                        "--nocapture",
                    ])
                    .env("EGO_EXTERNAL_TOKEN_TEST_CHILD", "1")
                    .env("EGO_HOME", home.path())
                    .env("OPENAI_CODEX_ACCESS_TOKEN", token)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let store = AuthStore::new(super::ego_home().unwrap());
        store
            .put(
                "openai-codex",
                AuthEntry::OAuth {
                    access: StoredSecret::from("expired-stored-access"),
                    refresh: Some(StoredSecret::from("must-not-refresh")),
                    expires: Some(1),
                    scope_binding: None,
                },
            )
            .unwrap();
        let credentials = request_credentials("openai-codex", &UnexpectedHttp)
            .await
            .unwrap();
        // A corrupt store makes even a read observable, after preparation.
        std::fs::write(store.path(), "not JSON").unwrap();
        // Preparation must not read the corrupt store either, even with a
        // configured renewal command.
        std::fs::write(
            store.path().with_file_name("config.toml"),
            "[auth.openai-codex]\ncredential_command='must-not-run'\n",
        )
        .unwrap();
        request_credentials("openai-codex", &UnexpectedHttp)
            .await
            .unwrap();
        let resolved = ProviderRow::openai_codex_default()
            .credential
            .resolve(credentials.as_ref());
        if std::env::var("OPENAI_CODEX_ACCESS_TOKEN").unwrap() != TOKEN {
            let error = resolved.unwrap_err();
            assert!(!error.to_string().contains("malformed-external-token"));
            return;
        }
        let credential = resolved.unwrap();
        assert!(
            credential
                .headers()
                .iter()
                .any(|header| header.name() == "authorization"
                    && header.value() == format!("Bearer {TOKEN}").as_bytes()
                    && header.is_secret())
        );
        assert!(
            credential
                .headers()
                .iter()
                .any(|header| header.name() == "chatgpt-account-id" && header.value() == b"acct_1")
        );
    }

    /// A subscription login still completes wearing ego's own name, and what
    /// it leaves behind is a token rather than an identity.
    ///
    /// The Copilot token exchange is the login step that used to carry the
    /// Copilot Chat / VS Code disguise; `ego-ai/tests/it/vendor_client_identity.rs`
    /// enforces that the disguise is gone. This is the other half: with only
    /// `accept`, the credential and ego's own `user-agent` on the wire, the
    /// exchange still builds, GitHub's answer still becomes an entry, and
    /// `ego auth status` reports that entry as an id, a kind and an expiry.
    /// **No header reaches the store or the report** — which is why an
    /// operator's `[auth.<provider>.headers]` overlay is request data and
    /// never a second thing to log in with.
    #[test]
    fn a_subscription_login_completes_on_egos_identity_and_status_reports_only_the_token() {
        let flow = BuiltInFlow::GitHubCopilot;
        let request = flow.copilot_exchange_request("gho_github").unwrap();
        let names: Vec<&str> = request.headers.iter().map(|header| header.name()).collect();
        assert_eq!(names, ["accept", "authorization", "user-agent"]);
        assert!(request.headers.iter().any(|header| {
            header.name() == "user-agent" && header.value().starts_with(b"ego/")
        }));

        let token = flow
            .read_token(
                TokenStatus::new(
                    200,
                    br#"{"token":"tid=1;proxy-ep=proxy.individual.githubcopilot.com","expires_at":4102444800}"#,
                ),
                0,
                Some("gho_github"),
            )
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = AuthStore::new(root.path());
        store
            .put("github-copilot", token_entry(flow, token).unwrap())
            .unwrap();

        assert_eq!(
            status(&store, None).unwrap(),
            "github-copilot\toauth\texpires=4102444800000\n"
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

    // Catches: a completed browser exchange writes a token that the next
    // credential resolution cannot read without starting a new process.
    #[tokio::test]
    async fn browser_oauth_exchange_is_readable_by_the_next_credential_request() {
        use std::sync::{Arc, Mutex};

        let output = Arc::new(Mutex::new(String::new()));
        let shown = Arc::clone(&output);
        let pasted = Arc::clone(&output);
        let token = super::browser_login_with_input(
            BuiltInFlow::OpenRouter,
            &LoginFixtureHttp,
            &ego_ai::oauth::LoginHeaders::default(),
            &move |message| {
                if message.starts_with("Open this URL") {
                    *shown.lock().unwrap() = message.to_owned();
                }
            },
            true,
            move || async move {
                let shown = pasted.lock().unwrap().clone();
                let url = url::Url::parse(shown.split_once('\n').unwrap().1).unwrap();
                assert_eq!(url.host_str(), Some("openrouter.ai"));
                Ok("fixture-code".to_owned())
            },
        )
        .await
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let store = AuthStore::new(home.path());
        let id = super::publish_credential(
            &store,
            home.path(),
            "openrouter",
            None,
            ego_core::config::ConnectionAccess::Key,
            token_entry(BuiltInFlow::OpenRouter, token).unwrap(),
        )
        .unwrap();
        let next = super::ConnectionCredentials {
            store,
            id,
            route: "openrouter".into(),
            key_source: Some((
                ego_ai::provider::OPENROUTER_KEY_VAR.into(),
                super::KeySource::Store,
            )),
            context_vars: Vec::new(),
            scope_binding: None,
        };
        let credential = ProviderRow::openrouter_default()
            .credential
            .resolve(&next)
            .unwrap();
        assert!(credential.headers().iter().any(|header| {
            header.name() == "authorization" && header.value() == b"Bearer sk-or-fixture"
        }));
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
            true,
            future::pending::<Result<String, AuthError>>(),
        )
        .await
        .unwrap();

        assert_eq!(code, "from-loopback");
        assert!(client.await.unwrap().starts_with("HTTP/1.1 200"));
    }

    // Catches: a browser-only provider whose authorization URL has no state
    // parameter rejects every valid loopback callback before token exchange.
    #[tokio::test]
    async fn loopback_callback_accepts_stateless_provider_but_rejects_wrong_state() {
        for (query, accepted) in [("code=fixture", true), ("code=fixture&state=wrong", false)] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let request =
                format!("GET /oauth/callback?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let client = tokio::spawn(async move {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
                response
            });
            let result = wait_for_browser_code(
                listener,
                "/oauth/callback",
                "expected",
                false,
                future::pending::<Result<String, AuthError>>(),
            )
            .await;
            assert_eq!(result.is_ok(), accepted);
            assert_eq!(client.await.unwrap().starts_with("HTTP/1.1 200"), accepted);
        }
    }

    #[tokio::test]
    async fn manual_paste_can_win_the_race_against_loopback() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let pasted = future::ready(Ok(
            "http://localhost/auth/callback?code=from-paste&state=expected".to_string(),
        ));

        let code = wait_for_browser_code(listener, "/auth/callback", "expected", true, pasted)
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
                true,
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
        let first_entry = AuthEntry::OAuth {
            access: StoredSecret::from("access"),
            refresh: Some(StoredSecret::from("refresh")),
            expires: Some(1),
            scope_binding: None,
        };
        store.put("openai-codex", first_entry).unwrap();

        let invalid = FlowError::ReloginRequired {
            provider: "openai-codex".to_string(),
            cause: TokenRefusal::GrantInvalidated,
        };
        let err = apply_refresh_failure(&store, "openai-codex", invalid).unwrap_err();
        assert!(matches!(&err, AuthError::ReloginRequired { .. }));
        // Catches: legacy recovery silently presents a new identity as an in-place repair.
        let legacy_remedy = err.to_string();
        assert!(
            legacy_remedy.contains("'ego auth login openai-codex' creates a new named connection")
        );
        assert!(legacy_remedy.contains("not a repair of this legacy identity"));
        assert!(
            legacy_remedy.contains("Existing unbound sessions cannot resume with that connection")
        );
        assert!(!store.read().unwrap().contains_key("openai-codex"));

        store
            .put(
                "openai-codex",
                AuthEntry::OAuth {
                    access: StoredSecret::from("access"),
                    refresh: Some(StoredSecret::from("refresh")),
                    expires: Some(1),
                    scope_binding: None,
                },
            )
            .unwrap();
        let limited = FlowError::Temporary {
            provider: "openai-codex".to_string(),
            status: 429,
            cause: TokenRefusal::RateLimited,
        };
        let err = apply_refresh_failure(&store, "openai-codex", limited).unwrap_err();
        assert!(matches!(err, AuthError::Temporary { status: 429, .. }));
        assert!(store.read().unwrap().contains_key("openai-codex"));
    }
    // Catches: a refused login exchange is presented as legacy expiry, or refresh loses expiry handling.
    #[tokio::test]
    async fn login_refusal_is_not_legacy_expiry_but_refresh_refusal_is() {
        use super::{built_in_login, refresh_built_in};
        use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
        use ego_ai::oauth::LoginHeaders;
        use ego_core::permit::{OneHopResponse, TransportError};
        use futures::future::BoxFuture;

        struct Refusal(u16);
        impl Http for Refusal {
            fn begin_call(&self) -> Box<dyn HttpCall> {
                Box::new(Refusal(self.0))
            }
        }
        impl HttpCall for Refusal {
            fn execute(
                &mut self,
                _call: PreparedHttpCall,
            ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
                let status = self.0;
                // A controlled transport refusal, not a recorded provider response or conformance claim.
                Box::pin(
                    async move { Ok(OneHopResponse::response(status, Vec::new(), Vec::new())) },
                )
            }
        }

        let headers = LoginHeaders::new(Default::default());
        let login = built_in_login("openai-codex", false, true, &Refusal(401), &headers)
            .await
            .err()
            .unwrap();
        let message = login.to_string();
        assert!(
            matches!(login, AuthError::MissingCredential { .. }),
            "{message}"
        );
        assert!(!message.contains("legacy"), "{message}");
        assert!(
            !message.contains("creates a new named connection"),
            "{message}"
        );
        assert!(
            message.contains("ego built-in openai-codex login"),
            "{message}"
        );

        let current = AuthEntry::OAuth {
            access: StoredSecret::from("owned-access"),
            refresh: Some(StoredSecret::from("owned-refresh")),
            expires: Some(1),
            scope_binding: None,
        };
        let refresh = refresh_built_in("openai-codex", &current, &Refusal(401), &headers)
            .await
            .err()
            .unwrap();
        assert!(matches!(refresh, AuthError::ReloginRequired { .. }));
        assert!(refresh.to_string().contains("legacy login expired"));

        let retryable = built_in_login("openai-codex", false, true, &Refusal(429), &headers)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            retryable,
            AuthError::Temporary { status: 429, .. }
        ));
    }

    // Catches: successful cloud publication is rejected by named request assembly,
    // loses project/location, or falls back to a legacy token after the named slot disappears.
    #[tokio::test]
    async fn account_findings_cloud_login_publication_reaches_the_real_credential_adapter() {
        const CHILD: &str = "EGO_ACCOUNT_FINDINGS_CLOUD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for case in ["global", "region", "missing-project", "invalid-location"] {
                let home = tempfile::tempdir().unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", "auth::tests::account_findings_cloud_login_publication_reaches_the_real_credential_adapter", "--nocapture"])
                    .env_clear()
                    .envs(crate::run::test_support::coverage_environment())
                    .envs(crate::run::test_support::windows_system_environment())
                    .env("PATH", crate::run::test_support::fixture_path())
                    .env("HOME", home.path()).env("EGO_HOME", home.path())
                    .env(CHILD, case)
                    .env("GOOGLE_OAUTH_ACCESS_TOKEN", "unrelated-ambient-token");
                if case != "missing-project" {
                    command.env("GOOGLE_CLOUD_PROJECT", "owned-project");
                }
                if case == "region" {
                    command.env("GOOGLE_CLOUD_LOCATION", "eu");
                }
                if case == "invalid-location" {
                    command.env("GOOGLE_CLOUD_LOCATION", "not/a/location");
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let case = std::env::var(CHILD).unwrap();
        let home = std::path::PathBuf::from(std::env::var_os("EGO_HOME").unwrap());
        let store = AuthStore::new(&home);
        let result = super::publish_login(
            &store,
            &home,
            "vertex",
            None,
            account_entry("named-cloud-token"),
        );
        if case == "missing-project" || case == "invalid-location" {
            assert!(
                result.is_err(),
                "unusable cloud context must refuse publication"
            );
            assert!(!store.path().exists());
            assert!(!home.join("config.toml").exists());
            return;
        }
        let id = result.unwrap();
        let metadata = super::connection_layer_at(&home).unwrap();
        let row = ProviderRow::vertex();
        let source = super::connection_credentials(
            &id,
            &metadata.connections[&id],
            "vertex",
            &row.credential,
            &Default::default(),
            None,
            &UnexpectedHttp,
            &super::OsCredentialCommand,
            &CancelToken::new(),
            None,
        )
        .await
        .unwrap();
        let resolved = row.credential.resolve(source.as_ref()).unwrap();
        assert_eq!(
            resolved.base_override().unwrap().as_str(),
            if case == "region" {
                "https://aiplatform.eu.rep.googleapis.com/v1/projects/owned-project/locations/eu/"
            } else {
                "https://aiplatform.googleapis.com/v1/projects/owned-project/locations/global/"
            }
        );
        let headers = resolved.headers();
        assert_eq!(
            headers
                .iter()
                .find(|header| header.name() == "authorization")
                .unwrap()
                .value(),
            b"Bearer named-cloud-token"
        );
        assert!(source.secret("GOOGLE_OAUTH_ACCESS_TOKEN").is_err());
        store
            .put("vertex", account_entry("unrelated-legacy-token"))
            .unwrap();
        store.remove(&super::connection_storage_key(&id)).unwrap();
        assert!(
            row.credential.resolve(source.as_ref()).is_err(),
            "a missing named slot must not use legacy or ambient tokens"
        );
    }
}
