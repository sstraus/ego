//! is an external completeness gate: only the packaged TUICommander backend
//! is launched, and it owns the configured ego child. The ordinary workspace
//! run executes only the deterministic sanitizer contract.
//! Required command:
//! ```text
//! EGO_HOST_TEST_TUIC_ARTIFACT=/absolute/path/to/tuic-remote \
//! EGO_HOST_TEST_EGO_EXECUTABLE=/absolute/path/to/ego \
//! EGO_HOST_TEST_TUIC_COMMIT=<clean-tuicommander-commit> \
//! EGO_HOST_TEST_EGO_COMMIT=<clean-ego-commit> \
//! EGO_HOST_TEST_TUIC_HOME=/absolute/task-owned/home \
//! EGO_HOST_TEST_TUIC_INSTANCE=<named-instance-id> \
//! EGO_HOST_TEST_TUIC_CONFIG_DIR=/absolute/path/to/named-config-dir \
//! EGO_HOST_TEST_AUTH_HEADER_FILE=/absolute/path/to/auth-header.txt \
//! EGO_HOST_TEST_ENDPOINT=http://127.0.0.1:<port> \
//! EGO_HOST_TEST_EVIDENCE=/absolute/path/to/host-test-evidence.json \
//! cargo test -p ego-cli --test tuicommander_host -- --ignored --test-threads=1
//! ```
//! Set `EGO_HOST_TEST_LIVE=1` only for operator-authorized provider proof. That
//! run also requires `EGO_MODEL` and `EGO_HOST_TEST_LIVE_CREDENTIAL_ENV`, whose
//! value names the selected provider row's existing credential environment
//! variable. The harness resolves that name from the canonical provider row
//! before reading a secret and forwards only that variable and `EGO_MODEL`.
//! Live mode is read-only by default. A separately authorized run may also set
//! `EGO_HOST_TEST_PERMISSION_LIVE=1` to exercise ONE exact disposable fixture
//! write under workspace confinement. Read-only approval does not authorize
//! this additional mutation. Both modes retain the complete acceptance matrix;
//! absent reasoning, cost, or permission proof keeps the matrix RED.
//! `EGO_HOST_TEST_AUTH_HEADER_FILE` contains exactly one `Cookie: tui-session=...`
//! header using a freshly generated pairing token. Use a debug `tuic-remote`
//! artifact: its production debug file vault avoids all native keychain access.
//! `EGO_HOST_TEST_TUIC_HOME` must be a fresh task-owned directory below TMPDIR,
//! containing the platform-resolved named config directory and ownership marker.
//! Every ego child receives a separate marked EGO_HOME below the workspace.
//! No operator HOME, vault, daemon or authentication state is used.

use std::collections::BTreeMap;
use std::ffi::OsString;

use agent_client_protocol_schema::v1;
use ego_ai::catalogue::ModelSlug;
use ego_ai::provider::ProviderRow;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use async_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::{HeaderValue, Request},
};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use url::Url;

const STATUS_MARKER: &str = "__EGO_HOST_TEST_STATUS__";
const FIXTURE_MARKER: &str = ".ego-host-test-isolated";
const LIVE_ENABLE: &str = "EGO_HOST_TEST_LIVE";
const LIVE_CREDENTIAL_ENV: &str = "EGO_HOST_TEST_LIVE_CREDENTIAL_ENV";
const PERMISSION_LIVE_ENABLE: &str = "EGO_HOST_TEST_PERMISSION_LIVE";
const PERMISSION_CONTENTS: &str = "authorized fixture replacement.\n";
const LIVE_BUDGET: &str = "0.05$";

struct Prerequisites {
    tuic_artifact: PathBuf,
    ego_executable: PathBuf,
    tuic_commit: String,
    ego_commit: String,
    tuic_home: PathBuf,
    tuic_instance: String,
    config_dir: PathBuf,
    endpoint: Url,
    auth_header_file: PathBuf,
    pairing_token: String,
    evidence: PathBuf,
    tuic_sha256: String,
    ego_sha256: String,
    os: &'static str,
    live: Option<LivePrerequisites>,
}
impl Prerequisites {
    fn discover() -> Self {
        let tuic_artifact = required_path("EGO_HOST_TEST_TUIC_ARTIFACT");
        let ego_executable = required_path("EGO_HOST_TEST_EGO_EXECUTABLE");
        for (name, path) in [
            ("EGO_HOST_TEST_TUIC_ARTIFACT", &tuic_artifact),
            ("EGO_HOST_TEST_EGO_EXECUTABLE", &ego_executable),
        ] {
            assert!(
                path.is_file(),
                "{name} must name an exact packaged file: {}",
                path.display()
            );
            assert!(
                path.is_absolute(),
                "{name} must be absolute: {}",
                path.display()
            );
        }
        let tuic_commit = required_commit("EGO_HOST_TEST_TUIC_COMMIT");
        let ego_commit = required_commit("EGO_HOST_TEST_EGO_COMMIT");
        let tuic_home = required_path("EGO_HOST_TEST_TUIC_HOME");
        let tuic_instance = required_named_instance("EGO_HOST_TEST_TUIC_INSTANCE");
        let config_dir = required_path("EGO_HOST_TEST_TUIC_CONFIG_DIR");
        let xdg_config_home = std::env::var_os("EGO_HOST_TEST_XDG_CONFIG_HOME").map(PathBuf::from);
        let appdata =
            (std::env::consts::OS == "windows").then(|| required_path("EGO_HOST_TEST_APPDATA"));
        assert!(
            tuic_artifact
                .components()
                .any(|part| part.as_os_str() == "debug"),
            "use the production debug artifact; release startup probes the native vault"
        );
        let temp_root = fs::canonicalize(std::env::temp_dir()).expect("task temp root exists");
        let fixture_home = fs::canonicalize(&tuic_home).expect("isolated HOME exists");
        assert!(
            fixture_home.starts_with(&temp_root) && fixture_home != temp_root,
            "host HOME must be a dedicated directory below the task TMPDIR"
        );
        for path in xdg_config_home.iter().chain(appdata.iter()) {
            assert!(
                path.is_absolute() && path.starts_with(&tuic_home),
                "startup config roots must be absolute and inside the isolated home"
            );
        }
        let expected_config_dir = resolved_config_dir(
            &tuic_home,
            xdg_config_home.as_deref(),
            appdata.as_deref(),
            &tuic_instance,
        );
        assert!(
            tuic_home.is_dir(),
            "TUICommander isolated home is missing: {}",
            tuic_home.display()
        );
        assert!(
            config_dir.is_dir(),
            "TUICommander config directory is missing: {}",
            config_dir.display()
        );
        assert!(
            config_dir.starts_with(&tuic_home),
            "config directory must be inside the isolated home"
        );
        assert_eq!(
            config_dir, expected_config_dir,
            "config directory does not match the packaged config_dir() path derived from the isolated startup environment"
        );
        assert!(
            fixture_marker_path(&config_dir).is_file(),
            "refusing to mutate a profile without {}",
            fixture_marker_path(&config_dir).display()
        );
        let config_file = config_dir.join("config.json");
        assert!(
            config_file.is_file(),
            "production-format config is missing: {}",
            config_file.display()
        );
        let config: Value = serde_json::from_slice(
            &fs::read(&config_file).expect("cannot read production-format config"),
        )
        .expect("TUICommander config is JSON");
        reject_plaintext_vault_secrets(&config);

        let endpoint = Url::parse(&required_env("EGO_HOST_TEST_ENDPOINT"))
            .unwrap_or_else(|error| panic!("EGO_HOST_TEST_ENDPOINT is not a URL: {error}"));
        assert!(
            matches!(endpoint.scheme(), "http" | "https"),
            "endpoint must be HTTP(S)"
        );
        let host = endpoint.host_str().expect("endpoint has a host");
        assert!(
            matches!(host, "127.0.0.1" | "localhost" | "::1"),
            "must use a loopback host endpoint: {host}"
        );
        assert!(
            endpoint.port_or_known_default().is_some(),
            "endpoint must include a known port"
        );
        assert!(
            endpoint.username().is_empty() && endpoint.password().is_none(),
            "credentials must not be embedded in the endpoint"
        );
        let auth_header_file = required_path("EGO_HOST_TEST_AUTH_HEADER_FILE");
        assert!(
            auth_header_file.is_file(),
            "auth header file is missing: {}",
            auth_header_file.display()
        );
        let pairing_token = validate_pairing_fixture(&auth_header_file);
        let evidence = required_path("EGO_HOST_TEST_EVIDENCE");
        assert!(evidence.is_absolute(), "evidence path must be absolute");
        let live = LivePrerequisites::discover();
        let tuic_sha256 = sha256(&tuic_artifact);
        let ego_sha256 = sha256(&ego_executable);
        eprintln!(
            "host-test prerequisites: tuic_artifact={} tuic_commit={} ego_executable={} ego_commit={} tuic_instance={} config_dir={} endpoint={} os={} workspace=<created-by-test>",
            tuic_artifact.display(),
            tuic_commit,
            ego_executable.display(),
            ego_commit,
            tuic_instance,
            config_dir.display(),
            endpoint,
            std::env::consts::OS
        );

        Self {
            tuic_artifact,
            ego_executable,
            tuic_commit,
            ego_commit,
            tuic_home,
            tuic_instance,
            config_dir,
            endpoint,
            auth_header_file,
            pairing_token,
            evidence,
            tuic_sha256,
            ego_sha256,
            os: std::env::consts::OS,
            live,
        }
    }
    fn endpoint_port(&self) -> u16 {
        self.endpoint
            .port_or_known_default()
            .expect("discovery checked endpoint port")
    }
}

/// The only provider material a live matrix is allowed to inherit. This type
/// intentionally does not implement `Debug`: the credential remains an
/// in-memory `OsString` and never reaches diagnostics or evidence.
struct LivePrerequisites {
    model: OsString,
    credential_env: OsString,
    credential: OsString,
    permission_fixture: bool,
    model_facts: Option<Vec<u8>>,
}

impl LivePrerequisites {
    fn discover() -> Option<Self> {
        let enabled = match std::env::var(LIVE_ENABLE) {
            Ok(value) if value == "1" => true,
            Ok(value) => panic!("{LIVE_ENABLE} must be exactly 1 when supplied, got {value:?}"),
            Err(std::env::VarError::NotPresent) => false,
            Err(std::env::VarError::NotUnicode(_)) => {
                panic!("{LIVE_ENABLE} must be valid Unicode")
            }
        };
        if !enabled {
            assert!(
                std::env::var_os(PERMISSION_LIVE_ENABLE).is_none(),
                "{PERMISSION_LIVE_ENABLE} requires explicit {LIVE_ENABLE}=1"
            );
            return None;
        }

        let model = std::env::var_os("EGO_MODEL")
            .filter(|value| !value.is_empty())
            .expect("live requires non-empty EGO_MODEL");
        let credential_env = required_env(LIVE_CREDENTIAL_ENV);
        let permission_fixture = match std::env::var(PERMISSION_LIVE_ENABLE) {
            Ok(value) if value == "1" => true,
            Err(std::env::VarError::NotPresent) => false,
            _ => panic!("{PERMISSION_LIVE_ENABLE} must be exactly 1 when supplied"),
        };
        let mut live = Self::from_selection(model, &credential_env, permission_fixture, |name| {
            std::env::var_os(name)
        })
        .expect("live requires the selected provider's canonical non-empty credential variable");
        live.model_facts = std::env::var_os("EGO_HOST_TEST_MODELS_FILE")
            .map(|path| fs::read(path).expect("recorded model facts are readable"));
        Some(live)
    }

    fn from_selection(
        model: OsString,
        requested: &str,
        permission_fixture: bool,
        read: impl FnOnce(&str) -> Option<OsString>,
    ) -> Option<Self> {
        let credential_env = canonical_credential_env(model.to_str()?, requested)?;
        let credential = read(&credential_env).filter(|value| !value.is_empty())?;
        Some(Self {
            model,
            credential_env: credential_env.into(),
            credential,
            permission_fixture,
            model_facts: None,
        })
    }
}
fn resolved_config_dir(
    home: &Path,
    xdg_config_home: Option<&Path>,
    appdata: Option<&Path>,
    instance: &str,
) -> PathBuf {
    let base = match std::env::consts::OS {
        "macos" => home.join("Library/Application Support"),
        "windows" => appdata
            .expect("Windows config resolution requires isolated APPDATA")
            .to_path_buf(),
        _ => xdg_config_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config")),
    };
    base.join("com.tuic.commander")
        .join("instances")
        .join(instance)
}

fn platform_config_base(config_dir: &Path) -> &Path {
    config_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("named config directory has a platform config base")
}

fn fixture_marker_path(config_dir: &Path) -> PathBuf {
    config_dir.join(FIXTURE_MARKER)
}

fn reject_plaintext_vault_secrets(config: &Value) {
    for pointer in [
        "/services/auth/session_token",
        "/services/relay/token",
        "/services/push/vapid_private_key",
    ] {
        assert!(
            config
                .pointer(pointer)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty),
            "production config contains a plaintext vault-backed secret"
        );
    }
}

