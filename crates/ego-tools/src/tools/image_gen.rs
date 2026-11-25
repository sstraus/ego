//! `image_gen`: one generated image, written to the workspace.
//!
//! The model-facing contract is the one the Codex `image_gen` tool has: a
//! prompt in, an image out. The mechanism differs on purpose. Codex lets the
//! active provider run a hosted tool inside the main model call, which works
//! only while that provider is the active model. This tool makes its own
//! one-shot call, so it works whichever model is active.
//!
//! Two backends, one tool:
//!
//! * **ChatGPT subscription** (preferred when `ego auth login openai-codex`
//!   has been done). A one-shot Responses request to the Codex backend with
//!   the hosted `image_generation` tool, read from the event stream as an
//!   `image_generation_call` item whose `result` is the base64 image. This
//!   is what Codex itself does with a ChatGPT login. No API key, no per-image
//!   charge; it uses the subscription's allowance.
//! * **API key** (`OPENAI_API_KEY`). OpenAI's Images API with `gpt-image-2`.
//!   Charged per image to that account, so it needs `allow_paid=true`.
//!
//! Two effects, both prepared before anything runs and both authorised
//! before execution:
//!
//! * `Op::Connect` to the backend host, bound to the exact request body, so
//!   the prompt that was approved is the prompt that is sent; and
//! * `Op::Write` of the output path with [`Change::Generated`], because the
//!   bytes do not exist until the response arrives (`prepare` runs before
//!   any permit and cannot compute them).
//!
//! Credentials never pass through this crate's types except as secret
//! headers: the subscription's come from a [`SubscriptionAccess`] that the
//! composition root implements over the provider layer, the API key from the
//! environment when the tool is built.
//!
//! Evidence: the Images API fields are from OpenAI's image-generation guide
//! read 2026-11-09. The subscription request follows Codex's own protocol
//! (`codex-rs/protocol/src/models.rs`, `ResponseItem::ImageGenerationCall`,
//! read 2026-11-09) and ego's `openai-codex` mapping. The story worklog records
//! two successful subscription probes on 2026-11-09. The API-key backend has
//! not been verified with a paid live request.
//!
//! DEFERRED (2026-11-09) — editing an existing image (`/v1/images/edits`).
//! The edit request carries the source file's bytes, which `prepare` cannot
//! read, so its request digest cannot be bound before authorisation. It needs
//! a read-then-bind step the permit seam does not have yet.
//!
//! DEFERRED (2026-11-09) — refreshing an expired subscription token. The
//! headers are bound at `prepare`, which is synchronous; the provider layer
//! refreshes asynchronously. An expired login is refused with the command to
//! renew it.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    Change, FileWriteEvidence, GuardedIo, HttpHeader, HttpRequest, HttpRoute, HttpScheme,
    NetworkTarget, OneHopResponse, Op, PrepareCtx, PreparedCall, ToolError, ToolResult, TypedTool,
};
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{resolve_target, wrong_op};

const API_HOST: &str = "api.openai.com";
const API_MODEL: &str = "gpt-image-2";
const API_PRODUCER: &str = "api.openai.com gpt-image-2";
const SUBSCRIPTION_PRODUCER: &str = "ChatGPT subscription image_generation";
const MEDIA_TYPE: &str = "image/png";
const KEY_VAR: &str = "OPENAI_API_KEY";
const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// A 3840x2160 PNG is a few megabytes of binary and a third more as base64.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(300);
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const SUBSCRIPTION_INSTRUCTIONS: &str = "You generate images. Call the image_generation tool exactly once for the user's request. Do not write any other text.";

/// Where the ChatGPT-subscription backend is and who is asking.
pub struct SubscriptionRoute {
    /// The full Responses endpoint, HTTPS.
    pub url: url::Url,
    /// Credential and client-identity headers. Secrets are flagged secret.
    pub headers: Vec<HttpHeader>,
    /// The model that hosts the image tool call.
    pub model: String,
    /// The hosting model's reasoning effort. `None` omits the field. Live probes
    /// (2026-11-09) showed the backend rejecting the image tool call with
    /// "Complexity '<effort>' is not supported for this image model" for `low`
    /// and `medium`, so the default is to send none.
    pub effort: Option<String>,
}

