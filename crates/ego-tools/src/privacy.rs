//! Private filesystem state with equivalent Unix modes and Windows DACLs.

use std::io;
use std::path::Path;

/// The filesystem object whose private policy is being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivatePathKind {
    /// A regular file, equivalent to Unix mode `0600`.
    File,
    /// A directory, equivalent to Unix mode `0700` and inherited by children.
    Directory,
}

/// Restrict a file or directory to the current operating-system user.
pub fn harden_private_path(path: &Path, kind: PrivatePathKind) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = match kind {
            PrivatePathKind::File => 0o600,
            PrivatePathKind::Directory => 0o700,
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    #[cfg(windows)]
    {
        use windows_permissions::LocalBox;
        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::utilities::current_process_sid;
        use windows_permissions::wrappers::SetNamedSecurityInfo;
        use windows_permissions::{SecurityDescriptor, Sid};

        let sid = current_process_sid().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to obtain the current Windows user SID: {error}"),
            )
        })?;
        let inheritance = match kind {
            PrivatePathKind::File => "",
            PrivatePathKind::Directory => "OICI",
        };
        let descriptor: LocalBox<SecurityDescriptor> = format!("D:P(A;{inheritance};FA;;;{sid})")
            .parse()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to build an owner-only Windows DACL: {error}"),
                )
            })?;
        let dacl = descriptor.dacl().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the owner-only Windows security descriptor has no DACL",
            )
        })?;
        return SetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None::<&Sid>,
            None::<&Sid>,
            Some(dacl),
            None,
        );
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, kind);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private filesystem permissions are unsupported on this platform",
        ))
    }
}

/// Refuse a path whose current permissions are not the private policy.
pub fn validate_private_path(path: &Path, kind: PrivatePathKind) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let expected_type = match kind {
        PrivatePathKind::File => metadata.is_file(),
        PrivatePathKind::Directory => metadata.is_dir(),
    };
    if metadata.file_type().is_symlink() || !expected_type {
        return Err(private_error(
            "the private path has an unexpected file type",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let expected = match kind {
            PrivatePathKind::File => 0o600,
            PrivatePathKind::Directory => 0o700,
        };
        if metadata.permissions().mode() & 0o777 == expected {
            return Ok(());
        }
        Err(private_error("the private path mode is not owner-only"))
    }

    #[cfg(windows)]
    {
        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::utilities::current_process_sid;
        use windows_permissions::wrappers::GetNamedSecurityInfo;

        let descriptor = GetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )?;
        let dacl = descriptor
            .dacl()
            .ok_or_else(|| private_error("the private path has no Windows DACL"))?;
        let sddl = descriptor.as_sddl()?.to_string_lossy().into_owned();
        let sid = current_process_sid()?;
        let inheritance = match kind {
            PrivatePathKind::File => "",
            PrivatePathKind::Directory => "OICI",
        };
        if sddl.starts_with("D:P")
            && dacl.len() == 1
            && sddl.ends_with(&format!("(A;{inheritance};FA;;;{sid})"))
        {
            return Ok(());
        }
        return Err(private_error(
            "the private path Windows DACL is not owner-only",
        ));
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, kind, metadata);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private filesystem permissions are unsupported on this platform",
        ))
    }
}

fn private_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
