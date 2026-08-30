//! Canonical process authority and the run-level guarantees required to use it.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::CanonPath;
use super::tool::{StreamProvenance, ToolContentBlock, ToolResult};
use crate::credentials::SecretMaterial;
use crate::hook::EffectScope;
use crate::store::evidence::EvidenceError;

/// A validated, host-native environment variable name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvName(OsString);

impl EnvName {
    pub fn new(name: impl Into<OsString>) -> Result<Self, EnvNameError> {
        let name = name.into();
        if !valid_env_name(&name) {
            return Err(EnvNameError { name });
        }
        Ok(Self(name))
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

impl fmt::Debug for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EnvName").field(&self.0).finish()
    }
}

impl fmt::Display for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.to_string_lossy())
    }
}

#[cfg(unix)]
fn valid_env_name(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = name.as_bytes();
    !bytes.is_empty() && !bytes.contains(&b'=') && !bytes.contains(&0)
}

#[cfg(windows)]
fn valid_env_name(name: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    let mut units = name.encode_wide().peekable();
    units.peek().is_some() && units.all(|unit| unit != u16::from(b'=') && unit != 0)
}

#[cfg(not(any(unix, windows)))]
fn valid_env_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| !name.is_empty() && !name.chars().any(|ch| ch == '=' || ch == '\0'))
}

/// An environment name is empty or cannot be represented by a child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvNameError {
    name: OsString,
}

impl fmt::Display for EnvNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid environment variable name {:?}", self.name)
    }
}

impl std::error::Error for EnvNameError {}

/// One exact child environment entry whose value exists only in live effect authority.
#[derive(Clone)]
pub struct ExplicitEnvBinding {
    name: EnvName,
    value: Arc<SecretMaterial>,
    encoding: ExplicitEnvEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplicitEnvEncoding {
    Utf8,
    Native,
}

impl ExplicitEnvBinding {
    pub fn new(name: EnvName, value: SecretMaterial) -> Self {
        Self {
            name,
            value: Arc::new(value),
            encoding: ExplicitEnvEncoding::Utf8,
        }
    }

    pub fn from_native(name: EnvName, value: OsString) -> Result<Self, ExplicitEnvValueError> {
        Ok(Self {
            name,
            value: Arc::new(SecretMaterial::new(encode_native_value(value)?)),
            encoding: ExplicitEnvEncoding::Native,
        })
    }

    pub fn name(&self) -> &EnvName {
        &self.name
    }

    /// Expose the value only to the adapter constructing the child process.
    pub fn expose_value(&self) -> &SecretMaterial {
        self.value.as_ref()
    }

    /// Decode the live value according to the representation admitted by its
    /// producer. Native bytes are inverted only for values produced by
    /// `OsString::into_encoded_bytes`; foreign UTF-8 is validated first.
    pub fn value_os_string(&self) -> Result<OsString, ExplicitEnvValueError> {
        match self.encoding {
            ExplicitEnvEncoding::Utf8 => {
                let value = std::str::from_utf8(self.value.expose_secret_bytes())
                    .map_err(|_| ExplicitEnvValueError::InvalidUtf8)?;
                Ok(OsString::from(value))
            }
            ExplicitEnvEncoding::Native => decode_native_value(self.value.expose_secret_bytes()),
        }
    }
}

#[cfg(unix)]
fn encode_native_value(value: OsString) -> Result<Vec<u8>, ExplicitEnvValueError> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(value.into_vec())
}

#[cfg(unix)]
fn decode_native_value(bytes: &[u8]) -> Result<OsString, ExplicitEnvValueError> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn encode_native_value(value: OsString) -> Result<Vec<u8>, ExplicitEnvValueError> {
    use std::os::windows::ffi::OsStrExt as _;

    Ok(value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>())
}