/// The composition root's view of the subscription login. Resolved at
/// `prepare` so the headers are part of the authorised, digest-bound call.
pub trait SubscriptionAccess: Send + Sync {
    /// `Err` carries a message for the model and the owner, with no secret.
    fn route(&self) -> Result<SubscriptionRoute, String>;
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageGenParams {
    /// What to draw. For a layout, quote any text verbatim and name the intended use.
    pub prompt: String,
    /// Where to write the PNG, absolute or relative to the working directory.
    /// The parent directory must exist and the file must not. Default: a new
    /// `generated-<id>.png` in the working directory.
    #[serde(default)]
    pub path: Option<String>,
    /// `auto` (default), `1024x1024`, `1536x1024`, `1024x1536`, or `WIDTHxHEIGHT`:
    /// both edges a multiple of 16, longest edge at most 3840, ratio at most 3:1,
    /// 655360 to 8294400 pixels in total. Used only with an OpenAI API key; the
    /// ChatGPT subscription backend chooses the size itself. The result reports
    /// the real width and height.
    #[serde(default)]
    pub size: Option<String>,
    /// `low` (fast draft), `medium`, `high`, or `auto` (default). Used only with an
    /// OpenAI API key; the ChatGPT subscription backend does not accept it.
    #[serde(default)]
    pub quality: Option<String>,
    /// Consent to a per-image charge. Required (true) only when the tool uses an
    /// OpenAI API key; ignored when it uses the ChatGPT subscription.
    #[serde(default)]
    pub allow_paid: bool,
}

enum Backend {
    ApiKey(ego_core::credentials::SecretMaterial),
    Subscription(Arc<dyn SubscriptionAccess>),
}

pub struct ImageGen {
    backend: Backend,
}

impl ImageGen {
    /// The Images API with an explicit key.
    pub fn new(key: ego_core::credentials::SecretMaterial) -> Self {
        Self {
            backend: Backend::ApiKey(key),
        }
    }

    /// The ChatGPT subscription, through the provider layer's login.
    pub fn with_subscription(access: Arc<dyn SubscriptionAccess>) -> Self {
        Self {
            backend: Backend::Subscription(access),
        }
    }

    /// `None` when `OPENAI_API_KEY` is unset or blank.
    pub fn from_environment() -> Option<Self> {
        let key = std::env::var(KEY_VAR).ok()?;
        if key.trim().is_empty() {
            return None;
        }
        Some(Self::new(ego_core::credentials::SecretMaterial::new(
            key.into_bytes(),
        )))
    }

