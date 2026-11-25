//! Local encrypted credential vault. A separate local key protects against
//! disclosure of the vault alone, not against copying the whole user profile.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use zeroize::{Zeroize, Zeroizing};

use super::{CredentialBackend, CredentialBackendError};
use crate::privacy::{self, PrivatePathKind};
use crate::store::{OsIo, StoreIo};

const MAGIC: &[u8; 8] = b"EGO-SEC\0";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const MAX_VAULT_BYTES: u64 = 16 * 1024 * 1024;
const AAD: &[u8] = b"ego:credential-vault:v1";

#[derive(Debug, Clone)]
pub struct FileCredentialBackend {
    root: PathBuf,
}

impl FileCredentialBackend {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn with_vault<T>(
        &self,
        write: bool,
        action: impl FnOnce(&mut Entries) -> Result<T, CredentialBackendError>,
    ) -> Result<T, CredentialBackendError> {
        self.prepare_root()?;
        let lock_path = self.root.join("secrets.lock");
        let file = open_private(&lock_path)?;
        let mut lock = RwLock::new(file);
        let _guard = lock
            .write()
            .map_err(|_| CredentialBackendError::Unavailable)?;
        let vault_path = self.root.join("secrets.enc");
        let key_path = self.root.join("secrets.key");
        let has_vault = exists_private(&vault_path)?;
        let has_key = exists_private(&key_path)?;
        if has_vault && !has_key {
            return Err(CredentialBackendError::BadStoreFormat);
        }
        let mut entries = if has_vault {
            let key = load_key(&key_path)?;
            decrypt(&vault_path, &key)?
        } else {
            Entries::default()
        };
        let result = action(&mut entries)?;
        if write && (has_vault || !entries.0.is_empty()) {
            let key = if has_key {
                load_key(&key_path)?
            } else {
                create_key(&key_path)?
            };
            let ciphertext = encrypt(&entries, &key)?;
            if has_vault {
                privacy::validate_private_path(&vault_path, PrivatePathKind::File)
                    .map_err(|_| CredentialBackendError::NoStorageAccess)?;
            }
            let staging_path = crate::store::tmp_path_for(&vault_path);
            if exists_private(&staging_path)? {
                fs::remove_file(&staging_path)
                    .map_err(|_| CredentialBackendError::NoStorageAccess)?;
            }
            OsIo.replace_atomic_private(&vault_path, &ciphertext)
                .map_err(|_| CredentialBackendError::Unavailable)?;
        }
        Ok(result)
    }

    fn prepare_root(&self) -> Result<(), CredentialBackendError> {
        if let Some(parent) = self.root.parent() {
            #[cfg(unix)]
            let mut parent_builder = fs::DirBuilder::new();
            #[cfg(not(unix))]
            let parent_builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                parent_builder.mode(0o700);
            }
            match parent_builder.create(parent) {
                Ok(()) => privacy::harden_private_path(parent, PrivatePathKind::Directory)
                    .map_err(|_| CredentialBackendError::NoStorageAccess)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(CredentialBackendError::NoStorageAccess),
            }
            let parent_type = fs::symlink_metadata(parent)
                .map_err(|_| CredentialBackendError::NoStorageAccess)?
                .file_type();
            if !parent_type.is_dir() || parent_type.is_symlink() {
                return Err(CredentialBackendError::NoStorageAccess);
            }
        }
        #[cfg(windows)]
        self.prepare_windows_root()?;
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&self.root) {
                Ok(()) => privacy::harden_private_path(&self.root, PrivatePathKind::Directory)
                    .map_err(|_| CredentialBackendError::NoStorageAccess)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(CredentialBackendError::NoStorageAccess),
            }
        }
        privacy::validate_private_path(&self.root, PrivatePathKind::Directory)
            .map_err(|_| CredentialBackendError::NoStorageAccess)
    }

    #[cfg(windows)]
    fn prepare_windows_root(&self) -> Result<(), CredentialBackendError> {
        match fs::symlink_metadata(&self.root) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(CredentialBackendError::NoStorageAccess),
        }
        let parent = self
            .root
            .parent()
            .ok_or(CredentialBackendError::NoStorageAccess)?;
        let staged = tempfile::Builder::new()
            .prefix(".ego-secrets-init-")
            .tempdir_in(parent)
            .map_err(|_| CredentialBackendError::NoStorageAccess)?;
        privacy::harden_private_path(staged.path(), PrivatePathKind::Directory)
            .map_err(|_| CredentialBackendError::NoStorageAccess)?;
        let lock = open_private(&staged.path().join("secrets.lock"))?;
        drop(lock);
        match fs::rename(staged.path(), &self.root) {
            Ok(()) => Ok(()),
            Err(_) if fs::symlink_metadata(&self.root).is_ok() => Ok(()),
            Err(_) => Err(CredentialBackendError::NoStorageAccess),
        }
    }
}