#[cfg(windows)]
fn decode_native_value(bytes: &[u8]) -> Result<OsString, ExplicitEnvValueError> {
    use std::os::windows::ffi::OsStringExt as _;

    let mut chunks = bytes.chunks_exact(2);
    let units = chunks
        .by_ref()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if !chunks.remainder().is_empty() {
        return Err(ExplicitEnvValueError::InvalidNativeEncoding);
    }
    Ok(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn encode_native_value(value: OsString) -> Result<Vec<u8>, ExplicitEnvValueError> {
    value
        .into_string()
        .map(String::into_bytes)
        .map_err(|_| ExplicitEnvValueError::InvalidNativeEncoding)
}

#[cfg(not(any(unix, windows)))]
fn decode_native_value(bytes: &[u8]) -> Result<OsString, ExplicitEnvValueError> {
    String::from_utf8(bytes.to_vec())
        .map(OsString::from)
        .map_err(|_| ExplicitEnvValueError::InvalidNativeEncoding)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplicitEnvValueError {
    InvalidUtf8,
    InvalidNativeEncoding,
}

impl fmt::Display for ExplicitEnvValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => {
                formatter.write_str("explicit environment value is not valid UTF-8")
            }
            Self::InvalidNativeEncoding => {
                formatter.write_str("explicit native environment value is invalid")
            }
        }
    }
}

impl std::error::Error for ExplicitEnvValueError {}

impl fmt::Debug for ExplicitEnvBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExplicitEnvBinding")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .field("encoding", &self.encoding)
            .finish()
    }
}

impl PartialEq for ExplicitEnvBinding {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.encoding == other.encoding
            && self.value.expose_secret_bytes() == other.value.expose_secret_bytes()
    }
}

impl Eq for ExplicitEnvBinding {}

/// How a child derives its environment from the host at spawn time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvPolicy {
    /// Preserve the real process environment.
    #[default]
    Inherited,
    /// Inherit only the named variables.
    AllowOnly(Vec<EnvName>),
    /// Inherit every variable except the named variables.
    InheritedExcept(Vec<EnvName>),
    /// Install only these explicit, zeroized values.
    Exact {
        names: Vec<EnvName>,
        bindings: Vec<ExplicitEnvBinding>,
    },
}

impl EnvPolicy {
    pub fn allow_only<I, N>(names: I) -> Result<Self, EnvNameError>
    where
        I: IntoIterator<Item = N>,
        N: Into<OsString>,
    {
        canonical_names(names).map(Self::AllowOnly)
    }

    pub fn inherited_except<I, N>(names: I) -> Result<Self, EnvNameError>
    where
        I: IntoIterator<Item = N>,
        N: Into<OsString>,
    {
        canonical_names(names).map(Self::InheritedExcept)
    }

    pub fn exact(mut bindings: Vec<ExplicitEnvBinding>) -> Result<Self, EnvNameError> {
        bindings.sort_by(|left, right| left.name.cmp(&right.name));
        bindings.dedup_by(|left, right| left.name == right.name);
        let names = bindings
            .iter()
            .map(|binding| binding.name.clone())
            .collect();
        Ok(Self::Exact { names, bindings })
    }

    /// The canonical name list, empty for an unfiltered inherited environment.
    pub fn names(&self) -> &[EnvName] {
        match self {
            Self::Inherited => &[],
            Self::AllowOnly(names) | Self::InheritedExcept(names) => names,
            Self::Exact { names, .. } => names,
        }
    }

    pub fn exact_bindings(&self) -> &[ExplicitEnvBinding] {
        match self {
            Self::Exact { bindings, .. } => bindings,
            Self::Inherited | Self::AllowOnly(_) | Self::InheritedExcept(_) => &[],
        }
    }
}

fn canonical_names<I, N>(names: I) -> Result<Vec<EnvName>, EnvNameError>
where
    I: IntoIterator<Item = N>,
    N: Into<OsString>,
{
    let mut names = names
        .into_iter()
        .map(EnvName::new)
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    names.dedup();
    Ok(names)
}

