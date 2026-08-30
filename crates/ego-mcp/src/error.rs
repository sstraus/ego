use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::TransportError;

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
}

impl McpError {
    pub fn new(failure: McpFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            delivery: None,
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
        }
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
        Self::with_delivery(McpFailure::Transport, error.to_string(), delivery)
    }
}

impl From<serde_json::Error> for McpError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(McpFailure::InvalidResponse, error.to_string())
    }
}
