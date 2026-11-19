//! Explicit provider-only HTTP CONNECT routing; never reads proxy environment variables.

use ego_core::permit::{NetworkTarget, TransportError, TransportFailure};
use rustls::pki_types::pem::PemObject as _;

/// Invocation-owned HTTP proxy endpoint and optional additional TLS trust anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProxy {
    target: NetworkTarget,
    ca_pem: Option<Vec<u8>>,
}

impl std::str::FromStr for ProviderProxy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            "--proxy requires an http://HOST URL (default port 80) without credentials, path, query or fragment"
                .to_owned()
        };
        let url = url::Url::parse(value).map_err(|_| invalid())?;
        if url.scheme() != "http"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let target = NetworkTarget::new(
            url.host_str().ok_or_else(invalid)?,
            url.port_or_known_default().ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        Ok(Self {
            target,
            ca_pem: None,
        })
    }
}

impl ProviderProxy {
    /// Add one PEM certificate, scoped to provider requests for this invocation.
    pub fn with_ca_pem(mut self, pem: Vec<u8>) -> Result<Self, String> {
        let invalid = || "--proxy-ca must contain one valid PEM certificate".to_owned();
        let mut certificates = rustls::pki_types::CertificateDer::pem_slice_iter(&pem);
        let certificate = certificates
            .next()
            .ok_or_else(invalid)?
            .map_err(|_| invalid())?;
        if certificates.next().is_some() {
            return Err(invalid());
        }
        // reqwest::Certificate::from_pem defers DER validation to client build.
        // RootCertStore validates now, before this invocation opens any socket.
        rustls::RootCertStore::empty()
            .add(certificate)
            .map_err(|_| invalid())?;
        self.ca_pem = Some(pem);
        Ok(self)
    }

    pub(super) fn target(&self) -> &NetworkTarget {
        &self.target
    }

    pub(super) fn configure(
        &self,
        builder: reqwest::ClientBuilder,
        socket: std::net::SocketAddr,
    ) -> Result<reqwest::ClientBuilder, TransportError> {
        let invalid = || {
            TransportError::definitely_unsent(TransportFailure::InvalidRequest(
                "invalid explicit provider proxy configuration".to_owned(),
            ))
        };
        // A literal socket prevents reqwest or the proxy connector from doing
        // another DNS lookup. CONNECT retains the original provider URL/SNI.
        let proxy = reqwest::Proxy::https(format!("http://{socket}")).map_err(|_| invalid())?;
        let builder = builder.proxy(proxy);
        match &self.ca_pem {
            Some(pem) => Ok(builder
                .tls_certs_merge([reqwest::Certificate::from_pem(pem).map_err(|_| invalid())?])),
            None => Ok(builder),
        }
    }
}

#[cfg(test)]
mod tests;
