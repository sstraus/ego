#![cfg(unix)]

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ego_core::endpoint::{PublicBind, Requested, admit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

fn endpoint(root: &Path) -> (ego_core::endpoint::Admitted, PathBuf) {
    let path = root.join("endpoint").join("ego.sock");
    let admitted = admit(
        Requested::Local {
            name: path.display().to_string(),
        },
        PublicBind::Refused,
    )
    .expect("local endpoint admission");
    (admitted, path)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn regular_endpoint_file_is_refused_and_preserved() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).expect("parent");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("private parent");
    std::fs::write(&path, b"do not delete").expect("regular file");
    let result = ego_listen::bind(&admitted).await;
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"do not delete");
}

#[tokio::test]
async fn endpoint_symlink_is_refused_and_preserved() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).expect("parent");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("private parent");
    let target = root.path().join("target");
    std::fs::write(&target, b"target bytes").expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
        .expect("private target");
    std::os::unix::fs::symlink(&target, &path).expect("symlink");
    let result = ego_listen::bind(&admitted).await;
    assert!(result.is_err());
    assert_eq!(std::fs::read_link(&path).unwrap(), target);
    assert_eq!(std::fs::read(&target).unwrap(), b"target bytes");
}

#[tokio::test]
async fn active_socket_is_not_unlinked_by_a_second_bind() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let first = ego_listen::bind(&admitted).await.expect("first bind");
    let second = ego_listen::bind(&admitted).await;
    assert!(second.is_err());
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    let client = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&path)
            .await
            .expect("first listener survives");
        stream.write_all(b"ping").await.expect("write");
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.expect("read");
        reply
    });
    let accepted = timeout(Duration::from_secs(1), first.accept())
        .await
        .expect("first listener accepted within timeout")
        .expect("accept result")
        .expect("a current-user client is accepted");
    let mut stream = accepted.stream;
    let mut request = [0; 4];
    timeout(Duration::from_secs(1), stream.read_exact(&mut request))
        .await
        .expect("read within timeout")
        .expect("read request");
    assert_eq!(&request, b"ping");
    stream.write_all(b"pong").await.expect("write response");
    assert_eq!(client.await.expect("client task"), *b"pong");
}

#[tokio::test]
async fn ordinary_socket_parent_gets_private_child_and_reported_endpoint() {
    use std::os::unix::fs::MetadataExt;
    for parent_mode in [0o755, 0o750] {
        let root = tempfile::tempdir().expect("fixture root");
        let (admitted, path) = endpoint(root.path());
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).expect("parent");
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(parent_mode)).unwrap();
        let bound = ego_listen::bind(&admitted).await.expect("private child");
        // The user's parent is never chmod-ed.
        assert_eq!(mode(parent), parent_mode);
        let reported = PathBuf::from(bound.advertised().expect("advertised"));
        assert_ne!(reported, path);
        let child = reported.parent().unwrap();
        assert_eq!(child.parent().unwrap(), parent);
        assert_eq!(mode(child), 0o700);
        assert_eq!(
            std::fs::metadata(child).unwrap().uid(),
            rustix::process::geteuid().as_raw()
        );
        assert!(
            std::fs::symlink_metadata(&reported)
                .unwrap()
                .file_type()
                .is_socket()
        );
        // The reported endpoint serves its owner.
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(&reported).await.expect("connect");
            stream.write_all(b"ping").await.expect("write");
        });
        let accepted = timeout(Duration::from_secs(1), bound.accept())
            .await
            .expect("accept timeout")
            .expect("accept result")
            .expect("peer");
        assert_eq!(accepted.peer, ego_listen::Peer::CurrentUser);
        client.await.expect("client task");
    }
}

#[tokio::test]
async fn socket_child_cannot_follow_hostile_link_or_replace_existing_file() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).expect("parent");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let child = parent.join("ego.sock.d");

    let target = root.path().join("hostile-target");
    std::fs::create_dir(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&target, &child).expect("hostile child link");
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(std::fs::read_link(&child).unwrap(), target);
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    std::fs::remove_file(&child).unwrap();

    std::fs::write(&child, b"do not delete").expect("child file");
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(std::fs::read(&child).unwrap(), b"do not delete");
    std::fs::remove_file(&child).unwrap();

    std::fs::create_dir(&child).expect("loose child");
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(mode(&child), 0o755);

    // A previous run's private child is reused, not refused or replaced.
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bound = ego_listen::bind(&admitted).await.expect("reuse own child");
    assert_eq!(
        bound.advertised().unwrap(),
        child.join("ego.sock").display().to_string()
    );
}

