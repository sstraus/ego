//! SigV4 is applied to the final HTTP request, before guarded admission.
use std::time::SystemTime;

use aws_credential_types::Credentials as AwsCredentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4::SigningParams;
use ego_core::config::ConnectionConfig;
use ego_core::credentials::SecretMaterial;
use ego_core::permit::HttpHeader;
use serde::Deserialize;
use url::Url;

use super::{CredentialStrategy, Secret, bedrock_runtime_base};
use crate::http::HttpRequest;

const SERVICE: &str = "bedrock";

pub(crate) fn strategy(
    connection: &ConnectionConfig,
    endpoint: &Url,
) -> Result<CredentialStrategy, String> {
    let Some(var) = &connection.key_env else {
        return Err(
            "bedrock-runtime-sigv4 requires an explicit credential-process JSON key_env".into(),
        );
    };
    if runtime_region(endpoint).is_none() {
        return Err(
            "bedrock-runtime-sigv4 requires a regional HTTPS bedrock-runtime AWS root".into(),
        );
    }
    Ok(CredentialStrategy::BedrockSigv4 {
        var: var.clone(),
        endpoint: endpoint.clone(),
    })
}

fn runtime_region(endpoint: &Url) -> Option<&str> {
    let region = endpoint
        .host_str()?
        .strip_prefix("bedrock-runtime.")?
        .strip_suffix(".amazonaws.com")?;
    (bedrock_runtime_base(region).as_ref() == Some(endpoint)).then_some(region)
}

/// AWS credential_process version 1 format. Borrow the fields from the
/// zeroizing input, then transfer them to the AWS credential owner's storage.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CredentialDocument<'a> {
    version: u8,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    session_token: Option<&'a str>,
    expiration: Option<&'a str>,
}

pub(crate) struct BedrockSigner {
    endpoint: Url,
    region: String,
    credentials: AwsCredentials,
}

impl BedrockSigner {
    /// A provider can reflect both complete signed headers and their fields.
    /// Use this attempt's signature and the credentials that actually signed it.
    pub(crate) fn redact_diagnostic(&self, diagnostic: &mut String, headers: &[HttpHeader]) {
        let authorization = headers
            .iter()
            .find(|header| header.name() == "authorization")
            .and_then(|header| std::str::from_utf8(header.value()).ok());
        let signature = authorization
            .and_then(|header| header.rsplit_once(", Signature="))
            .map(|(_, signature)| signature);
        let mut values = [
            authorization,
            signature,
            self.credentials.session_token(),
            Some(self.credentials.secret_access_key()),
            Some(self.credentials.access_key_id()),
        ];
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.map_or(0, str::len)));
        for value in values
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
        {
            if diagnostic.contains(value) {
                *diagnostic = diagnostic.replace(value, "[REDACTED]");
            }
        }
    }

    pub(super) fn resolve(endpoint: &Url, secret: &Secret) -> Option<Self> {
        let region = runtime_region(endpoint)?.to_owned();
        let doc: CredentialDocument<'_> =
            serde_json::from_slice(secret.expose_secret_bytes()).ok()?;
        let valid = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic());
        if doc.version != 1
            || !doc.access_key_id.bytes().all(|b| b.is_ascii_alphanumeric())
            || !valid(doc.access_key_id)
            || !valid(doc.secret_access_key)
            || doc.session_token.is_some_and(|value| !valid(value))
            || (doc.session_token.is_some() && doc.expiration.is_none())
        {
            return None;
        }
        let expiration = if let Some(raw) = doc.expiration {
            let parsed =
                time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
                    .ok()?;
            let expiration = SystemTime::from(parsed);
            if expiration <= SystemTime::now() {
                return None;
            }
            Some(expiration)
        } else {
            None
        };
        Some(Self {
            endpoint: endpoint.clone(),
            region,
            credentials: AwsCredentials::new(
                doc.access_key_id,
                doc.secret_access_key,
                doc.session_token.map(str::to_owned),
                expiration,
                "ego scoped Bedrock connection",
            ),
        })
    }

    pub(crate) fn sign(
        &self,
        request: &mut HttpRequest,
        now: SystemTime,
    ) -> Result<(), &'static str> {
        if request.url.origin() != self.endpoint.origin() {
            return Err("SigV4 request leaves the captured Bedrock runtime origin");
        }
        if self
            .credentials
            .expiry()
            .is_some_and(|expiry| expiry <= now)
        {
            return Err("AWS signing credentials expired; renew the named connection credentials");
        }
        let headers = signing_headers(&self.credentials, &self.region, SERVICE, request, now)?;
        request.headers.extend(headers);
        Ok(())
    }
}

fn signing_headers(
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    request: &HttpRequest,
    now: SystemTime,
) -> Result<Vec<HttpHeader>, &'static str> {
    const FAILED: &str = "cannot sign the final Bedrock request with AWS SigV4";
    let identity = credentials.clone().into();
    let params = SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(now)
        .settings(SigningSettings::default())
        .build()
        .map_err(|_| FAILED)?
        .into();
    let headers = request
        .headers
        .iter()
        .map(|header| {
            std::str::from_utf8(header.value())
                .map(|value| (header.name(), value))
                .map_err(|_| FAILED)
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The signer can trace its input. Pass the hash so even its explicit body
    // logging option cannot reveal prompts or tool results.
    let payload_hash = ego_core::digest::sha256_hex(request.body.as_deref().unwrap_or_default());
    let signed = SignableRequest::new(
        request.method.as_str(),
        request.url.as_str(),
        headers.into_iter(),
        SignableBody::Precomputed(payload_hash),
    )
    .map_err(|_| FAILED)?;
    let (instructions, _) = sign(signed, &params).map_err(|_| FAILED)?.into_parts();
    instructions
        .headers()
        .map(|(name, value)| {
            HttpHeader::secret(name, SecretMaterial::new(value.as_bytes().to_vec()))
                .map_err(|_| FAILED)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;

    // Catches losing the session token or changing the canonical signature.
    #[test]
    fn matches_aws_sigv4_published_session_token_vector() {
        // AWS smithy-rs aws-signing-test-suite/v4/get-vanilla-with-session-token.
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some(token.into()),
            None,
            "AWS published test vector",
        );
        let request = HttpRequest {
            url: Url::parse("https://example.amazonaws.com/").unwrap(),
            method: Method::Get,
            headers: vec![],
            body: None,
        };
        let now = httpdate::parse_http_date("Sun, 30 Aug 2015 12:36:00 GMT").unwrap();
        let headers = signing_headers(&credentials, "us-east-1", "service", &request, now).unwrap();
        let value = |name| {
            headers
                .iter()
                .find(|header| header.name() == name)
                .unwrap()
                .value()
        };
        assert_eq!(value("x-amz-security-token"), token.as_bytes());
        assert_eq!(value("x-amz-date"), b"20150830T123600Z");
        assert_eq!(value("authorization"), b"AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8");
    }
}
