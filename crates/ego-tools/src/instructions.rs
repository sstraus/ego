//! Reading `AGENTS.md` off a real filesystem.
//!
//! The `std::fs` half of `ego_core::config`'s instruction discovery; the
//! decision half — which paths, in what order, under what trust — is pure and
//! lives there. See
//! `ARCHITECTURE.md`.
//!
//! This is not `OsFs`. That one is the **model-controlled** seam a tool
//! reaches through a permit; this one is trusted-side and runs before a loop,
//! a permit or a tool exists. Keeping them apart is the same rule that keeps
//! `StoreIo` and `Fs` apart: a seam that is a security boundary for one caller
//! and not for another is true right up until someone adds a method.

use std::fs;
use std::io::Read;
use std::path::Path;

use ego_core::config::{Found, InstructionIo, Refusal};

/// [`InstructionIo`] over `std::fs`.
pub struct OsInstructionIo;

impl InstructionIo for OsInstructionIo {
    fn load(&self, path: &Path, max_bytes: u64) -> Found {
        let file = match open_no_links(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Found::Absent,
            Err(error) if error.raw_os_error() == Some(nix_symlink_error()) => {
                return Found::Refused(Refusal::Symlink);
            }
            Err(error) => {
                return Found::Refused(Refusal::Unreadable {
                    message: error.to_string(),
                });
            }
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(error) => {
                return Found::Refused(Refusal::Unreadable {
                    message: error.to_string(),
                });
            }
        };
        if !meta.is_file() {
            return Found::Refused(Refusal::NotAFile);
        }
        if meta.len() > max_bytes {
            return Found::Refused(Refusal::TooLarge { limit: max_bytes });
        }

        // One byte past the limit, and no further: enough to tell "exactly at
        // the limit" from "over it" without pulling an arbitrarily large file
        // into memory when the metadata above was wrong or raced.
        let mut bytes = Vec::new();
        if let Err(err) = file.take(max_bytes + 1).read_to_end(&mut bytes) {
            return Found::Refused(Refusal::Unreadable {
                message: err.to_string(),
            });
        }
        if bytes.len() as u64 > max_bytes {
            return Found::Refused(Refusal::TooLarge { limit: max_bytes });
        }

        Found::Read { bytes }
    }
}

#[cfg(unix)]
fn nix_symlink_error() -> i32 {
    nix::libc::ELOOP
}
#[cfg(not(unix))]
fn nix_symlink_error() -> i32 {
    -1
}

/// Every lookup is relative to an already opened directory. Substituting a
/// parent or leaf symlink cannot redirect the read between check and open.
#[cfg(unix)]
fn open_no_links(path: &Path) -> std::io::Result<fs::File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use std::path::Component;
    if !path.is_absolute() {
        return Err(std::io::Error::other("instruction path must be absolute"));
    }
    let mut directory = fs::File::open("/")?;
    let parts: Vec<_> = path
        .components()
        .filter(|part| !matches!(part, Component::RootDir))
        .collect();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(std::io::Error::other("instruction path must be normalized"));
        };
        let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        if index + 1 < parts.len() {
            flags |= OFlag::O_DIRECTORY;
        }
        let fd = openat(&directory, Path::new(name), flags, Mode::empty()).map_err(|error| {
            // Platforms can report ENOTDIR for a no-follow parent link.
            std::io::Error::from_raw_os_error(if error == nix::errno::Errno::ENOTDIR {
                nix::libc::ELOOP
            } else {
                error as i32
            })
        })?;
        directory = fs::File::from(fd);
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_no_links(path: &Path) -> std::io::Result<fs::File> {
    open_portable(path)
}

#[cfg(any(not(unix), test))]
fn open_portable(_path: &Path) -> std::io::Result<fs::File> {
    Err(std::io::Error::other(
        "race-safe instruction reads are unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches refusing ordinary in-tree instructions on non-Unix platforms.
    #[test]
    fn portable_in_tree_agents_is_read_instead_of_unsupported() {
        let tree = tempfile::tempdir().unwrap();
        let path = tree.path().join("AGENTS.md");
        fs::write(&path, "workspace rules").unwrap();
        let mut text = String::new();
        open_portable(&path).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "workspace rules");
    }
}
