//! The two cloud-platform rows: Claude on Amazon Bedrock and on Google Cloud
//! Vertex AI, and the addresses their deployments name.

use ego_core::permit::HttpHeader;
use url::Url;

use super::{
    CostExpectation, CredentialStrategy, Credentials, MissingCredential, PriceTableName, Promotion,
    ProviderRow, admitted_base, secret_text,
};
use crate::http::bearer_header;
use crate::resolver::Supplied;
use crate::wire::partner_messages;

/// The Amazon Bedrock row id.
pub const BEDROCK: &str = "bedrock";
/// The environment variable an Amazon Bedrock API key is read from: the name
/// the AWS SDKs read a Bedrock bearer token from.
pub const BEDROCK_KEY_VAR: &str = "AWS_BEARER_TOKEN_BEDROCK";
/// The region used when the environment names none.
pub const BEDROCK_DEFAULT_REGION: &str = "us-east-1";
/// The Google Cloud Vertex AI row id.
pub const VERTEX: &str = "vertex";
/// The environment variable that names the Google Cloud project.
pub const VERTEX_PROJECT_VAR: &str = "GOOGLE_CLOUD_PROJECT";
/// The environment variable that names the Vertex AI location.
pub const VERTEX_LOCATION_VAR: &str = "GOOGLE_CLOUD_LOCATION";
/// The location used when the environment names none: Google's recommended
/// global endpoint, the one Claude 4.7 and later models serve.
pub const VERTEX_DEFAULT_LOCATION: &str = "global";
/// The Vertex AI API root, before a project and location are known.
pub const VERTEX_BASE: &str = "https://aiplatform.googleapis.com/v1/";

static BEDROCK_SUPPLIED: Supplied = Supplied::new(&partner_messages::BEDROCK);
static VERTEX_SUPPLIED: Supplied = Supplied::new(&partner_messages::VERTEX);

/// A region, project or location value, if it is one DNS label or path
/// segment of the characters the clouds use.
///
/// These values become part of a host name or a path. A value carrying `/`,
/// `#` or `@`, or a `.`/`..` segment, would move the request to another host
/// or resource, so it is refused rather than encoded.
fn cloud_name<'a>(value: &'a str, extra: &[char]) -> Option<&'a str> {
    let value = value.trim();
    (!value.is_empty()
        && !value.starts_with(['-', '.'])
        && value.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || extra.contains(&c)
        }))
    .then_some(value)
}

/// The Claude Messages base on Amazon Bedrock for `region`.
///
/// `https://bedrock-mantle.{region}.api.aws/anthropic/v1/`, the endpoint
/// Anthropic and AWS document for Claude 4.7 and later models.
pub fn bedrock_base(region: &str) -> Option<Url> {
    let region = cloud_name(region, &[])?;
    Url::parse(&format!(
        "https://bedrock-mantle.{region}.api.aws/anthropic/v1/"
    ))
    .ok()
}

/// Which part of a Vertex AI address a refused value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexAddressPart {
    Project,
    Location,
}

