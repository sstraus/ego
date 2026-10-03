//! Bounded private logs; paths select handles, handles own all subsequent I/O.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use fd_lock::RwLock;

pub(super) fn append(root: &Path, line: &[u8], max_bytes: u64) -> io::Result<()> {
    let added = u64::try_from(line.len())
        .ok()
        .and_then(|len| len.checked_add(1))
        .filter(|len| *len <= max_bytes)
        .ok_or_else(|| io::Error::other("private log record exceeds retention"))?;
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let created = match builder.create(root) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    let directory = open_directory(root, created)?;
    let mut lock = RwLock::new(open_child(&directory, root, "errors.lock", false)?);
    let _guard = lock.write()?;
    let mut log = open_child(&directory, root, "errors.jsonl", true)?;
    let len = log.metadata()?.len();
    if len > max_bytes {
        log.set_len(0)?;
    } else {
        recover_tail(&mut log, len)?;
    }
    if log
        .metadata()?
        .len()
        .checked_add(added)
        .is_none_or(|len| len > max_bytes)
    {
        // Retain the new segment, never reread or rewrite previous records.
        log.set_len(0)?;
    }
    log.write_all(line)?;
    log.write_all(b"\n")?;
    log.sync_all()?;
    directory.sync_all()?;
    Ok(())
}

fn recover_tail(log: &mut File, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let mut end = len;
    let mut buffer = [0_u8; 4096];
    // Ordinary complete tails read one byte; damaged tails scan at most retention.
    log.seek(SeekFrom::Start(len - 1))?;
    log.read_exact(&mut buffer[..1])?;
    if buffer[0] == b'\n' {
        return Ok(());
    }
    while end != 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let count = usize::try_from(end - start).expect("bounded chunk");
        log.seek(SeekFrom::Start(start))?;
        log.read_exact(&mut buffer[..count])?;
        if let Some(index) = buffer[..count].iter().rposition(|byte| *byte == b'\n') {
            return log.set_len(start + index as u64 + 1);
        }
        end = start;
    }
    log.set_len(0)
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "private log handle is not owner-only",
    )
}

#[cfg(unix)]
fn validate(file: &File, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = file.metadata()?;
    let expected = if directory { 0o700 } else { 0o600 };
    let kind = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file() && metadata.nlink() == 1
    };
    if !kind
        || metadata.permissions().mode() & 0o777 != expected
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path, _created: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC)
        .open(path)?;
    validate(&file, true)?;
    Ok(file)
}

#[cfg(unix)]
fn open_child(directory: &File, _root: &Path, name: &str, append: bool) -> io::Result<File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use std::os::unix::fs::PermissionsExt;
    let flags = OFlag::O_RDWR
        | OFlag::O_NOFOLLOW
        | OFlag::O_CLOEXEC
        | OFlag::O_NONBLOCK
        | if append {
            OFlag::O_APPEND
        } else {
            OFlag::empty()
        };
    let mode = Mode::S_IRUSR | Mode::S_IWUSR;
    let (fd, created) = match openat(
        directory,
        name,
        flags | OFlag::O_CREAT | OFlag::O_EXCL,
        mode,
    ) {
        Ok(fd) => (fd, true),
        Err(nix::errno::Errno::EEXIST) => (
            openat(directory, name, flags, Mode::empty()).map_err(io::Error::from)?,
            false,
        ),
        Err(error) => return Err(io::Error::from(error)),
    };
    let file = File::from(fd);
    if created {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    validate(&file, false)?;
    Ok(file)
}

#[cfg(windows)]
fn open_directory(path: &Path, created: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        // BACKUP_SEMANTICS | OPEN_REPARSE_POINT; omit DELETE sharing to pin the path.
        .access_mode(0x80000000 | 0x40000000 | 0x00020000 | 0x00040000)
        .custom_flags(0x02000000 | 0x00200000)
        .share_mode(0x1 | 0x2)
        .open(path)?;
    if created {
        harden_handle(&mut file, true)?;
    }
    validate(&file, true)?;
    Ok(file)
}

#[cfg(windows)]
fn open_child(_directory: &File, root: &Path, name: &str, append: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = root.join(name);
    let options = |create: bool| {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .append(append)
            .create_new(create)
            .access_mode(0x80000000 | 0x40000000 | 0x00020000 | 0x00040000)
            .custom_flags(0x00200000)
            .share_mode(0x1 | 0x2);
        options
    };
    let (mut file, created) = match options(true).open(&path) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            (options(false).open(&path)?, false)
        }
        Err(error) => return Err(error),
    };
    if created {
        harden_handle(&mut file, false)?;
    }
    validate(&file, false)?;
    Ok(file)
}

#[cfg(windows)]
fn harden_handle(file: &mut File, directory: bool) -> io::Result<()> {
    use windows_permissions::constants::{SeObjectType, SecurityInformation};
    use windows_permissions::utilities::current_process_sid;
    use windows_permissions::wrappers::SetSecurityInfo;
    use windows_permissions::{LocalBox, SecurityDescriptor, Sid};
    let sid = current_process_sid()?;
    let inheritance = if directory { "OICI" } else { "" };
    let descriptor: LocalBox<SecurityDescriptor> = format!("D:P(A;{inheritance};FA;;;{sid})")
        .parse()
        .map_err(|_| invalid())?;
    SetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None::<&Sid>,
        None::<&Sid>,
        Some(descriptor.dacl().ok_or_else(invalid)?),
        None,
    )
}

#[cfg(windows)]
fn validate(file: &File, directory: bool) -> io::Result<()> {
    use windows_permissions::constants::{SeObjectType, SecurityInformation};
    use windows_permissions::utilities::current_process_sid;
    use windows_permissions::wrappers::GetSecurityInfo;
    let info = winapi_util::file::information(file)?;
    let metadata = file.metadata()?;
    let kind = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file() && info.number_of_links() == 1
    };
    if !kind || info.file_attributes() & 0x400 != 0 {
        return Err(invalid());
    }
    let descriptor = GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )?;
    let sid = current_process_sid()?;
    let inheritance = if directory { "OICI" } else { "" };
    let dacl = descriptor.dacl().ok_or_else(invalid)?;
    let sddl = descriptor.as_sddl()?.to_string_lossy().into_owned();
    if !sddl.starts_with("D:P")
        || dacl.len() != 1
        || !sddl.ends_with(&format!("(A;{inheritance};FA;;;{sid})"))
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn open_directory(_path: &Path, _created: bool) -> io::Result<File> {
    Err(invalid())
}
#[cfg(not(any(unix, windows)))]
fn open_child(_directory: &File, _root: &Path, _name: &str, _append: bool) -> io::Result<File> {
    Err(invalid())
}