    fn producer(&self) -> &'static str {
        match self.backend {
            Backend::ApiKey(_) => API_PRODUCER,
            Backend::Subscription(_) => SUBSCRIPTION_PRODUCER,
        }
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidParams(message.into())
}

fn failure(message: impl Into<String>) -> ToolError {
    ToolError::foreign(message, DeliveryCertainty::PossiblySent)
}

/// The `gpt-image-2` size rule: `auto`, or `WIDTHxHEIGHT` with both edges a
/// multiple of 16, the longest at most 3840, the ratio at most 3:1 and the
/// pixel count within 655,360 to 8,294,400.
fn valid_size(size: &str) -> bool {
    if size == "auto" {
        return true;
    }
    let Some((width, height)) = size.split_once('x') else {
        return false;
    };
    let (Ok(width), Ok(height)) = (width.parse::<u64>(), height.parse::<u64>()) else {
        return false;
    };
    let (long, short) = (width.max(height), width.min(height));
    short > 0
        && width.is_multiple_of(16)
        && height.is_multiple_of(16)
        && long <= 3840
        && long <= short * 3
        && (655_360..=8_294_400).contains(&(width * height))
}

fn api_request(
    key: &ego_core::credentials::SecretMaterial,
    prompt: &str,
    size: &str,
    quality: &str,
) -> Result<(HttpRoute, HttpRequest), ToolError> {
    let target =
        NetworkTarget::new(API_HOST, 443).map_err(|_| invalid("invalid image API target"))?;
    let route = HttpRoute::new(HttpScheme::Https, target)
        .map_err(|_| invalid("invalid image API route"))?;
    let headers = vec![
        HttpHeader::new("content-type", b"application/json".to_vec())
            .map_err(|_| invalid("invalid content type"))?,
        HttpHeader::secret(
            "authorization",
            ego_core::credentials::SecretMaterial::new(
                [b"Bearer ".as_slice(), key.expose_secret_bytes()].concat(),
            ),
        )
        .map_err(|_| invalid("OPENAI_API_KEY is not a valid HTTP header"))?,
    ];
    let body = serde_json::to_vec(&json!({
        "model": API_MODEL,
        "prompt": prompt,
        "n": 1,
        "size": size,
        "quality": quality,
        "output_format": "png",
    }))
    .map_err(|_| invalid("invalid image request"))?;
    let request = HttpRequest::new("POST", "/v1/images/generations", headers, body)
        .map_err(|_| invalid("invalid image request"))?;
    Ok((route, request))
}

fn subscription_request(
    access: &dyn SubscriptionAccess,
    prompt: &str,
) -> Result<(HttpRoute, HttpRequest), ToolError> {
    let resolved = access.route().map_err(invalid)?;
    let url = &resolved.url;
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(invalid("subscription endpoint has no host"));
    };
    if url.scheme() != "https" {
        return Err(invalid("subscription endpoint must use HTTPS"));
    }
    let target = NetworkTarget::new(host.trim_matches(['[', ']']), port)
        .map_err(|_| invalid("invalid subscription target"))?;
    let route = HttpRoute::new(HttpScheme::Https, target)
        .map_err(|_| invalid("invalid subscription route"))?;
    let mut headers = vec![
        HttpHeader::new("content-type", b"application/json".to_vec())
            .map_err(|_| invalid("invalid content type"))?,
        HttpHeader::new("accept", b"text/event-stream".to_vec())
            .map_err(|_| invalid("invalid accept header"))?,
    ];
    headers.extend(
        resolved
            .headers
            .iter()
            .filter(|header| {
                !header.name().eq_ignore_ascii_case("content-type")
                    && !header.name().eq_ignore_ascii_case("accept")
            })
            .cloned(),
    );
    // `size` and `quality` are not forwarded. Live probes (2026-11-09): requests
    // for 1024x1024 and 1024x1536 produced 1536x1024 and 1254x1254, so the
    // backend accepts `size` without honouring it; `quality` has not been shown
    // to be accepted. Both are API-key-backend options.
    let tool = json!({
        "type": "image_generation",
        "output_format": "png",
    });
    let mut body = json!({
        "model": resolved.model,
        "stream": true,
        "store": false,
        "instructions": SUBSCRIPTION_INSTRUCTIONS,
        "input": [{"role": "user", "content": prompt}],
        "tools": [tool],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
    });
    // `quality` is not forwarded either: it has not been shown to be accepted.
    if let Some(effort) = &resolved.effort {
        body["reasoning"] = json!({"effort": effort});
    }
    let body = serde_json::to_vec(&body).map_err(|_| invalid("invalid image request"))?;
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let request = HttpRequest::new("POST", &path, headers, body)
        .map_err(|_| invalid("invalid image request"))?;
    Ok((route, request))
}

