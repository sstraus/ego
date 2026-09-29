//! One non-serializable authority joining safe MCP identity with live transport values.

use std::ffi::OsString;
use std::fmt;

use ego_core::config::{McpEnvironmentName, McpHttpAuth, McpServerConfig, McpServerName};
use ego_core::credentials::SecretMaterial;
use ego_core::permit::{EnvName, ExplicitEnvBinding};

use crate::{McpError, McpFailure};

pub struct McpEnvironmentBinding {
    name: McpEnvironmentName,
    value: EnvironmentValue,
}

enum EnvironmentValue {
    Utf8(SecretMaterial),
    Native(ExplicitEnvBinding),
}

impl McpEnvironmentBinding {
    pub fn new(name: McpEnvironmentName, value: SecretMaterial) -> Self {
        Self {
            name,
            value: EnvironmentValue::Utf8(value),
        }
    }

    pub fn from_native(name: McpEnvironmentName, value: OsString) -> Result<Self, McpError> {
        let native_name =
            EnvName::new(name.as_str()).map_err(|error| invalid(error.to_string()))?;
        let value = ExplicitEnvBinding::from_native(native_name, value)
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            name,
            value: EnvironmentValue::Native(value),
        })
    }

    pub fn name(&self) -> &McpEnvironmentName {
        &self.name
    }

    pub(crate) fn explicit_binding(&self) -> Result<ExplicitEnvBinding, McpError> {
        match &self.value {
            EnvironmentValue::Utf8(value) => {
                std::str::from_utf8(value.expose_secret_bytes())
                    .map_err(|_| invalid("MCP environment material is not valid UTF-8"))?;
                let name =
                    EnvName::new(self.name.as_str()).map_err(|error| invalid(error.to_string()))?;
                Ok(ExplicitEnvBinding::new(
                    name,
                    SecretMaterial::new(value.expose_secret_bytes().to_vec()),
                ))
            }
            EnvironmentValue::Native(binding) => Ok(binding.clone()),
        }
    }

    fn has_valid_foreign_encoding(&self) -> bool {
        match &self.value {
            EnvironmentValue::Utf8(value) => {
                std::str::from_utf8(value.expose_secret_bytes()).is_ok()
            }
            EnvironmentValue::Native(_) => true,
        }
    }
}

impl fmt::Debug for McpEnvironmentBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpEnvironmentBinding")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

pub struct McpHeaderBinding {
    name: String,
    value: SecretMaterial,
}

impl McpHeaderBinding {
    pub fn new(name: impl Into<String>, value: SecretMaterial) -> Result<Self, McpError> {
        let name = name.into();
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let valid_value = !value
            .expose_secret_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\r' | b'\n' | 0));
        let name = name.to_ascii_lowercase();
        let wire_owned = matches!(
            name.as_str(),
            "accept"
                | "connection"
                | "content-length"
                | "content-type"
                | "host"
                | "mcp-protocol-version"
                | "mcp-session-id"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        );
        if !valid_name || !valid_value || wire_owned {
            return Err(invalid(
                "MCP HTTP header material is invalid or protocol-owned",
            ));
        }
        Ok(Self { name, value })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &SecretMaterial {
        &self.value
    }
}

impl fmt::Debug for McpHeaderBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpHeaderBinding")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

pub struct McpTransportMaterial {
    transport: TransportMaterial,
}

enum TransportMaterial {
    Stdio(Vec<McpEnvironmentBinding>),
    Http(Vec<McpHeaderBinding>),
    Acp,
}

impl McpTransportMaterial {
    pub fn stdio(environment: Vec<McpEnvironmentBinding>) -> Self {
        Self {
            transport: TransportMaterial::Stdio(environment),
        }
    }

    pub fn http(headers: Vec<McpHeaderBinding>) -> Self {
        Self {
            transport: TransportMaterial::Http(headers),
        }
    }

    pub fn acp() -> Self {
        Self {
            transport: TransportMaterial::Acp,
        }
    }
}

impl fmt::Debug for McpTransportMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.transport {
            TransportMaterial::Stdio(environment) => formatter
                .debug_tuple("McpTransportMaterial::Stdio")
                .field(environment)
                .finish(),
            TransportMaterial::Http(headers) => formatter
                .debug_tuple("McpTransportMaterial::Http")
                .field(headers)
                .finish(),
            TransportMaterial::Acp => formatter.write_str("McpTransportMaterial::Acp"),
        }
    }
}

pub struct McpServerAdmission {
    name: McpServerName,
    config: McpServerConfig,
    material: McpTransportMaterial,
}

impl McpServerAdmission {
    pub fn new(
        name: McpServerName,
        config: McpServerConfig,
        mut material: McpTransportMaterial,
    ) -> Result<Self, McpError> {
        config
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        match (&config, &mut material.transport) {
            (McpServerConfig::Stdio(config), TransportMaterial::Stdio(environment)) => {
                if environment
                    .iter()
                    .any(|binding| !binding.has_valid_foreign_encoding())
                {
                    return Err(invalid("MCP environment material is not valid UTF-8"));
                }
                environment.sort_by(|left, right| {
                    left.name
                        .as_str()
                        .to_ascii_lowercase()
                        .cmp(&right.name.as_str().to_ascii_lowercase())
                        .then_with(|| left.name.cmp(&right.name))
                });
                if environment.windows(2).any(|pair| {
                    pair[0]
                        .name
                        .as_str()
                        .eq_ignore_ascii_case(pair[1].name.as_str())
                }) {
                    return Err(invalid("duplicate MCP environment material"));
                }
                if config.inherit_env.iter().any(|configured| {
                    !environment.iter().any(|binding| {
                        binding
                            .name
                            .as_str()
                            .eq_ignore_ascii_case(configured.as_str())
                    })
                }) {
                    return Err(invalid(
                        "MCP admission is missing configured environment material",
                    ));
                }
            }
            (McpServerConfig::Http(config), TransportMaterial::Http(headers)) => {
                headers.sort_by(|left, right| left.name.cmp(&right.name));
                if headers.windows(2).any(|pair| pair[0].name == pair[1].name) {
                    return Err(invalid("duplicate MCP HTTP header material"));
                }
                if config.auth == McpHttpAuth::Oauth
                    && !headers.iter().any(|header| header.name == "authorization")
                {
                    return Err(invalid(
                        "OAuth MCP admission has no authorization header material",
                    ));
                }
            }
            (McpServerConfig::Acp { .. }, TransportMaterial::Acp) => {}
            (McpServerConfig::Stdio(_), TransportMaterial::Http(_))
            | (McpServerConfig::Http(_), TransportMaterial::Stdio(_))
            | (McpServerConfig::Acp { .. }, _)
            | (_, TransportMaterial::Acp) => {
                return Err(invalid(
                    "MCP transport material does not match server configuration",
                ));
            }
        }
        Ok(Self {
            name,
            config,
            material,
        })
    }

    pub fn name(&self) -> &McpServerName {
        &self.name
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub(crate) fn stdio_environment(&self) -> Option<&[McpEnvironmentBinding]> {
        match &self.material.transport {
            TransportMaterial::Stdio(environment) => Some(environment),
            TransportMaterial::Http(_) | TransportMaterial::Acp => None,
        }
    }

    pub(crate) fn http_headers(&self) -> Option<&[McpHeaderBinding]> {
        match &self.material.transport {
            TransportMaterial::Http(headers) => Some(headers),
            TransportMaterial::Stdio(_) | TransportMaterial::Acp => None,
        }
    }
}

impl fmt::Debug for McpServerAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerAdmission")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("material", &self.material)
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::InvalidValue, message)
}