/// The exact process specification classified by policy and executed by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    argv: Vec<OsString>,
    cwd: Option<CanonPath>,
    timeout: Option<Duration>,
    env: EnvPolicy,
}

impl SpawnSpec {
    pub fn new(
        argv: Vec<OsString>,
        cwd: Option<CanonPath>,
        timeout: Option<Duration>,
        env: EnvPolicy,
    ) -> Result<Self, SpawnSpecError> {
        if argv.is_empty() {
            return Err(SpawnSpecError::EmptyArgv);
        }
        if let Some((index, _)) = argv
            .iter()
            .enumerate()
            .find(|(_, argument)| contains_nul(argument))
        {
            return Err(SpawnSpecError::NulArgument { index });
        }
        if let Some(cwd) = &cwd
            && !cwd.as_path().is_absolute()
        {
            return Err(SpawnSpecError::RelativeCwd { cwd: cwd.clone() });
        }
        if timeout == Some(Duration::ZERO) {
            return Err(SpawnSpecError::ZeroTimeout);
        }
        Ok(Self {
            argv,
            cwd,
            timeout,
            env,
        })
    }

    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    pub fn cwd(&self) -> Option<&CanonPath> {
        self.cwd.as_ref()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn env_policy(&self) -> &EnvPolicy {
        &self.env
    }
}

#[cfg(unix)]
fn contains_nul(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    value.as_bytes().contains(&0)
}

#[cfg(windows)]
fn contains_nul(value: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    value.encode_wide().any(|unit| unit == 0)
}

#[cfg(not(any(unix, windows)))]
fn contains_nul(value: &OsStr) -> bool {
    value.to_str().is_none_or(|value| value.contains('\0'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnSpecError {
    EmptyArgv,
    NulArgument { index: usize },
    RelativeCwd { cwd: CanonPath },
    ZeroTimeout,
}

impl fmt::Display for SpawnSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArgv => f.write_str("spawn argv must not be empty"),
            Self::NulArgument { index } => {
                write!(f, "spawn argv element {index} contains a NUL")
            }
            Self::RelativeCwd { cwd } => write!(f, "spawn cwd must be absolute: {cwd}"),
            Self::ZeroTimeout => f.write_str("spawn timeout must be greater than zero"),
        }
    }
}

impl std::error::Error for SpawnSpecError {}

/// Filesystem confinement selected once for a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confinement {
    Ro,
    #[default]
    Workspace,
    Off,
}

impl fmt::Display for Confinement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ro => "ro",
            Self::Workspace => "workspace",
            Self::Off => "off",
        })
    }
}

impl FromStr for Confinement {
    type Err = ConfinementParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ro" => Ok(Self::Ro),
            "workspace" => Ok(Self::Workspace),
            "off" => Ok(Self::Off),
            _ => Err(ConfinementParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementParseError {
    value: String,
}

impl fmt::Display for ConfinementParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid sandbox {:?}: expected ro, workspace, or off",
            self.value
        )
    }
}

impl std::error::Error for ConfinementParseError {}

/// Guarantees a process backend has demonstrated for one effective policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecCaps(u8);