impl TypedTool for ImageGen {
    type Params = ImageGenParams;
    fn name(&self) -> &str {
        "image_gen"
    }
    fn description(&self) -> &str {
        match self.backend {
            Backend::ApiKey(_) => {
                "Generate one raster image from a text prompt with OpenAI gpt-image-2 and write it as a PNG file in the workspace. Sends the prompt to api.openai.com using OPENAI_API_KEY; each image is charged to that account, so allow_paid=true is required. Needs network and file-write permission. Never overwrites a file. Returns the written path; make one call per image. Use it for photos, illustrations, textures, sprites and mockups, not for icons or diagrams that code can draw. Editing an existing image is not supported."
            }
            Backend::Subscription(_) => {
                "Generate one raster image from a text prompt with the signed-in ChatGPT subscription and write it as a PNG file in the workspace. Sends the prompt to chatgpt.com; it uses the subscription's allowance and needs no API key. The backend chooses the image size; the result reports it. Needs network and file-write permission. Never overwrites a file. Returns the written path; make one call per image. Use it for photos, illustrations, textures, sprites and mockups, not for icons or diagrams that code can draw. Editing an existing image is not supported."
            }
        }
    }
    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError> {
        if matches!(self.backend, Backend::ApiKey(_)) && !params.allow_paid {
            return Err(invalid(
                "image generation requires explicit cost consent: allow_paid=true",
            ));
        }
        if params.prompt.trim().is_empty() || params.prompt.len() > MAX_PROMPT_BYTES {
            return Err(invalid(format!(
                "prompt must contain 1 to {MAX_PROMPT_BYTES} bytes"
            )));
        }
        let size = params.size.as_deref().unwrap_or("auto");
        if !valid_size(size) {
            return Err(invalid(
                "size must be auto or WIDTHxHEIGHT: both edges a multiple of 16, longest edge at most 3840, ratio at most 3:1, 655360 to 8294400 pixels",
            ));
        }
        let quality = params.quality.as_deref().unwrap_or("auto");
        if !matches!(quality, "low" | "medium" | "high" | "auto") {
            return Err(invalid("quality must be low, medium, high or auto"));
        }
        let path = match &params.path {
            Some(path) => {
                let target = resolve_target(ctx, path)?;
                if ctx.resolver.canonicalize(target.as_path()).is_ok() {
                    return Err(invalid(format!(
                        "{path} already exists; image_gen never overwrites. Choose a new path"
                    )));
                }
                target
            }
            None => {
                let id = hex12(&Sha256::digest(params.prompt.as_bytes()));
                let mut chosen = None;
                for attempt in 0..100u32 {
                    let name = match attempt {
                        0 => format!("generated-{id}.png"),
                        n => format!("generated-{id}-{n}.png"),
                    };
                    let target = resolve_target(ctx, &name)?;
                    if ctx.resolver.canonicalize(target.as_path()).is_err() {
                        chosen = Some(target);
                        break;
                    }
                }
                chosen.ok_or_else(|| invalid("100 default image names are taken; pass path"))?
            }
        };
        let (route, request) = match &self.backend {
            Backend::ApiKey(key) => api_request(key, &params.prompt, size, quality)?,
            Backend::Subscription(access) => subscription_request(access.as_ref(), &params.prompt)?,
        };
        Ok(
            PreparedCall::web_http(self.name(), route, request).with_op(Op::Write {
                path,
                change: Change::Generated {
                    media_type: MEDIA_TYPE.into(),
                    producer: self.producer().into(),
                },
            }),
        )
    }
    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let request = call
                .web_http_envelope()
                .ok_or_else(|| invalid("missing image request"))?
                .1
                .clone();
            let path = match call.ops() {
                [
                    Op::Connect { .. },
                    Op::Write {
                        path,
                        change: Change::Generated { .. },
                    },
                ] => path.clone(),
                _ => return Err(wrong_op(self.name())),
            };
            let subscription = matches!(self.backend, Backend::Subscription(_));
            let fetched = async {
                let response = io
                    .guarded_http(call, request)
                    .await
                    .map_err(|error| ToolError::foreign(error.to_string(), error.delivery()))?;
                let OneHopResponse::Response {
                    status, mut body, ..
                } = response
                else {
                    return Err(failure(
                        "image backend redirect refused; credentials were not forwarded",
                    ));
                };
                let mut bytes = Vec::new();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk
                        .map_err(|error| ToolError::foreign(error.to_string(), error.delivery()))?;
                    if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                        return Err(failure("image response exceeds 64 MiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if !(200..300).contains(&status) {
                    let hint = if subscription && matches!(status, 401 | 403) {
                        "; sign in again with 'ego auth login openai-codex'"
                    } else {
                        ""
                    };
                    return Err(failure(format!(
                        "image backend returned HTTP {status}: {}{hint}",
                        api_error(&bytes)
                    )));
                }
                if subscription {
                    decode_stream(&bytes)
                } else {
                    decode_images_api(&bytes)
                }
            };
            let mut timer = crate::exec::runtime_handle().spawn(async {
                tokio::time::sleep(TIMEOUT).await;
            });
            let outcome = match futures::future::select(fetched.boxed(), &mut timer).await {
                futures::future::Either::Left((outcome, _)) => outcome,
                futures::future::Either::Right(_) => {
                    Err(failure("image generation exceeded 300 seconds"))
                }
            };
            timer.abort();
            let (png, usage) = outcome?;
            let dimensions = png_dimensions(&png);
            let attempt = io.create_attempt(&path, &png);
            let notice = if attempt.result.is_ok() || attempt.checkpoint.is_some() {
                io.write_notice(&path)
            } else {
                None
            };
            let evidence = FileWriteEvidence {
                path: path.to_string(),
                completed: attempt.result.is_ok(),
                before_image: attempt.before_image,
                after_image: None,
                checkpoint: attempt.checkpoint,
                possibly_modified: attempt.possibly_modified,
                summary: format!(" ({} bytes, PNG)", png.len()),
                notice,
            };
            match attempt.result {
                Ok(()) => Ok(ToolResult::builtin(
                    json!({
                        "status": "completed",
                        "path": path.to_string(),
                        "bytes": png.len(),
                        "width": dimensions.map(|(width, _)| width),
                        "height": dimensions.map(|(_, height)| height),
                        "media_type": MEDIA_TYPE,
                        "via": if subscription { "chatgpt-subscription" } else { API_MODEL },
                        "usage": usage,
                    })
                    .to_string(),
                )
                .with_file_writes(vec![evidence])),
                Err(error) => Err(ToolError::WriteFailed(Box::new(
                    ToolResult::builtin(
                        json!({
                            "status": "failed",
                            "path": path.to_string(),
                            "error": error.to_string(),
                            "possibly_modified": evidence.possibly_modified,
                        })
                        .to_string(),
                    )
                    .with_file_writes(vec![evidence]),
                ))),
            }
        })
    }
}

