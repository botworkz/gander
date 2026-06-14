// SPDX-License-Identifier: GPL-3.0-or-later

//! Generic transport error and geesed transport module declaration.

pub mod geesed;

/// A transport-level error.
///
/// Wraps backend-specific errors (e.g. [`geesed::GeesedError`]) as a
/// human-readable string for callers that do not need to inspect the
/// original variant.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("{0}")]
    Other(String),
}

impl From<geesed::GeesedError> for TransportError {
    fn from(e: geesed::GeesedError) -> Self {
        TransportError::Other(e.to_string())
    }
}