/// The Vertex AI base for one project and location.
///
/// Google documents three host forms: `global` uses
/// `aiplatform.googleapis.com`, the `us` and `eu` multi-regions use
/// `aiplatform.{location}.rep.googleapis.com`, and a region uses
/// `{location}-aiplatform.googleapis.com`. The path names the project and
/// the location in every form.
pub fn vertex_base(project: &str, location: &str) -> Result<Url, VertexAddressPart> {
    // A domain-scoped project id is `example.com:project`.
    let project = cloud_name(project, &['.', ':']).ok_or(VertexAddressPart::Project)?;
    let location = cloud_name(location, &[]).ok_or(VertexAddressPart::Location)?;
    let host = match location {
        "global" => "aiplatform.googleapis.com".to_string(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        region => format!("{region}-aiplatform.googleapis.com"),
    };
    Url::parse(&format!(
        "https://{host}/v1/projects/{project}/locations/{location}/"
    ))
    .map_err(|_| VertexAddressPart::Project)
}

/// [`CredentialStrategy::GoogleCloud`], resolved: the bearer token and the
/// project base its requests go to.
pub(super) fn google_cloud_credential(
    creds: &dyn Credentials,
    provider: &str,
    project_var: &str,
    location_var: &str,
) -> Result<(Vec<HttpHeader>, Option<Url>), MissingCredential> {
    let project = creds.secret(project_var)?;
    let location = creds.secret(location_var).ok();
    let location = match &location {
        Some(location) => secret_text(location)?,
        None => VERTEX_DEFAULT_LOCATION,
    };
    let base = vertex_base(secret_text(&project)?, location).map_err(|part| MissingCredential {
        var: match part {
            VertexAddressPart::Project => project_var.to_string(),
            VertexAddressPart::Location => location_var.to_string(),
        },
    })?;
    let secret = creds
        .secret(&format!("oauth:{provider}"))
        .map_err(|_| MissingCredential {
            var: format!("ego auth login {provider}"),
        })?;
    Ok((
        vec![bearer_header(secret.expose_secret_bytes())],
        Some(base),
    ))
}

impl ProviderRow {
    /// Claude in Amazon Bedrock, on the Messages endpoint at `base`.
    ///
    /// The credential is an Amazon Bedrock API key, which the endpoint reads
    /// from `x-api-key` like the first-party API. AWS documents both long-term
    /// keys and short-term tokens that `aws-bedrock-token-generator` mints
    /// from IAM credentials; both arrive in the same variable.
    ///
    /// Selectable on the published contract
    /// (`ARCHITECTURE.md`).
    /// No request has been recorded against Bedrock yet; a wrong body comes
    /// back as a 400 with the provider's own text.
    pub fn bedrock(base: Url) -> Self {
        let (base, http_profile) = admitted_base(BEDROCK, base);
        ProviderRow {
            id: BEDROCK.to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            // The table files Claude on the Bedrock Messages endpoint as
            // `bedrock_mantle/<id>` (measured 2026-11-05).
            price_table: Some(PriceTableName {
                provider: "bedrock_mantle",
                key_prefix: "bedrock_mantle",
            }),
            cost_expectation: CostExpectation::Positive,
            resolver: &BEDROCK_SUPPLIED,
            credential: CredentialStrategy::HeaderFromEnv {
                header: "x-api-key".to_string(),
                var: BEDROCK_KEY_VAR.to_string(),
            },
            // Usage dimensions and no monetary total, as on the first-party API.
            cost: None,
            promotion: Promotion::Selectable,
        }
    }

    /// The Bedrock row in the default region.
    pub fn bedrock_default() -> Self {
        Self::bedrock(bedrock_base(BEDROCK_DEFAULT_REGION).expect("a constant this file controls"))
    }

    /// Claude Models on Google Cloud, on Vertex AI.
    ///
    /// The row base is the API root. Each resolved credential supplies the
    /// project and location base its requests go to
    /// ([`CredentialStrategy::GoogleCloud`]).
    ///
    /// Selectable on the published contract, like [`bedrock`](Self::bedrock).
    pub fn vertex() -> Self {
        let (base, http_profile) = admitted_base(
            VERTEX,
            Url::parse(VERTEX_BASE).expect("a constant this file controls"),
        );
        ProviderRow {
            id: VERTEX.to_string(),
            base,
            http_profile,
            extra_headers: Vec::new(),
            // `vertex_ai/<id>` in the `vertex_ai-anthropic_models` namespace
            // (measured 2026-11-05).
            price_table: Some(PriceTableName {
                provider: "vertex_ai-anthropic_models",
                key_prefix: "vertex_ai",
            }),
            cost_expectation: CostExpectation::Positive,
            resolver: &VERTEX_SUPPLIED,
            credential: CredentialStrategy::GoogleCloud {
                provider: VERTEX.to_string(),
                project_var: VERTEX_PROJECT_VAR.to_string(),
                location_var: VERTEX_LOCATION_VAR.to_string(),
            },
            cost: None,
            promotion: Promotion::Selectable,
        }
    }
}
