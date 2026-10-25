use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{CanonPath, TransportError, TransportFailure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailure {
    Absent,
    InvalidValue,
    InvalidResponse,
    Authority,
    Transport,
    Protocol,
    Authorization,
    Credential,
    Cancelled,
}

#[derive(Debug)]
pub struct McpError {
    failure: McpFailure,
    message: String,
    delivery: Option<DeliveryCertainty>,
    denied_executable: Option<CanonPath>,
}

impl McpError {
    pub fn new(failure: McpFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            delivery: None,
            denied_executable: None,
        }
    }

    pub fn with_delivery(
        failure: McpFailure,
        message: impl Into<String>,
        delivery: DeliveryCertainty,
    ) -> Self {
        Self {
            failure,
            message: message.into(),
            delivery: Some(delivery),
            denied_executable: None,
        }
    }

    pub(crate) fn denied_executable(&self) -> Option<&CanonPath> {
        self.denied_executable.as_ref()
    }

    pub fn failure(&self) -> McpFailure {
        self.failure
    }

    pub fn delivery(&self) -> Option<DeliveryCertainty> {
        self.delivery
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpError {}

impl From<TransportError> for McpError {
    fn from(error: TransportError) -> Self {
        let delivery = error.delivery();
        let mut result = Self::with_delivery(McpFailure::Transport, error.to_string(), delivery);
        if let TransportFailure::ExecutableDenied(path) = error.failure() {
            result.denied_executable = Some(path.clone());
        }
        result
    }
}

impl From<serde_json::Error> for McpError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(McpFailure::InvalidResponse, error.to_string())
    }
}
