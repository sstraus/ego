//! The one inbound listener: bind, accept, and establish who connected.
//!
//! `docs/decisions/2026-09-07-one-inbound-endpoint-serves-every-host-protocol.md`
//! separates the endpoint from the protocol. [`ego_core::endpoint`] decides
//! which endpoint kinds exist and which authentication each compels; this crate
//! is the thin impure half that opens one and answers *who is on the other
//! end*. It owns no protocol and cannot reach one: A2A serves `hyper` over the
//! stream it yields and ACP serves `ByteStreams` over the same stream, and
//! neither binds anything.
//!
//! **The local endpoint is authenticated by the operating system, twice.** The
//! socket lives in a directory the current user alone may enter, so the kernel
//! refuses a connection from anyone else before this crate sees it. Then every
//! accepted peer's uid is compared with the owner captured when the endpoint
//! was bound; authentication never consults mutable pathname metadata.
//!
//! **Windows is deliberately not served here yet.** The named pipe named by the
//! ADR is the right primitive, but this crate cannot be compiled or run against
//! it on the machine it was written on, and story `regression_case` exists to build and
//! test this repository natively on Windows. Shipping an untested
//! `#[cfg(windows)]` path would be a claim without evidence, so the platform
//! refuses by name instead — the same discipline the provider rows use.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use ego_core::endpoint::{Admitted, Authentication};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

mod limits;

pub use limits::{
    ConnectionGate, ConnectionGateError, ConnectionLimits, ConnectionLimitsError, GuardedStream,
};

/// Who connected, as far as the transport can prove it.
///
/// Not a uid or a `SocketAddr`: what a protocol above needs to decide is
/// whether the transport already answered the authentication question, and
/// those two spellings would invite it to make that judgement itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// The operating system vouched for this peer: it holds the identity that
    /// owns the endpoint. Nothing further is required before routing.
    CurrentUser,
    /// The transport proves nothing about who this is. A credential is
    /// required before routing, and [`Admitted::authentication`] said so
    /// before the socket existed.
    Unauthenticated { address: std::net::SocketAddr },
}

/// A connection that has already been through whatever the transport enforces.
pub struct Accepted {
    pub stream: Stream,
    pub peer: Peer,
}

/// The byte stream a protocol serves over, whichever transport produced it.
///
/// An enum rather than `Box<dyn AsyncRead + AsyncWrite>` because both consumers
/// need the concrete `AsyncRead + AsyncWrite + Unpin` bound — `hyper`'s
/// `TokioIo` and `ByteStreams` each take it by value — and a trait object would
/// buy dynamism no caller wants at the cost of an allocation per connection.
pub enum Stream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

macro_rules! delegate {
    ($self:ident, $stream:ident => $call:expr) => {
        match $self.get_mut() {
            #[cfg(unix)]
            Stream::Unix($stream) => $call,
            Stream::Tcp($stream) => $call,
        }
    };
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_read(cx, buf))
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, stream => Pin::new(stream).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_shutdown(cx))
    }
}

/// An endpoint that is open.
pub enum Bound {
    #[cfg(unix)]
    Local {
        listener: UnixListener,
        path: PathBuf,
        owner: u32,
        _lease: fd_lock::RwLock<std::fs::File>,
    },
    Tcp(TcpListener),
}

impl Bound {
    /// How this endpoint should be named to a caller that has to reach it —
    /// the address the socket actually got, not the one that was requested.
    ///
    /// Port 0 is how a test asks the operating system for a free port, and a
    /// card or a log line built from the requested address would then advertise
    /// port 0 to the world.
    pub fn advertised(&self) -> io::Result<String> {
        match self {
            #[cfg(unix)]
            Self::Local { path, .. } => Ok(path.display().to_string()),
            Self::Tcp(listener) => Ok(listener.local_addr()?.to_string()),
        }
    }