fn validate_pairing_fixture(path: &Path) -> String {
    let contents = fs::read_to_string(path).expect("pairing header is readable");
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());
    let token = lines
        .next()
        .and_then(|line| line.strip_prefix("Cookie: tui-session="))
        .expect("pairing header must use Cookie: tui-session=<token>");
    assert!(
        lines.next().is_none(),
        "pairing fixture contains extra headers"
    );
    assert!(
        token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "pairing token must be a fresh 32-byte hex value"
    );
    token.to_owned()
}
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing external prerequisite {name}"))
}

fn required_named_instance(name: &str) -> String {
    let value = required_env(name);
    assert!(
        is_valid_named_instance(&value),
        "{name} must be a lowercase ASCII DNS label of 1-63 characters other than \"default\""
    );
    value
}

fn is_valid_named_instance(id: &str) -> bool {
    if id.is_empty() || id.len() > 63 || id == "default" {
        return false;
    }
    let bytes = id.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Resolve only static row metadata; this must run before any secret lookup.
fn canonical_credential_env(model: &str, requested: &str) -> Option<String> {
    let slug = ModelSlug::parse(model).ok()?;
    ego_ai::conformance::final_provider_rows(
        ProviderRow::ollama_default(),
        ProviderRow::openai(),
        ProviderRow::anthropic_default(),
        ProviderRow::bedrock_default(),
    )
    .into_iter()
    .find(|row| row.id() == slug.provider)?
    .credential
    .environment_var()
    .filter(|canonical| *canonical == requested)
    .map(str::to_owned)
}

fn required_path(name: &str) -> PathBuf {
    let path = PathBuf::from(required_env(name));
    assert!(
        path.is_absolute(),
        "{name} must be absolute: {}",
        path.display()
    );
    path
}

fn required_commit(name: &str) -> String {
    let value = required_env(name);
    assert!(
        (7..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit()),
        "{name} must be a clean commit hash"
    );
    value
}

fn sha256(path: &Path) -> String {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .unwrap_or_else(|_| panic!("sha256 prerequisite failed for {}", path.display()));
    assert!(
        output.status.success(),
        "sha256 prerequisite failed for {}",
        path.display()
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .filter(|digest| digest.len() == 64 && digest.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("sha256 produced no digest for {}", path.display()))
}

struct ProfileRestore {
    path: PathBuf,
    original: Vec<u8>,
}

impl ProfileRestore {
    fn configure(prerequisites: &Prerequisites) -> Self {
        let path = prerequisites.config_dir.join("config.json");
        let original =
            fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let mut config: Value =
            serde_json::from_slice(&original).expect("TUICommander config is JSON");
        let root = config
            .as_object_mut()
            .expect("TUICommander config is an object");
        root.insert(
            "ego_executable".to_owned(),
            json!(&prerequisites.ego_executable),
        );
        let services = root
            .entry("services")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("services is an object");
        let server = services
            .entry("server")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("services.server is an object");
        server.insert("enabled".to_owned(), json!(true));
        server.insert("port".to_owned(), json!(prerequisites.endpoint_port()));
        let rewritten = serde_json::to_vec_pretty(&config).expect("TUICommander config serializes");
        fs::write(&path, rewritten).unwrap_or_else(|error| {
            panic!("cannot configure isolated TUICommander profile: {error}")
        });
        Self { path, original }
    }
}

impl Drop for ProfileRestore {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.original);
    }
}

struct ProcessGuard {
    child: Child,
}

impl ProcessGuard {
    fn command(prerequisites: &Prerequisites, workspace: &Path, ego_home: &Path) -> Command {
        let mut command = Command::new(&prerequisites.tuic_artifact);
        command
            .arg("--instance")
            .arg(&prerequisites.tuic_instance)
            .args(["--bind", "127.0.0.1", "--no-agent-configs"])
            .env_clear()
            .env("HOME", &prerequisites.tuic_home)
            .env("TMPDIR", workspace.parent().expect("task temporary root"))
            .env("XDG_CACHE_HOME", prerequisites.tuic_home.join(".cache"))
            .env("TUIC_PAIRING_TOKEN", &prerequisites.pairing_token)
            .env("EGO_HOME", ego_home)
            .env("PATH", "/usr/bin:/bin")
            .env("TUIC_PORT", prerequisites.endpoint_port().to_string())
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(live) = &prerequisites.live {
            command
                .env("EGO_MODEL", &live.model)
                .env(&live.credential_env, &live.credential);
        }
        let platform_config = platform_config_base(&prerequisites.config_dir);
        match std::env::consts::OS {
            "windows" => {
                command.env("APPDATA", platform_config);
            }
            "linux" => {
                command.env("XDG_CONFIG_HOME", platform_config);
            }
            _ => {}
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
    }

    fn start(prerequisites: &Prerequisites, workspace: &Path, ego_home: &Path) -> Self {
        let child = Self::command(prerequisites, workspace, ego_home)
            .spawn()
            .unwrap_or_else(|error| panic!("cannot launch packaged TUICommander: {error}"));
        Self { child }
    }

    fn wait_for_endpoint(&mut self, prerequisites: &Prerequisites) {
        let host = prerequisites.endpoint.host_str().expect("endpoint host");
        let address = if host.contains(':') {
            format!("[{host}]:{}", prerequisites.endpoint_port())
        } else {
            format!("{host}:{}", prerequisites.endpoint_port())
        };
        for _ in 0..150 {
            if TcpStream::connect(&address).is_ok() {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "packaged TUICommander exited before its production endpoint was ready: {status}"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "packaged TUICommander did not expose {} within 15 seconds",
            prerequisites.endpoint
        );
    }

    fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(unix)]
        {
            let group = format!("-{}", self.child.id());
            let _ = Command::new("/bin/kill").args(["-TERM", &group]).status();
            std::thread::sleep(Duration::from_millis(100));
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = Command::new("/bin/kill").args(["-KILL", &group]).status();
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owns the one fresh ego user-layer directory the packaged host may pass to
/// its configured child. It lives below the matrix `TempDir`, so dropping the
/// task workspace removes no account-owned profile or session store.
struct EgoHomeFixture {
    path: PathBuf,
}

impl EgoHomeFixture {
    fn claim(workspace: &Path, live: Option<&LivePrerequisites>) -> Self {
        let path = workspace.join("ego-home");
        assert!(
            !path.exists(),
            "refusing to reuse a pre-existing ego home: {}",
            path.display()
        );
        fs::create_dir(&path).unwrap_or_else(|error| {
            panic!(
                "cannot create isolated ego home {}: {error}",
                path.display()
            )
        });
        fs::write(
            fixture_marker_path(&path),
            b"host-test task-owned ego home\n",
        )
        .unwrap_or_else(|error| {
            panic!("cannot mark isolated ego home {}: {error}", path.display())
        });
        if live.is_some() {
            fs::write(
                path.join("config.toml"),
                format!(
                    "budget = {LIVE_BUDGET:?}\nsandbox = {:?}\nmode = \"default\"\n",
                    if live.is_some_and(|live| live.permission_fixture) {
                        ego_core::permit::Confinement::Workspace.as_str()
                    } else {
                        ego_core::permit::Confinement::Ro.as_str()
                    }
                ),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "cannot write bounded live config {}: {error}",
                    path.display()
                )
            });
        }
        if let Some(facts) = live.and_then(|live| live.model_facts.as_ref()) {
            fs::write(path.join("models.toml"), facts)
                .expect("write recorded model facts into the task-owned ego home");
        }
        Self { path }
    }
}

struct HostMatrix {
    endpoint: Url,
    auth_header_file: PathBuf,
    profile: ProfileRestore,
    _ego_home: EgoHomeFixture,
    process: ProcessGuard,
}

impl HostMatrix {
    fn launch(prerequisites: &Prerequisites, workspace: &Path) -> Self {
        let profile = ProfileRestore::configure(prerequisites);
        let ego_home = EgoHomeFixture::claim(workspace, prerequisites.live.as_ref());
        let mut process = ProcessGuard::start(prerequisites, workspace, &ego_home.path);
        process.wait_for_endpoint(prerequisites);
        Self {
            endpoint: prerequisites.endpoint.clone(),
            auth_header_file: prerequisites.auth_header_file.clone(),
            profile,
            _ego_home: ego_home,
            process,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.endpoint.as_str().trim_end_matches('/'), path)
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> HttpResponse {
        let mut command = Command::new("curl");
        command.args([
            "--silent",
            "--show-error",
            "--connect-timeout",
            "3",
            "--max-time",
            "30",
            "-X",
            method,
            "-H",
            "content-type: application/json",
        ]);
        command
            .arg("-H")
            .arg(format!("@{}", self.auth_header_file.display()));
        if let Some(body) = body {
            command
                .arg("--data")
                .arg(serde_json::to_string(body).expect("request JSON"));
        }
        command
            .arg("--write-out")
            .arg(format!("\n{STATUS_MARKER}%{{http_code}}"));
        command.arg(self.url(path));
        let output = command
            .output()
            .expect("curl is required for the HTTP API driver");
        let raw = String::from_utf8_lossy(&output.stdout);
        let (raw_body, raw_status) = raw.rsplit_once(STATUS_MARKER).unwrap_or(("", "000"));
        let status = raw_status.trim().parse::<u16>().unwrap_or(0);
        let value = if raw_body.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(raw_body.trim()).unwrap_or_else(|_| json!({"unparseable": true}))
        };
        assert!(
            output.status.success() || status != 0,
            "curl could not reach the TUICommander endpoint for {method} {path}"
        );
        HttpResponse { status, value }
    }

    fn get(&self, path: &str) -> HttpResponse {
        self.request("GET", path, None)
    }
    fn post(&self, path: &str, body: &Value) -> HttpResponse {
        self.request("POST", path, Some(body))
    }
    fn delete(&self, path: &str) -> HttpResponse {
        self.request("DELETE", path, None)
    }

    async fn subscribe_stream(&self, connection_id: &str, after: u64) -> StreamCollector {
        let path = format!("/acp/connections/{connection_id}/stream?after={after}");
        let mut websocket_url = Url::parse(&self.url(&path)).expect("stream URL is valid");
        let websocket_scheme = match websocket_url.scheme() {
            "http" => "ws",
            "https" => "wss",
            _ => panic!("stream endpoint must use HTTP(S) before WebSocket conversion"),
        };
        websocket_url
            .set_scheme(websocket_scheme)
            .expect("HTTP(S) endpoint converts to WebSocket scheme");
        let mut request = websocket_url
            .as_str()
            .into_client_request()
            .expect("stream URL is a WebSocket client request");
        add_auth_header(&mut request, &self.auth_header_file);
        let (socket, response) = async_tungstenite::tokio::connect_async(request)
            .await
            .expect("production ACP stream WebSocket connects");
        assert_eq!(
            response.status().as_u16(),
            101,
            "production ACP stream did not upgrade"
        );
        StreamCollector {
            socket,
            last_sequence: None,
        }
    }

