// SPDX-License-Identifier: GPL-3.0-or-later

//! Geesed transport: ACP socket handshake and path helpers.
//!
//! Resolves the geesed ACP socket path, sends the `connect_profile`
//! JSON-RPC handshake, and returns a post-handshake [`tokio::net::UnixStream`]
//! ready for use by the ACP SDK.
//!
//! Everything in this module is geesed-specific.  Generic ACP worker logic
//! lives in `crate::acp`.

use std::{env, path::PathBuf};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Errors that can occur when connecting to geesed's ACP socket.
///
/// The four geesed-protocol variants (`ProfileNotFound`, `ProfileInUse`,
/// `GooseBinaryUnavailable`, `SpawnFailed`) correspond to geesed JSON-RPC
/// error codes.  The remaining variants cover generic I/O and protocol
/// failures.
///
/// This type is re-exported from `crate::acp` as `ConnectError` so that
/// call sites continue to use `crate::acp::ConnectError`.
#[derive(Debug, thiserror::Error)]
pub enum GeesedError {
    #[error("profile not found")]
    ProfileNotFound,
    #[error("profile already in use by another client")]
    ProfileInUse,
    #[error("goose binary not available: {0}")]
    GooseBinaryUnavailable(String),
    #[error("goose failed to spawn: {0}")]
    SpawnFailed(String),
    #[error("could not connect to geesed acp socket: {0}")]
    SocketConnect(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Geesed transport handle.
///
/// Currently a zero-sized struct — all state is in the returned socket.
/// Exists as a named type so future versions can carry connection-pool or
/// retry state without changing the call sites.
pub struct GeesedTransport;

impl GeesedTransport {
    /// Connect to geesed's ACP socket and complete the `connect_profile`
    /// handshake.
    ///
    /// Returns the post-handshake socket, ready to be used as a raw byte
    /// relay for the ACP protocol SDK.
    pub async fn connect(profile: &str) -> Result<UnixStream, GeesedError> {
        let socket_path = acp_socket_path();
        let mut stream = UnixStream::connect(&socket_path).await?;

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "connect_profile",
            "params": { "name": profile }
        });
        let mut line = request.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;

        let mut reader = BufReader::new(&mut stream);
        let mut response_line = String::new();
        let bytes = reader.read_line(&mut response_line).await?;
        if bytes == 0 {
            return Err(GeesedError::Protocol(
                "connection closed before handshake response".into(),
            ));
        }
        drop(reader); // release the borrow so we can move stream below

        let response: Value = serde_json::from_str(response_line.trim_end())
            .map_err(|e| GeesedError::Protocol(e.to_string()))?;

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_owned();
            return Err(match code {
                -32001 => GeesedError::ProfileNotFound,
                -32020 => GeesedError::ProfileInUse,
                -32010 => GeesedError::GooseBinaryUnavailable(message),
                -32011 => GeesedError::SpawnFailed(message),
                _ => GeesedError::Protocol(message),
            });
        }

        Ok(stream)
    }
}

// ---------------------------------------------------------------------------
// Path helpers — mirrors the runtime_dir logic in geese-client
// ---------------------------------------------------------------------------

fn acp_socket_path() -> PathBuf {
    runtime_dir().join("acp.sock")
}

fn runtime_dir() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("geese"),
        None => {
            // Defensive fallback for non-XDG environments (e.g. CI, minimal
            // containers). On a real COSMIC desktop XDG_RUNTIME_DIR is always
            // set, but we don't want a panic if it isn't.
            let home = env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
            PathBuf::from(home).join(".cache").join("geese")
        }
    }
}