    /// Accept one connection, refusing any peer the transport cannot vouch for
    /// on an endpoint whose whole authentication *is* the transport.
    ///
    /// A refused local peer returns `Ok(None)` rather than an error: a caller
    /// looping on `accept` must treat one rejected peer as a connection that
    /// did not happen, not as a reason to stop serving the peers that are
    /// entitled to be here.
    pub async fn accept(&self) -> io::Result<Option<Accepted>> {
        match self {
            #[cfg(unix)]
            Self::Local {
                listener,
                path,
                owner,
                ..
            } => {
                let (stream, _) = listener.accept().await?;
                if stream.peer_cred()?.uid() != *owner {
                    tracing::info!(
                        target: "ego::listen",
                        endpoint = %path.display(),
                        "a local connection was refused: its peer is not the endpoint's owner"
                    );
                    return Ok(None);
                }
                Ok(Some(Accepted {
                    stream: Stream::Unix(stream),
                    peer: Peer::CurrentUser,
                }))
            }
            Self::Tcp(listener) => {
                let (stream, address) = listener.accept().await?;
                Ok(Some(Accepted {
                    stream: Stream::Tcp(stream),
                    peer: Peer::Unauthenticated { address },
                }))
            }
        }
    }
}

/// Open `admitted`.
///
/// The admission decision is the caller's and is already made: this function
/// takes the endpoint policy selected by the composition root.
pub async fn bind(admitted: &Admitted) -> io::Result<Bound> {
    match admitted {
        Admitted::Local { name } => bind_local(Path::new(name)).await,
        Admitted::Loopback(address) | Admitted::Public(address) => {
            Ok(Bound::Tcp(TcpListener::bind(address).await?))
        }
    }
}

#[cfg(unix)]
async fn bind_local(path: &Path) -> io::Result<Bound> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the local endpoint parent must be a directory, not a symlink or file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        Err(error) => return Err(error),
    }
    let owner = rustix::process::geteuid().as_raw();
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the local endpoint needs a directory owned by the current user with mode 0700; choose a private directory",
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the local endpoint needs a socket filename",
        )
    })?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    // Reuse the session store's kernel lease mechanism, without its session
    // persistence semantics or an inward dependency on ego-tools.
    let file = std::fs::File::from(rustix::fs::open(
        parent.join(lock_name),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the local endpoint lease must be a private regular file owned by the current user",
        ));
    }
    let mut lease = fd_lock::RwLock::new(file);
    match lease.try_write() {
        // As in the session-store lease, closing the owned descriptor releases
        // the lock. The stable lock file stays in place for the next owner.
        Ok(guard) => std::mem::forget(guard),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another listener owns this local endpoint",
            ));
        }
        Err(error) => return Err(error),
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == owner => {
            match tokio::time::timeout(std::time::Duration::from_secs(1), UnixStream::connect(path))
                .await
            {
                Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "the local endpoint is active or its owner could not be determined",
                    ));
                }
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the local endpoint path exists and is not an owned socket; choose another path",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(Bound::Local {
        listener: UnixListener::bind(path)?,
        path: path.to_path_buf(),
        owner,
        _lease: lease,
    })
}

/// The platform that has no local endpoint here yet, refusing by name.
///
/// See the module documentation: the named pipe is the right primitive and
/// story `regression_case` is where it can be built and tested. Until then this says
/// so, rather than a listener silently becoming a TCP one.
#[cfg(not(unix))]
async fn bind_local(path: &Path) -> io::Result<Bound> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "a local endpoint ({}) is not served on this platform yet: it needs \
             a named pipe, which regression case builds and tests natively. Bind \
             to 127.0.0.1 with a credential in the meantime",
            path.display()
        ),
    ))
}

