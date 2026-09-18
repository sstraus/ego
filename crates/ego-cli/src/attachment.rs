//! CLI acquisition for image attachments.
//!
//! Paths and URLs end here. The durable and provider-facing value contains
//! only exact validated bytes or the session-local reference to those bytes.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::generator::capabilities::{ImageInputSupport, ImageLimits, ModelCapabilities};
use ego_core::generator::types::{
    Image, ImageMediaType, ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_PROMPT, MAX_TOTAL_IMAGE_BYTES,
};
use ego_core::permit::{
    GuardedHttp, HttpRequest, HttpRoute, HttpScheme, NetworkTarget, OneHopResponse, PreparedConnect,
};
use ego_core::store::blob::{BlobStore, BlobWriter};
use ego_tools::exec::RunEffectPorts;
use futures::StreamExt;
use url::Url;

use crate::cli::UsageError;

const READ_CHUNK: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 10;

enum Source {
    Path(PathBuf),
    Https(Url),
}

struct Staged {
    image: Image,
    writer: Box<dyn BlobWriter>,
}

/// Acquire, validate, and publish one ordered CLI attachment batch.
///
/// Capability support and count are proved before a source is touched. Blob
/// commits happen only after every exact byte sequence satisfies both product
/// and selected-model limits, and therefore before any prompt can reference
/// the returned values.
pub(crate) async fn acquire(
    specs: &[String],
    capabilities: &ModelCapabilities,
    profile: &str,
    resolved_model: &str,
    cwd: &Path,
    ports: &RunEffectPorts,
    blobs: &dyn BlobStore,
) -> Result<Vec<ImageRef>, UsageError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }

    let limits = supported_limits(capabilities, profile, resolved_model)?;
    let max_images = limits.max_images().map_or(MAX_IMAGES_PER_PROMPT, |value| {
        usize::try_from(value.get()).expect("model image count is product-bounded at 100")
    });
    if specs.len() > max_images {
        return Err(UsageError(format!(
            "model profile `{profile}` resolved to `{resolved_model}` accepts at most \
             {max_images} images, but {} --attach values were supplied",
            specs.len()
        )));
    }

    let sources = specs
        .iter()
        .map(|spec| parse_source(spec, cwd))
        .collect::<Result<Vec<_>, _>>()?;
    let max_per_image = limits
        .max_bytes_per_image()
        .map_or(MAX_IMAGE_BYTES, |value| value.get());
    let max_total = limits
        .max_total_bytes()
        .map_or(MAX_TOTAL_IMAGE_BYTES, |value| value.get());
    let mut total = 0_u64;
    let mut staged = Vec::with_capacity(sources.len());

    for (offset, source) in sources.into_iter().enumerate() {
        let index = offset + 1;
        let mut writer = blobs.begin().map_err(|error| {
            UsageError(format!(
                "attachment {index} could not begin its session blob: {error}"
            ))
        })?;
        let bytes = match source {
            Source::Path(path) => read_path_once(
                &path,
                writer.as_mut(),
                index,
                max_per_image,
                max_total,
                &mut total,
            )?,
            Source::Https(url) => {
                fetch_https(
                    url,
                    writer.as_mut(),
                    index,
                    max_per_image,
                    max_total,
                    &mut total,
                    ports,
                )
                .await?
            }
        };
        let media_type = media_type(&bytes).ok_or_else(|| {
            UsageError(format!(
                "attachment {index} is not a JPEG, PNG, single-frame GIF, or WebP image"
            ))
        })?;
        let image = Image::new(bytes, media_type).map_err(|error| {
            UsageError(format!(
                "attachment {index} is not an admitted image: {error}"
            ))
        })?;
        validate_model_image(&image, index, limits, profile, resolved_model)?;
        staged.push(Staged { image, writer });
    }

    commit_staged(staged)
}

