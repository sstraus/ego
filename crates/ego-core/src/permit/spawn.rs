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
use crate::store::evidence::{EvidenceError, EvidenceRef};

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

    let (chunks, remainder) = bytes.as_chunks::<2>();
    let units = chunks
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect::<Vec<_>>();
    if !remainder.is_empty() {
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
    identity: Option<Arc<super::analysis::ExecutionIdentity>>,
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
            identity: None,
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

    pub fn identity(&self) -> Option<&super::analysis::ExecutionIdentity> {
        self.identity.as_deref()
    }

    /// Bind host acquisition without changing the user argv used by rules.
    pub fn with_execution_identity(
        mut self,
        env: EnvPolicy,
        identity: super::analysis::ExecutionIdentity,
    ) -> Self {
        self.env = env;
        self.identity = Some(Arc::new(identity));
        self
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

impl Confinement {
    /// Every confinement, strongest first — the order a picker renders.
    pub const ALL: [Self; 3] = [Self::Ro, Self::Workspace, Self::Off];

    /// The one spelling: what `--sandbox` takes, what the config key holds,
    /// and what a host sends as a value id. `FromStr` reads these back.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ro => "ro",
            Self::Workspace => "workspace",
            Self::Off => "off",
        }
    }

    /// How much this confines, for comparison only. Never stored, never sent.
    ///
    /// Written out rather than derived from `Ord` on the declaration order:
    /// the variants are ordered for reading, and a derive would let somebody
    /// reverse a security predicate by moving a line.
    const fn strength(self) -> u8 {
        match self {
            Self::Ro => 2,
            Self::Workspace => 1,
            Self::Off => 0,
        }
    }

    /// The guarantees a process backend must have demonstrated to enforce
    /// this confinement.
    ///
    /// Lifted out of [`RunConfinement::new`] rather than copied beside it: an
    /// offer computed from one table and a run checked against another is two
    /// tables that can disagree, and the disagreement would be a session
    /// offered a confinement its own spawn then refuses. Offline networking is
    /// deliberately not here — it is a separate axis the run owns.
    pub const fn required_caps(self) -> ExecCaps {
        match self {
            Self::Ro => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_DENIED),
            Self::Workspace => ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            Self::Off => ExecCaps::empty(),
        }
    }

    /// Whether this confines at least as much as `ceiling` does.
    ///
    /// The only question a ceiling is ever asked
    /// (`docs/decisions/2026-09-07-a-host-picks-confinement-under-a-ceiling-it-cannot-raise.md`).
    /// `ro` is within every ceiling; `off` is within `off` alone.
    pub const fn within(self, ceiling: Self) -> bool {
        self.strength() >= ceiling.strength()
    }
}

impl fmt::Display for Confinement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

    /// Each guarantee in this set, in the words a person can act on.
    ///
    /// A refusal that prints the bit pattern tells the operator which integer
    /// was absent, which is the one fact they cannot do anything with.
    pub fn guarantees(self) -> Vec<&'static str> {
        [
            (
                Self::READ_SCOPED,
                "read only inside the workspace and the fixed runtime roots",
            ),
            (Self::WRITE_DENIED, "no filesystem mutation at all"),
            (
                Self::WRITE_SCOPED,
                "write only inside the workspace and ego's private scratch directory",
            ),
            (
                Self::NO_IP_NETWORK,
                "no IP networking for spawned processes, loopback included",
            ),
        ]
        .into_iter()
        .filter(|(capability, _)| self.contains(*capability))
        .map(|(_, words)| words)
        .collect()
    }
}

impl fmt::Display for ExecCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guarantees = self.guarantees();
        if guarantees.is_empty() {
            return f.write_str("no confinement guarantee");
        }
        f.write_str(&guarantees.join(", "))
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
                probe_evidence: None,
                backend: None,
            })
        }
    }
}

/// The exact guarantees absent from a selected backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingExecCaps {
    required: ExecCaps,
    available: ExecCaps,
    missing: ExecCaps,
    probe_evidence: Option<EvidenceRef>,
    /// `None` when the refusal was raised before a backend was selected —
    /// which is why it is an option rather than a required field. A refusal
    /// that cannot name a backend says so instead of inventing one.
    backend: Option<BackendIdentity>,
}

impl MissingExecCaps {
    pub const fn required(&self) -> ExecCaps {
        self.required
    }

    pub const fn available(&self) -> ExecCaps {
        self.available
    }

    pub const fn missing(&self) -> ExecCaps {
        self.missing
    }

    /// The measurement transcript this refusal came out of, when one was
    /// taken. A refusal raised before any probe ran has nothing to cite.
    pub const fn probe_evidence(&self) -> Option<&EvidenceRef> {
        self.probe_evidence.as_ref()
    }