    async fn observe_stream_head(&self, connection_id: &str, after: u64) -> Vec<Value> {
        let mut stream = self.subscribe_stream(connection_id, after).await;
        vec![
            tokio::time::timeout(Duration::from_secs(15), stream.next_event())
                .await
                .expect("host ready event did not arrive within 15 seconds"),
        ]
    }
}

/// A persistent host journal subscription. The matrix must subscribe before a
/// prompt and consume through that prompt's matching settlement: reconnecting
/// for individual frames could hide an ordering gap or another turn's events.
struct StreamCollector {
    socket: async_tungstenite::WebSocketStream<async_tungstenite::tokio::ConnectStream>,
    last_sequence: Option<u64>,
}

impl StreamCollector {
    async fn next_event(&mut self) -> Value {
        for _ in 0..4 {
            // The caller owns the whole-operation deadline. A quiet provider
            // interval is not a broken WebSocket or a five-second model SLA.
            let message = self
                .socket
                .next()
                .await
                .expect("ACP stream closed before a frame")
                .expect("ACP stream frame error");
            match message {
                Message::Text(text) => {
                    let frame: Value =
                        serde_json::from_str(text.as_ref()).expect("ACP stream frame is JSON");
                    return self.record(frame);
                }
                Message::Binary(bytes) => {
                    let frame: Value =
                        serde_json::from_slice(&bytes).expect("ACP binary stream frame is JSON");
                    return self.record(frame);
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => panic!("ACP stream closed before a frame"),
                _ => {}
            }
        }
        panic!("ACP stream produced no event frame")
    }

    fn record(&mut self, frame: Value) -> Value {
        assert_eq!(
            frame["kind"], "event",
            "ACP stream ended with a non-event frame"
        );
        let sequence = frame["sequence"]
            .as_u64()
            .expect("ACP event frame omitted its sequence");
        if let Some(previous) = self.last_sequence {
            assert!(
                sequence > previous,
                "ACP stream sequence did not increase: {previous} then {sequence}"
            );
        }
        self.last_sequence = Some(sequence);
        frame
    }
}

const MATRIX_ROWS: &[&str] = &[
    "connect",
    "initialize_capability_snapshot",
    "unsupported_rows",
    "ordered_host_stream",
    "session_new",
    "session_list",
    "permission_surface",
    "permission_response",
    "model_config",
    "prompt_cancel_validation",
    "ordered_message_updates",
    "ordered_reasoning_updates",
    "ordered_tool_call_updates",
    "ordered_tool_progress_updates",
    "usage_cost_frame_shape",
    "namespaced_pause_resume",
    "session_fork_delete",
    "session_load_resume_close",
    "reconnect",
];

fn initial_rows() -> Vec<Value> {
    MATRIX_ROWS
        .iter()
        .map(|operation| {
            json!({
                "operation": operation, "status": "unproven",
                "facts": {"reasonCode": "not-yet-observed"}
            })
        })
        .collect()
}

fn matches_turn(frame: &Value, connection: &str, session: &str, turn: &str) -> bool {
    frame["kind"] == "event"
        && frame["connectionId"] == connection
        && frame["sessionId"] == session
        && frame["turnId"] == turn
        && frame["generation"].as_u64().is_some()
        && frame["sequence"].as_u64().is_some()
}

fn session_update(event: &Value) -> Option<v1::SessionUpdate> {
    if event["kind"] != "sessionUpdate" {
        return None;
    }
    serde_json::from_value(event.get("update")?.clone()).ok()
}

/// Read-only authorization never grants. The SDK option kind selects the
/// actual offered rejection ID; no guessed ID is sent to the host.
fn refusal_response(event: &Value, session: &str) -> Option<(String, Value)> {
    if event["kind"] != "permissionRequested" {
        return None;
    }
    let id = event["requestId"].as_str()?;
    uuid::Uuid::parse_str(id).ok()?;
    let request: v1::RequestPermissionRequest =
        serde_json::from_value(event["request"].clone()).ok()?;
    if request.session_id.to_string() != session {
        return None;
    }
    let option = request
        .options
        .iter()
        .find(|option| option.kind == v1::PermissionOptionKind::RejectOnce)?;
    let response = v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Selected(
        v1::SelectedPermissionOutcome::new(option.option_id.clone()),
    ));
    Some((
        id.to_owned(),
        serde_json::to_value(response).expect("SDK response serializes"),
    ))
}

/// Compare the producer's canonical operation rendering and the original
/// offered call. No permission title parser or prompt-based authority exists.
fn fixture_permission_response(
    prior: &[Value],
    frame: &Value,
    connection: &str,
    session: &str,
    turn: &str,
    target: &Path,
) -> Option<(String, Value)> {
    if !matches_turn(frame, connection, session, turn) {
        return None;
    }
    let event = &frame["event"];
    let (request_id, _) = refusal_response(event, session)?;
    let request: v1::RequestPermissionRequest =
        serde_json::from_value(event["request"].clone()).ok()?;
    let calls: Vec<_> = prior
        .iter()
        .filter(|entry| {
            matches_turn(entry, connection, session, turn)
                && entry["generation"] == frame["generation"]
                && entry["sequence"].as_u64() < frame["sequence"].as_u64()
        })
        .filter_map(|entry| session_update(&entry["event"]))
        .filter_map(|update| match update {
            v1::SessionUpdate::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    eprintln!("host-test permission prior tool count={}", calls.len());
    for call in &calls {
        eprintln!(
            "host-test permission call pending={} id={} title={} path={} contents={} exact={}",
            call.status == v1::ToolCallStatus::Pending,
            call.tool_call_id == request.tool_call.tool_call_id,
            call.title == format!("write {}", target.display()),
            call.raw_input
                .as_ref()
                .is_some_and(|raw| raw["files"][0]["path"] == json!(target)),
            call.raw_input
                .as_ref()
                .is_some_and(|raw| raw["files"][0]["contents"] == PERMISSION_CONTENTS),
            call.raw_input.as_ref()
                == Some(&json!({"files": [{"path": target, "contents": PERMISSION_CONTENTS}]}))
        );
    }
    if prior
        .iter()
        .filter(|entry| matches_turn(entry, connection, session, turn))
        .any(|entry| {
            matches!(
                entry["event"]["kind"].as_str(),
                Some("permissionRequested" | "turnSettled")
            ) || matches!(
                session_update(&entry["event"]),
                Some(v1::SessionUpdate::ToolCallUpdate(ref update)) if matches!(update.fields.status, Some(v1::ToolCallStatus::Completed | v1::ToolCallStatus::Failed))
            )
        })
    {
        return None;
    }
    if !prior.iter().any(|entry| {
        matches_turn(entry, connection, session, turn)
            && entry["generation"] == frame["generation"]
            && entry["event"]["kind"] == "turnStarted"
    }) {
        return None;
    }
    let [call] = calls.as_slice() else {
        return None;
    };
    if call.status != v1::ToolCallStatus::Pending
        || call.tool_call_id != request.tool_call.tool_call_id
        || call.title != format!("write {}", target.display())
        || call.raw_input.as_ref()
            != Some(&json!({"files": [{"path": target, "contents": PERMISSION_CONTENTS}]}))
    {
        return None;
    }
    let option = request
        .options
        .iter()
        .find(|option| option.kind == v1::PermissionOptionKind::AllowOnce)?;
    // Only the canonical ego option can grant in the pinned adapter.
    if option.option_id.to_string() != ego_acp::permission::AcpPermissionAdapter::ALLOW_ONCE {
        return None;
    }
    let response = v1::RequestPermissionResponse::new(v1::RequestPermissionOutcome::Selected(
        v1::SelectedPermissionOutcome::new(option.option_id.clone()),
    ));
    Some((request_id, serde_json::to_value(response).ok()?))
}

fn permission_roundtrip(
    frames: &[Value],
    connection: &str,
    session: &str,
    turn: &str,
    request_id: &str,
    response: &Value,
) -> bool {
    let mut requested_at = None;
    let mut settled_at = None;
    let mut tool_id = None;
    let mut tool_calls = 0;
    let mut completed = false;
    for frame in frames
        .iter()
        .filter(|frame| matches_turn(frame, connection, session, turn))
    {
        let event = &frame["event"];
        if event["requestId"] == request_id {
            if event["kind"] == "permissionRequested" {
                requested_at = frame["sequence"].as_u64();
                tool_id = event["request"]["toolCall"]["toolCallId"].as_str();
            }
            if event["kind"] == "permissionSettled"
                && requested_at.is_some_and(|sequence| {
                    frame["sequence"]
                        .as_u64()
                        .is_some_and(|settled| settled > sequence)
                })
                && event["outcome"] == response["outcome"]
            {
                settled_at = frame["sequence"].as_u64();
            }
        }
        match session_update(event) {
            Some(v1::SessionUpdate::ToolCall(_)) => tool_calls += 1,
            Some(v1::SessionUpdate::ToolCallUpdate(update))
                if tool_id == Some(update.tool_call_id.to_string().as_str())
                    && update.fields.status == Some(v1::ToolCallStatus::Completed)
                    && update
                        .fields
                        .content
                        .as_ref()
                        .is_some_and(|content| !content.is_empty())
                    && settled_at.is_some_and(|sequence| {
                        frame["sequence"]
                            .as_u64()
                            .is_some_and(|done| done > sequence)
                    }) =>
            {
                completed = true
            }
            _ => {}
        }
    }
    completed && tool_calls == 1
}

/// Assess one complete host turn, retaining only numeric facts and identifiers.
/// Missing optional observations never promote the corresponding matrix row.
fn observed_live_rows(
    frames: &[Value],
    connection: &str,
    session: &str,
    turn: &str,
) -> Result<Vec<(&'static str, Value)>, &'static str> {
    let mut started = false;
    let mut settled = false;
    let mut generation = None;
    let mut sequence = None;
    let mut tools = BTreeMap::new();
    let mut messages = 0;
    let mut reasoning = 0;
    let mut usage = Vec::new();
    let mut last_cost: Option<(f64, String)> = None;
    let mut cost_observations = 0;
    for frame in frames
        .iter()
        .filter(|frame| matches_turn(frame, connection, session, turn))
    {
        let current_generation = frame["generation"].as_u64().ok_or("missing-generation")?;
        let current_sequence = frame["sequence"].as_u64().ok_or("missing-sequence")?;
        if generation.is_some_and(|previous| previous != current_generation)
            || sequence.is_some_and(|previous| previous >= current_sequence)
        {
            return Err("journal-order-or-generation-mismatch");
        }
        generation = Some(current_generation);
        sequence = Some(current_sequence);
        if settled {
            return Err("events-after-turn-settlement");
        }
        let event = &frame["event"];
        match event["kind"].as_str() {
            Some("promptSent") if !started => {}
            Some("turnStarted") if !started => started = true,
            Some("turnStarted") => return Err("duplicate-turn-start"),
            Some("turnSettled") if started => {
                let stop: v1::StopReason = serde_json::from_value(event["stopReason"].clone())
                    .map_err(|_| "invalid-turn-stop-reason")?;
                if stop != v1::StopReason::EndTurn {
                    return Err("turn-did-not-end-normally");
                }
                settled = true;
            }
            Some("sessionUpdate") if started => {
                match session_update(event).ok_or("invalid-sdk-session-update")? {
                    v1::SessionUpdate::AgentMessageChunk(chunk) => {
                        if matches!(chunk.content, v1::ContentBlock::Text(ref text) if !text.text.is_empty())
                        {
                            messages += 1;
                        }
                    }
                    v1::SessionUpdate::AgentThoughtChunk(chunk) => {
                        if matches!(chunk.content, v1::ContentBlock::Text(ref text) if !text.text.is_empty())
                        {
                            reasoning += 1;
                        }
                    }
                    v1::SessionUpdate::ToolCall(call) => {
                        if call.status != v1::ToolCallStatus::Pending
                            || call.tool_call_id.to_string().is_empty()
                            || tools.insert(call.tool_call_id.to_string(), false).is_some()
                        {
                            return Err("duplicate-or-empty-tool-call");
                        }
                    }
                    v1::SessionUpdate::ToolCallUpdate(update) => {
                        let completed = tools
                            .get_mut(&update.tool_call_id.to_string())
                            .ok_or("update-without-matching-tool-call")?;
                        if *completed {
                            return Err("update-after-tool-completion");
                        }
                        match update.fields.status {
                            Some(v1::ToolCallStatus::Completed) => {
                                if update.fields.content.as_ref().is_none_or(Vec::is_empty) {
                                    return Err("terminal-tool-omitted-final-content");
                                }
                                *completed = true;
                            }
                            Some(v1::ToolCallStatus::Failed) => {
                                return Err("live-read-tool-failed");
                            }
                            _ => {}
                        }
                    }
                    v1::SessionUpdate::UsageUpdate(update) => {
                        if update.size == 0 {
                            return Err("invalid-context-window-size");
                        }
                        let mut facts = json!({"sequence": current_sequence, "used": update.used, "size": update.size});
                        if let Some(cost) = update.cost {
                            if !cost.amount.is_finite()
                                || cost.amount < 0.0
                                || ego_core::cost::CurrencyCode::new(&cost.currency).is_err()
                            {
                                return Err("invalid-cumulative-cost");
                            }
                            if last_cost.as_ref().is_some_and(|(amount, currency)| {
                                cost.amount < *amount || cost.currency != *currency
                            }) {
                                return Err("cost-decreased-or-currency-changed");
                            }
                            last_cost = Some((cost.amount, cost.currency.clone()));
                            facts["cost"] =
                                json!({"amount": cost.amount, "currency": cost.currency});
                            cost_observations += 1;
                        } else if event["update"]
                            .get("cost")
                            .is_some_and(|cost| !cost.is_null())
                        {
                            return Err("malformed-cost-was-dropped-by-sdk");
                        }
                        usage.push(facts);
                    }
                    _ => {}
                }
            }

            _ if !started => return Err("event-before-turn-start"),
            _ => {}
        }
    }
    if !started || !settled {
        return Err("missing-correlated-turn-lifecycle");
    }
    if tools.values().any(|completed| !completed) {
        return Err("tool-not-terminal");
    }
    let mut rows = Vec::new();
    for (operation, count) in [
        ("ordered_message_updates", messages),
        ("ordered_reasoning_updates", reasoning),
        ("ordered_tool_call_updates", tools.len()),
        ("ordered_tool_progress_updates", tools.len()),
    ] {
        if count > 0 {
            rows.push((operation, json!({"turnId": turn, "count": count, "fidelity": "acp-v1-start-terminal-final-content"})));
        }
    }
    if cost_observations >= 2
        && usage
            .last()
            .is_some_and(|facts| facts.get("cost").is_some())
    {
        rows.push((
            "usage_cost_frame_shape",
            json!({"turnId": turn, "observations": usage}),
        ));
    }
    Ok(rows)
}

/// Drive the read-only scenario or the separately authorized exact fixture
/// write. Workspace reads do not ask a human under production policy.
async fn prove_live_provider_turn(
    host: &HostMatrix,
    connection_id: &str,
    session_id: &str,
    workspace: &Path,
    evidence: &mut Evidence,
    permission_fixture: bool,
) {
    let fixture = workspace.join("host-test-live-tool-input.txt");
    fs::write(&fixture, "permits this file to be read only.\n")
        .expect("cannot create live tool input fixture");
    evidence.row(
        "permission_response",
        "unproven",
        json!({"reasonCode": "workspace-read-autoapproved-outside-read-refused-before-human"}),
    );
    let fixture = fs::canonicalize(fixture).expect("task fixture has a canonical target");
    let prompt = if permission_fixture {
        format!(
            "Call the write tool exactly once with this JSON: {{\"files\":[{{\"path\":{},\"contents\":{}}}]}}. The field name is contents, plural. Do not use any other tool or target. Then reply with one word.",
            serde_json::to_string(&fixture).expect("fixture path"),
            serde_json::to_string(PERMISSION_CONTENTS).expect("fixture contents")
        )
    } else {
        "Use an available workspace reading tool to read only host-test-live-tool-input.txt. Do not write, edit, or run any other command. Then reply with one word.".to_owned()
    };
    let snapshot = host.get(&format!("/acp/connections/{connection_id}"));
    let snapshot = assert_ok(&snapshot, "pre-prompt connection snapshot");
    assert_eq!(snapshot["connectionId"], connection_id);
    let generation = snapshot["generation"]
        .as_u64()
        .expect("connection snapshot omitted generation");
    let mut stream = host.subscribe_stream(connection_id, 0).await;
    let prompted = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/prompt"),
        &json!({"prompt": [{"type": "text", "text": prompt}]}),
    );
    let prompted = assert_ok(&prompted, "live prompt");
    let turn_id = prompt_turn_id(prompted);
    let (frames, granted) = tokio::time::timeout(Duration::from_secs(90), async {
        let mut frames = Vec::new();
        let mut granted = None;
        let mut rejected = false;
        for _ in 0..512 {
            let frame = stream.next_event().await;
            let correlated = matches_turn(&frame, connection_id, session_id, &turn_id);
            eprintln!(
                "host-test frame sequence={} correlated={} event={} update={}",
                frame["sequence"],
                correlated,
                frame["event"]["kind"].as_str().unwrap_or("unknown"),
                frame["event"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("none")
            );
            if let Some(v1::SessionUpdate::ConfigOptionUpdate(_)) = session_update(&frame["event"])
            {
                let fixture_path = evidence.path.with_extension("host-config.json");
                fs::write(fixture_path, safe_json(&frame))
                    .expect("record sanitized real-host config update");
            }
            if correlated && frame["event"]["kind"] == "turnFailed" {
                let message = frame["event"]["message"].as_str().unwrap_or("");
                let categories: Vec<_> = [
                    "model",
                    "capabilities",
                    "pricing",
                    "budget",
                    "ceiling",
                    "credential",
                    "permission",
                    "authentication",
                    "HTTP",
                    "401",
                    "403",
                    "400",
                    "reasoning",
                    "context",
                    "transport",
                    "sandbox",
                    "network",
                    "refused",
                    "configuration",
                    "unknown",
                    "unselected",
                    "tools",
                    "max_output",
                    "stream",
                    "failed",
                    "schema",
                    "environment",
                ]
                .into_iter()
                .filter(|word| message.contains(word))
                .collect();
                eprintln!("host-test failure diagnostic categories: {categories:?}");
                // A bounded vocabulary identifies admission failures without
                // forwarding provider diagnostics, paths, or user content.
                let vocabulary = [
                    "sandbox",
                    "refused",
                    "measurement",
                    "probe",
                    "spawn",
                    "permission",
                    "denied",
                    "Operation",
                    "permitted",
                    "No",
                    "such",
                    "file",
                    "directory",
                    "network",
                    "connection",
                    "Connection",
                    "read",
                    "write",
                    "exec",
                    "loopback",
                    "Seatbelt",
                    "workspace",
                    "unavailable",
                    "default",
                    "profile",
                    "named",
                    "unknown",
                    "capabilities",
                    "root",
                    "EGO_HOME",
                    "config",
                    "TMPDIR",
                    "cannot",
                    "failed",
                    "backend",
                    "process",
                    "host",
                    "port",
                    "os",
                    "error",
                ];
                let diagnostic: Vec<_> = message
                    .split_whitespace()
                    .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
                    .filter(|word| vocabulary.contains(word))
                    .collect();
                eprintln!("host-test admission diagnostic words: {diagnostic:?}");
                // Alphabetic words only: keys, paths and ids contain digits or
                // separators and never pass this filter.
                let prose: Vec<_> = message
                    .split_whitespace()
                    .filter(|word| {
                        word.len() <= 24
                            && word
                                .chars()
                                .all(|c| c.is_ascii_alphabetic() || ".,:;()".contains(c))
                    })
                    .take(60)
                    .collect();
                eprintln!("host-test failed turn sanitized prose: {prose:?}");
                keep_failed_turn_journal(&host._ego_home.path, &evidence.path);
                panic!("the live provider turn failed; the matrix cannot continue past it");
            }
            if correlated {
                assert_eq!(
                    frame["generation"].as_u64(),
                    Some(generation),
                    "accepted turn crossed the pre-request connection generation"
                );
            }
            if correlated && frame["event"]["kind"] == "permissionRequested" {
                let allowed = if permission_fixture && granted.is_none() {
                    fixture_permission_response(
                        &frames,
                        &frame,
                        connection_id,
                        session_id,
                        &turn_id,
                        &fixture,
                    )
                } else {
                    None
                };
                let (request_id, response) = if let Some(allowed) = allowed {
                    granted = Some((allowed.0.clone(), allowed.1.clone()));
                    allowed
                } else {
                    rejected = true;
                    refusal_response(&frame["event"], session_id).expect(
                        "permission request cannot be safely answered with an offered rejection",
                    )
                };
                let answered = host.post(
                    &format!("/acp/connections/{connection_id}/permissions/{request_id}/response"),
                    &response,
                );
                assert_ok(&answered, "answer exact fixture permission");
            }
            let settled = correlated && frame["event"]["kind"] == "turnSettled";
            frames.push(frame);
            if settled {
                assert!(
                    !rejected,
                    "unexpected permission was refused; fixture scenario failed"
                );
                return (frames, granted);
            }
        }
        panic!("live provider turn exceeded 512 ordered stream frames")
    })
    .await
    .expect("live provider turn did not settle within 90 seconds");
    let mut event_counts = BTreeMap::new();
    for frame in &frames {
        if matches_turn(frame, connection_id, session_id, &turn_id) {
            let kind = frame["event"]["kind"].as_str().unwrap_or("unknown");
            *event_counts.entry(kind).or_insert(0_u64) += 1;
        }
    }
    eprintln!("host-test correlated host event counts: {event_counts:?}");
    let observed = match observed_live_rows(&frames, connection_id, session_id, &turn_id) {
        Ok(observed) => observed,
        Err(reason) => {
            for operation in [
                "permission_response",
                "ordered_message_updates",
                "ordered_reasoning_updates",
                "ordered_tool_call_updates",
                "ordered_tool_progress_updates",
                "usage_cost_frame_shape",
            ] {
                evidence.row(
                    operation,
                    "unproven",
                    json!({"reasonCode": reason, "turnId": turn_id}),
                );
            }
            return;
        }
    };
    for (operation, facts) in observed {
        evidence.row(operation, "proven", facts);
    }
    if permission_fixture {
        let Some((request_id, response)) = granted else {
            evidence.row(
                "permission_response",
                "unproven",
                json!({"reasonCode": "exact-fixture-permission-not-requested"}),
            );
            return;
        };
        assert!(
            permission_roundtrip(
                &frames,
                connection_id,
                session_id,
                &turn_id,
                &request_id,
                &response
            ),
            "host did not settle the exact permission response"
        );
        assert_eq!(
            fs::read(&fixture).expect("task-owned output"),
            PERMISSION_CONTENTS.as_bytes(),
            "authorized fixture contents did not reach disk"
        );
        evidence.row(
            "permission_response",
            "proven",
            json!({"turnId": turn_id, "requestId": request_id}),
        );
    }
}

impl Drop for HostMatrix {
    fn drop(&mut self) {
        self.process.stop();
        let _ = &self.profile;
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    value: Value,
}

fn add_auth_header(request: &mut Request<()>, path: &Path) {
    let contents = fs::read_to_string(path).expect("auth header file is readable");
    let line = contents
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("auth header file contains a pairing cookie");
    let (_, value) = line
        .split_once(':')
        .expect("auth header file uses Header: value lines");
    request.headers_mut().insert(
        "cookie",
        HeaderValue::from_str(value.trim()).expect("pairing cookie is valid HTTP"),
    );
}

fn assert_ok<'a>(response: &'a HttpResponse, operation: &str) -> &'a Value {
    assert_eq!(
        response.status,
        200,
        "{operation} did not answer 200: {}",
        safe_json(&response.value)
    );
    &response.value
}

fn prompt_turn_id(value: &Value) -> String {
    // The production host serializes its transparent AcpTurnId UUID directly.
    let id = value
        .as_str()
        .expect("prompt acceptance must be a UUID scalar");
    assert!(
        uuid::Uuid::parse_str(id).is_ok(),
        "prompt acceptance must be a valid UUID"
    );
    id.to_owned()
}

fn string_field(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "response omitted string field {field}: {}",
                safe_json(value)
            )
        })
        .to_owned()
}