impl ExecCaps {
    pub const READ_SCOPED: Self = Self(1 << 0);
    pub const WRITE_DENIED: Self = Self(1 << 1);
    pub const WRITE_SCOPED: Self = Self(1 << 2);
    pub const NO_IP_NETWORK: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn null() -> Self {
        Self::empty()
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn check(self, available: Self) -> Result<(), MissingExecCaps> {
        ExecRequirements::new(self).check(available)
    }

    const fn missing_from(self, available: Self) -> Self {
        Self(self.0 & !available.0)
    }
}

impl Serialize for ExecCaps {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut names = Vec::new();
        for (capability, name) in [
            (Self::READ_SCOPED, "read_scoped"),
            (Self::WRITE_DENIED, "write_denied"),
            (Self::WRITE_SCOPED, "write_scoped"),
            (Self::NO_IP_NETWORK, "no_ip_network"),
        ] {
            if self.contains(capability) {
                names.push(name);
            }
        }
        names.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExecCaps {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let names = Vec::<String>::deserialize(deserializer)?;
        let mut capabilities = Self::empty();
        for name in names {
            let capability = match name.as_str() {
                "read_scoped" => Self::READ_SCOPED,
                "write_denied" => Self::WRITE_DENIED,
                "write_scoped" => Self::WRITE_SCOPED,
                "no_ip_network" => Self::NO_IP_NETWORK,
                _ => {
                    return Err(serde::de::Error::unknown_variant(
                        &name,
                        &[
                            "read_scoped",
                            "write_denied",
                            "write_scoped",
                            "no_ip_network",
                        ],
                    ));
                }
            };
            if capabilities.contains(capability) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate execution capability {name:?}"
                )));
            }
            capabilities = capabilities.union(capability);
        }
        Ok(capabilities)
    }
}

/// The selected execution backend, recorded as evidence rather than authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendIdentity {
    name: String,
    revision: String,
}

impl BackendIdentity {
    pub fn new(name: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            revision: revision.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// A validated lowercase SHA-256 digest of the effective execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PolicyDigest(String);

impl PolicyDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PolicyDigest {
    type Error = PolicyDigestError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if crate::digest::is_lower_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(PolicyDigestError(value))
        }
    }
}

impl TryFrom<&str> for PolicyDigest {
    type Error = PolicyDigestError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.to_owned().try_into()
    }
}

impl From<PolicyDigest> for String {
    fn from(value: PolicyDigest) -> Self {
        value.0
    }
}

impl fmt::Display for PolicyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDigestError(String);

impl fmt::Display for PolicyDigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid execution policy digest {:?}", self.0)
    }
}

impl std::error::Error for PolicyDigestError {}

/// What one backend demonstrated for the effective execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecObservation {
    backend: BackendIdentity,
    capabilities: ExecCaps,
    policy_digest: PolicyDigest,
}

impl ExecObservation {
    pub fn new(
        backend: BackendIdentity,
        capabilities: ExecCaps,
        policy_digest: PolicyDigest,
    ) -> Self {
        Self {
            backend,
            capabilities,
            policy_digest,
        }
    }

    pub fn backend(&self) -> &BackendIdentity {
        &self.backend
    }

    pub const fn capabilities(&self) -> ExecCaps {
        self.capabilities
    }

    pub fn policy_digest(&self) -> &PolicyDigest {
        &self.policy_digest
    }
}

/// Capability requirements derived from the immutable run policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecRequirements {
    required: ExecCaps,
}

impl ExecRequirements {
    pub const fn new(required: ExecCaps) -> Self {
        Self { required }
    }

    pub const fn required(self) -> ExecCaps {
        self.required
    }

    pub fn check(self, available: ExecCaps) -> Result<(), MissingExecCaps> {
        let missing = self.required.missing_from(available);
        if missing == ExecCaps::empty() {
            Ok(())
        } else {
            Err(MissingExecCaps {
                required: self.required,
                available,
                missing,
            })
        }
    }
}

/// The exact guarantees absent from a selected backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingExecCaps {
    required: ExecCaps,
    available: ExecCaps,
    missing: ExecCaps,
}

impl MissingExecCaps {
    pub const fn required(self) -> ExecCaps {
        self.required
    }

    pub const fn available(self) -> ExecCaps {
        self.available
    }

    pub const fn missing(self) -> ExecCaps {
        self.missing
    }
}

impl fmt::Display for MissingExecCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "process backend capabilities {:?} do not satisfy {:?}; missing {:?}",
            self.available, self.required, self.missing
        )
    }
}