/// Publish an ordered batch of images that arrived already decoded.
///
/// The sibling of [`acquire`] for content ego did not have to fetch: an ACP
/// `session/prompt` carries its images inside the request, so there is no path
/// to open and no URL to dial, and the two steps that remain are the two that
/// are actually about admission — proving the selected model accepts each
/// image, and publishing the exact bytes into the session's store.
///
/// Both halves are shared with `acquire` rather than restated, so a limit that
/// is enforced for `--attach` cannot silently not be enforced here. The order
/// of the batch is the order it was sent in, and, as in `acquire`, nothing is
/// committed until every image in it has passed: a prompt that is going to be
/// refused leaves no blobs behind for the collector to find.
pub(crate) fn publish(
    images: &[Image],
    capabilities: &ModelCapabilities,
    profile: &str,
    resolved_model: &str,
    blobs: &dyn BlobStore,
) -> Result<Vec<ImageRef>, UsageError> {
    if images.is_empty() {
        return Ok(Vec::new());
    }

    let limits = supported_limits(capabilities, profile, resolved_model)?;
    let max_images = limits.max_images().map_or(MAX_IMAGES_PER_PROMPT, |value| {
        usize::try_from(value.get()).expect("model image count is product-bounded at 100")
    });
    if images.len() > max_images {
        return Err(UsageError(format!(
            "model profile `{profile}` resolved to `{resolved_model}` accepts at most \
             {max_images} images, but {} were sent with the prompt",
            images.len()
        )));
    }
    let max_per_image = limits
        .max_bytes_per_image()
        .map_or(MAX_IMAGE_BYTES, |value| value.get());
    let max_total = limits
        .max_total_bytes()
        .map_or(MAX_TOTAL_IMAGE_BYTES, |value| value.get());

    let mut total = 0_u64;
    let mut staged = Vec::with_capacity(images.len());
    for (offset, image) in images.iter().enumerate() {
        let index = offset + 1;
        let mut writer = blobs.begin().map_err(|error| {
            UsageError(format!(
                "image {index} could not begin its session blob: {error}"
            ))
        })?;
        // `Image` already proved the bytes decode and are within the product
        // bounds; what is left is this model's ceiling and the running total,
        // which is the same check `--attach` streams against.
        stage_bounded(
            0,
            writer.as_mut(),
            image.bytes(),
            index,
            max_per_image,
            max_total,
            &mut total,
        )?;
        validate_model_image(image, index, limits, profile, resolved_model)?;
        staged.push(Staged {
            image: image.clone(),
            writer,
        });
    }
    commit_staged(staged)
}

/// Commit an ordered staged batch and name each blob durably.
///
/// The tail both admission paths end in. Kept as one function because the rule
/// it encodes — every image passes, *then* anything is committed — is the rule
/// that keeps a refused prompt from leaving blobs behind, and two copies of it
/// could drift into disagreeing about that.
fn commit_staged(staged: Vec<Staged>) -> Result<Vec<ImageRef>, UsageError> {
    let mut references = Vec::with_capacity(staged.len());
    for (offset, staged) in staged.into_iter().enumerate() {
        let index = offset + 1;
        let reference = staged.writer.commit().map_err(|error| {
            UsageError(format!(
                "attachment {index} could not be committed to the session blob store: {error}"
            ))
        })?;
        references.push(
            ImageRef::new(
                reference,
                staged.image.media_type(),
                staged.image.width(),
                staged.image.height(),
            )
            .map_err(|error| {
                UsageError(format!(
                    "attachment {index} produced invalid durable metadata: {error}"
                ))
            })?,
        );
    }
    Ok(references)
}

fn supported_limits<'a>(
    capabilities: &'a ModelCapabilities,
    profile: &str,
    resolved_model: &str,
) -> Result<&'a ImageLimits, UsageError> {
    match &capabilities.image_input {
        ImageInputSupport::Supported(limits) => Ok(limits),
        ImageInputSupport::Unknown => Err(UsageError(format!(
            "model profile `{profile}` resolved to `{resolved_model}`, but its image-input \
             capability is unknown; no attachment was fetched"
        ))),
        ImageInputSupport::Unsupported => Err(UsageError(format!(
            "model profile `{profile}` resolved to `{resolved_model}` does not support image \
             input; no attachment was fetched"
        ))),
    }
}

