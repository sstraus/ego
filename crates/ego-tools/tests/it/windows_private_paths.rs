#![cfg(windows)]

use std::path::Path;

use ego_tools::privacy::{PrivatePathKind, harden_private_path};
use tempfile::tempdir;
use windows_permissions::constants::{SeObjectType, SecurityInformation};
use windows_permissions::utilities::current_process_sid;
use windows_permissions::wrappers::GetNamedSecurityInfo;

fn dacl(path: &Path) -> (String, u32) {
    let descriptor = GetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .expect("the private path has a security descriptor");
    let ace_count = descriptor
        .dacl()
        .expect("the private path has a DACL")
        .len();
    let sddl = descriptor
        .as_sddl()
        .expect("the private path security descriptor serializes")
        .to_string_lossy()
        .into_owned();
    (sddl, ace_count)
}

#[test]
fn private_files_and_directories_admit_only_the_current_windows_user() {
    let root = tempdir().unwrap();
    let directory = root.path().join("private");
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("secret.json");
    std::fs::write(&file, b"secret").unwrap();

    harden_private_path(&directory, PrivatePathKind::Directory).unwrap();
    harden_private_path(&file, PrivatePathKind::File).unwrap();

    let sid = current_process_sid().unwrap();
    let (directory_dacl, directory_aces) = dacl(&directory);
    assert!(directory_dacl.starts_with("D:P"), "{directory_dacl}");
    assert_eq!(directory_aces, 1, "{directory_dacl}");
    assert!(
        directory_dacl.ends_with(&format!("(A;OICI;FA;;;{sid})")),
        "{directory_dacl}"
    );

    let (file_dacl, file_aces) = dacl(&file);
    assert!(file_dacl.starts_with("D:P"), "{file_dacl}");
    assert_eq!(file_aces, 1, "{file_dacl}");
    assert!(
        file_dacl.ends_with(&format!("(A;;FA;;;{sid})")),
        "{file_dacl}"
    );
}