impl CredentialBackend for FileCredentialBackend {
    fn get(&self, service: &str, user: &str) -> Result<Option<Vec<u8>>, CredentialBackendError> {
        let id = entry_id(service, user)?;
        self.with_vault(false, |entries| Ok(entries.0.get(&id).cloned()))
    }

    fn put(&self, service: &str, user: &str, value: Vec<u8>) -> Result<(), CredentialBackendError> {
        let id = entry_id(service, user)?;
        let value = Zeroizing::new(value);
        self.with_vault(true, |entries| {
            entries.0.insert(id, value.to_vec());
            Ok(())
        })
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialBackendError> {
        let id = entry_id(service, user)?;
        self.with_vault(true, |entries| Ok(entries.0.remove(&id).is_some()))
    }
}

#[derive(Default)]
struct Entries(BTreeMap<String, Vec<u8>>);

impl Drop for Entries {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

fn entry_id(service: &str, user: &str) -> Result<String, CredentialBackendError> {
    if service.is_empty() || user.is_empty() || service.contains('\0') || user.contains('\0') {
        return Err(CredentialBackendError::Invalid);
    }
    Ok(format!("{service}\0{user}"))
}

fn exists_private(path: &Path) -> Result<bool, CredentialBackendError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            privacy::validate_private_path(path, PrivatePathKind::File)
                .map_err(|_| CredentialBackendError::NoStorageAccess)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(CredentialBackendError::NoStorageAccess),
    }
}

#[cfg(not(windows))]
fn open_private(path: &Path) -> Result<File, CredentialBackendError> {
    if exists_private(path)? {
        return OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| CredentialBackendError::NoStorageAccess);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return if exists_private(path)? {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|_| CredentialBackendError::NoStorageAccess)
            } else {
                Err(CredentialBackendError::NoStorageAccess)
            };
        }
        Err(_) => return Err(CredentialBackendError::NoStorageAccess),
    };
    privacy::harden_private_path(path, PrivatePathKind::File)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    Ok(file)
}

#[cfg(windows)]
fn open_private(path: &Path) -> Result<File, CredentialBackendError> {
    if exists_private(path)? {
        return OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| CredentialBackendError::NoStorageAccess);
    }
    let parent = path
        .parent()
        .ok_or(CredentialBackendError::NoStorageAccess)?;
    let staged = tempfile::Builder::new()
        .prefix(".ego-lock-init-")
        .tempfile_in(parent)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    privacy::harden_private_path(staged.path(), PrivatePathKind::File)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    let staged_path = staged.path().to_path_buf();
    match staged.persist_noclobber(path) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(staged_path);
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|_| CredentialBackendError::NoStorageAccess)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            if !exists_private(path)? {
                return Err(CredentialBackendError::NoStorageAccess);
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|_| CredentialBackendError::NoStorageAccess)
        }
        Err(_) => Err(CredentialBackendError::NoStorageAccess),
    }
}

