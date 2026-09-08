use std::fmt;
use std::future::Future as _;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

const DEFAULT_MAX_CONNECTIONS: u32 = 32;
const DEFAULT_CONNECTION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_MAX_INGRESS_BYTES: u64 = 128 * 1024 * 1024;

/// Finite resource limits applied independently to every inbound listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    max_connections: u32,
    connection_lifetime: Duration,
    max_ingress_bytes: u64,
}

impl ConnectionLimits {
    pub fn new(
        max_connections: u32,
        connection_lifetime: Duration,
        max_ingress_bytes: u64,
    ) -> Result<Self, ConnectionLimitsError> {
        if max_connections == 0 {
            return Err(ConnectionLimitsError::ZeroMaxConnections);
        }
        if u128::from(max_connections) > Semaphore::MAX_PERMITS as u128 {
            return Err(ConnectionLimitsError::TooManyConnections);
        }
        if connection_lifetime.is_zero() {
            return Err(ConnectionLimitsError::ZeroConnectionLifetime);
        }
        if max_ingress_bytes == 0 {
            return Err(ConnectionLimitsError::ZeroMaxIngressBytes);
        }
        Ok(Self {
            max_connections,
            connection_lifetime,
            max_ingress_bytes,
        })
    }

    pub const fn max_connections(self) -> u32 {
        self.max_connections
    }

    pub const fn connection_lifetime(self) -> Duration {
        self.connection_lifetime
    }

    pub const fn max_ingress_bytes(self) -> u64 {
        self.max_ingress_bytes
    }
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            connection_lifetime: DEFAULT_CONNECTION_LIFETIME,
            max_ingress_bytes: DEFAULT_MAX_INGRESS_BYTES,
        }
    }
}

/// Why a listener resource policy could not be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionLimitsError {
    ZeroMaxConnections,
    TooManyConnections,
    ZeroConnectionLifetime,
    ZeroMaxIngressBytes,
}

impl fmt::Display for ConnectionLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroMaxConnections => "maximum connections must be greater than zero",
            Self::TooManyConnections => "maximum connections exceeds the runtime limit",
            Self::ZeroConnectionLifetime => "connection lifetime must be greater than zero",
            Self::ZeroMaxIngressBytes => "maximum ingress bytes must be greater than zero",
        })
    }
}

impl std::error::Error for ConnectionLimitsError {}

struct GateState {
    limits: ConnectionLimits,
    permits: Arc<Semaphore>,
    shutdown: CancellationToken,
}

/// Process-owned admission and shutdown state for one inbound listener.
#[derive(Clone)]
pub struct ConnectionGate {
    state: Arc<GateState>,
}

impl ConnectionGate {
    pub fn new(limits: ConnectionLimits) -> Self {
        let max_connections = usize::try_from(limits.max_connections)
            .expect("validated maximum connections always fits in usize");
        Self {
            state: Arc::new(GateState {
                limits,
                permits: Arc::new(Semaphore::new(max_connections)),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Admit `stream`, waiting until an occupancy permit is available.
    ///
    /// The returned I/O value owns the permit, deadline, ingress counter and
    /// shutdown listener. Splitting or moving that value into an upgraded
    /// protocol connection therefore cannot release admission early.
    pub async fn guard<S>(&self, stream: S) -> Result<GuardedStream<S>, ConnectionGateError> {
        let permit = self
            .state
            .shutdown
            .clone()
            .run_until_cancelled_owned(Arc::clone(&self.state.permits).acquire_owned())
            .await
            .ok_or(ConnectionGateError::ShuttingDown)?
            .map_err(|_| ConnectionGateError::ShuttingDown)?;
        if self.state.shutdown.is_cancelled() {
            drop(permit);
            return Err(ConnectionGateError::ShuttingDown);
        }
        Ok(GuardedStream {
            inner: stream,
            _permit: permit,
            remaining_ingress: self.state.limits.max_ingress_bytes,
            deadline: Box::pin(tokio::time::sleep(self.state.limits.connection_lifetime)),
            shutdown: Box::pin(self.state.shutdown.clone().cancelled_owned()),
        })
    }

    /// Stop new admissions and wake every guarded I/O operation.
    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
    }

    async fn drain(&self) -> Result<(), ConnectionGateError> {
        let permits = Arc::clone(&self.state.permits)
            .acquire_many_owned(self.state.limits.max_connections)
            .await
            .map_err(|_| ConnectionGateError::ShuttingDown)?;
        drop(permits);
        Ok(())
    }

    /// Stop admission, close active transport I/O and wait for it to be owned
    /// by nobody.
    pub async fn shutdown_and_drain(&self) -> Result<(), ConnectionGateError> {
        self.shutdown();
        self.drain().await
    }
}

/// Why a stream could not enter or finish draining from a connection gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionGateError {
    ShuttingDown,
}

impl fmt::Display for ConnectionGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the listener is shutting down")
    }
}

impl std::error::Error for ConnectionGateError {}

/// One admitted byte stream with listener-owned resource accounting.
pub struct GuardedStream<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
    remaining_ingress: u64,
    deadline: Pin<Box<Sleep>>,
    shutdown: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl<S> GuardedStream<S> {
    fn poll_open(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        if self.shutdown.as_mut().poll(context).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the listener is shutting down",
            ));
        }
        if self.deadline.as_mut().poll(context).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the connection lifetime was exhausted",
            ));
        }
        Ok(())
    }
}

impl<S> AsyncRead for GuardedStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_open(context) {
            return Poll::Ready(Err(error));
        }
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.remaining_ingress == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the connection ingress budget was exhausted",
            )));
        }

        let allowed = usize::try_from(this.remaining_ingress)
            .unwrap_or(usize::MAX)
            .min(buffer.remaining());
        let initialized = buffer.initialize_unfilled_to(allowed);
        let mut limited = ReadBuf::new(initialized);
        match Pin::new(&mut this.inner).poll_read(context, &mut limited) {
            Poll::Ready(Ok(())) => {
                let read = limited.filled().len();
                buffer.advance(read);
                this.remaining_ingress -=
                    u64::try_from(read).expect("a read length always fits in u64");
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> AsyncWrite for GuardedStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.inner).poll_shutdown(context)
    }
}