/// Whether a bound endpoint still owes a credential check before routing.
///
/// The one question a protocol above asks. It is answered from the admission
/// decision rather than from the accepted peer, so a protocol cannot be handed
/// a connection and infer its own answer from what the transport happened to
/// provide.
pub fn credential_required(admitted: &Admitted) -> bool {
    admitted.authentication() == Authentication::BearerCredential
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ego_core::endpoint::{PublicBind, Requested, admit};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn scratch(root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        path.push("endpoint");
        path.push("ego.sock");
        path
    }

    fn local() -> (TempDir, Admitted, PathBuf) {
        let root = tempfile::tempdir().expect("fixture root");
        let path = scratch(root.path());
        let admitted = admit(
            Requested::Local {
                name: path.display().to_string(),
            },
            PublicBind::Refused,
        )
        .expect("a local endpoint is admitted");
        (root, admitted, path)
    }

    #[tokio::test]
    async fn a_local_endpoint_serves_its_owner_and_says_the_transport_authenticated_them() {
        let (_root, admitted, path) = local();
        let bound = bind(&admitted).await.expect("bind");
        assert_eq!(
            bound.advertised().expect("advertised"),
            path.display().to_string()
        );
        assert!(!credential_required(&admitted));

        let client_path = path.clone();
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(client_path).await.expect("connect");
            stream.write_all(b"ping").await.expect("write");
            let mut read = [0u8; 4];
            stream.read_exact(&mut read).await.expect("read");
            read
        });

        let accepted = bound
            .accept()
            .await
            .expect("accept")
            .expect("the owner is not refused");
        assert_eq!(accepted.peer, Peer::CurrentUser);
        let mut stream = accepted.stream;
        let mut read = [0u8; 4];
        stream.read_exact(&mut read).await.expect("read");
        assert_eq!(&read, b"ping");
        stream.write_all(b"pong").await.expect("write");
        assert_eq!(&client.await.expect("client"), b"pong");
    }

    /// The directory is the enforcement, so it is the thing asserted. A socket
    /// in a world-traversable directory is reachable by every local process,
    /// which is the property this endpoint exists not to have.
    #[tokio::test]
    async fn the_directory_holding_a_local_endpoint_admits_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_root, admitted, path) = local();
        let _bound = bind(&admitted).await.expect("bind");
        let parent = path.parent().expect("the socket has a parent");
        let mode = std::fs::metadata(parent)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the endpoint's directory is {mode:o}");
    }

    /// Binding twice is the ordinary case after a crash, not an exotic one.
    #[tokio::test]
    async fn a_socket_left_by_a_previous_process_does_not_block_the_next_one() {
        let (_root, admitted, _) = local();
        let first = bind(&admitted).await.expect("first bind");
        drop(first);
        let _second = bind(&admitted)
            .await
            .expect("a stale socket must not be fatal");
    }

    #[tokio::test]
    async fn a_loopback_endpoint_advertises_the_port_it_got_and_still_owes_a_credential() {
        let admitted = admit(
            Requested::Tcp("127.0.0.1:0".parse().expect("address")),
            PublicBind::Refused,
        )
        .expect("loopback is admitted");
        let bound = bind(&admitted).await.expect("bind");
        let advertised = bound.advertised().expect("advertised");
        assert!(advertised.starts_with("127.0.0.1:"), "{advertised}");
        assert!(
            !advertised.ends_with(":0"),
            "port 0 was advertised: {advertised}"
        );
        assert!(
            credential_required(&admitted),
            "a loopback endpoint must not serve without a credential"
        );
    }

    /// The transport vouches for nobody on TCP, and the type says so rather
    /// than leaving a protocol above to decide what an address means.
    #[tokio::test]
    async fn a_loopback_peer_is_reported_unauthenticated() {
        let admitted = admit(
            Requested::Tcp("127.0.0.1:0".parse().expect("address")),
            PublicBind::Refused,
        )
        .expect("loopback is admitted");
        let bound = bind(&admitted).await.expect("bind");
        let address = bound.advertised().expect("advertised");
        let client =
            tokio::spawn(async move { TcpStream::connect(address).await.expect("connect") });
        let accepted = bound
            .accept()
            .await
            .expect("accept")
            .expect("tcp accepts everyone");
        assert!(matches!(accepted.peer, Peer::Unauthenticated { .. }));
        drop(client.await.expect("client"));
    }
}