fn parse_source(spec: &str, cwd: &Path) -> Result<Source, UsageError> {
    if spec.is_empty() {
        return Err(UsageError(
            "--attach requires a non-empty path or HTTPS URL".to_string(),
        ));
    }
    let path = PathBuf::from(spec);
    if path.is_absolute() {
        return Ok(Source::Path(path));
    }
    match Url::parse(spec) {
        Ok(url) => {
            validate_https_url(&url)?;
            Ok(Source::Https(url))
        }
        Err(url::ParseError::RelativeUrlWithoutBase) => Ok(Source::Path(cwd.join(path))),
        Err(error) => Err(UsageError(format!(
            "--attach value `{spec}` is neither an explicit path nor a valid HTTPS URL: {error}"
        ))),
    }
}

fn validate_https_url(url: &Url) -> Result<(), UsageError> {
    if url.scheme() != "https" {
        return Err(UsageError(format!(
            "attachment URL uses `{}`; only HTTPS URLs are accepted",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UsageError(
            "attachment URL contains user-info, which is not accepted".to_string(),
        ));
    }
    if url.fragment().is_some() {
        return Err(UsageError(
            "attachment URL contains a fragment, which is not sent and is not accepted".to_string(),
        ));
    }
    if url.host_str().is_none() {
        return Err(UsageError("attachment URL has no network host".to_string()));
    }
    Ok(())
}

fn read_path_once(
    path: &Path,
    writer: &mut dyn BlobWriter,
    index: usize,
    max_per_image: u64,
    max_total: u64,
    total: &mut u64,
) -> Result<Vec<u8>, UsageError> {
    let mut file = File::open(path).map_err(|error| {
        UsageError(format!(
            "attachment {index} could not open {}: {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK];
    loop {
        let read = file.read(&mut chunk).map_err(|error| {
            UsageError(format!(
                "attachment {index} could not read {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        append_bounded(
            &mut bytes,
            writer,
            &chunk[..read],
            index,
            max_per_image,
            max_total,
            total,
        )?;
    }
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_https(
    mut url: Url,
    writer: &mut dyn BlobWriter,
    index: usize,
    max_per_image: u64,
    max_total: u64,
    total: &mut u64,
    ports: &RunEffectPorts,
) -> Result<Vec<u8>, UsageError> {
    let deadline = ports.begin_http_call();
    let guarded = GuardedHttp::new(ports.resolver_handle(), ports.one_hop_handle());
    let mut seen = BTreeSet::new();

    for redirects in 0..=MAX_REDIRECTS {
        validate_https_url(&url)?;
        if !seen.insert(url.as_str().to_string()) {
            return Err(UsageError(format!(
                "attachment {index} redirect cycle reached `{url}`"
            )));
        }
        let host = url.host_str().expect("validated HTTPS URLs have a host");
        let port = url
            .port_or_known_default()
            .expect("HTTPS has a default port");
        let target = NetworkTarget::new(host, port).map_err(|error| {
            UsageError(format!(
                "attachment {index} URL `{url}` has an invalid exact route: {error}"
            ))
        })?;
        let route = HttpRoute::new(HttpScheme::Https, target.clone()).map_err(|error| {
            UsageError(format!(
                "attachment {index} URL `{url}` could not form an HTTPS route: {error}"
            ))
        })?;
        let mut path = url.path().to_string();
        if path.is_empty() {
            path.push('/');
        }
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        let request = HttpRequest::get(&path).map_err(|error| {
            UsageError(format!(
                "attachment {index} URL `{url}` has an invalid request target: {error}"
            ))
        })?;
        let connect = PreparedConnect::new(target, &request);
        let response = guarded
            .execute_admitted(route, connect, request, deadline)
            .await
            .map_err(|error| {
                UsageError(format!(
                    "attachment {index} could not fetch `{url}` through the admitted transport: {error}"
                ))
            })?;

        match response {
            OneHopResponse::Response {
                status, mut body, ..
            } => {
                if !(200..300).contains(&status) {
                    return Err(UsageError(format!(
                        "attachment {index} URL `{url}` returned HTTP {status}"
                    )));
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(|error| {
                        UsageError(format!(
                            "attachment {index} response body from `{url}` failed: {error}"
                        ))
                    })?;
                    append_bounded(
                        &mut bytes,
                        writer,
                        &chunk,
                        index,
                        max_per_image,
                        max_total,
                        total,
                    )?;
                }
                return Ok(bytes);
            }
            OneHopResponse::Redirect { location, .. } => {
                if redirects == MAX_REDIRECTS {
                    return Err(UsageError(format!(
                        "attachment {index} exceeded {MAX_REDIRECTS} HTTPS redirects"
                    )));
                }
                let location = std::str::from_utf8(&location).map_err(|_| {
                    UsageError(format!(
                        "attachment {index} URL `{url}` returned a non-UTF-8 redirect location"
                    ))
                })?;
                url = url.join(location).map_err(|error| {
                    UsageError(format!(
                        "attachment {index} URL `{url}` returned invalid redirect `{location}`: {error}"
                    ))
                })?;
                // The next loop validates HTTPS and constructs a fresh exact
                // route plus request-bound one-shot authority for this hop.
            }
        }
    }
    unreachable!("the redirect loop returns at its explicit bound")
}

/// Stage one chunk against both ceilings, and account for it.
///
/// Separate from [`append_bounded`] because the two admission paths differ in
/// exactly one thing: a fetched attachment is accumulated chunk by chunk as it
/// arrives, while an image that came inside the request is already whole and
/// copying it a second time to reuse that accumulation would be the largest
/// avoidable allocation on the path. What must not differ is the arithmetic,
/// so that lives here and neither caller restates it.
fn stage_bounded(
    staged_len: usize,
    writer: &mut dyn BlobWriter,
    chunk: &[u8],
    index: usize,
    max_per_image: u64,
    max_total: u64,
    total: &mut u64,
) -> Result<(), UsageError> {
    let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
    let image_len = u64::try_from(staged_len)
        .unwrap_or(u64::MAX)
        .saturating_add(chunk_len);
    if image_len > max_per_image {
        return Err(UsageError(format!(
            "attachment {index} exceeds the selected model's {max_per_image}-byte per-image limit"
        )));
    }
    let next_total = total.checked_add(chunk_len).unwrap_or(u64::MAX);
    if next_total > max_total {
        return Err(UsageError(format!(
            "attachments exceed the selected model's {max_total}-byte aggregate image limit"
        )));
    }
    writer.append(chunk).map_err(|error| {
        UsageError(format!(
            "attachment {index} could not stage exact bytes in the session blob store: {error}"
        ))
    })?;
    *total = next_total;
    Ok(())
}

fn append_bounded(
    bytes: &mut Vec<u8>,
    writer: &mut dyn BlobWriter,
    chunk: &[u8],
    index: usize,
    max_per_image: u64,
    max_total: u64,
    total: &mut u64,
) -> Result<(), UsageError> {
    stage_bounded(
        bytes.len(),
        writer,
        chunk,
        index,
        max_per_image,
        max_total,
        total,
    )?;
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn media_type(bytes: &[u8]) -> Option<ImageMediaType> {
    if bytes.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]) {
        Some(ImageMediaType::Png)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        Some(ImageMediaType::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageMediaType::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageMediaType::Webp)
    } else {
        None
    }
}

fn validate_model_image(
    image: &Image,
    index: usize,
    limits: &ImageLimits,
    profile: &str,
    resolved_model: &str,
) -> Result<(), UsageError> {
    let refuse = |reason: String| {
        UsageError(format!(
            "attachment {index} is incompatible with model profile `{profile}` resolved to \
             `{resolved_model}`: {reason}"
        ))
    };
    if !limits.accepted_media_types().contains(&image.media_type()) {
        return Err(refuse(format!(
            "{} is not an accepted image media type",
            image.media_type().mime()
        )));
    }
    if let Some(max) = limits.max_width()
        && image.width() > max.get()
    {
        return Err(refuse(format!(
            "width {} exceeds {} pixels",
            image.width(),
            max.get()
        )));
    }
    if let Some(max) = limits.max_height()
        && image.height() > max.get()
    {
        return Err(refuse(format!(
            "height {} exceeds {} pixels",
            image.height(),
            max.get()
        )));
    }
    let pixels = u64::from(image.width()).saturating_mul(u64::from(image.height()));
    if let Some(max) = limits.max_pixels()
        && pixels > max.get()
    {
        return Err(refuse(format!(
            "{pixels} pixels exceed the {}-pixel limit",
            max.get()
        )));
    }
    Ok(())
}
