// SPDX-License-Identifier: GPL-3.0-or-later

//! ACP connection to geesed's acp socket.
//!
//! Each open tab gets one [`AcpConnection`], created when the tab opens and
//! dropped when the tab closes. The connection:
//!
//! 1. Connects to `$XDG_RUNTIME_DIR/geese/acp.sock`.
//! 2. Sends a `connect_profile` JSON-RPC handshake; geesed spawns goose and
//!    returns success, or returns a typed error code.
//! 3. After the handshake, the socket becomes a raw byte relay to goose's
//!    stdio. We layer the ACP protocol SDK over it.
//! 4. Exposes [`AcpConnection::send`] (for user prompts) and
//!    [`AcpConnection::recv`] (for streaming events) so the rest of gander
//!    never touches the socket directly.
//!
//! Dropping an [`AcpConnection`] cancels the background task, closing the
//! socket. Geesed detects the socket close and stops the goose process for
//! that profile.

use std::{env, path::PathBuf};

use agent_client_protocol::{
    ByteStreams, SessionMessage,
    schema::{
        ContentBlock, InitializeRequest, ListSessionsRequest, NewSessionResponse, ProtocolVersion,
        ResumeSessionRequest, SessionId, SessionNotification, SessionUpdate, StopReason,
    },
    util::MatchDispatch,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A live ACP connection to goose for one profile tab.
///
/// Both channel ends are public so `AppModel` can clone `send` into the
/// per-tab IPC handler and poll `recv` from the GTK pump tick.
pub struct AcpConnection {
    /// Receive ACP events produced by the background task.
    pub recv: mpsc::Receiver<AcpEvent>,
    /// Send commands (user prompts, clean shutdown) to the background task.
    ///
    /// In the `connect_with_rx` path this is a stub sender (disconnected);
    /// the real sender lives in the webview IPC handler closure.
    #[allow(dead_code)]
    pub send: mpsc::Sender<AcpCommand>,
    /// Background task. Cancelled (and socket closed) when this is dropped.
    _task: JoinHandle<()>,
}

/// A trimmed-down session descriptor for the sidebar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ListedSession {
    /// Goose session ID.
    pub id: String,
    /// Human-readable label (title from goose, or `"Session"` as fallback).
    pub label: String,
    /// ISO 8601 `updated_at` from `session/list`, if reported by the agent.
    pub last_active: Option<String>,
}

/// Events sent from the ACP background task to the GTK pump.
#[derive(Debug)]
pub enum AcpEvent {
    /// A streaming text chunk from the assistant.
    TextChunk(String),
    /// The assistant's response is complete.
    #[allow(dead_code)]
    Complete(StopReason),
    /// An error mid-conversation; the tab enters a disconnected state.
    Error(String),
    /// Up to 5 most-recently-active sessions for this profile.
    ///
    /// Sent once on connect (after the initial session is established) and
    /// again after a `session_new` command creates a fresh session.
    SessionList(Vec<ListedSession>),
    /// The active session has been established or switched.
    ///
    /// The `String` is the session ID. History is not replayed —
    /// `session/resume` in ACP v1 does not return prior messages.
    SessionActive(String),
}

/// Commands sent from the UI/IPC handler to the ACP background task.
#[derive(Debug)]
pub enum AcpCommand {
    /// Send a user prompt and begin streaming the reply.
    Prompt(String),
    /// Close the connection cleanly.
    #[allow(dead_code)]
    Shutdown,
    /// Switch the active session to an existing session by ID.
    SessionSelect(String),
    /// Create a brand-new session and make it active.
    SessionNew,
    /// The webview bridge is ready; re-emit the session list and active ID.
    RequestSessionInfo,
}

/// Errors that can occur during handshake.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
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

// ---------------------------------------------------------------------------
// AcpConnection impl
// ---------------------------------------------------------------------------