    #[must_use]
    pub fn with_probe_evidence(mut self, reference: Option<EvidenceRef>) -> Self {
        self.probe_evidence = reference;
        self
    }

    /// Which backend fell short.
    ///
    /// `ExecRequirements::check` compares two capability sets and has no idea
    /// whose they are, so the refusal it builds said "the process backend" —
    /// true on every platform and useful on none. The caller that selected the
    /// backend knows its name and revision; this is where it says so, and the
    /// difference for an operator is between "something did not measure up"
    /// and "Seatbelt did not, and here is the revision that was probed".
    #[must_use]
    pub fn from_backend(mut self, backend: BackendIdentity) -> Self {
        self.backend = Some(backend);
        self
    }
}

impl fmt::Display for MissingExecCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.backend {
            Some(backend) => write!(
                f,
                "the {} process backend (revision {}) demonstrated {}, but this \
                 run requires {}; missing {}",
                backend.name(),
                backend.revision(),
                self.available,
                self.required,
                self.missing
            )?,
            None => write!(
                f,
                "the process backend demonstrated {}, but this run requires \
                 {}; missing {}",
                self.available, self.required, self.missing
            )?,
        }
        if let Some(reference) = &self.probe_evidence {
            write!(f, " (probe transcript {})", reference.id.as_str())?;
        }
        Ok(())
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
        let filesystem = level.required_caps();
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

    /// Whether this invocation may open an MCP HTTP transport.
    ///
    /// HTTP MCP runs in process, so only an online invocation with no
    /// filesystem sandbox can admit it.
    pub const fn admits_mcp_http(&self) -> bool {
        !self.offline && matches!(self.level, Confinement::Off)
    }

    /// Whether this invocation may spawn an MCP stdio transport.
    pub const fn admits_mcp_stdio(&self) -> bool {
        !self.offline
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
    /// An adapter that cannot observe identity leaves it incomplete, never
    /// inventing facts that could support reusable approval.
    fn observe(&self, spec: &SpawnSpec) -> Result<SpawnSpec, ProcessError> {
        Ok(spec.clone())
    }

    fn revalidate(&self, _spec: &SpawnSpec) -> Result<(), ProcessError> {
        Ok(())
    }

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
    /// The reconstructed backend no longer matches the facts admitted for the run.
    /// No user effects have been constructed when this error is returned.
    ConfinementChanged {
        admitted: Box<ExecObservation>,
        observed: Box<ExecObservation>,
    },
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
            Self::ConfinementChanged { admitted, observed } => write!(
                f,
                "confinement changed before execution: admitted backend `{}` (revision {}) with capabilities [{}] and policy {}; observed backend `{}` (revision {}) with capabilities [{}] and policy {}; the session must be reinitialized",
                admitted.backend().name(),
                admitted.backend().revision(),
                admitted.capabilities(),
                admitted.policy_digest(),
                observed.backend().name(),
                observed.backend().revision(),
                observed.capabilities(),
                observed.policy_digest(),
            ),
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

#[cfg(test)]
mod confinement {
    use super::Confinement;

    /// The ceiling predicate, stated as the table it is.
    ///
    /// Written as every pair rather than as a few interesting ones: the whole
    /// point of `within` is that it decides a security question, and a
    /// spot-check would pass just as happily with the comparison inverted.
    #[test]
    fn a_confinement_is_within_a_ceiling_exactly_when_it_confines_at_least_as_much() {
        use Confinement::{Off, Ro, Workspace};
        let expected = [
            ((Ro, Ro), true),
            ((Ro, Workspace), true),
            ((Ro, Off), true),
            ((Workspace, Ro), false),
            ((Workspace, Workspace), true),
            ((Workspace, Off), true),
            ((Off, Ro), false),
            ((Off, Workspace), false),
            ((Off, Off), true),
        ];
        for ((value, ceiling), within) in expected {
            assert_eq!(
                value.within(ceiling),
                within,
                "`{value}` within a `{ceiling}` ceiling"
            );
        }
    }

    /// `ALL` is the picker's whole offer, so a variant missing from it is a
    /// confinement nobody can select rather than a cosmetic omission.
    #[test]
    fn every_confinement_is_offered_and_the_strongest_comes_first() {
        // Named one by one, because iterating `ALL` to check `ALL` proves
        // nothing: a variant dropped from the offer would drop out of the
        // loop with it.
        for confinement in [Confinement::Ro, Confinement::Workspace, Confinement::Off] {
            assert!(
                Confinement::ALL.contains(&confinement),
                "`{confinement}` exists and is offered to nobody"
            );
        }
        assert!(
            Confinement::ALL
                .windows(2)
                .all(|pair| pair[0].within(pair[1]) && !pair[1].within(pair[0])),
            "the offer is not ordered strongest first: {:?}",
            Confinement::ALL
        );
    }
}