fn authority(root: &Path) -> Value {
    json!({"authority": {"cwd": root.display().to_string(), "additionalDirectories": []}})
}

fn safe_json(value: &Value) -> String {
    serde_json::to_string(&sanitize(value, None)).expect("sanitized JSON")
}

fn is_numeric_usage_key(key: &str) -> bool {
    matches!(
        key,
        "inputTokens"
            | "outputTokens"
            | "cost"
            | "cachedInputTokens"
            | "cacheReadInputTokens"
            | "cacheWriteInputTokens"
            | "totalTokens"
    )
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "authorization",
        "token",
        "password",
        "secret",
        "credential",
        "apikey",
        "api_key",
        "prompt",
        "message",
        "reasoning",
        "content",
        "arguments",
        "toolresult",
        "tool_result",
        "result",
        "output",
        "path",
        "cwd",
        "root",
        "directory",
        "workspace",
        "worktree",
        "command",
        "diagnostic",
    ]
    .iter()
    .any(|part| key.contains(part))
}

fn safe_string_key(key: &str) -> bool {
    matches!(
        key,
        "schemaVersion"
            | "story"
            | "status"
            | "operation"
            | "row"
            | "kind"
            | "state"
            | "code"
            | "protocol"
            | "protocolVersion"
            | "generation"
            | "sequence"
            | "version"
            | "mcpStdio"
            | "clientBooleanConfig"
            | "providers"
            | "sessionSetMode"
            | "holdAvailability"
            | "compactAvailability"
            | "outcome"
            | "stopReason"
            | "tuicCommit"
            | "egoCommit"
            | "tuicArtifactSha256"
            | "tuicInstance"
            | "egoSha256"
            | "endpoint"
            | "httpStatus"
            | "requestId"
            | "connectionId"
            | "sessionId"
            | "turnId"
            | "configId"
            | "value"
            | "valueId"
            | "os"
            | "arch"
            | "reasonCode"
            | "currency"
            | "fidelity"
    )
}

fn sanitize(value: &Value, parent_key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => {
            let mut sanitized = Map::new();
            for (key, value) in object {
                if is_sensitive_key(key)
                    && !((is_numeric_usage_key(key) || key == "promptStatus") && value.is_number())
                    && !(matches!(
                        key.as_str(),
                        "promptImage" | "promptAudio" | "promptEmbeddedContext" | "forkAtMessage"
                    ) && value.is_boolean())
                {
                    sanitized.insert(key.clone(), Value::String("<redacted>".to_owned()));
                } else {
                    sanitized.insert(key.clone(), sanitize(value, Some(key)));
                }
            }
            Value::Object(sanitized)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize(value, parent_key))
                .collect(),
        ),
        Value::String(value) if parent_key.is_some_and(safe_string_key) => {
            Value::String(value.clone())
        }
        Value::String(_) => Value::String("<redacted>".to_owned()),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn assert_sanitized(serialized: &str, workspace: &Path, forbidden: &[&str]) {
    let workspace = workspace.to_string_lossy();
    assert!(
        !serialized.contains(workspace.as_ref()),
        "sanitized evidence contains the workspace path"
    );
    for marker in forbidden {
        assert!(
            !serialized.contains(marker),
            "sanitized evidence contains forbidden marker {marker}"
        );
    }
}

struct Evidence {
    path: PathBuf,
    workspace: PathBuf,
    prerequisites: Value,
    rows: Vec<Value>,
}

impl Evidence {
    fn new(prerequisites: &Prerequisites, workspace: &Path) -> Self {
        Self {
            path: prerequisites.evidence.clone(),
            workspace: workspace.to_path_buf(),
            prerequisites: json!({
                "tuicArtifactPath": prerequisites.tuic_artifact.display().to_string(),
                "egoExecutablePath": prerequisites.ego_executable.display().to_string(),
                "tuicCommit": &prerequisites.tuic_commit,
                "egoCommit": &prerequisites.ego_commit,
                "tuicArtifactSha256": &prerequisites.tuic_sha256,
                "egoSha256": &prerequisites.ego_sha256,
                "tuicInstance": &prerequisites.tuic_instance,
                "configLocation": prerequisites.config_dir.display().to_string(),
                "endpoint": prerequisites.endpoint.as_str(),
                "os": prerequisites.os,
                "arch": std::env::consts::ARCH,
                "workspaceRoot": workspace.display().to_string(),
                "billable": prerequisites.live.is_some(),
                "permissionFixture": prerequisites.live.as_ref().is_some_and(|live| live.permission_fixture),
            }),
            rows: initial_rows(),
        }
    }