impl AcpConnection {
    /// Connect to geesed's acp socket, handshake `connect_profile(name)`,
    /// then enter the streaming loop. Returns once the SDK has initialised
    /// and a session is ready. The background task is already running when
    /// this future resolves.
    ///
    /// Use this when you want a self-contained connection that owns both ends
    /// of the command channel. Use [`connect_with_rx`](Self::connect_with_rx)
    /// when the command sender must be captured by a webview IPC handler before
    /// the ACP task starts.
    #[allow(dead_code)]
    pub async fn connect(profile: &str) -> Result<Self, ConnectError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AcpCommand>(64);
        let conn = Self::connect_with_rx(profile, cmd_rx).await?;
        Ok(AcpConnection {
            recv: conn.recv,
            send: cmd_tx,
            _task: conn._task,
        })
    }

    /// Like [`Self::connect`], but uses a caller-supplied command receiver.
    ///
    /// Use this when the command sender (`cmd_tx`) must be captured by the
    /// webview IPC handler *before* the ACP task starts — the caller creates
    /// the `(cmd_tx, cmd_rx)` pair, moves `cmd_tx` into the IPC handler, and
    /// passes `cmd_rx` here.
    ///
    /// The returned connection's `send` field is a disconnected stub; callers
    /// should use the `cmd_tx` they created themselves.
    pub async fn connect_with_rx(
        profile: &str,
        cmd_rx: mpsc::Receiver<AcpCommand>,
    ) -> Result<Self, ConnectError> {
        let stream = do_handshake(profile).await?;

        let (evt_tx, evt_rx) = mpsc::channel::<AcpEvent>(64);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        // A disconnected sender so the struct always has a valid `send` field.
        // The real command path goes through the caller-owned `cmd_tx`.
        let (stub_tx, _stub_rx) = mpsc::channel::<AcpCommand>(1);

        let task = tokio::spawn(run_worker(stream, cmd_rx, evt_tx, ready_tx));

        // Wait for the ACP SDK to initialise and start a session. This
        // typically completes in <100 ms.
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                task.abort();
                return Err(ConnectError::Protocol(msg));
            }
            Err(_) => {
                task.abort();
                return Err(ConnectError::Protocol(
                    "acp worker stopped unexpectedly".into(),
                ));
            }
        }

        Ok(AcpConnection {
            recv: evt_rx,
            send: stub_tx,
            _task: task,
        })
    }
}

// ---------------------------------------------------------------------------
// Handshake helper
// ---------------------------------------------------------------------------

/// Connect to geesed's acp socket and complete the `connect_profile` handshake.
///
/// Returns the post-handshake socket, ready to be used as a raw byte relay.
async fn do_handshake(profile: &str) -> Result<UnixStream, ConnectError> {
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
        return Err(ConnectError::Protocol(
            "connection closed before handshake response".into(),
        ));
    }
    drop(reader); // release the borrow so we can move stream below

    let response: Value = serde_json::from_str(response_line.trim_end())
        .map_err(|e| ConnectError::Protocol(e.to_string()))?;

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_owned();
        return Err(match code {
            -32001 => ConnectError::ProfileNotFound,
            -32020 => ConnectError::ProfileInUse,
            -32010 => ConnectError::GooseBinaryUnavailable(message),
            -32011 => ConnectError::SpawnFailed(message),
            _ => ConnectError::Protocol(message),
        });
    }

    Ok(stream)
}

// ---------------------------------------------------------------------------
// Worker task
// ---------------------------------------------------------------------------

/// Retrieve up to 5 most-recently-active sessions from `session/list`.
///
/// Errors from the call are swallowed — an empty list is the safe fallback.
async fn fetch_top_sessions<R: agent_client_protocol::role::Role>(
    cx: &agent_client_protocol::ConnectionTo<R>,
) -> Vec<ListedSession>
where
    R: agent_client_protocol::role::HasPeer<R>,
{
    let response = match cx
        .send_request(ListSessionsRequest::new())
        .block_task()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(%err, "session/list failed; starting fresh");
            return Vec::new();
        }
    };

    let mut sessions = response.sessions;
    // Sort most-recently-active first (ISO 8601 sorts lexicographically).
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.truncate(5);

    sessions
        .into_iter()
        .map(|s| ListedSession {
            label: s
                .title
                .filter(|t| !t.is_empty())
                // Keep in sync with DEFAULT_SESSION_LABEL in crates/gander-chat/src/lib.rs.
                .unwrap_or_else(|| "Session".to_string()),
            id: s.session_id.to_string(),
            last_active: s.updated_at,
        })
        .collect()
}