#[tokio::test]
async fn existing_private_parent_is_accepted_without_permission_change() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).expect("parent");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _bound = ego_listen::bind(&admitted).await.expect("private parent");
    assert_eq!(mode(parent), 0o700);
}

#[tokio::test]
async fn new_parent_is_created_private() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let _bound = ego_listen::bind(&admitted).await.expect("new parent");
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[tokio::test]
async fn non_directory_parent_is_refused_and_preserved() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let parent = path.parent().unwrap();
    std::fs::write(parent, b"parent file").expect("parent file");
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(std::fs::read(parent).unwrap(), b"parent file");
}

#[tokio::test]
async fn symlink_parent_is_refused_and_preserved() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let parent = path.parent().unwrap();
    let target = root.path().join("target-parent");
    std::fs::create_dir(&target).expect("target parent");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
        .expect("private target parent");
    std::os::unix::fs::symlink(&target, parent).expect("parent symlink");
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(std::fs::read_link(parent).unwrap(), target);
}

#[tokio::test]
async fn a_symlinked_lease_is_refused_without_following_or_replacing_it() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).expect("parent");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = root.path().join("lease-target");
    std::fs::write(&target, b"lease target").expect("lease target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    let lease = parent.join("ego.sock.lock");
    std::os::unix::fs::symlink(&target, &lease).expect("lease symlink");
    assert!(ego_listen::bind(&admitted).await.is_err());
    assert_eq!(std::fs::read_link(&lease).unwrap(), target);
    assert_eq!(std::fs::read(&target).unwrap(), b"lease target");
}

#[tokio::test]
async fn a_live_non_ego_listener_is_not_replaced() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).expect("parent");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let raw = tokio::net::UnixListener::bind(&path).expect("raw listener");
    assert!(ego_listen::bind(&admitted).await.is_err());
    let (probe, _) = timeout(Duration::from_secs(1), raw.accept())
        .await
        .expect("raw accept timeout")
        .expect("raw accept");
    drop(probe);
    let mut client = UnixStream::connect(&path)
        .await
        .expect("raw listener survives");
    client.write_all(b"ping").await.expect("write ping");
    let (mut peer, _) = timeout(Duration::from_secs(1), raw.accept())
        .await
        .expect("raw second accept timeout")
        .expect("raw second accept");
    let mut request = [0; 4];
    peer.read_exact(&mut request).await.expect("read ping");
    assert_eq!(&request, b"ping");
    peer.shutdown().await.expect("raw shutdown");
}

#[tokio::test]
async fn a_stale_socket_is_reused_after_the_previous_listener_closes() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let first = ego_listen::bind(&admitted).await.expect("first bind");
    drop(first);
    let second = ego_listen::bind(&admitted)
        .await
        .expect("stale socket reuse");
    let client = tokio::spawn(async move {
        let mut stream = UnixStream::connect(&path).await.expect("second listener");
        stream.write_all(b"ping").await.expect("write");
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.expect("read");
        reply
    });
    let accepted = timeout(Duration::from_secs(1), second.accept())
        .await
        .expect("accept timeout")
        .expect("accept result")
        .expect("peer");
    let mut stream = accepted.stream;
    let mut request = [0; 4];
    stream.read_exact(&mut request).await.expect("read request");
    stream.write_all(b"pong").await.expect("write response");
    assert_eq!(client.await.expect("client task"), *b"pong");
}

#[tokio::test]
async fn a_connected_peer_survives_unlinking_the_socket_path() {
    let root = tempfile::tempdir().expect("fixture root");
    let (admitted, path) = endpoint(root.path());
    let bound = ego_listen::bind(&admitted).await.expect("bind");
    let (connected, ready) = oneshot::channel();
    let client_path = path.clone();
    let client = tokio::spawn(async move {
        let mut stream = UnixStream::connect(client_path).await.expect("connect");
        connected.send(()).expect("connection notification");
        stream.write_all(b"ping").await.expect("write");
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.expect("read");
        reply
    });
    timeout(Duration::from_secs(1), ready)
        .await
        .expect("connection setup timeout")
        .expect("connection notification");
    std::fs::remove_file(&path).expect("unlink pathname");
    let accepted = timeout(Duration::from_secs(1), bound.accept())
        .await
        .expect("accept timeout")
        .expect("accept result")
        .expect("peer");
    assert_eq!(accepted.peer, ego_listen::Peer::CurrentUser);
    let mut stream = accepted.stream;
    let mut request = [0; 4];
    stream.read_exact(&mut request).await.expect("read request");
    stream.write_all(b"pong").await.expect("write response");
    assert_eq!(client.await.expect("client task"), *b"pong");
}
