//! Hostile cases for the file that keeps "allow always" sandbox grants.
//!
//! The file widens what every later session in a workspace can reach, so a
//! lost update, a swallowed corruption or a loose mode each turn into reach
//! nobody asked for.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ego_acp::sandbox::SandboxGrantStore;
use ego_cli::acp_sandbox_grants::FileSandboxGrants;
use ego_core::permit::{CanonPath, SandboxAccess, SandboxGrant};

fn remember(
    store: &dyn SandboxGrantStore,
    workspace: &Path,
    grant: &SandboxGrant,
) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(store.remember(workspace, grant))
}

fn grant(path: &str, access: SandboxAccess) -> SandboxGrant {
    SandboxGrant::new(CanonPath::new(path), access)
}

/// Catches: two sessions (here, two stores on one home, as two processes would
/// be) answering "always" at once lose each other's grant because the read-modify-
/// write is not exclusive.
#[test]
fn concurrent_always_answers_all_survive() {
    let home = tempfile::tempdir().expect("home");
    let workspace = PathBuf::from("/work/space");
    let stores = [
        Arc::new(FileSandboxGrants::new(home.path().to_path_buf())),
        Arc::new(FileSandboxGrants::new(home.path().to_path_buf())),
    ];
    let threads = (0..16)
        .map(|index| {
            let store = Arc::clone(&stores[index % 2]);
            let workspace = workspace.clone();
            std::thread::spawn(move || {
                remember(
                    store.as_ref(),
                    &workspace,
                    &grant(&format!("/outside/path{index}"), SandboxAccess::Read),
                )
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().expect("thread").expect("remember");
    }
    assert_eq!(stores[0].load(&workspace).len(), 16);
}

/// Catches: a corrupt document is read as "no grants" and the next answer
/// replaces it with a fresh one, silently destroying the other workspaces'
/// grants (or, if load reported grants from garbage, widening the sandbox).
#[test]
fn a_corrupt_document_grants_nothing_and_is_never_overwritten() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("sandbox-grants.json");
    std::fs::write(&path, b"{\"grants\": [ not json").expect("corrupt document");
    let store = FileSandboxGrants::new(home.path().to_path_buf());
    let workspace = Path::new("/work/space");
    assert!(store.load(workspace).is_empty());
    let outcome = remember(&store, workspace, &grant("/outside/a", SandboxAccess::Read));
    assert!(
        outcome.is_err(),
        "remembering over garbage must fail, not replace it"
    );
    assert_eq!(
        std::fs::read(&path).expect("document"),
        b"{\"grants\": [ not json",
        "the unreadable document was overwritten"
    );
}

/// Catches: "always for this workspace" leaks into another workspace, including
/// one whose path only starts with the same text.
#[test]
fn a_stored_grant_is_visible_only_to_its_own_workspace() {
    let home = tempfile::tempdir().expect("home");
    let store = FileSandboxGrants::new(home.path().to_path_buf());
    let g = grant("/outside/a", SandboxAccess::Write);
    remember(&store, Path::new("/work/space"), &g).expect("remember");
    assert_eq!(store.load(Path::new("/work/space")), vec![g]);
    assert!(store.load(Path::new("/work/space-evil")).is_empty());
    assert!(store.load(Path::new("/work")).is_empty());
    assert!(store.load(Path::new("/work/space/sub")).is_empty());
}

/// Catches: the same answer given twice stores two rows, so a revoke by hand
/// has to find both, and the profile is rendered with duplicates.
#[test]
fn remembering_the_same_grant_twice_stores_it_once() {
    let home = tempfile::tempdir().expect("home");
    let store = FileSandboxGrants::new(home.path().to_path_buf());
    let workspace = Path::new("/work/space");
    let g = grant("/outside/a", SandboxAccess::Read);
    remember(&store, workspace, &g).expect("first");
    remember(&store, workspace, &g).expect("second");
    assert_eq!(store.load(workspace), vec![g]);
}

/// Catches: the ledger is created world-readable; it names every path a person
/// opened to commands, so it is private to the user like the trust ledger.
#[cfg(unix)]
#[test]
fn the_ledger_and_its_home_are_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let parent = tempfile::tempdir().expect("parent");
    let home = parent.path().join("ego-home");
    let store = FileSandboxGrants::new(home.clone());
    remember(
        &store,
        Path::new("/work/space"),
        &grant("/outside/a", SandboxAccess::Read),
    )
    .expect("remember");
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&home.join("sandbox-grants.json")), 0o600);
    assert_eq!(mode(&home), 0o700);
}
