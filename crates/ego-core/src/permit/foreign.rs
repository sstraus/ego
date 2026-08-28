//! Protocol-neutral foreign transport authority and raw effect ports.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::BoxStream;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use super::{CanonPath, EnvName, EnvPolicy, SpawnSpec, ToolResult};
use crate::generator::attempt::DeliveryCertainty;
use crate::store::evidence::{EvidenceError, EvidenceRead};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForeignToolId {
    server: String,
    name: String,
}

impl ForeignToolId {
    pub fn new(server: &str, name: &str) -> Result<Self, ForeignValueError> {
        if !valid_identity_part(server) || !valid_identity_part(name) {
            return Err(ForeignValueError::InvalidTool);
        }
        Ok(Self {
            server: server.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

fn valid_identity_part(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

impl fmt::Display for ForeignToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.server, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InvocationDigest(String);

impl InvocationDigest {
    /// Hash canonical invocation bytes into the permission grammar's digest form.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn parse(value: &str) -> Result<Self, ForeignValueError> {
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(ForeignValueError::InvalidDigest);
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ForeignValueError::InvalidDigest);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvocationDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkTarget {
    host: String,
    port: u16,
}

impl NetworkTarget {
    pub fn new(host: &str, port: u16) -> Result<Self, ForeignValueError> {
        if port == 0 {
            return Err(ForeignValueError::InvalidNetworkTarget);
        }
        let unbracketed = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        let authority = match unbracketed.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => format!("[{unbracketed}]"),
            _ => unbracketed.to_owned(),
        };
        let url = url::Url::parse(&format!("http://{authority}:{port}/"))
            .map_err(|_| ForeignValueError::InvalidNetworkTarget)?;
        let host = canonical_url_host(&url)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(port)
        {
            return Err(ForeignValueError::InvalidNetworkTarget);
        }
        Ok(Self { host, port })
    }

    pub fn parse(value: &str) -> Result<Self, ForeignValueError> {
        let url = url::Url::parse(&format!("http://{value}/"))
            .map_err(|_| ForeignValueError::InvalidNetworkTarget)?;
        let host = canonical_url_host(&url)?;
        let port = url.port().ok_or(ForeignValueError::InvalidNetworkTarget)?;
        if !url.username().is_empty() || url.password().is_some() || url.path() != "/" {
            return Err(ForeignValueError::InvalidNetworkTarget);
        }
        Self::new(&host, port)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }
}

fn canonical_url_host(url: &url::Url) -> Result<String, ForeignValueError> {
    match url.host() {
        Some(url::Host::Domain(host)) => Ok(host.to_owned()),
        Some(url::Host::Ipv4(host)) => Ok(host.to_string()),
        Some(url::Host::Ipv6(host)) => Ok(host.to_string()),
        None => Err(ForeignValueError::InvalidNetworkTarget),
    }
}

impl fmt::Display for NetworkTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpScheme {
    Http,
    Https,
}

impl HttpScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpRoute {
    scheme: HttpScheme,
    target: NetworkTarget,
}

impl HttpRoute {
    pub fn new(scheme: HttpScheme, target: NetworkTarget) -> Result<Self, ForeignValueError> {
        Ok(Self { scheme, target })
    }

    pub fn scheme(&self) -> HttpScheme {
        self.scheme
    }

    pub fn target(&self) -> &NetworkTarget {
        &self.target
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpRequestDigest([u8; 32]);

impl HttpRequestDigest {
    pub fn of(request: &HttpRequest) -> Self {
        let mut hasher = Sha256::new();
        hash_http_frame(&mut hasher, b"ego.http-request");
        hash_http_frame(&mut hasher, b"1");
        hash_http_frame(&mut hasher, request.method.as_bytes());
        hash_http_frame(&mut hasher, request.path.as_bytes());
        hasher.update((request.headers.len() as u128).to_be_bytes());
        for header in &request.headers {
            hash_http_frame(&mut hasher, header.name.as_bytes());
            hash_http_frame(&mut hasher, &header.value);
        }
        hash_http_frame(&mut hasher, &request.body);
        Self(hasher.finalize().into())
    }
}

fn hash_http_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u128).to_be_bytes());
    hasher.update(bytes);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedConnect {
    target: NetworkTarget,
    request_digest: HttpRequestDigest,
}

impl PreparedConnect {
    pub fn new(target: NetworkTarget, request: &HttpRequest) -> Self {
        Self {
            target,
            request_digest: HttpRequestDigest::of(request),
        }
    }

    pub(crate) fn from_parts(target: NetworkTarget, request_digest: HttpRequestDigest) -> Self {
        Self {
            target,
            request_digest,
        }
    }

    pub fn target(&self) -> &NetworkTarget {
        &self.target
    }

    pub fn request_digest(&self) -> &HttpRequestDigest {
        &self.request_digest
    }

    pub fn matches(&self, request: &HttpRequest) -> bool {
        self.request_digest == HttpRequestDigest::of(request)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForeignTransportTarget(ForeignTransportKind);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ForeignTransportKind {
    Http(HttpRoute),
    Stdio { server: String },
}

impl ForeignTransportTarget {
    pub fn http(route: HttpRoute) -> Self {
        Self(ForeignTransportKind::Http(route))
    }

    pub fn stdio(server: &str) -> Result<Self, ForeignValueError> {
        if !valid_identity_part(server) {
            return Err(ForeignValueError::InvalidTransport);
        }
        Ok(Self(ForeignTransportKind::Stdio {
            server: server.to_owned(),
        }))
    }

    pub(crate) fn http_route(&self) -> Option<&HttpRoute> {
        match &self.0 {
            ForeignTransportKind::Http(route) => Some(route),
            ForeignTransportKind::Stdio { .. } => None,
        }
    }

    pub(crate) fn into_http_route(self) -> Option<HttpRoute> {
        match self.0 {
            ForeignTransportKind::Http(route) => Some(route),
            ForeignTransportKind::Stdio { .. } => None,
        }
    }

    pub(crate) fn is_stdio(&self) -> bool {
        matches!(&self.0, ForeignTransportKind::Stdio { .. })
    }
}

impl fmt::Display for ForeignTransportTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ForeignTransportKind::Http(route) => {
                write!(f, "{}://{}", route.scheme.as_str(), route.target)
            }
            ForeignTransportKind::Stdio { server } => write!(f, "stdio:{server}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignValueError {
    InvalidTool,
    InvalidDigest,
    InvalidNetworkTarget,
    InvalidTransport,
    EmptyResolution,
}

impl fmt::Display for ForeignValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid foreign authority: {self:?}")
    }
}
impl std::error::Error for ForeignValueError {}

/// The closed, lossless subject vocabulary used by permission diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Path(CanonPath),
    Process(SpawnSpec),
    Network(NetworkTarget),
    Foreign {
        tool: ForeignToolId,
        params_digest: InvocationDigest,
    },
    Evidence(EvidenceRead),
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{path}"),
            Self::Process(spec) => write!(f, "process {:?}", spec.argv()),
            Self::Network(target) => write!(f, "{target}"),
            Self::Foreign {
                tool,
                params_digest,
            } => write!(f, "{tool} ({params_digest})"),
            Self::Evidence(request) => {
                write!(
                    f,
                    "evidence {} at byte {}",
                    request.reference, request.offset
                )
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SubjectWire {
    Path {
        version: u8,
        platform: String,
        value: String,
    },
    Process {
        version: u8,
        platform: String,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_seconds: Option<u64>,
        timeout_nanos: Option<u32>,
        env: EnvWire,
    },
    Network {
        version: u8,
        host: String,
        port: u16,
    },
    Foreign {
        version: u8,
        server: String,
        name: String,
        params_digest: String,
    },
    Evidence {
        version: u8,
        request: EvidenceRead,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "names",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum EnvWire {
    Inherited,
    AllowOnly(Vec<String>),
    InheritedExcept(Vec<String>),
}

impl Serialize for Subject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match self {
            Self::Path(path) => SubjectWire::Path {
                version: 1,
                platform: native_platform().to_owned(),
                value: encode_native(path.as_path().as_os_str()),
            },
            Self::Process(spec) => {
                let env = match spec.env_policy() {
                    EnvPolicy::Inherited => EnvWire::Inherited,
                    EnvPolicy::AllowOnly(names) => EnvWire::AllowOnly(
                        names
                            .iter()
                            .map(|name| encode_native(name.as_os_str()))
                            .collect(),
                    ),
                    EnvPolicy::InheritedExcept(names) => EnvWire::InheritedExcept(
                        names
                            .iter()
                            .map(|name| encode_native(name.as_os_str()))
                            .collect(),
                    ),
                };
                SubjectWire::Process {
                    version: 1,
                    platform: native_platform().to_owned(),
                    argv: spec
                        .argv()
                        .iter()
                        .map(|argument| encode_native(argument))
                        .collect(),
                    cwd: spec
                        .cwd()
                        .map(|path| encode_native(path.as_path().as_os_str())),
                    timeout_seconds: spec.timeout().map(|timeout| timeout.as_secs()),
                    timeout_nanos: spec.timeout().map(|timeout| timeout.subsec_nanos()),
                    env,
                }
            }
            Self::Network(target) => SubjectWire::Network {
                version: 1,
                host: target.host().to_owned(),
                port: target.port(),
            },
            Self::Foreign {
                tool,
                params_digest,
            } => SubjectWire::Foreign {
                version: 1,
                server: tool.server().to_owned(),
                name: tool.name().to_owned(),
                params_digest: params_digest.as_str().to_owned(),
            },
            Self::Evidence(request) => SubjectWire::Evidence {
                version: 1,
                request: request.clone(),
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Subject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SubjectWire::deserialize(deserializer)?;
        match wire {
            SubjectWire::Path {
                version,
                platform,
                value,
            } => {
                validate_wire(version, &platform).map_err(serde::de::Error::custom)?;
                Ok(Self::Path(CanonPath::new(PathBuf::from(
                    decode_native(&value).map_err(serde::de::Error::custom)?,
                ))))
            }
            SubjectWire::Process {
                version,
                platform,
                argv,
                cwd,
                timeout_seconds,
                timeout_nanos,
                env,
            } => {
                validate_wire(version, &platform).map_err(serde::de::Error::custom)?;
                if timeout_seconds.is_some() != timeout_nanos.is_some() {
                    return Err(serde::de::Error::custom("incomplete process timeout"));
                }
                if timeout_nanos.is_some_and(|nanos| nanos >= 1_000_000_000) {
                    return Err(serde::de::Error::custom("invalid process timeout nanos"));
                }
                let argv = argv
                    .into_iter()
                    .map(|argument| decode_native(&argument))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(serde::de::Error::custom)?;
                let cwd = cwd
                    .map(|path| {
                        decode_native(&path).map(|path| CanonPath::new(PathBuf::from(path)))
                    })
                    .transpose()
                    .map_err(serde::de::Error::custom)?;
                let timeout = timeout_seconds
                    .zip(timeout_nanos)
                    .map(|(seconds, nanos)| Duration::new(seconds, nanos));
                let env = match env {
                    EnvWire::Inherited => EnvPolicy::Inherited,
                    EnvWire::AllowOnly(names) => {
                        EnvPolicy::AllowOnly(decode_env(names).map_err(serde::de::Error::custom)?)
                    }
                    EnvWire::InheritedExcept(names) => EnvPolicy::InheritedExcept(
                        decode_env(names).map_err(serde::de::Error::custom)?,
                    ),
                };
                SpawnSpec::new(argv, cwd, timeout, env)
                    .map(Self::Process)
                    .map_err(serde::de::Error::custom)
            }
            SubjectWire::Network {
                version,
                host,
                port,
            } => {
                validate_version(version).map_err(serde::de::Error::custom)?;
                NetworkTarget::new(&host, port)
                    .map(Self::Network)
                    .map_err(serde::de::Error::custom)
            }
            SubjectWire::Foreign {
                version,
                server,
                name,
                params_digest,
            } => {
                validate_version(version).map_err(serde::de::Error::custom)?;
                Ok(Self::Foreign {
                    tool: ForeignToolId::new(&server, &name).map_err(serde::de::Error::custom)?,
                    params_digest: InvocationDigest::parse(&params_digest)
                        .map_err(serde::de::Error::custom)?,
                })
            }
            SubjectWire::Evidence { version, request } => {
                validate_version(version).map_err(serde::de::Error::custom)?;
                Ok(Self::Evidence(request))
            }
        }
    }
}

fn decode_env(names: Vec<String>) -> Result<Vec<EnvName>, String> {
    names
        .into_iter()
        .map(|name| {
            decode_native(&name)
                .map_err(str::to_owned)
                .and_then(|name| EnvName::new(name).map_err(|error| error.to_string()))
        })
        .collect()
}

fn validate_version(version: u8) -> Result<(), &'static str> {
    if version == 1 {
        Ok(())
    } else {
        Err("unsupported permission subject version")
    }
}

fn validate_wire(version: u8, platform: &str) -> Result<(), &'static str> {
    validate_version(version)?;
    if platform == native_platform() {
        Ok(())
    } else {
        Err("permission subject platform does not match this host")
    }
}

#[cfg(unix)]
fn native_platform() -> &'static str {
    "unix-bytes"
}

#[cfg(windows)]
fn native_platform() -> &'static str {
    "windows-utf16le"
}

#[cfg(not(any(unix, windows)))]
fn native_platform() -> &'static str {
    "utf8"
}

#[cfg(unix)]
fn encode_native(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    base64url_encode(value.as_bytes())
}

#[cfg(windows)]
fn encode_native(value: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    let bytes = value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64url_encode(&bytes)
}

#[cfg(not(any(unix, windows)))]
fn encode_native(value: &OsStr) -> String {
    base64url_encode(value.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn decode_native(value: &str) -> Result<OsString, &'static str> {
    use std::os::unix::ffi::OsStringExt as _;
    base64url_decode(value).map(OsString::from_vec)
}

#[cfg(windows)]
fn decode_native(value: &str) -> Result<OsString, &'static str> {
    use std::os::windows::ffi::OsStringExt as _;
    let bytes = base64url_decode(value)?;
    if bytes.len() % 2 != 0 {
        return Err("invalid native string encoding");
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
    Ok(OsString::from_wide(&units.collect::<Vec<_>>()))
}

#[cfg(not(any(unix, windows)))]
fn decode_native(value: &str) -> Result<OsString, &'static str> {
    String::from_utf8(base64url_decode(value)?)
        .map(OsString::from)
        .map_err(|_| "invalid native string encoding")
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(char::from(BASE64URL[((bits >> 18) & 63) as usize]));
        encoded.push(char::from(BASE64URL[((bits >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            encoded.push(char::from(BASE64URL[((bits >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            encoded.push(char::from(BASE64URL[(bits & 63) as usize]));
        }
    }
    encoded
}

fn base64url_decode(value: &str) -> Result<Vec<u8>, &'static str> {
    if value.len() % 4 == 1 || value.bytes().any(|byte| decode_digit(byte).is_none()) {
        return Err("invalid base64url native string");
    }
    let mut decoded = Vec::with_capacity(value.len() / 4 * 3 + 2);
    for chunk in value.as_bytes().chunks(4) {
        let mut bits = 0u32;
        for byte in chunk {
            bits = (bits << 6) | u32::from(decode_digit(*byte).expect("validated digit"));
        }
        bits <<= 6 * (4 - chunk.len());
        decoded.push((bits >> 16) as u8);
        if chunk.len() > 2 {
            decoded.push((bits >> 8) as u8);
        }
        if chunk.len() > 3 {
            decoded.push(bits as u8);
        }
    }
    if base64url_encode(&decoded) == value {
        Ok(decoded)
    } else {
        Err("non-canonical base64url native string")
    }
}

fn decode_digit(byte: u8) -> Option<u8> {
    BASE64URL
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|index| index as u8)
}

#[derive(Debug)]
pub struct DnsResolution {
    target: NetworkTarget,
    addresses: Vec<IpAddr>,
}

impl DnsResolution {
    pub fn new(
        target: NetworkTarget,
        mut addresses: Vec<IpAddr>,
    ) -> Result<Self, ForeignValueError> {
        addresses.sort();
        addresses.dedup();
        if addresses.is_empty() {
            return Err(ForeignValueError::EmptyResolution);
        }
        Ok(Self { target, addresses })
    }

    pub fn target(&self) -> &NetworkTarget {
        &self.target
    }
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

#[derive(Debug)]
pub struct ResolvedConnect {
    route: HttpRoute,
    selected: IpAddr,
    request_digest: HttpRequestDigest,
}

impl ResolvedConnect {
    pub(crate) fn from_resolution(
        route: HttpRoute,
        connect: PreparedConnect,
        resolution: DnsResolution,
        selected: IpAddr,
    ) -> Result<Self, TransportError> {
        if route.target != connect.target
            || resolution.target != connect.target
            || resolution.addresses.is_empty()
            || !resolution.addresses.contains(&selected)
        {
            return Err(authority_mismatch());
        }
        Ok(Self {
            route,
            selected,
            request_digest: connect.request_digest,
        })
    }

    pub(crate) fn literal(
        route: HttpRoute,
        connect: PreparedConnect,
    ) -> Result<Self, TransportError> {
        let selected = connect.target.ip().ok_or_else(authority_mismatch)?;
        if route.target != connect.target {
            return Err(authority_mismatch());
        }
        Ok(Self {
            route,
            selected,
            request_digest: connect.request_digest,
        })
    }

    pub fn route(&self) -> &HttpRoute {
        &self.route
    }
    pub fn target(&self) -> &NetworkTarget {
        &self.route.target
    }
    pub fn selected(&self) -> IpAddr {
        self.selected
    }
    pub fn request_digest(&self) -> &HttpRequestDigest {
        &self.request_digest
    }
}

pub trait Resolver: Send + Sync {
    fn resolve(
        &self,
        target: NetworkTarget,
        deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    name: String,
    value: Vec<u8>,
}

impl HttpHeader {
    pub fn new(name: &str, value: Vec<u8>) -> Result<Self, ForeignValueError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value.iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
        {
            return Err(ForeignValueError::InvalidTransport);
        }
        Ok(Self {
            name: name.to_ascii_lowercase(),
            value,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<HttpHeader>,
    body: Vec<u8>,
}

impl HttpRequest {
    pub fn get(path: &str) -> Result<Self, ForeignValueError> {
        Self::new("GET", path, Vec::new(), Vec::new())
    }
    pub fn new(
        method: &str,
        path: &str,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    ) -> Result<Self, ForeignValueError> {
        if method.is_empty()
            || !method.bytes().all(|byte| byte.is_ascii_uppercase())
            || !path.starts_with('/')
            || path.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
        {
            return Err(ForeignValueError::InvalidTransport);
        }
        Ok(Self {
            method: method.to_owned(),
            path: path.to_owned(),
            headers,
            body,
        })
    }
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn headers(&self) -> &[HttpHeader] {
        &self.headers
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

pub type HttpBody = BoxStream<'static, Result<Vec<u8>, TransportError>>;

pub enum OneHopResponse {
    #[non_exhaustive]
    Response {
        status: u16,
        headers: Vec<HttpHeader>,
        body: HttpBody,
        delivery: DeliveryCertainty,
    },
    #[non_exhaustive]
    Redirect {
        status: u16,
        headers: Vec<HttpHeader>,
        location: Vec<u8>,
        delivery: DeliveryCertainty,
    },
}

impl OneHopResponse {
    pub fn response(status: u16, headers: Vec<HttpHeader>, body: Vec<u8>) -> Self {
        Self::streaming_response(
            status,
            headers,
            Box::pin(futures::stream::once(futures::future::ready(Ok(body)))),
        )
    }

    pub fn streaming_response(status: u16, headers: Vec<HttpHeader>, body: HttpBody) -> Self {
        Self::Response {
            status,
            headers,
            body,
            delivery: DeliveryCertainty::PossiblySent,
        }
    }
    pub fn redirect(status: u16, headers: Vec<HttpHeader>, location: Vec<u8>) -> Self {
        Self::Redirect {
            status,
            headers,
            location,
            delivery: DeliveryCertainty::PossiblySent,
        }
    }
    pub fn status(&self) -> u16 {
        match self {
            Self::Response { status, .. } | Self::Redirect { status, .. } => *status,
        }
    }
    pub fn delivery(&self) -> DeliveryCertainty {
        match self {
            Self::Response { delivery, .. } | Self::Redirect { delivery, .. } => *delivery,
        }
    }
    pub fn body(&mut self) -> Option<&mut HttpBody> {
        match self {
            Self::Response { body, .. } => Some(body),
            Self::Redirect { .. } => None,
        }
    }
}

impl fmt::Debug for OneHopResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response {
                status,
                headers,
                delivery,
                ..
            } => f
                .debug_struct("Response")
                .field("status", status)
                .field("headers", headers)
                .field("body", &"<stream>")
                .field("delivery", delivery)
                .finish(),
            Self::Redirect {
                status,
                headers,
                location,
                delivery,
            } => f
                .debug_struct("Redirect")
                .field("status", status)
                .field("headers", headers)
                .field("location", location)
                .field("delivery", delivery)
                .finish(),
        }
    }
}

pub trait OneHop: Send + Sync {
    fn execute(
        &self,
        connect: ResolvedConnect,
        request: HttpRequest,
        deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>>;
}

#[derive(Clone)]
pub struct GuardedHttp {
    resolver: Arc<dyn Resolver>,
    one_hop: Arc<dyn OneHop>,
}

impl GuardedHttp {
    pub fn new(resolver: Arc<dyn Resolver>, one_hop: Arc<dyn OneHop>) -> Self {
        Self { resolver, one_hop }
    }

    pub fn execute_admitted(
        &self,
        route: HttpRoute,
        connect: PreparedConnect,
        request: HttpRequest,
        deadline: Option<std::time::Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        if route.target != connect.target || !connect.matches(&request) {
            return Box::pin(futures::future::ready(Err(authority_mismatch())));
        }

        let resolver = Arc::clone(&self.resolver);
        let one_hop = Arc::clone(&self.one_hop);
        Box::pin(async move {
            let resolved = if connect.target.ip().is_some() {
                ResolvedConnect::literal(route, connect)?
            } else {
                let resolution = resolver.resolve(connect.target.clone(), deadline).await?;
                let selected = resolution
                    .addresses
                    .first()
                    .copied()
                    .ok_or_else(authority_mismatch)?;
                ResolvedConnect::from_resolution(route, connect, resolution, selected)?
            };
            one_hop.execute(resolved, request, deadline).await
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    failure: TransportFailure,
    delivery: DeliveryCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFailure {
    AuthoritySpent,
    AuthorityMismatch,
    Offline,
    Resolution(String),
    InvalidRequest(String),
    Connect(String),
    Exchange(String),
    ResponseBody(String),
    Cancelled,
    TimedOut,
}

impl TransportError {
    pub fn definitely_unsent(failure: TransportFailure) -> Self {
        Self {
            failure,
            delivery: DeliveryCertainty::DefinitelyUnsent,
        }
    }

    pub fn possibly_sent(failure: TransportFailure) -> Self {
        Self {
            failure,
            delivery: DeliveryCertainty::PossiblySent,
        }
    }

    pub fn failure(&self) -> &TransportFailure {
        &self.failure
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

fn authority_mismatch() -> TransportError {
    TransportError::definitely_unsent(TransportFailure::AuthorityMismatch)
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "foreign transport failed: {:?} ({:?})",
            self.failure, self.delivery
        )
    }
}
impl std::error::Error for TransportError {}

pub type DuplexHandle = Box<dyn DuplexSession>;

pub trait DuplexSpawner: Send + Sync {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>>;
}

pub trait DuplexSession: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    fn delivery(&self) -> DeliveryCertainty;
    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement>;
    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplexEnd {
    Exited { code: i32 },
    Signalled { signal: Option<i32> },
    Cancelled,
    TimedOut { timeout: Duration },
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupCertainty {
    Complete,
    Uncertain,
}

#[derive(Debug)]
pub struct DuplexSettlement {
    end: DuplexEnd,
    cleanup: CleanupCertainty,
    delivery: DeliveryCertainty,
    evidence: Result<ToolResult, EvidenceError>,
}

impl DuplexSettlement {
    pub fn new(
        end: DuplexEnd,
        cleanup: CleanupCertainty,
        delivery: DeliveryCertainty,
        evidence: Result<ToolResult, EvidenceError>,
    ) -> Self {
        Self {
            end,
            cleanup,
            delivery,
            evidence,
        }
    }
    pub fn end(&self) -> &DuplexEnd {
        &self.end
    }
    pub fn cleanup(&self) -> &CleanupCertainty {
        &self.cleanup
    }
    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
    pub fn evidence(&self) -> &Result<ToolResult, EvidenceError> {
        &self.evidence
    }
}
