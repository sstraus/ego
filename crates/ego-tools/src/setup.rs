//! Bounded native setup reads. No command, writer, hook or transport is owned.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::permit::CanonPath;
use ego_core::setup::{Observation, ProbeReader};

pub struct NativeProbeReader {
    path: Vec<PathBuf>,
}

impl NativeProbeReader {
    pub fn capture() -> Self {
        Self {
            path: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
        }
    }
}

fn identity(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime()
        )
    }
    #[cfg(not(unix))]
    {
        format!("{}:{:?}", metadata.len(), metadata.modified().ok())
    }
}

fn observe(
    path: &Path,
    canonical: PathBuf,
    metadata: &fs::Metadata,
    bytes: Vec<u8>,
    truncated: bool,
) -> Observation {
    Observation {
        requested: path.to_path_buf(),
        canonical: CanonPath::new(canonical),
        regular: metadata.is_file(),
        bytes,
        truncated,
        identity: identity(metadata),
    }
}

impl ProbeReader for NativeProbeReader {
    fn metadata(&self, path: &Path) -> Result<Observation, String> {
        let canonical =
            fs::canonicalize(path).map_err(|error| format!("metadata unavailable: {error}"))?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| format!("metadata unavailable: {error}"))?;
        Ok(observe(path, canonical, &metadata, Vec::new(), false))
    }

    fn read(&self, canonical: &CanonPath, max_bytes: usize) -> Result<Observation, String> {
        let path = canonical.as_path();
        let before =
            fs::symlink_metadata(path).map_err(|error| format!("file unavailable: {error}"))?;
        if !before.is_file() {
            return Err("probe read requires a regular file".into());
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|error| format!("probe open failed: {error}"))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("probe metadata failed: {error}"))?;
        if !opened.is_file() || identity(&opened) != identity(&before) {
            return Err("probe file identity changed before open".into());
        }
        let mut bytes = Vec::new();
        file.take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("probe read failed: {error}"))?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        Ok(observe(path, path.to_path_buf(), &opened, bytes, truncated))
    }

    fn tool(&self, name: &str) -> Result<Observation, String> {
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err("tool probe needs one PATH name".into());
        }
        for directory in self.path.iter().take(128) {
            let target = directory.join(name);
            if let Ok(observation) = self.metadata(&target) {
                if !observation.regular {
                    continue;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if fs::metadata(observation.canonical.as_path())
                        .map_err(|error| error.to_string())?
                        .permissions()
                        .mode()
                        & 0o111
                        == 0
                    {
                        continue;
                    }
                }
                return Ok(observation);
            }
        }
        Err("requested tool is missing from bounded operator PATH inventory".into())
    }
}
