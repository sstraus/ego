use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{ExecutableDenial, TransportError, TransportFailure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailure {
    Absent,
    InvalidValue,
    InvalidResponse,
    Authority,
    Transport,
    Protocol,
    /// The server does not speak the protocol version ego requires.
    ProtocolVersion,
    Authorization,
    Credential,
    Cancelled,
    /// The server did not finish admission before its deadline.
    Timeout,
}

#[derive(Debug)]
pub struct McpError {
    failure: McpFailure,
    message: String,
    delivery: Option<DeliveryCertainty>,
    denied_executable: Option<ExecutableDenial>,
    cancelled_before_claim: bool,
}

impl McpError {
    pub fn new(failure: McpFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            delivery: None,
            denied_executable: None,
            cancelled_before_claim: false,
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
            cancelled_before_claim: false,
        }
    }

    pub(crate) fn mark_cancelled_before_claim(mut self) -> Self {
        self.cancelled_before_claim = true;
        self
    }

    pub(crate) fn cancelled_before_claim(&self) -> bool {
        self.cancelled_before_claim
    }

    pub(crate) fn denied_executable(&self) -> Option<&ExecutableDenial> {
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
        let mut result =
            Self::with_delivery(McpFailure::Transport, transport_sentence(&error), delivery);
        if let TransportFailure::ExecutableDenied(denial) = error.failure() {
            result.denied_executable = Some(denial.clone());
        }
        result
    }
}

impl From<serde_json::Error> for McpError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(McpFailure::InvalidResponse, error.to_string())
    }
}

/// The transport failure in words the model can act on. `TransportError`'s own
/// text is a `Debug` dump, which the native web tools pin in their tests.
fn transport_sentence(error: &TransportError) -> String {
    match error.failure() {
        TransportFailure::AuthoritySpent => {
            "the permission for this request was already used".to_owned()
        }
        TransportFailure::AuthorityMismatch => {
            "the request does not match the permission that was granted".to_owned()
        }
        TransportFailure::ExecutableDenied(denial) => denial.to_string(),
        TransportFailure::Offline => "HTTP is unavailable under offline confinement".to_owned(),
        TransportFailure::Resolution(detail) => {
            format!("the server name could not be resolved: {detail}")
        }
        TransportFailure::InvalidRequest(detail) => {
            format!("the request could not be built: {detail}")
        }
        TransportFailure::Connect(detail) => {
            format!("the connection to the server failed: {detail}")
        }
        TransportFailure::Exchange(detail) => format!("the HTTP exchange failed: {detail}"),
        TransportFailure::ResponseBody(detail) => {
            format!("the response body could not be read: {detail}")
        }
        TransportFailure::IdleTimeout => "the server stopped sending data for too long".to_owned(),
        TransportFailure::Cancelled => "the request was cancelled".to_owned(),
        TransportFailure::TimedOut => "the request timed out".to_owned(),
    }
}