    fn row(&mut self, operation: &str, status: &str, facts: Value) {
        let row = self
            .rows
            .iter_mut()
            .find(|row| row["operation"] == operation)
            .expect("operation must belong to the complete matrix inventory");
        *row = json!({"operation": operation, "status": status, "facts": facts});
    }

    fn document(&self) -> Value {
        json!({"schemaVersion": 1, "story": "regression", "prerequisites": &self.prerequisites, "rows": &self.rows})
    }

    fn write(&self) {
        let serialized = serde_json::to_string_pretty(&sanitize(&self.document(), None))
            .expect("sanitized evidence JSON");
        assert_sanitized(
            &serialized,
            &self.workspace,
            &["HOST-TEST-PROMPT", "HOST-TEST-SECRET", "HOST-TEST-TOOL-ARG"],
        );
        fs::write(&self.path, format!("{serialized}\n")).unwrap_or_else(|error| {
            panic!("cannot publish evidence {}: {error}", self.path.display())
        });
    }
}

/// An idle HTTP refusal does not prove cancellation of an accepted turn.
async fn prove_active_cancel(
    host: &HostMatrix,
    connection: &str,
    session: &str,
    evidence: &mut Evidence,
) {
    let mut stream = host.subscribe_stream(connection, 0).await;
    let accepted = host.post(
        &format!("/acp/connections/{connection}/sessions/{session}/prompt"),
        &json!({"prompt": [{"type": "text", "text": "Reply with one word."}]}),
    );
    let turn = prompt_turn_id(assert_ok(&accepted, "cancel fixture prompt"));
    let cancelled = host.post(
        &format!("/acp/connections/{connection}/sessions/{session}/cancel"),
        &json!({}),
    );
    assert_ok(&cancelled, "cancel accepted turn");
    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        for _ in 0..512 {
            let frame = stream.next_event().await;
            if matches_turn(&frame, connection, session, &turn) {
                if frame["event"]["kind"] == "turnFailed" {
                    let vocabulary = ["cancelled", "cancel", "lease", "running", "settlement", "admitted", "receipt", "unknown", "session", "turn", "closed", "busy", "not", "already", "store", "error", "internal", "failed", "run", "has", "a", "without", "preparing", "outcome", "uncertain", "deadline"];
                    let diagnostic: Vec<_> = frame["event"]["message"].as_str().unwrap_or("").split_whitespace().map(|word| word.trim_matches(|c: char| !c.is_alphanumeric())).filter(|word| vocabulary.contains(word)).collect();
                    evidence.row("prompt_cancel_validation", "unproven", json!({"reasonCode": "accepted-cancel-ended-in-turnFailed", "turnId": turn, "sequence": frame["sequence"]}));
                    let prose: Vec<_> = frame["event"]["message"].as_str().unwrap_or("").split_whitespace().filter(|word| word.len() <= 24 && word.chars().all(|c| c.is_ascii_alphabetic() || ".,:;()".contains(c))).collect();
                    panic!("cancelled turn failed; diagnostic words: {diagnostic:?}; sanitized prose: {prose:?}");
                }
                if frame["event"]["kind"] == "turnSettled" {
                    return frame;
                }
            }
        }
        panic!("cancel did not settle in the bounded host journal");
    })
    .await
    .expect("cancel did not settle within 30 seconds");
    assert_eq!(
        terminal["event"]["stopReason"], "cancelled",
        "host did not cancel the accepted turn"
    );
    evidence.row("prompt_cancel_validation", "proven", json!({"promptStatus": accepted.status, "cancelStatus": cancelled.status, "turnId": turn, "stopReason": terminal["event"]["stopReason"], "sequence": terminal["sequence"]}));
}

/// A failed turn's cause lives only in ego's journal, and the fixture home is
/// deleted with the workspace. Keep a copy next to the evidence file and print
/// each journal error's class and sanitized prose.
fn keep_failed_turn_journal(ego_home: &Path, evidence: &Path) {
    let kept = evidence.with_extension("ego-home");
    copy_dir(ego_home, &kept).unwrap_or_else(|error| panic!("cannot keep the ego home: {error}"));
    eprintln!("host-test failed turn ego home kept at {}", kept.display());
    for line in journal_error_summaries(&kept) {
        eprintln!("host-test journal error {line}");
    }
}

fn journal_error_summaries(ego_home: &Path) -> Vec<String> {
    let mut summaries = Vec::new();
    let Ok(sessions) = fs::read_dir(ego_home.join("sessions")) else {
        return summaries;
    };
    for session in sessions.flatten() {
        let Ok(journal) = fs::read_to_string(session.path().join("events.jsonl")) else {
            continue;
        };
        for line in journal.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let kind = &record["payload"]["event"]["kind"];
            if kind["kind"] != "Error" {
                continue;
            }
            // ego already bounds and masks the upstream excerpt. Tokens that
            // look like secrets or paths are still dropped here.
            let prose: String = kind["message"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .map(|word| {
                    if word.contains("sk-") || word.contains('/') {
                        "<dropped>"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(400)
                .collect();
            summaries.push(format!("kind={} prose={prose:?}", kind["error_kind"]));
        }
    }
    summaries
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The host's own record of one attachment, read from the connection snapshot.
fn attachment_state(host: &HostMatrix, connection: &str, session: &str) -> Option<String> {
    let snapshot = host.get(&format!("/acp/connections/{connection}"));
    assert_ok(&snapshot, "connection snapshot")["attachments"]
        .as_array()?
        .iter()
        .find(|attachment| attachment["sessionId"] == session)
        .and_then(|attachment| attachment["state"].as_str())
        .map(str::to_owned)
}

/// The newest journal sequence, so a new stream sees only what happens next.
fn latest_sequence(host: &HostMatrix, connection: &str) -> u64 {
    let snapshot = host.get(&format!("/acp/connections/{connection}"));
    assert_ok(&snapshot, "connection snapshot")["latestSequence"]
        .as_u64()
        .expect("connection snapshot omitted latestSequence")
}

/// A loaded session replays its durable history through the journal. The
/// attach response alone cannot show that any message came back.
async fn expect_user_message_replay(stream: &mut StreamCollector, session: &str, operation: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        for _ in 0..512 {
            let frame = stream.next_event().await;
            if frame["sessionId"] == session
                && frame["event"]["kind"] == "sessionUpdate"
                && frame["event"]["update"]["sessionUpdate"] == "user_message_chunk"
            {
                return;
            }
        }
        panic!("{operation} replayed no user message in the bounded host journal");
    })
    .await
    .unwrap_or_else(|_| panic!("{operation} replayed no user message within 30 seconds"));
}

/// A cancelled turn must leave the session able to run the next prompt.
async fn prove_prompt_completes(
    host: &HostMatrix,
    connection: &str,
    session: &str,
    evidence: &Evidence,
) {
    let mut stream = host
        .subscribe_stream(connection, latest_sequence(host, connection))
        .await;
    let accepted = host.post(
        &format!("/acp/connections/{connection}/sessions/{session}/prompt"),
        &json!({"prompt": [{"type": "text", "text": "Reply with one word."}]}),
    );
    let turn = prompt_turn_id(assert_ok(&accepted, "prompt after cancel"));
    let terminal = tokio::time::timeout(Duration::from_secs(90), async {
        for _ in 0..512 {
            let frame = stream.next_event().await;
            if !matches_turn(&frame, connection, session, &turn) {
                continue;
            }
            match frame["event"]["kind"].as_str() {
                Some("turnFailed") => {
                    keep_failed_turn_journal(&host._ego_home.path, &evidence.path);
                    panic!("the prompt after cancel failed");
                }
                Some("turnSettled") => return frame,
                _ => {}
            }
        }
        panic!("the prompt after cancel did not settle in the bounded host journal");
    })
    .await
    .expect("the prompt after cancel did not settle within 90 seconds");
    assert_eq!(
        terminal["event"]["stopReason"], "end_turn",
        "the session did not complete a prompt after the cancelled one"
    );
}

fn prove_model_config(
    host: &HostMatrix,
    connection: &str,
    session: &str,
    raw: &Value,
    evidence: &mut Evidence,
) {
    let options: Vec<v1::SessionConfigOption> =
        serde_json::from_value(raw.clone()).expect("host load contains typed config options");
    let Some(option) = options
        .iter()
        .find(|option| option.category == Some(v1::SessionConfigOptionCategory::Model))
    else {
        record_unpublished_model_config(evidence);
        return;
    };
    let v1::SessionConfigKind::Select(select) = &option.kind else {
        panic!("model control must be a select");
    };
    let candidates: Vec<_> = match &select.options {
        v1::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        v1::SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|group| &group.options).collect()
        }
        _ => panic!("unknown model option shape"),
    };
    let Some(next) = candidates.iter().find(|candidate| {
        candidate.value != select.current_value
            && candidate.value.0.as_ref() != "__ego_unselected__"
    }) else {
        evidence.row(
            "model_config",
            "unproven",
            json!({"reasonCode": "no-alternate-model-published"}),
        );
        return;
    };
    let changed = host.post(
        &format!("/acp/connections/{connection}/sessions/{session}/config"),
        &json!({"configId": option.id, "value": {"value": next.value}}),
    );
    let changed = assert_ok(&changed, "switch offered model");
    let changed: Vec<v1::SessionConfigOption> = serde_json::from_value(changed.clone())
        .expect("config returns complete typed replacement options");
    assert!(changed.iter().any(|candidate| candidate.id == option.id && matches!(&candidate.kind, v1::SessionConfigKind::Select(updated) if updated.current_value == next.value)), "model selection did not change in the host offer");
    evidence.row("model_config", "proven", json!({"configId": option.id, "optionCount": candidates.len(), "outcome": "changed-to-offered-alternative"}));
}

fn record_unpublished_model_config(evidence: &mut Evidence) {
    evidence.row(
        "model_config",
        "unproven",
        json!({"reasonCode": "no-select-config-option-published"}),
    );
}

impl Drop for Evidence {
    fn drop(&mut self) {
        let serialized = serde_json::to_string_pretty(&sanitize(&self.document(), None))
            .expect("sanitized evidence JSON");
        let _ = fs::write(&self.path, format!("{serialized}\n"));
    }
}

#[test]
fn sanitizer_redacts_credentials_contents_paths_and_tool_payloads() {
    let workspace = Path::new("/private/host-test/workspace");
    let raw = json!({
        "authorization": "Bearer HOST-TEST-SECRET",
        "prompt": [{"type": "text", "text": "HOST-TEST-PROMPT"}],
        "event": {"kind": "toolCall", "arguments": "HOST-TEST-TOOL-ARG", "result": "HOST-TEST-TOOL-RESULT"},
        "cwd": workspace,
        "requestId": "01932d5e-0000-7000-8000-0000000000cc",
        "usage": {"inputTokens": 17, "outputTokens": 9, "cost": 0}
    });
    let serialized = serde_json::to_string(&sanitize(&raw, None)).expect("sanitized fixture JSON");
    assert_sanitized(
        &serialized,
        workspace,
        &[
            "HOST-TEST-SECRET",
            "HOST-TEST-PROMPT",
            "HOST-TEST-TOOL-ARG",
            "HOST-TEST-TOOL-RESULT",
        ],
    );
    assert!(serialized.contains("01932d5e-0000-7000-8000-0000000000cc"));
    assert!(serialized.contains("17") && serialized.contains("9"));
}

#[test]
fn sanitizer_retains_capabilities_versions_digests_request_ids_and_numeric_usage() {
    let raw = json!({
        "schemaVersion": 1,
        "story": "regression",
        "tuicCommit": "49db2e63",
        "egoCommit": "4c418e7",
        "tuicArtifactSha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "capabilities": {"load": true, "mcpStdio": false, "egoHoldVersion": 1, "promptImage": true, "forkAtMessage": true},
        "requestId": "01932d5e-0000-7000-8000-0000000000cc",
        "usage": {"inputTokens": 17, "outputTokens": 9, "cost": 0.0}
    });
    let sanitized = sanitize(&raw, None);
    assert_eq!(sanitized["schemaVersion"], 1);
    assert_eq!(sanitized["tuicCommit"], "49db2e63");
    assert_eq!(sanitized["capabilities"]["mcpStdio"], false);
    assert_eq!(sanitized["capabilities"]["promptImage"], true);
    assert_eq!(sanitized["capabilities"]["forkAtMessage"], true);
    assert_eq!(sanitized["usage"]["inputTokens"], 17);
    assert_eq!(sanitized["usage"]["cost"], 0.0);
}

