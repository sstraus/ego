//! Best-effort owner logs: exclusively create once, then write only the pinned handle.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::io::PrivateLogWriter;

const RETAINED_FILES: usize = 16;

struct OwnerLog {
    directory: File,
    file: File,
    written: u64,
    cap: u64,
    failed: bool,
}

impl PrivateLogWriter for OwnerLog {
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("private writer previously failed"));
        }
        let added = u64::try_from(line.len())
            .ok()
            .and_then(|len| len.checked_add(1))
            .filter(|len| *len <= self.cap)
            .ok_or_else(|| io::Error::other("private log record exceeds retention"))?;
        if self
            .written
            .checked_add(added)
            .is_none_or(|len| len > self.cap)
        {
            return Ok(());
        }
        validate(&self.directory, true)?;
        validate(&self.file, false)?;
        // Charge the entire attempted write, including any partial write, and
        // never retry into a damaged tail. No reopen, recovery or crash barrier.
        self.written += added;
        if let Err(error) = write_record(&mut self.file, line) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }
}

pub(super) fn open(root: &Path, cap: u64) -> io::Result<Box<dyn PrivateLogWriter>> {
    let directory = private_directory(root)?;
    // Cleanup failure cannot turn optional old-log retention into admission.
    prune(&directory, root);
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("private name unavailable"))?;
    let name = format!("owner-{:032x}.jsonl", u128::from_ne_bytes(random));
    let file = create_owner(&directory, root, &name)?;
    Ok(Box::new(OwnerLog {
        directory,
        file,
        written: 0,
        cap,
        failed: false,
    }))
}

fn write_record(log: &mut impl Write, line: &[u8]) -> io::Result<()> {
    let mut record = Vec::with_capacity(line.len() + 1);
    record.extend_from_slice(line);
    record.push(b'\n');
    log.write_all(&record)
}

struct RetainedFile {
    name: std::ffi::OsString,
    modified: SystemTime,
}

fn prune(directory: &File, root: &Path) {
    let Ok(mut files) = retained_files(directory, root) else {
        return;
    };
    files.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.name.cmp(&b.name)));
    let mut remaining = files.len();
    for file in files {
        if remaining < RETAINED_FILES {
            break;
        }
        if remove_retained(directory, root, &file.name).is_ok() {
            remaining -= 1;
        }
    }
}

#[cfg(unix)]
fn retained_files(directory: &File, _root: &Path) -> io::Result<Vec<RetainedFile>> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    use std::os::unix::ffi::OsStringExt;
    let mut files = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let stat = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => return Err(error.into()),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            continue;
        }
        let seconds = u64::try_from(stat.st_mtime).unwrap_or(0);
        let nanos = u32::try_from(stat.st_mtime_nsec)
            .unwrap_or(0)
            .min(999_999_999);
        files.push(RetainedFile {
            name: std::ffi::OsString::from_vec(name.to_bytes().to_vec()),
            modified: SystemTime::UNIX_EPOCH
                .checked_add(Duration::new(seconds, nanos))
                .unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(files)
}

#[cfg(unix)]
fn remove_retained(directory: &File, _root: &Path, name: &std::ffi::OsStr) -> io::Result<()> {
    match rustix::fs::unlinkat(directory, name, rustix::fs::AtFlags::empty()) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn retained_files(_directory: &File, root: &Path) -> io::Result<Vec<RetainedFile>> {
    use std::os::windows::fs::MetadataExt;
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_file() && metadata.file_attributes() & 0x400 == 0 {
            files.push(RetainedFile {
                name: entry.file_name(),
                modified: metadata.modified()?,
            });
        }
    }
    Ok(files)
}

#[cfg(windows)]
fn remove_retained(_directory: &File, root: &Path, name: &std::ffi::OsStr) -> io::Result<()> {
    match fs::remove_file(root.join(name)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(unix)]
fn create_owner(directory: &File, _root: &Path, name: &str) -> io::Result<File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use std::os::unix::fs::PermissionsExt;
    let flags =
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let file = File::from(
        openat(directory, name, flags, Mode::S_IRUSR | Mode::S_IWUSR).map_err(io::Error::from)?,
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    validate(&file, false)?;
    Ok(file)
}

#[cfg(windows)]
fn create_owner(_directory: &File, root: &Path, name: &str) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(0x80000000 | 0x40000000 | 0x00020000 | 0x00040000)
        .custom_flags(0x00200000)
        .share_mode(0x1 | 0x2)
        .open(root.join(name))?;
    harden_handle(&mut file, false)?;
    validate(&file, false)?;
    Ok(file)
}

fn private_parent(root: &Path) -> &Path {
    root.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn private_directory(root: &Path) -> io::Result<File> {
    match open_directory(root, false) {
        Ok(directory) => return Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(private_parent(root))?;
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
    open_directory(root, created)
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
        metadata.is_file() && metadata.nlink() <= 1
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
fn open_directory(path: &Path, created: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC)
        .open(path)?;
    if created {
        use std::os::unix::fs::PermissionsExt;
        // DirBuilder's initial permissions are masked by the process umask.
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    validate(&file, true)?;
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

#[cfg(test)]
mod owner_tests {
    use super::*;

    // Catches: splitting a record into separate payload/newline submissions.
    #[test]
    fn owner_record_uses_one_buffer() {
        struct Once(Vec<u8>);
        impl Write for Once {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.0.is_empty() {
                    return Err(io::Error::other("second submission"));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Once(Vec::new());
        write_record(&mut writer, b"payload").unwrap();
        assert_eq!(writer.0, b"payload\n");
    }

    // Catches: create-new accidentally falling back to opening a collision,
    // including a symlink/FIFO/regular leaf selected before creation.
    #[cfg(unix)]
    #[test]
    fn exclusive_owner_creation_never_opens_a_colliding_leaf() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let directory = private_directory(&root).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        for kind in ["regular", "symlink", "fifo", "directory"] {
            let path = root.join(kind);
            match kind {
                "regular" => fs::write(&path, b"unchanged").unwrap(),
                "symlink" => symlink(&victim, &path).unwrap(),
                "fifo" => nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            assert!(create_owner(&directory, &root, kind).is_err());
        }
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
        assert_eq!(fs::read(root.join("regular")).unwrap(), b"unchanged");
    }
}