fn load_key(path: &Path) -> Result<Zeroizing<[u8; KEY_LEN]>, CredentialBackendError> {
    let file = File::open(path).map_err(|_| CredentialBackendError::NoStorageAccess)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(KEY_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    let key: [u8; KEY_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| CredentialBackendError::BadStoreFormat)?;
    Ok(Zeroizing::new(key))
}

fn create_key(path: &Path) -> Result<Zeroizing<[u8; KEY_LEN]>, CredentialBackendError> {
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    getrandom::fill(&mut *key).map_err(|_| CredentialBackendError::Unavailable)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    privacy::harden_private_path(path, PrivatePathKind::File)
        .and_then(|()| file.write_all(key.as_slice()))
        .and_then(|()| file.sync_all())
        .map_err(|_| CredentialBackendError::Unavailable)?;
    Ok(key)
}

fn cipher(key: &[u8; KEY_LEN]) -> Result<LessSafeKey, CredentialBackendError> {
    UnboundKey::new(&aead::AES_256_GCM, key)
        .map(LessSafeKey::new)
        .map_err(|_| CredentialBackendError::BadStoreFormat)
}

fn encrypt(entries: &Entries, key: &[u8; KEY_LEN]) -> Result<Vec<u8>, CredentialBackendError> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| CredentialBackendError::Unavailable)?;
    let mut plaintext = Zeroizing::new(
        serde_json::to_vec(&entries.0).map_err(|_| CredentialBackendError::Unavailable)?,
    );
    if plaintext.len() as u64 > MAX_VAULT_BYTES - 64 {
        return Err(CredentialBackendError::TooLong);
    }
    cipher(key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(AAD),
            &mut *plaintext,
        )
        .map_err(|_| CredentialBackendError::Unavailable)?;
    let mut output = Vec::with_capacity(MAGIC.len() + NONCE_LEN + plaintext.len());
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&plaintext);
    Ok(output)
}