#[test]
fn named_instance_matches_packaged_contract() {
    let max_id = "a".repeat(63);
    for valid in ["a", "1a", "a1", "host-test-host", &max_id] {
        assert!(is_valid_named_instance(valid), "rejected {valid:?}");
    }
    let overlong_id = "a".repeat(64);
    for invalid in [
        "",
        "default",
        ".",
        "..",
        "-host-test",
        "host-test-",
        "/tmp/host-test",
        "host-test/other",
        r"host-test\other",
        "host-test.host",
        "host-test_host",
        "",
        "host-test\t",
        "host-test\n",
        "host-test\0",
        "Host-test",
        "é",
        "storyé",
        &overlong_id,
    ] {
        assert!(!is_valid_named_instance(invalid), "accepted {invalid:?}");
    }
}

#[test]
fn named_instance_config_is_separate_from_the_default_profile() {
    let home = Path::new("/private/host-test-home");
    let (config, expected_base) = match std::env::consts::OS {
        "macos" => (
            resolved_config_dir(home, None, None, "host-test-host"),
            home.join("Library/Application Support"),
        ),
        "windows" => {
            let appdata = home.join("AppData/Roaming");
            (
                resolved_config_dir(home, None, Some(&appdata), "host-test-host"),
                appdata,
            )
        }
        _ => (
            resolved_config_dir(home, None, None, "host-test-host"),
            home.join(".config"),
        ),
    };
    assert!(config.ends_with("com.tuic.commander/instances/host-test-host"));
    assert_ne!(config, home.join(".config/com.tuic.commander"));
    assert_eq!(platform_config_base(&config), expected_base);
}

#[test]
fn fixture_marker_is_scoped_to_the_named_config_directory() {
    let home = Path::new("/private/host-test-account-home");
    let appdata = home.join("AppData/Roaming");
    let config = resolved_config_dir(home, None, Some(&appdata), "host-test-host-unique");

    assert_eq!(
        fixture_marker_path(&config),
        config.join(FIXTURE_MARKER),
        "the marker identifies only the named config fixture"
    );
    assert_ne!(
        fixture_marker_path(&config),
        home.join(FIXTURE_MARKER),
        "the fixture must never label the account HOME as isolated"
    );
}

#[test]
fn ego_home_fixture_is_new_task_owned_and_marked() {
    let workspace = TempDir::new().expect("workspace");
    let fixture = EgoHomeFixture::claim(workspace.path(), None);

    assert_eq!(fixture.path, workspace.path().join("ego-home"));
    assert!(fixture_marker_path(&fixture.path).is_file());
    assert!(fixture.path.starts_with(workspace.path()));
    assert_ne!(fixture.path, PathBuf::from("/Users/operator/.ego"));
}

#[test]
fn live_ego_home_writes_bounded_budget_and_read_only_config() {
    let workspace = TempDir::new().expect("workspace");
    let live = LivePrerequisites {
        model: "openai/test".into(),
        credential_env: "TEST_PROVIDER_KEY".into(),
        credential: "test-only-secret".into(),
        permission_fixture: false,
        model_facts: None,
    };
    let fixture = EgoHomeFixture::claim(workspace.path(), Some(&live));

    assert_eq!(
        fs::read_to_string(fixture.path.join("config.toml")).expect("live config"),
        "budget = \"0.05$\"\nsandbox = \"ro\"\nmode = \"default\"\n"
    );
    assert!(fixture_marker_path(&fixture.path).is_file());
}

#[test]
fn provider_credential_is_bound_to_the_canonical_row() {
    assert_eq!(
        canonical_credential_env("openai/test", "OPENAI_API_KEY").as_deref(),
        Some("OPENAI_API_KEY")
    );
    for (model, name) in [
        ("openai/test", "ANTHROPIC_API_KEY"),
        ("anthropic/test", "OPENAI_API_KEY"),
        ("unknown/test", "OPENAI_API_KEY"),
        ("openai/", "OPENAI_API_KEY"),
        ("ollama/test", "OPENAI_API_KEY"),
        ("openai-codex/test", "OPENAI_API_KEY"),
    ] {
        assert!(canonical_credential_env(model, name).is_none());
    }
}

