//! Invocation-only proxy selection and bounded certificate input.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use ego_tools::http::ProviderProxy;

use super::UsageError;

/// One trust anchor is normally a few KiB; 64 KiB also allows PEM comments.
const MAX_CA_BYTES: u64 = 64 * 1024;

pub(super) fn load(
    endpoint: Option<String>,
    ca: Option<PathBuf>,
    directory: Option<&Path>,
) -> Result<Option<ProviderProxy>, UsageError> {
    let proxy = endpoint
        .map(|value| value.parse::<ProviderProxy>().map_err(UsageError))
        .transpose()?;
    match (proxy, ca) {
        (Some(proxy), Some(path)) => {
            let path = if path.is_absolute() {
                path
            } else {
                directory.unwrap_or_else(|| Path::new("")).join(path)
            };
            let file = std::fs::File::open(path)
                .map_err(|_| UsageError("could not read --proxy-ca certificate".to_owned()))?;
            let mut pem = Vec::new();
            file.take(MAX_CA_BYTES + 1)
                .read_to_end(&mut pem)
                .map_err(|_| UsageError("could not read --proxy-ca certificate".to_owned()))?;
            if pem.len() as u64 > MAX_CA_BYTES {
                return Err(UsageError(
                    "--proxy-ca certificate exceeds 64 KiB".to_owned(),
                ));
            }
            Ok(Some(proxy.with_ca_pem(pem).map_err(UsageError)?))
        }
        (proxy, None) => Ok(proxy),
        (None, Some(_)) => Err(UsageError("--proxy-ca requires --proxy".to_owned())),
    }
}