async fn run_worker(
    stream: UnixStream,
    mut cmd_rx: mpsc::Receiver<AcpCommand>,
    evt_tx: mpsc::Sender<AcpEvent>,
    ready_tx: oneshot::Sender<Result<(), String>>,
) {
    let (read_half, write_half) = stream.into_split();
    let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());

    let evt_tx_clone = evt_tx.clone();
    let mut ready_tx_opt = Some(ready_tx);

    let result = agent_client_protocol::Client
        .builder()
        .name("gander")
        .connect_with(transport, async move |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));

            // --- Session bootstrap -------------------------------------------
            // List existing sessions (silently falls back to empty on error).
            let listed = fetch_top_sessions(&cx).await;

            // Resume the most-recent session, or create a fresh one.
            let mut active_id: String;
            let mut current_session = if let Some(first) = listed.first() {
                let sid = SessionId::new(first.id.clone());
                match cx
                    .send_request(ResumeSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await
                {
                    Ok(_) => {
                        active_id = first.id.clone();
                        cx.attach_session(NewSessionResponse::new(sid), vec![])?
                    }
                    Err(err) => {
                        tracing::warn!(%err, "session/resume failed; creating new session");
                        let s = cx.build_session_cwd()?.block_task().start_session().await?;
                        active_id = s.session_id().to_string();
                        s
                    }
                }
            } else {
                let s = cx.build_session_cwd()?.block_task().start_session().await?;
                active_id = s.session_id().to_string();
                s
            };

            // Signal the connect() caller that we are ready.
            if let Some(tx) = ready_tx_opt.take() {
                let _ = tx.send(Ok(()));
            }

            // Cache the initial session list so RequestSessionInfo can re-send it.
            let mut cached_listed = listed;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    AcpCommand::Prompt(text) => {
                        if let Err(error) = current_session.send_prompt(&text) {
                            let _ = evt_tx_clone.send(AcpEvent::Error(error.to_string())).await;
                            continue;
                        }

                        // Drain events for this prompt.
                        loop {
                            let update = match current_session.read_update().await {
                                Ok(update) => update,
                                Err(error) => {
                                    let _ =
                                        evt_tx_clone.send(AcpEvent::Error(error.to_string())).await;
                                    break;
                                }
                            };

                            match update {
                                SessionMessage::SessionMessage(dispatch) => {
                                    let tx = evt_tx_clone.clone();
                                    let handled = MatchDispatch::new(dispatch)
                                        .if_notification(async move |n: SessionNotification| {
                                            if let SessionUpdate::AgentMessageChunk(chunk) =
                                                n.update
                                            {
                                                if let ContentBlock::Text(text) = chunk.content {
                                                    let _ = tx
                                                        .send(AcpEvent::TextChunk(text.text))
                                                        .await;
                                                }
                                            }
                                            Ok(())
                                        })
                                        .await
                                        .otherwise_ignore();

                                    if let Err(error) = handled {
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::Error(error.to_string()))
                                            .await;
                                        break;
                                    }
                                }
                                SessionMessage::StopReason(stop_reason) => {
                                    let _ =
                                        evt_tx_clone.send(AcpEvent::Complete(stop_reason)).await;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }

                    AcpCommand::SessionSelect(id) => {
                        let sid = SessionId::new(id.clone());
                        match cx
                            .send_request(ResumeSessionRequest::new(sid.clone(), cwd.clone()))
                            .block_task()
                            .await
                        {
                            Ok(_) => {
                                match cx.attach_session(NewSessionResponse::new(sid), vec![]) {
                                    Ok(s) => {
                                        current_session = s;
                                        active_id = id.clone();
                                        let _ =
                                            evt_tx_clone.send(AcpEvent::SessionActive(id)).await;
                                    }
                                    Err(err) => {
                                        tracing::warn!(%err, "attach_session failed after resume");
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::Error(format!(
                                                "session switch failed: {err}"
                                            )))
                                            .await;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!(%err, "session/resume failed");
                                let _ = evt_tx_clone
                                    .send(AcpEvent::Error(format!("session/resume failed: {err}")))
                                    .await;
                            }
                        }
                    }

                    AcpCommand::SessionNew => {
                        match cx.build_session_cwd()?.block_task().start_session().await {
                            Ok(s) => {
                                active_id = s.session_id().to_string();
                                current_session = s;
                                // Refresh the list so the new session appears.
                                cached_listed = fetch_top_sessions(&cx).await;
                                let _ = evt_tx_clone
                                    .send(AcpEvent::SessionList(cached_listed.clone()))
                                    .await;
                                let _ = evt_tx_clone
                                    .send(AcpEvent::SessionActive(active_id.clone()))
                                    .await;
                            }
                            Err(err) => {
                                let _ = evt_tx_clone
                                    .send(AcpEvent::Error(format!("session/new failed: {err}")))
                                    .await;
                            }
                        }
                    }

                    AcpCommand::RequestSessionInfo => {
                        let _ = evt_tx_clone
                            .send(AcpEvent::SessionList(cached_listed.clone()))
                            .await;
                        let _ = evt_tx_clone
                            .send(AcpEvent::SessionActive(active_id.clone()))
                            .await;
                    }

                    AcpCommand::Shutdown => break,
                }
            }

            Ok(())
        })
        .await;

    if let Err(error) = result {
        let _ = evt_tx.send(AcpEvent::Error(error.to_string())).await;
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