#[test]
fn pairing_header_rejects_injection_instead_of_forwarding_extra_authority() {
    let temp = TempDir::new().unwrap();
    let header = temp.path().join("header");
    fs::write(&header, format!("Cookie: tui-session={}\n", "a".repeat(64))).unwrap();
    assert_eq!(validate_pairing_fixture(&header), "a".repeat(64));
    fs::write(
        &header,
        format!(
            "Cookie: tui-session={}\nAuthorization: Basic invalid\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    assert!(std::panic::catch_unwind(|| validate_pairing_fixture(&header)).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_named_fixture_uses_isolated_home_for_config_resolution() {
    let account_home = Path::new("/private/host-test-account-home");
    let config = resolved_config_dir(account_home, None, None, "host-test-host-unique");

    assert_eq!(
        config,
        Path::new(
            "/private/host-test-account-home/Library/Application Support/com.tuic.commander/instances/host-test-host-unique"
        )
    );
    assert!(fixture_marker_path(&config).starts_with(&config));
}

#[test]
fn live_regression_isolation_variables_are_not_credentials() {
    for name in [
        "EGO_HOME",
        "HOME",
        "PATH",
        "TUIC_PORT",
        "EGO_MODEL",
        "APPDATA",
        "XDG_CONFIG_HOME",
    ] {
        assert!(
            canonical_credential_env("openai/test", name).is_none(),
            "accepted isolation variable {name}"
        );
    }
}

#[test]
fn live_regression_actual_sdk_tool_discriminators_are_observed() {
    let updates: Vec<Value> = include_str!("../../ego-acp/tests/golden/session_v1.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let event = json!({"kind": "sessionUpdate", "update": updates[3]});
    assert!(matches!(
        session_update(&event),
        Some(v1::SessionUpdate::ToolCall(_))
    ));
    let event = json!({"kind": "sessionUpdate", "update": updates[4]});
    assert!(matches!(
        session_update(&event),
        Some(v1::SessionUpdate::ToolCallUpdate(_))
    ));
}

#[test]
fn live_regression_fixture_enforces_read_only_confinement() {
    let workspace = TempDir::new().unwrap();
    let live = LivePrerequisites {
        model: "openai/test".into(),
        credential_env: "OPENAI_API_KEY".into(),
        credential: "test-only-secret".into(),
        permission_fixture: false,
        model_facts: None,
    };
    let home = EgoHomeFixture::claim(workspace.path(), Some(&live));
    let config: ego_core::config::ConfigLayer =
        toml::from_str(&fs::read_to_string(home.path.join("config.toml")).unwrap()).unwrap();
    assert_eq!(config.sandbox, Some(ego_core::permit::Confinement::Ro));
}

#[cfg(test)]
mod live_contract {
    use super::*;
    use ego_acp::permission::AcpPermissionAdapter;
    use ego_core::permit::{CanonPath, Change, Escalation, Op, OpName};

    fn frame(sequence: u64, event: Value) -> Value {
        json!({"kind": "event", "connectionId": "connection", "generation": 3,
            "sequence": sequence, "sessionId": "session", "turnId": "turn", "event": event})
    }

    fn update(sequence: u64, value: Value) -> Value {
        frame(sequence, json!({"kind": "sessionUpdate", "update": value}))
    }

    fn golden_turn() -> Vec<Value> {
        let golden: Vec<Value> = include_str!("../../ego-acp/tests/golden/session_v1.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        vec![
            frame(1, json!({"kind": "turnStarted"})),
            update(2, golden[1].clone()),
            update(3, golden[3].clone()),
            update(4, golden[4].clone()),
            frame(
                9,
                json!({"kind": "turnSettled", "stopReason": "end_turn", "usage": null}),
            ),
        ]
    }

    fn rows(frames: &[Value]) -> Result<Vec<(&'static str, Value)>, &'static str> {
        observed_live_rows(frames, "connection", "session", "turn")
    }

    #[test]
    fn prompt_acceptance_is_a_uuid_scalar_and_pins_the_stream_turn() {
        let turn = "01993730-0000-7000-8000-000000000001";
        let response = HttpResponse {
            status: 200,
            value: json!(turn),
        };
        let accepted = prompt_turn_id(assert_ok(&response, "prompt"));
        assert_eq!(accepted, turn);
        let mut frames = golden_turn();
        for frame in &mut frames {
            frame["turnId"] = json!(turn);
            frame["sequence"] = json!(frame["sequence"].as_u64().unwrap() + 10);
        }
        // A subscribed journal can replay a previous turn before this request.
        let mut replay = golden_turn();
        replay.extend(frames);
        assert!(observed_live_rows(&replay, "connection", "session", &accepted).is_ok());
    }

    #[test]
    fn prompt_acceptance_rejects_objects_null_invalid_ids_and_http_refusals() {
        for value in [
            json!({"turnId": "01993730-0000-7000-8000-000000000001"}),
            Value::Null,
            json!("not-a-uuid"),
            json!({"error": "refused"}),
        ] {
            assert!(std::panic::catch_unwind(|| prompt_turn_id(&value)).is_err());
        }
        let refused = HttpResponse {
            status: 409,
            value: json!({"code": "turn_in_progress"}),
        };
        assert!(
            std::panic::catch_unwind(|| prompt_turn_id(assert_ok(&refused, "prompt"))).is_err()
        );
    }

    #[test]
    fn actual_golden_lifecycle_proves_v1_fidelity_without_invented_reasoning_or_cost() {
        let observed = rows(&golden_turn()).unwrap();
        assert_eq!(
            observed.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            [
                "ordered_message_updates",
                "ordered_tool_call_updates",
                "ordered_tool_progress_updates"
            ]
        );
        assert_eq!(
            observed[2].1["fidelity"],
            "acp-v1-start-terminal-final-content"
        );
    }

    #[test]
    fn foreign_envelopes_recursive_decoys_and_incomplete_tools_cannot_prove_rows() {
        for key in ["connectionId", "sessionId", "turnId"] {
            let mut frames = golden_turn();
            for value in &mut frames {
                value[key] = json!("another");
            }
            assert!(rows(&frames).is_err(), "accepted another {key}");
        }
        let mut frames = golden_turn();
        frames[2]["event"] = json!({"kind": "unrelated", "payload": {"kind": "toolCall"}});
        assert!(rows(&frames).is_err());
        for (key, value) in [
            ("toolCallId", json!("another")),
            ("status", json!("in_progress")),
            ("status", json!("failed")),
            ("status", json!("unknown")),
            ("content", json!([])),
        ] {
            let mut frames = golden_turn();
            frames[3]["event"]["update"][key] = value;
            assert!(rows(&frames).is_err(), "accepted invalid terminal {key}");
        }
        for (index, key, value) in [(3, "sequence", json!(3)), (3, "generation", json!(4))] {
            let mut frames = golden_turn();
            frames[index][key] = value;
            assert!(rows(&frames).is_err());
        }
        let mut frames = golden_turn();
        frames.swap(2, 3);
        assert!(rows(&frames).is_err());
        let mut frames = golden_turn();
        frames[4]["event"]["stopReason"] = json!("cancelled");
        assert!(rows(&frames).is_err());
    }

    fn usage(sequence: u64, amount: f64) -> Value {
        update(
            sequence,
            serde_json::to_value(v1::SessionUpdate::UsageUpdate(
                v1::UsageUpdate::new(165, 200_000).cost(v1::Cost::new(amount, "USD")),
            ))
            .unwrap(),
        )
    }

    #[test]
    fn cumulative_cost_requires_multiple_numeric_observations_and_stable_currency() {
        let mut frames = golden_turn();
        frames.insert(4, usage(5, 0.001));
        assert!(
            !rows(&frames)
                .unwrap()
                .iter()
                .any(|(name, _)| *name == "usage_cost_frame_shape")
        );
        frames.insert(5, usage(6, 0.002));
        // Occupancy is not cumulative and may fall after compaction.
        frames[5]["event"]["update"]["used"] = json!(100);
        let observed = rows(&frames).unwrap();
        let facts = &observed
            .iter()
            .find(|(name, _)| *name == "usage_cost_frame_shape")
            .unwrap()
            .1;
        assert_eq!(facts["observations"][1]["cost"]["amount"], 0.002);
        assert_eq!(
            sanitize(facts, None)["observations"][1]["cost"]["currency"],
            "USD"
        );
        for (pointer, value) in [
            ("/event/update/used", json!("165")),
            ("/event/update/size", json!(0)),
            ("/event/update/cost/amount", json!(-1)),
            ("/event/update/cost/amount", json!(0.0001)),
            ("/event/update/cost/amount", json!("0.002")),
            ("/event/update/cost/currency", json!("EUR")),
            ("/event/update/cost/currency", json!("invalid")),
        ] {
            let mut bad = frames.clone();
            *bad[5].pointer_mut(pointer).unwrap() = value;
            assert!(rows(&bad).is_err(), "accepted malformed usage {pointer}");
        }
        frames.insert(
            6,
            update(
                7,
                json!({"sessionUpdate": "usage_update", "used": 100, "size": 200_000}),
            ),
        );
        assert!(
            !rows(&frames)
                .unwrap()
                .iter()
                .any(|(name, _)| *name == "usage_cost_frame_shape")
        );
        for value in &mut frames[4..6] {
            value["event"]["update"]
                .as_object_mut()
                .unwrap()
                .remove("cost");
        }
        assert!(
            !rows(&frames)
                .unwrap()
                .iter()
                .any(|(name, _)| *name == "usage_cost_frame_shape")
        );
    }

    fn permission_fixture(target: &Path) -> (Vec<Value>, Value) {
        let item = ego_core::event::id::ItemId::new();
        let path = CanonPath::new(target.to_str().unwrap());
        let request = AcpPermissionAdapter::permission_request(
            &v1::SessionId::new("session"),
            item,
            "write",
            &[Op::Write {
                path: path.clone(),
                change: Change::Whole(PERMISSION_CONTENTS.as_bytes().to_vec()),
            }],
            &Escalation::ModeAsks {
                mode: ego_core::config::Mode::Default,
                op: OpName::Write,
                path,
            },
        );
        let call = v1::SessionUpdate::ToolCall(
            v1::ToolCall::new(item.to_string(), format!("write {}", target.display()))
                .raw_input(json!({"files": [{"path": target, "contents": PERMISSION_CONTENTS}]})),
        );
        (
            vec![
                frame(1, json!({"kind": "turnStarted"})),
                update(2, serde_json::to_value(call).unwrap()),
            ],
            frame(
                3,
                json!({"kind": "permissionRequested", "requestId": "01932d5e-0000-7000-8000-0000000000cc", "request": request}),
            ),
        )
    }

    #[test]
    fn permission_uses_production_request_id_and_offered_ids_with_exact_operation() {
        let target = Path::new("/workspace/host-test-live-tool-input.txt");
        let (prior, request) = permission_fixture(target);
        let (_, refused) = refusal_response(&request["event"], "session").unwrap();
        assert_eq!(
            refused["outcome"]["optionId"],
            AcpPermissionAdapter::REJECT_ONCE
        );
        let (id, response) =
            fixture_permission_response(&prior, &request, "connection", "session", "turn", target)
                .unwrap();
        assert_eq!(
            response["outcome"]["optionId"],
            AcpPermissionAdapter::ALLOW_ONCE
        );
        let mut frames = prior;
        frames.push(request.clone());
        frames.push(frame(
            4,
            json!({"kind": "permissionSettled", "requestId": id, "outcome": response["outcome"]}),
        ));
        assert!(!permission_roundtrip(
            &frames,
            "connection",
            "session",
            "turn",
            &id,
            &response
        ));
        frames.push(update(5, json!({"sessionUpdate": "tool_call_update",
            "toolCallId": request["event"]["request"]["toolCall"]["toolCallId"],
            "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "wrote fixture"}}]})));
        assert!(permission_roundtrip(
            &frames,
            "connection",
            "session",
            "turn",
            &id,
            &response
        ));
        let mut early = frames.clone();
        early.swap(3, 4);
        assert!(!permission_roundtrip(
            &early,
            "connection",
            "session",
            "turn",
            &id,
            &response
        ));
        let mut extra = frames.clone();
        extra.push(frames[1].clone());
        assert!(!permission_roundtrip(
            &extra,
            "connection",
            "session",
            "turn",
            &id,
            &response
        ));
        frames[3]["event"]["requestId"] = json!("another");
        assert!(!permission_roundtrip(
            &frames,
            "connection",
            "session",
            "turn",
            &id,
            &response
        ));
        let mut malformed = request["event"].clone();
        let id = malformed
            .as_object_mut()
            .unwrap()
            .remove("requestId")
            .unwrap();
        malformed["request_id"] = id;
        assert!(refusal_response(&malformed, "session").is_none());
        let mut offered = request["event"].clone();
        offered["request"]["options"][1]["optionId"] = json!("offered-rejection");
        assert_eq!(
            refusal_response(&offered, "session").unwrap().1["outcome"]["optionId"],
            "offered-rejection"
        );
    }

    #[test]
    fn unrelated_mutations_targets_args_sessions_and_replays_are_never_granted() {
        let target = Path::new("/workspace/host-test-live-tool-input.txt");
        let (prior, request) = permission_fixture(target);
        for (pointer, value) in [
            ("/event/update/title", json!("exec")),
            (
                "/event/update/rawInput/files/0/path",
                json!("/workspace/other"),
            ),
            (
                "/event/update/rawInput/files/0/contents",
                json!("different bytes"),
            ),
            (
                "/event/update/rawInput",
                json!({"files": [{"path": target, "contents": PERMISSION_CONTENTS, "extra": true}]} ),
            ),
            ("/event/update/toolCallId", json!("another")),
        ] {
            let mut bad = prior.clone();
            *bad[1].pointer_mut(pointer).unwrap() = value;
            assert!(
                fixture_permission_response(
                    &bad,
                    &request,
                    "connection",
                    "session",
                    "turn",
                    target
                )
                .is_none()
            );
        }
        for (pointer, value) in [
            ("/event/request/sessionId", json!("other")),
            ("/sessionId", json!("other")),
            ("/generation", json!(4)),
            ("/event/request/options/0/optionId", json!("allow_once")),
        ] {
            let mut bad = request.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(
                fixture_permission_response(&prior, &bad, "connection", "session", "turn", target)
                    .is_none()
            );
        }
        let mut replay = prior.clone();
        replay.push(request.clone());
        assert!(
            fixture_permission_response(&replay, &request, "connection", "session", "turn", target)
                .is_none()
        );
        assert!(
            fixture_permission_response(
                &prior[1..],
                &request,
                "connection",
                "session",
                "turn",
                target
            )
            .is_none()
        );
    }

    #[test]
    fn mismatched_credentials_are_rejected_before_any_secret_lookup() {
        for (model, name) in [
            ("openai/test", "EGO_HOME"),
            ("openai/test", "HOME"),
            ("openai/test", "PATH"),
            ("openai/test", "TUIC_PORT"),
            ("openai/test", "ANTHROPIC_API_KEY"),
            ("unknown/test", "OPENAI_API_KEY"),
        ] {
            assert!(
                LivePrerequisites::from_selection(model.into(), name, false, |_| panic!(
                    "credential lookup must not occur"
                ))
                .is_none()
            );
        }
        let live = LivePrerequisites::from_selection(
            "openai/test".into(),
            "OPENAI_API_KEY",
            false,
            |name| {
                assert_eq!(name, "OPENAI_API_KEY");
                Some("dummy".into())
            },
        )
        .unwrap();
        assert!(!live.permission_fixture);
    }

    fn prerequisites(path: &Path, live: Option<LivePrerequisites>) -> Prerequisites {
        Prerequisites {
            tuic_artifact: path.join("tuic"),
            ego_executable: path.join("ego"),
            tuic_commit: "a".repeat(40),
            ego_commit: "b".repeat(40),
            tuic_home: path.to_owned(),
            tuic_instance: "host-test-test".to_owned(),
            config_dir: path.join("com.tuic.commander/instances/host-test-test"),
            endpoint: Url::parse("http://127.0.0.1:1234").unwrap(),
            auth_header_file: path.join("auth"),
            pairing_token: "a".repeat(64),
            evidence: path.join("evidence.json"),
            tuic_sha256: "a".repeat(64),
            ego_sha256: "b".repeat(64),
            os: std::env::consts::OS,
            live,
        }
    }

    #[test]
    fn evidence_inventory_and_billability_follow_the_live_selection() {
        // Acceptance inventory stays independent of the implementation rows.
        let expected_operations = [
            "connect",
            "initialize_capability_snapshot",
            "unsupported_rows",
            "ordered_host_stream",
            "session_new",
            "session_list",
            "permission_surface",
            "permission_response",
            "model_config",
            "prompt_cancel_validation",
            "ordered_message_updates",
            "ordered_reasoning_updates",
            "ordered_tool_call_updates",
            "ordered_tool_progress_updates",
            "usage_cost_frame_shape",
            "namespaced_pause_resume",
            "session_fork_delete",
            "session_load_resume_close",
            "reconnect",
        ];
        let temp = TempDir::new().unwrap();
        for permission in [None, Some(false), Some(true)] {
            let live = permission.map(|enabled| {
                LivePrerequisites::from_selection(
                    "openai/test".into(),
                    "OPENAI_API_KEY",
                    enabled,
                    |_| Some("dummy".into()),
                )
                .unwrap()
            });
            let prerequisites = prerequisites(temp.path(), live);
            let mut evidence = Evidence::new(&prerequisites, temp.path());
            assert_eq!(evidence.prerequisites["billable"], permission.is_some());
            assert_eq!(
                evidence
                    .rows
                    .iter()
                    .map(|row| row["operation"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                expected_operations
            );
            assert!(evidence.rows.iter().all(|row| row["status"] == "unproven"));
            evidence.row("ordered_tool_call_updates", "proven", json!({}));
            assert_eq!(evidence.rows.len(), expected_operations.len());
            assert!(
                evidence
                    .rows
                    .iter()
                    .any(|row| row["operation"] == "ordered_reasoning_updates"
                        && row["status"] == "unproven")
            );
        }
    }

    /// Catches: a host TMPDIR inside the workspace makes the sandbox probe
    /// see its own scratch files as workspace content and fail falsely.
    #[test]
    fn host_tmpdir_lies_outside_the_workspace_and_credentials_follow_the_live_selection() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        for permission in [None, Some(false), Some(true)] {
            let live = permission.map(|enabled| {
                LivePrerequisites::from_selection(
                    "openai/test".into(),
                    "OPENAI_API_KEY",
                    enabled,
                    |_| Some("dummy".into()),
                )
                .unwrap()
            });
            let prerequisites = prerequisites(temp.path(), live);
            let home = temp.path().join("isolated-ego");
            let command = ProcessGuard::command(&prerequisites, &workspace, &home);
            let environment: BTreeMap<_, _> = command
                .get_envs()
                .map(|(key, value)| (key, value.unwrap()))
                .collect();
            assert_eq!(
                environment[std::ffi::OsStr::new("HOME")],
                temp.path().as_os_str()
            );
            let tmpdir = Path::new(&environment[std::ffi::OsStr::new("TMPDIR")]);
            assert!(
                !tmpdir.starts_with(&workspace),
                "TMPDIR {} must not lie inside the workspace",
                tmpdir.display()
            );
            assert!(
                workspace.starts_with(tmpdir),
                "the workspace must remain below the host TMPDIR"
            );
            assert_eq!(
                environment[std::ffi::OsStr::new("TUIC_PAIRING_TOKEN")],
                std::ffi::OsStr::new(&"a".repeat(64))
            );
            assert_eq!(
                environment[std::ffi::OsStr::new("EGO_HOME")],
                home.as_os_str()
            );
            assert_eq!(
                environment.contains_key(std::ffi::OsStr::new("OPENAI_API_KEY")),
                permission.is_some()
            );
            assert!(!environment.contains_key(std::ffi::OsStr::new("ANTHROPIC_API_KEY")));
        }
    }

    #[test]
    fn an_absent_select_config_option_leaves_model_config_unproven() {
        let temp = TempDir::new().unwrap();
        let prerequisites = prerequisites(temp.path(), None);
        let mut evidence = Evidence::new(&prerequisites, temp.path());

        record_unpublished_model_config(&mut evidence);

        let row = evidence
            .rows
            .iter()
            .find(|row| row["operation"] == "model_config")
            .unwrap();
        assert_eq!(row["status"], "unproven");
        assert_eq!(
            row["facts"]["reasonCode"],
            "no-select-config-option-published"
        );
    }

    /// Catches: a failed live turn loses its journal cause, or the summary
    /// leaks a credential or path from the provider's message.
    #[test]
    fn failed_turn_journal_summary_keeps_the_class_and_drops_secrets() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("sessions/s1");
        fs::create_dir_all(&session).unwrap();
        let error = json!({"payload": {"kind": "event", "event": {"kind": {
            "kind": "Error", "error_kind": "invalid",
            "message": "the provider returned HTTP 400: bad key sk-or-v1-abc123 at /Users/x/secret reasoning unsupported"
        }}}});
        let other =
            json!({"payload": {"kind": "event", "event": {"kind": {"kind": "TurnStarted"}}}});
        fs::write(
            session.join("events.jsonl"),
            format!("{other}\nnot json\n{error}\n"),
        )
        .unwrap();

        let summaries = journal_error_summaries(temp.path());

        assert_eq!(summaries.len(), 1);
        let line = &summaries[0];
        assert!(line.contains("kind=\"invalid\""));
        assert!(line.contains("HTTP 400:") && line.contains("reasoning unsupported"));
        assert!(!line.contains("sk-or") && !line.contains("/Users"));

        let other_root = TempDir::new().unwrap();
        let kept = other_root.path().join("copy");
        copy_dir(temp.path(), &kept).unwrap();
        assert!(kept.join("sessions/s1/events.jsonl").is_file());
    }

    #[test]
    fn fixture_mutation_has_a_separate_explicit_ceiling() {
        let temp = TempDir::new().unwrap();
        let live =
            LivePrerequisites::from_selection("openai/test".into(), "OPENAI_API_KEY", true, |_| {
                Some("dummy".into())
            })
            .unwrap();
        let home = EgoHomeFixture::claim(temp.path(), Some(&live));
        let config: ego_core::config::ConfigLayer =
            toml::from_str(&fs::read_to_string(home.path.join("config.toml")).unwrap()).unwrap();
        assert_eq!(
            config.sandbox,
            Some(ego_core::permit::Confinement::Workspace)
        );
        assert_eq!(config.mode, Some(ego_core::config::Mode::Default));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the packaged production TUICommander backend, an isolated production-format profile, and the packaged ego executable; run the exact command in this module's module documentation"]
async fn packaged_tuicommander_host_matrix_is_complete() {
    let prerequisites = Prerequisites::discover();
    let workspace = TempDir::new().expect("workspace root");
    eprintln!("host-test workspace root: {}", workspace.path().display());
    let mut evidence = Evidence::new(&prerequisites, workspace.path());
    let host = HostMatrix::launch(&prerequisites, workspace.path());

    let connected = host.post(
        "/acp/connections",
        &json!({"root": workspace.path().display().to_string()}),
    );
    let connected = assert_ok(&connected, "connect");
    let connection_id = string_field(connected, "connectionId");
    evidence.row(
        "connect",
        "proven",
        json!({"connectionId": connection_id, "state": connected["state"]}),
    );

    let snapshot = host.get(&format!("/acp/connections/{connection_id}"));
    let snapshot = assert_ok(&snapshot, "connection snapshot");
    let capabilities = snapshot["capabilities"].clone();
    assert_eq!(
        capabilities["protocol"], 1,
        "TUICommander must retain the ACP v1 numeric protocol snapshot"
    );
    assert_eq!(
        capabilities["mcpStdio"], false,
        "stdio is an explicit unsupported row"
    );
    assert_eq!(
        capabilities["clientBooleanConfig"], false,
        "boolean config is an explicit unsupported row"
    );
    evidence.row(
        "initialize_capability_snapshot",
        "proven",
        capabilities.clone(),
    );
    evidence.row("unsupported_rows", "proven", json!({"mcpStdio": "unsupported", "clientBooleanConfig": "unsupported", "providers": "not-advertised", "sessionSetMode": "not-advertised"}));

    let stream_frames = host.observe_stream_head(&connection_id, 0).await;
    assert_eq!(
        stream_frames.len(),
        1,
        "stream did not yield the ready event"
    );
    assert_eq!(stream_frames[0]["kind"], "event");
    assert_eq!(stream_frames[0]["event"]["kind"], "connectionState");
    evidence.row(
        "ordered_host_stream",
        "proven",
        json!({"frames": stream_frames}),
    );

    let new_session = host.post(
        &format!("/acp/connections/{connection_id}/sessions"),
        &authority(workspace.path()),
    );
    let new_session = assert_ok(&new_session, "session/new");
    let session_id = string_field(new_session, "sessionId");
    evidence.row(
        "session_new",
        "proven",
        json!({"sessionId": session_id, "state": new_session["state"]}),
    );

    let listed = host.get(&format!("/acp/connections/{connection_id}/sessions"));
    let listed = assert_ok(&listed, "session/list");
    assert!(listed["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .any(|session| session["sessionId"] == session_id)
    }));
    evidence.row(
        "session_list",
        "proven",
        json!({"count": listed["sessions"].as_array().map_or(0, Vec::len)}),
    );

    let interactions = host.get(&format!("/acp/connections/{connection_id}/interactions"));
    let interactions = assert_ok(&interactions, "pending interactions");
    assert!(
        interactions.as_array().is_some_and(Vec::is_empty),
        "non-billable matrix must not inherit a permission request"
    );
    if prerequisites.live.is_none() {
        evidence.row(
            "permission_response",
            "unproven",
            json!({"reasonCode": "provider-traffic-requires-operator-authorized-credential-and-request-or-spend-cap"}),
        );
    }

    // session/new may defer its measured offer. Check controls after load,
    // when the offer is complete, rather than declaring absent controls here.
    let invalid_prompt = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/prompt"),
        &json!({"prompt": [{"type": "audio", "data": "", "mimeType": "audio/wav"}]}),
    );
    assert!(
        invalid_prompt.status >= 400,
        "unsupported audio prompt unexpectedly reached a generation: {}",
        safe_json(&invalid_prompt.value)
    );
    let idle_cancel = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/cancel"),
        &json!({}),
    );
    evidence.row(
        "prompt_cancel_validation",
        "proven",
        json!({"promptStatus": invalid_prompt.status, "cancelStatus": idle_cancel.status}),
    );
    if prerequisites.live.is_some() {
        prove_live_provider_turn(
            &host,
            &connection_id,
            &session_id,
            workspace.path(),
            &mut evidence,
            prerequisites
                .live
                .as_ref()
                .is_some_and(|live| live.permission_fixture),
        )
        .await;
        prove_active_cancel(&host, &connection_id, &session_id, &mut evidence).await;
    } else {
        for operation in [
            "ordered_message_updates",
            "ordered_reasoning_updates",
            "ordered_tool_call_updates",
            "ordered_tool_progress_updates",
        ] {
            evidence.row(
                operation,
                "unproven",
                json!({"reasonCode": "provider-traffic-requires-operator-authorized-credential-and-request-or-spend-cap"}),
            );
        }
        evidence.row(
            "usage_cost_frame_shape",
            "unproven",
            json!({"reasonCode": "provider-traffic-requires-operator-authorized-credential-and-request-or-spend-cap"}),
        );
    }

    // The surface is only meaningful once requests have had a chance to open
    // and settle: an answered permission must not stay pending in the host.
    let after_turns = host.get(&format!("/acp/connections/{connection_id}/interactions"));
    let after_turns = assert_ok(&after_turns, "pending interactions after the turns");
    assert!(
        after_turns.as_array().is_some_and(Vec::is_empty),
        "an answered or refused permission stayed pending in the host"
    );
    assert_eq!(
        attachment_state(&host, &connection_id, &session_id).as_deref(),
        Some("idle"),
        "the session must be idle before the hold"
    );
    evidence.row(
        "permission_surface",
        "proven",
        json!({"pendingBeforeTurns": 0, "pendingAfterTurns": 0}),
    );

    let request_id = "01932d5e-0000-7000-8000-0000000000cc";
    let paused = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/pause"),
        &json!({"requestId": request_id}),
    );
    if paused.status == 200 {
        let paused_body = assert_ok(&paused, "_ego/pause");
        assert_eq!(paused_body["state"], "paused", "an idle hold must land");
        assert_eq!(paused_body["requestId"], request_id);
        assert_eq!(
            attachment_state(&host, &connection_id, &session_id).as_deref(),
            Some("paused"),
            "the host did not record the paused attachment"
        );
        let resumed = host.post(
            &format!("/acp/connections/{connection_id}/sessions/{session_id}/resume-turn"),
            &json!({"requestId": request_id}),
        );
        let resumed_body = assert_ok(&resumed, "_ego/resume");
        assert_eq!(
            resumed_body["state"], "running",
            "resume must release the hold"
        );
        assert_eq!(resumed_body["requestId"], request_id);
        assert_eq!(
            attachment_state(&host, &connection_id, &session_id).as_deref(),
            Some("idle"),
            "the host did not return the released session to idle"
        );
        evidence.row(
            "namespaced_pause_resume",
            "proven",
            json!({"pause": paused_body, "resume": resumed_body}),
        );
    } else {
        evidence.row(
            "namespaced_pause_resume",
            "unproven",
            json!({"reasonCode": "host-or-agent-refused-idle-hold", "httpStatus": paused.status}),
        );
    }

    let forked = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/fork"),
        &authority(workspace.path()),
    );
    let forked = assert_ok(&forked, "session/fork");
    let forked_id = string_field(forked, "sessionId");
    assert_ok(
        &host.delete(&format!(
            "/acp/connections/{connection_id}/sessions/{forked_id}"
        )),
        "session/delete fork",
    );
    evidence.row(
        "session_fork_delete",
        "proven",
        json!({"forkedSessionId": forked_id}),
    );

    assert_ok(
        &host.post(
            &format!("/acp/connections/{connection_id}/sessions/{session_id}/close"),
            &json!({}),
        ),
        "session/close before load",
    );
    let mut load_stream = host
        .subscribe_stream(&connection_id, latest_sequence(&host, &connection_id))
        .await;
    let loaded = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/load"),
        &authority(workspace.path()),
    );
    let loaded = assert_ok(&loaded, "session/load");
    assert_eq!(loaded["sessionId"], session_id);
    assert_eq!(loaded["state"], "idle");
    let history_expected = prerequisites.live.is_some();
    if history_expected {
        expect_user_message_replay(&mut load_stream, &session_id, "session/load").await;
    }
    prove_model_config(
        &host,
        &connection_id,
        &session_id,
        &loaded["configOptions"],
        &mut evidence,
    );
    assert_ok(
        &host.post(
            &format!("/acp/connections/{connection_id}/sessions/{session_id}/close"),
            &json!({}),
        ),
        "session/close before resume",
    );
    let resumed = host.post(
        &format!("/acp/connections/{connection_id}/sessions/{session_id}/resume"),
        &authority(workspace.path()),
    );
    let resumed = assert_ok(&resumed, "session/resume");
    assert_eq!(resumed["sessionId"], session_id);
    assert_eq!(
        attachment_state(&host, &connection_id, &session_id).as_deref(),
        Some("idle"),
        "the resumed session is not attached idle"
    );
    assert_ok(
        &host.post(
            &format!("/acp/connections/{connection_id}/sessions/{session_id}/close"),
            &json!({}),
        ),
        "session/close after resume",
    );
    if history_expected {
        evidence.row(
            "session_load_resume_close",
            "proven",
            json!({"sessionId": session_id, "replayedUserMessage": true}),
        );
    } else {
        evidence.row(
            "session_load_resume_close",
            "unproven",
            json!({"reasonCode": "no-durable-history-without-a-provider-turn"}),
        );
    }

    let reconnected = host.post(
        &format!("/acp/connections/{connection_id}/reconnect"),
        &json!({"root": workspace.path().display().to_string()}),
    );
    let reconnected = assert_ok(&reconnected, "reconnect");
    let reconnected_id = string_field(reconnected, "connectionId");
    assert_ne!(
        reconnected_id, connection_id,
        "reconnect must return a fresh connection identity"
    );
    let old_snapshot = host.get(&format!("/acp/connections/{connection_id}"));
    let old_snapshot = assert_ok(&old_snapshot, "settled old connection snapshot");
    assert!(
        old_snapshot["settlement"].is_object(),
        "reconnect lost the old connection settlement"
    );
    let mut reconnect_stream = host
        .subscribe_stream(&reconnected_id, latest_sequence(&host, &reconnected_id))
        .await;
    let restored = host.post(
        &format!("/acp/connections/{reconnected_id}/sessions/{session_id}/load"),
        &authority(workspace.path()),
    );
    let restored = assert_ok(&restored, "session/load after reconnect");
    assert_eq!(restored["sessionId"], session_id);
    assert_eq!(
        attachment_state(&host, &reconnected_id, &session_id).as_deref(),
        Some("idle"),
        "the session is not attached idle on the new connection"
    );
    if history_expected {
        expect_user_message_replay(&mut reconnect_stream, &session_id, "load after reconnect")
            .await;
    }
    assert_ok(
        &host.delete(&format!(
            "/acp/connections/{reconnected_id}/sessions/{session_id}"
        )),
        "session/delete after reconnect",
    );
    assert_ok(
        &host.delete(&format!("/acp/connections/{reconnected_id}")),
        "disconnect after reconnect",
    );
    if history_expected {
        evidence.row(
            "reconnect",
            "proven",
            json!({"oldConnectionId": connection_id, "newConnectionId": reconnected_id, "replayedUserMessage": true}),
        );
    } else {
        evidence.row(
            "reconnect",
            "unproven",
            json!({"reasonCode": "no-durable-history-without-a-provider-turn"}),
        );
    }

    let _ = host.profile.path.as_os_str();
    evidence.write();
    assert_eq!(
        evidence
            .rows
            .iter()
            .filter_map(|row| row["operation"].as_str())
            .collect::<Vec<_>>(),
        MATRIX_ROWS,
        "the complete matrix inventory must be retained"
    );
    let unresolved: Vec<_> = evidence
        .rows
        .iter()
        .filter(|row| row["status"] == "unproven")
        .filter_map(|row| row["operation"].as_str())
        .collect();
    assert!(
        unresolved.is_empty(),
        "remains RED for rows: {unresolved:?}"
    );
}

/// Catches: session/cancel races admission and leaves a recoverable run uncertain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the same isolated real TUIC prerequisites as the host matrix"]
async fn accepted_cancellation_does_not_leave_durable_run_uncertain() {
    let prerequisites = Prerequisites::discover();
    let workspace = TempDir::new().expect("isolated cancellation workspace");
    let host = HostMatrix::launch(&prerequisites, workspace.path());
    let mut evidence = Evidence::new(&prerequisites, workspace.path());
    let connected = host.post("/acp/connections", &json!({"root": workspace.path()}));
    let connected = assert_ok(&connected, "connect cancellation host");
    let connection = string_field(connected, "connectionId");
    let created = host.post(
        &format!("/acp/connections/{connection}/sessions"),
        &authority(workspace.path()),
    );
    let session = string_field(assert_ok(&created, "new cancellation session"), "sessionId");
    prove_active_cancel(&host, &connection, &session, &mut evidence).await;
    prove_prompt_completes(&host, &connection, &session, &evidence).await;
}