/// Width and height from the PNG's IHDR chunk, when it is there.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    (png.get(12..16) == Some(b"IHDR".as_slice())).then(|| {
        let word = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().expect("4 bytes"));
        (png.len() >= 24).then(|| (word(16), word(20)))
    })?
}

fn png_from_base64(encoded: &str) -> Result<Vec<u8>, ToolError> {
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| failure("image backend returned invalid base64"))?;
    if !png.starts_with(PNG_MAGIC) {
        return Err(failure("image backend did not return a PNG"));
    }
    Ok(png)
}

/// The PNG and the usage object, from the Images API response body.
fn decode_images_api(bytes: &[u8]) -> Result<(Vec<u8>, Value), ToolError> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Image>,
        #[serde(default)]
        usage: Value,
    }
    #[derive(Deserialize)]
    struct Image {
        b64_json: Option<String>,
    }
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| failure("invalid image API response"))?;
    let encoded = response
        .data
        .into_iter()
        .find_map(|image| image.b64_json)
        .ok_or_else(|| failure("image API response carried no image"))?;
    Ok((png_from_base64(&encoded)?, response.usage))
}

/// The PNG and the usage object, from a Responses event stream.
///
/// The image is the `result` of an `image_generation_call` item, taken from
/// `response.output_item.done` or, failing that, from the completed
/// response's `output`. Partial-image events and text are not the image.
fn decode_stream(bytes: &[u8]) -> Result<(Vec<u8>, Value), ToolError> {
    let text = std::str::from_utf8(bytes).map_err(|_| failure("image stream is not UTF-8"))?;
    let mut image: Option<String> = None;
    let mut usage = Value::Null;
    let mut error: Option<String> = None;
    let mut said = String::new();
    let result_of = |item: &Value| {
        (item["type"] == "image_generation_call")
            .then(|| item["result"].as_str())
            .flatten()
            .filter(|result| !result.is_empty())
            .map(str::to_owned)
    };
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim_start()) else {
            continue;
        };
        match event["type"].as_str() {
            Some("response.output_item.done") => {
                if let Some(result) = result_of(&event["item"]) {
                    image = Some(result);
                }
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event["delta"].as_str()
                    && said.len() < 400
                {
                    said.push_str(delta);
                }
            }
            Some("response.completed") => {
                usage = event["response"]["usage"].clone();
                if image.is_none() {
                    image = event["response"]["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(result_of);
                }
            }
            Some("response.failed" | "error") => {
                error = event["response"]["error"]["message"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .or_else(|| event["message"].as_str())
                    .map(str::to_owned)
                    .or(Some("no message".to_owned()));
            }
            _ => {}
        }
    }
    let Some(encoded) = image else {
        let said: String = said.chars().take(200).collect();
        return Err(failure(match error {
            Some(message) => format!("image stream failed: {message}"),
            None if said.trim().is_empty() => "the model returned no image".to_owned(),
            None => format!("the model returned no image; it said: {said}"),
        }));
    };
    Ok((png_from_base64(&encoded)?, usage))
}

/// The provider's own error message, bounded; never the request or a key.
fn api_error(bytes: &[u8]) -> String {
    let parsed = serde_json::from_slice::<Value>(bytes).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| {
            value["error"]["message"]
                .as_str()
                .or_else(|| value["detail"].as_str())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| "no error message".to_owned());
    message.chars().take(512).collect()
}

fn hex12(digest: &[u8]) -> String {
    digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