fn decrypt(path: &Path, key: &[u8; KEY_LEN]) -> Result<Entries, CredentialBackendError> {
    let length = fs::metadata(path)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?
        .len();
    if length > MAX_VAULT_BYTES {
        return Err(CredentialBackendError::TooLong);
    }
    let mut contents = Zeroizing::new(Vec::with_capacity(length as usize));
    File::open(path)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?
        .take(MAX_VAULT_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CredentialBackendError::NoStorageAccess)?;
    if contents.len() as u64 > MAX_VAULT_BYTES {
        return Err(CredentialBackendError::TooLong);
    }
    if contents.len() < MAGIC.len() + NONCE_LEN + aead::AES_256_GCM.tag_len()
        || !contents.starts_with(MAGIC)
    {
        return Err(CredentialBackendError::BadStoreFormat);
    }
    let nonce: [u8; NONCE_LEN] = contents[MAGIC.len()..MAGIC.len() + NONCE_LEN]
        .try_into()
        .map_err(|_| CredentialBackendError::BadStoreFormat)?;
    let plaintext = cipher(key)?
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(AAD),
            &mut contents[MAGIC.len() + NONCE_LEN..],
        )
        .map_err(|_| CredentialBackendError::BadDataFormat)?;
    let entries =
        serde_json::from_slice(plaintext).map_err(|_| CredentialBackendError::BadStoreFormat)?;
    Ok(Entries(entries))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // Catches a plaintext vault, lost bindings, or a second process replacing another entry.
    #[test]
    fn bindings_are_encrypted_and_concurrent_updates_survive() {
        let home = tempfile::tempdir().unwrap();
        let backend = Arc::new(FileCredentialBackend::new(home.path().join("secrets")));
        let handles: Vec<_> = (0..12)
            .map(|index| {
                let backend = Arc::clone(&backend);
                std::thread::spawn(move || {
                    backend
                        .put(
                            "ego",
                            &format!("account-{index}"),
                            format!("private-{index}").into_bytes(),
                        )
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let vault = fs::read(home.path().join("secrets/secrets.enc")).unwrap();
        assert!(
            !vault
                .windows(b"private-0".len())
                .any(|part| part == b"private-0")
        );
        assert_eq!(
            fs::read(home.path().join("secrets/secrets.key"))
                .unwrap()
                .len(),
            32
        );
        for index in 0..12 {
            assert_eq!(
                backend.get("ego", &format!("account-{index}")).unwrap(),
                Some(format!("private-{index}").into_bytes())
            );
        }
        assert!(backend.delete("ego", "account-0").unwrap());
        assert_eq!(backend.get("ego", "account-0").unwrap(), None);
        assert_eq!(
            backend.get("ego", "account-1").unwrap(),
            Some(b"private-1".to_vec())
        );
    }

    // Catches a key replacement on missing key and acceptance of altered ciphertext.
    #[test]
    fn missing_key_or_modified_vault_is_not_reset() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("secrets");
        let backend = FileCredentialBackend::new(&root);
        backend.put("ego", "user", b"value".to_vec()).unwrap();
        let vault = root.join("secrets.enc");
        let original = fs::read(&vault).unwrap();
        let key = root.join("secrets.key");
        let key_bytes = fs::read(&key).unwrap();
        fs::remove_file(&key).unwrap();
        assert!(backend.put("ego", "user", b"new".to_vec()).is_err());
        assert_eq!(fs::read(&vault).unwrap(), original);
        fs::write(&key, &key_bytes).unwrap();
        privacy::harden_private_path(&key, PrivatePathKind::File).unwrap();
        let mut modified = original.clone();
        *modified.last_mut().unwrap() ^= 1;
        fs::write(&vault, modified).unwrap();
        assert_eq!(
            backend.get("ego", "user"),
            Err(CredentialBackendError::BadDataFormat)
        );
        assert!(backend.put("ego", "user", b"new".to_vec()).is_err());
        fs::write(&vault, original).unwrap();
        let mut wrong_key = key_bytes;
        wrong_key[0] ^= 1;
        fs::write(&key, wrong_key).unwrap();
        assert_eq!(
            backend.get("ego", "user"),
            Err(CredentialBackendError::BadDataFormat)
        );
    }

    // Catches a crash leaving the fixed atomic-publication staging name occupied.
    #[test]
    fn abandoned_staging_file_does_not_block_next_credential_update() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("secrets");
        let backend = FileCredentialBackend::new(&root);
        backend.put("ego", "before", b"original".to_vec()).unwrap();
        let vault = root.join("secrets.enc");
        let staged = crate::store::tmp_path_for(&vault);
        fs::write(&staged, fs::read(&vault).unwrap()).unwrap();
        privacy::harden_private_path(&staged, PrivatePathKind::File).unwrap();
        backend.put("ego", "after", b"new".to_vec()).unwrap();
        assert_eq!(
            backend.get("ego", "before").unwrap(),
            Some(b"original".to_vec())
        );
        assert_eq!(backend.get("ego", "after").unwrap(), Some(b"new".to_vec()));
        assert!(!staged.exists());
    }

    // Catches a first-login failure when EGO_HOME has not yet been created.
    #[test]
    fn fresh_home_creates_private_secret_directory() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("new-home");
        let backend = FileCredentialBackend::new(home.join("secrets"));
        backend.put("ego", "user", b"secret".to_vec()).unwrap();
        assert_eq!(
            backend.get("ego", "user").unwrap(),
            Some(b"secret".to_vec())
        );
        privacy::validate_private_path(&home, PrivatePathKind::Directory).unwrap();
        privacy::validate_private_path(&home.join("secrets"), PrivatePathKind::Directory).unwrap();
    }

    // Catches silently following an attacker-controlled link in a private directory.
    #[cfg(unix)]
    #[test]
    fn unsafe_paths_fail_closed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("secrets");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = home.path().join("outside");
        fs::write(&target, b"sentinel").unwrap();
        symlink(&target, root.join("secrets.key")).unwrap();
        let backend = FileCredentialBackend::new(&root);
        assert_eq!(
            backend.put("ego", "user", b"secret".to_vec()),
            Err(CredentialBackendError::NoStorageAccess)
        );
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
        fs::remove_file(root.join("secrets.key")).unwrap();
        backend.put("ego", "user", b"secret".to_vec()).unwrap();
        fs::set_permissions(root.join("secrets.key"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            backend.get("ego", "user"),
            Err(CredentialBackendError::NoStorageAccess)
        );
    }
}
