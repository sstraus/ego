//! Dedicated PTY composition: production CLI, isolated fixture credentials.
//! No environment flag can install this backend in the production executable.

#[cfg(debug_assertions)]
#[path = "../../src/main.rs"]
mod process;

#[cfg(debug_assertions)]
mod credentials {
    use ego_tools::credentials::{CredentialBackend, CredentialBackendError};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    pub struct FixtureStore(pub PathBuf);

    impl FixtureStore {
        fn path(&self, service: &str, user: &str) -> PathBuf {
            self.0.join(format!(
                "{:x}",
                Sha256::digest(format!("{service}\0{user}").as_bytes())
            ))
        }
    }

    impl CredentialBackend for FixtureStore {
        fn get(
            &self,
            service: &str,
            user: &str,
        ) -> Result<Option<Vec<u8>>, CredentialBackendError> {
            match std::fs::read(self.path(service, user)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(CredentialBackendError::Unavailable),
            }
        }

        fn put(
            &self,
            service: &str,
            user: &str,
            value: Vec<u8>,
        ) -> Result<(), CredentialBackendError> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(self.path(service, user))
                .map_err(|_| CredentialBackendError::Unavailable)?;
            file.write_all(&value)
                .map_err(|_| CredentialBackendError::Unavailable)
        }

        fn delete(&self, service: &str, user: &str) -> Result<bool, CredentialBackendError> {
            match std::fs::remove_file(self.path(service, user)) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(CredentialBackendError::Unavailable),
            }
        }
    }
}

#[cfg(debug_assertions)]
fn main() -> std::process::ExitCode {
    let root = std::path::PathBuf::from(std::env::var_os("EGO_HOME").expect("fixture EGO_HOME"))
        .join("fixture-credentials");
    std::fs::create_dir_all(&root).expect("fixture credential directory");
    assert!(
        ego_tools::credentials::install_test_credential_backend(std::sync::Arc::new(
            credentials::FixtureStore(root)
        ),)
        .is_ok()
    );
    process::main()
}

// A release build, even with all features, contains neither the fixture store
// nor the programmatic override. It cannot start this test process.
#[cfg(not(debug_assertions))]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(125)
}
