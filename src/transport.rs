// SPDX-License-Identifier: GPL-3.0-or-later

//! Transport trait and geesed transport module declaration.
//!
//! The [`Transport`] trait is the only thing `crate::acp` depends on.
//! Geesed-specific types live entirely in [`geesed`].

pub mod geesed;

use tokio::net::UnixStream;

/// Abstraction point for the ACP socket connection.
///
/// Implemented today by [`geesed::GeesedTransport`].  `Box<dyn Transport>` is
/// passed to [`crate::acp::AcpConnection::connect_with_rx`] so the ACP worker
/// stays free of geesed-private details.
///
/// `UnixStream` is fine for now; abstracting over read/write halves is YAGNI
/// until a second transport exists.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Perform any handshake and return the post-handshake socket.
    async fn connect(self: Box<Self>) -> Result<UnixStream, TransportError>;
}

/// A transport-level error.
///
/// Geesed-specific errors (e.g. [`geesed::GeesedError`]) surface through
/// `Other(Box::new(...))`.  Callers that need to inspect the concrete error
/// (e.g. `app.rs`) downcast via `Box<dyn Error>::downcast_ref`.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport: {0}")]
    Other(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl From<geesed::GeesedError> for TransportError {
    fn from(e: geesed::GeesedError) -> Self {
        TransportError::Other(Box::new(e))
    }
}
