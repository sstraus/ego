use ego_core::permit::TransportError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailure {
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
}

impl McpError {
    pub(crate) fn new(failure: McpFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
        }
    }

    pub fn failure(&self) -> McpFailure {
        self.failure
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
        Self::new(McpFailure::Transport, error.to_string())
    }
}

impl From<serde_json::Error> for McpError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(McpFailure::InvalidResponse, error.to_string())
    }
}
