//! Reading `AGENTS.md` off a real filesystem.
//!
//! This is the `std::fs` half of instruction discovery. Core owns the pure
//! path, ordering and trust decisions.
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
        read_instruction(open_in_tree(path), max_bytes)
    }

    fn load_external(&self, path: &Path, max_bytes: u64) -> Found {
        read_instruction(open_no_links(path), max_bytes)
    }
}

fn read_instruction(opened: std::io::Result<fs::File>, max_bytes: u64) -> Found {
    let file = match opened {
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

fn open_in_tree(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        open_no_links(path)
    }
    #[cfg(not(unix))]
    {
        open_portable(path)
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
    let _ = path;
    unsupported_external()
}

#[cfg(any(not(unix), test))]
fn unsupported_external() -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "race-safe external instruction reads are unsupported on this platform",
    ))
}

#[cfg(not(unix))]
fn open_portable(path: &Path) -> std::io::Result<fs::File> {
    // In-tree reads retain the static-link policy. External imports never use
    // this check-then-open path because they need substitution protection.
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
            return Err(std::io::Error::from_raw_os_error(nix_symlink_error()));
        }
    }
    fs::File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches selecting the portable reader for an out-of-scope import.
    #[test]
    fn portable_policy_routes_only_external_imports_to_safe_reader() {
        use ego_core::config::{Limits, Selection, load_with_home};
        use std::cell::RefCell;
        struct PolicyIo {
            reads: RefCell<Vec<bool>>,
        }
        impl InstructionIo for PolicyIo {
            fn load(&self, path: &Path, _: u64) -> Found {
                self.reads.borrow_mut().push(false);
                if path.ends_with("AGENTS.md") {
                    Found::Read {
                        bytes: b"@inside.md\n@../external.md".to_vec(),
                    }
                } else {
                    Found::Read {
                        bytes: b"inside".to_vec(),
                    }
                }
            }
            fn load_external(&self, _: &Path, _: u64) -> Found {
                self.reads.borrow_mut().push(true);
                read_instruction(unsupported_external(), 100)
            }
        }
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().join("workspace");
        let io = PolicyIo {
            reads: RefCell::new(Vec::new()),
        };
        let set = load_with_home(
            &io,
            &root,
            &root,
            &Selection::Ambient,
            Limits::default(),
            Some(tree.path()),
        )
        .unwrap();
        assert_eq!(*io.reads.borrow(), [false, false, true]);
        assert_eq!(set.loaded.len(), 2);
        assert!(matches!(set.skipped[0].reason, Refusal::Unreadable { .. }));
    }

    /// Catches unsupported external imports being opened with the portable path.
    #[cfg(not(unix))]
    #[test]
    fn portable_external_import_stays_unsupported() {
        let tree = tempfile::tempdir().unwrap();
        let path = tree.path().join("external.md");
        fs::write(&path, "external rules").unwrap();
        let Found::Refused(Refusal::Unreadable { message }) =
            OsInstructionIo.load_external(&path, 100)
        else {
            panic!("external instructions must fail closed on this platform")
        };
        assert!(message.contains("race-safe external instruction reads are unsupported"));
    }

    /// Catches losing absence, exact byte limits, or regular-file checks.
    #[test]
    fn portable_in_tree_reads_preserve_bounds_and_absence() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        assert_eq!(OsInstructionIo.load(&path, 4), Found::Absent);
        fs::write(&path, "rule").unwrap();
        assert_eq!(
            OsInstructionIo.load(&path, 4),
            Found::Read {
                bytes: b"rule".to_vec()
            }
        );
        assert_eq!(
            OsInstructionIo.load(&path, 3),
            Found::Refused(Refusal::TooLarge { limit: 3 })
        );
        assert_eq!(
            OsInstructionIo.load(&root, 4),
            Found::Refused(Refusal::NotAFile)
        );
    }

    /// Catches static leaf or parent links bypassing the portable policy.
    #[cfg(unix)]
    #[test]
    fn portable_in_tree_reads_refuse_leaf_and_parent_links() {
        use std::os::unix::fs::symlink;
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/AGENTS.md"), "secret").unwrap();
        symlink(root.join("real/AGENTS.md"), root.join("AGENTS.md")).unwrap();
        symlink(root.join("real"), root.join("linked")).unwrap();
        for path in [root.join("AGENTS.md"), root.join("linked/AGENTS.md")] {
            assert_eq!(
                OsInstructionIo.load(&path, 100),
                Found::Refused(Refusal::Symlink)
            );
        }
    }

    /// Catches refusing ordinary in-tree instructions on non-Unix platforms.
    #[test]
    fn portable_in_tree_agents_is_read_instead_of_unsupported() {
        let tree = tempfile::tempdir().unwrap();
        let path = tree.path().canonicalize().unwrap().join("AGENTS.md");
        fs::write(&path, "workspace rules").unwrap();
        assert_eq!(
            OsInstructionIo.load(&path, 100),
            Found::Read {
                bytes: b"workspace rules".to_vec()
            }
        );
    }
}