impl std::error::Error for MissingExecCaps {}

/// Run-owned confinement policy, independent of permission mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfinement {
    level: Confinement,
    offline: bool,
    workspace: CanonPath,
    requirements: ExecRequirements,
}

impl RunConfinement {
    pub fn new(
        level: Confinement,
        offline: bool,
        workspace: CanonPath,
    ) -> Result<Self, RunConfinementError> {
        if !workspace.as_path().is_absolute() {
            return Err(RunConfinementError { workspace });
        }
        let filesystem = match level {
            Confinement::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
            Confinement::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            Confinement::Off => ExecCaps::empty(),
        };
        let required = if offline {
            filesystem.union(ExecCaps::NO_IP_NETWORK)
        } else {
            filesystem
        };
        Ok(Self {
            level,
            offline,
            workspace,
            requirements: ExecRequirements::new(required),
        })
    }

    pub const fn level(&self) -> Confinement {
        self.level
    }

    pub const fn offline(&self) -> bool {
        self.offline
    }

    pub fn workspace(&self) -> &CanonPath {
        &self.workspace
    }

    pub const fn requirements(&self) -> ExecRequirements {
        self.requirements
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfinementError {
    workspace: CanonPath,
}

impl fmt::Display for RunConfinementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run workspace must be absolute: {}", self.workspace)
    }
}

impl std::error::Error for RunConfinementError {}

/// Host process execution behind the permit-checking facade.
pub trait ProcessExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>>;

    /// Execute one admitted hook handler with its strict request on stdin.
    /// Owners that do not provide this effect fail before spawning anything.
    fn execute_handler<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
        _input: &'a [u8],
        _scope: EffectScope,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(futures::future::ready(Err(ProcessError::Invariant(
            "process executor has no hook-handler input seam".to_owned(),
        ))))
    }
}

/// Why an admitted process did not complete successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    MissingCapabilities(MissingExecCaps),
    Backend(String),
    Invariant(String),
    Launch {
        executable: OsString,
        reason: String,
    },
    Exit {
        code: Option<i32>,
        result: ToolResult,
    },
    Signal {
        signal: Option<i32>,
        result: ToolResult,
    },
    Timeout {
        timeout: Duration,
    },
    Cancelled,
    EvidenceLimit {
        stream: StreamProvenance,
        limit: u64,
    },
    Evidence(EvidenceError),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCapabilities(missing) => write!(f, "{missing}"),
            Self::Backend(reason) => write!(f, "process backend failed: {reason}"),
            Self::Invariant(reason) => write!(f, "process invariant failed: {reason}"),
            Self::Launch { executable, reason } => {
                write!(f, "could not launch {executable:?}: {reason}")
            }
            Self::Exit { code, .. } => {
                write!(f, "process exited unsuccessfully with code {code:?}")
            }
            Self::Signal { signal, .. } => {
                write!(f, "process terminated by signal {signal:?}")
            }
            Self::Timeout { timeout } => write!(f, "process timed out after {timeout:?}"),
            Self::Cancelled => f.write_str("process was cancelled"),
            Self::EvidenceLimit { stream, limit } => {
                write!(f, "{stream} exceeded the hard {limit}-byte evidence limit")
            }
            Self::Evidence(error) => write!(f, "process evidence failed: {error}"),
        }
    }
}

impl std::error::Error for ProcessError {}

impl ProcessError {
    /// Preserve complete committed streams only for statuses observed after
    /// both pipes reached EOF, then append the one bounded diagnostic block.
    pub fn into_failed_result(self) -> ToolResult {
        let diagnostic = self.to_string();
        let mut result = match self {
            Self::Exit { result, .. } | Self::Signal { result, .. } => result,
            _ => ToolResult::new(Vec::new()),
        };
        result
            .content
            .push(ToolContentBlock::builtin(super::tool::bounded_diagnostic(
                &diagnostic,
            )));
        result
    }
}
