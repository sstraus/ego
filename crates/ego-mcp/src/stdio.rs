//! Ownership adapter for one-shot guarded stdio capabilities.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use ego_core::permit::{DuplexHandle, DuplexSettlement};
use futures::io::{AsyncRead, AsyncWrite};

use crate::{McpError, McpFailure};

#[derive(Clone)]
pub(crate) struct SharedDuplex {
    inner: Arc<Mutex<Option<DuplexHandle>>>,
}

impl SharedDuplex {
    pub(crate) fn new(duplex: DuplexHandle) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(duplex))),
        }
    }

    fn take(&self) -> Result<DuplexHandle, McpError> {
        self.inner
            .lock()
            .map_err(|_| McpError::new(McpFailure::Transport, "stdio authority lock poisoned"))?
            .take()
            .ok_or_else(|| McpError::new(McpFailure::Authority, "stdio authority was spent"))
    }

    pub(crate) async fn settle(self) -> Result<DuplexSettlement, McpError> {
        Ok(self.take()?.settle().await)
    }

    pub(crate) async fn interrupt(self) -> Result<DuplexSettlement, McpError> {
        Ok(self.take()?.interrupt().await)
    }
}

impl AsyncRead for SharedDuplex {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = self.inner.lock().expect("stdio duplex lock poisoned");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_read(cx, buffer),
            None => Poll::Ready(Ok(0)),
        }
    }
}

impl AsyncWrite for SharedDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = self.inner.lock().expect("stdio duplex lock poisoned");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_write(cx, buffer),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.inner.lock().expect("stdio duplex lock poisoned");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_flush(cx),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.inner.lock().expect("stdio duplex lock poisoned");
        match guard.as_mut() {
            Some(duplex) => std::pin::Pin::new(duplex.as_mut()).poll_close(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

pub(crate) async fn settle(duplex: DuplexHandle) -> DuplexSettlement {
    duplex.settle().await
}
