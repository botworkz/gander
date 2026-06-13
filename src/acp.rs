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

use std::{collections::HashMap, env, path::PathBuf, sync::Arc};

use agent_client_protocol::{
    ActiveSession, Agent, ByteStreams, SessionMessage,
    schema::{
        ContentBlock, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
        NewSessionResponse, ProtocolVersion, SessionId, SessionNotification, SessionUpdate,
        StopReason, ToolCall, ToolCallId,
    },
    util::MatchDispatch,
};
use serde_json::Value;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, mpsc, oneshot},
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
    /// A streaming text chunk from the agent (live prompt or history replay).
    AgentText(String),
    /// A user message chunk (history replay only).
    UserText(String),
    /// Merged tool-call snapshot.  Emitted once when the tool call is created
    /// and again on every update with the fully-merged state, so the UI can
    /// find-or-create a card by `tool_call_id` and replace its content.
    ToolCall(Box<ToolCall>),
    /// History replay starting — the UI should clear its message list.
    SessionLoadStart,
    /// History replay complete — the UI may accept new user input.
    SessionLoadEnd,
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
    /// The active session has been established or switched to a new session.
    ///
    /// Emitted by `SessionNew` and on initial connect. Not emitted by
    /// `SessionSelect` — that path uses `SessionLoadStart`/`SessionLoadEnd`
    /// to replay history.
    SessionActive(String),
    /// Session metadata for the footer bar.
    ///
    /// Emitted once on initial connect and again on `RequestSessionInfo`.
    /// `model` is the agent name reported by `InitializeResponse.agent_info`.
    /// `tool_count` is `None` when the count cannot be determined via ACP v1.
    SessionInfo {
        cwd: String,
        model: String,
        tool_count: Option<u32>,
    },
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
async fn fetch_top_sessions<R>(cx: &agent_client_protocol::ConnectionTo<R>) -> Vec<ListedSession>
where
    R: agent_client_protocol::role::Role,
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

    // Per-session tool-call map: receives ToolCall on creation and merges
    // ToolCallUpdate patches in, emitting a full snapshot each time so the UI
    // stays dumb.  Cleared on every session switch to prevent history leaks.
    let tool_calls: Arc<Mutex<HashMap<ToolCallId, ToolCall>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let result = agent_client_protocol::Client
        .builder()
        .name("gander")
        .connect_with(transport, async move |cx| {
            let init_resp = cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            // Extract the agent name from the InitializeResponse to display in the footer.
            // This is typically the agent implementation name (e.g. "goose"), not the LLM
            // model name, which ACP v1 does not expose directly.
            let agent_model = init_resp
                .agent_info
                .as_ref()
                .map(|i| i.title.clone().unwrap_or_else(|| i.name.clone()))
                .unwrap_or_default();

            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));

            // --- Session bootstrap -------------------------------------------
            // List existing sessions (silently falls back to empty on error).
            let listed = fetch_top_sessions(&cx).await;

            // Resume the most-recent session, or create a fresh one.
            let mut active_id: String;
            let mut current_session = if let Some(first) = listed.first() {
                let sid = SessionId::new(first.id.clone());
                match cx
                    .send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await
                {
                    Ok(_) => {
                        active_id = first.id.clone();
                        cx.attach_session(NewSessionResponse::new(sid), vec![])?
                    }
                    Err(err) => {
                        tracing::warn!(%err, "session/load failed; creating new session");
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

            // Cache session metadata for reuse on RequestSessionInfo.
            let cwd_str = cwd.to_string_lossy().into_owned();
            let mut cached_listed = listed;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    AcpCommand::Prompt(text) => {
                        if let Err(error) = current_session.send_prompt(&text) {
                            let _ = evt_tx_clone.send(AcpEvent::Error(error.to_string())).await;
                            continue;
                        }

                        drain_session_updates(
                            &mut current_session,
                            &evt_tx_clone,
                            &tool_calls,
                        )
                        .await;
                    }

                    AcpCommand::SessionSelect(id) => {
                        let sid = SessionId::new(id.clone());
                        match cx
                            .send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                            .block_task()
                            .await
                        {
                            Ok(_) => {
                                match cx.attach_session(NewSessionResponse::new(sid), vec![]) {
                                    Ok(s) => {
                                        current_session = s;
                                        active_id = id;
                                        // Clear stale tool-call state before replaying the
                                        // new session's history.
                                        tool_calls.lock().await.clear();
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::SessionLoadStart)
                                            .await;
                                        drain_history_replay(
                                            &mut current_session,
                                            &evt_tx_clone,
                                            &tool_calls,
                                        )
                                        .await;
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::SessionLoadEnd)
                                            .await;
                                    }
                                    Err(err) => {
                                        tracing::warn!(%err, "attach_session failed after session/load");
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::Error(format!(
                                                "session switch failed: {err}"
                                            )))
                                            .await;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!(%err, "session/load failed");
                                let _ = evt_tx_clone
                                    .send(AcpEvent::Error(format!("session/load failed: {err}")))
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
                        let _ = evt_tx_clone
                            .send(AcpEvent::SessionInfo {
                                cwd: cwd_str.clone(),
                                model: agent_model.clone(),
                                tool_count: None,
                            })
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
// Session update drain
// ---------------------------------------------------------------------------

/// Read `SessionMessage`s from `session` until a `StopReason` arrives (or an
/// error), forwarding each `SessionUpdate` to `evt_tx` as an [`AcpEvent`].
///
/// Used for live prompt streaming.
async fn drain_session_updates(
    session: &mut ActiveSession<'_, Agent>,
    evt_tx: &mpsc::Sender<AcpEvent>,
    tool_calls: &Arc<Mutex<HashMap<ToolCallId, ToolCall>>>,
) {
    loop {
        let update = match session.read_update().await {
            Ok(u) => u,
            Err(error) => {
                let _ = evt_tx.send(AcpEvent::Error(error.to_string())).await;
                break;
            }
        };

        match update {
            SessionMessage::SessionMessage(dispatch) => {
                let tx = evt_tx.clone();
                let tc_arc = Arc::clone(tool_calls);
                let handled = MatchDispatch::new(dispatch)
                    .if_notification(async move |n: SessionNotification| {
                        forward_update(n.update, &tx, &tc_arc).await;
                        Ok(())
                    })
                    .await
                    .otherwise_ignore();

                if let Err(error) = handled {
                    let _ = evt_tx.send(AcpEvent::Error(error.to_string())).await;
                    break;
                }
            }
            SessionMessage::StopReason(stop_reason) => {
                let _ = evt_tx.send(AcpEvent::Complete(stop_reason)).await;
                break;
            }
            _ => {}
        }
    }
}

/// Drain buffered [`SessionMessage`]s from the session channel after a
/// successful `session/load` response.
///
/// ## Why this is correct without a timeout
///
/// The ACP SDK's `incoming_actor` processes wire messages sequentially in a
/// single loop. Goose sends all history notifications before returning the
/// `LoadSessionResponse`; the actor routes each notification into the session
/// channel **before** it routes the response to the `block_task` awaiter.
/// Therefore, when `LoadSessionRequest.block_task().await` resolves with
/// `Ok(_)`, every history notification is already queued in the session
/// channel — a non-blocking drain (zero-duration timeout) is both sufficient
/// and correct.
///
/// Using `Duration::ZERO` acts like `try_recv`: the inner future is polled
/// once; if the channel has a message it is returned immediately, otherwise
/// the timeout fires at once and we break. No waiting, no truncation risk,
/// no timing heuristics.
///
/// Replaces the previous 500 ms idle-period heuristic (gander#48).
async fn drain_history_replay(
    session: &mut ActiveSession<'_, Agent>,
    evt_tx: &mpsc::Sender<AcpEvent>,
    tool_calls: &Arc<Mutex<HashMap<ToolCallId, ToolCall>>>,
) {
    loop {
        let next = tokio::time::timeout(Duration::ZERO, session.read_update()).await;

        let update = match next {
            Err(_elapsed) => break, // channel empty — all history consumed
            Ok(Ok(u)) => u,
            Ok(Err(error)) => {
                let _ = evt_tx.send(AcpEvent::Error(error.to_string())).await;
                break;
            }
        };

        match update {
            SessionMessage::SessionMessage(dispatch) => {
                let tx = evt_tx.clone();
                let tc_arc = Arc::clone(tool_calls);
                let handled = MatchDispatch::new(dispatch)
                    .if_notification(async move |n: SessionNotification| {
                        forward_update(n.update, &tx, &tc_arc).await;
                        Ok(())
                    })
                    .await
                    .otherwise_ignore();
                if let Err(error) = handled {
                    let _ = evt_tx.send(AcpEvent::Error(error.to_string())).await;
                    break;
                }
            }
            SessionMessage::StopReason(_) => break, // unexpected but treat as end
            _ => {}
        }
    }
}

/// Apply the fields of a [`ToolCallUpdate`] onto an existing [`ToolCall`] in place.
fn apply_tool_call_update(
    tc: &mut ToolCall,
    fields: agent_client_protocol::schema::ToolCallUpdateFields,
) {
    if let Some(kind) = fields.kind {
        tc.kind = kind;
    }
    if let Some(status) = fields.status {
        tc.status = status;
    }
    if let Some(title) = fields.title {
        tc.title = title;
    }
    if let Some(content) = fields.content {
        tc.content = content;
    }
    if let Some(locations) = fields.locations {
        tc.locations = locations;
    }
    if let Some(raw_input) = fields.raw_input {
        tc.raw_input = Some(raw_input);
    }
    if let Some(raw_output) = fields.raw_output {
        tc.raw_output = Some(raw_output);
    }
}

/// Map a single [`SessionUpdate`] variant to the appropriate [`AcpEvent`] and
/// send it.  Unrecognised variants are silently ignored for forward
/// compatibility.
///
/// For `ToolCall` and `ToolCallUpdate`, the per-session `tool_calls` map is
/// updated and a **full snapshot** of the merged `ToolCall` is emitted.  This
/// keeps the Leptos side dumb: it receives complete state and never needs to
/// apply deltas itself.
async fn forward_update(
    update: SessionUpdate,
    tx: &mpsc::Sender<AcpEvent>,
    tool_calls: &Arc<Mutex<HashMap<ToolCallId, ToolCall>>>,
) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = chunk.content {
                let _ = tx.send(AcpEvent::AgentText(text.text)).await;
            }
        }
        SessionUpdate::UserMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = chunk.content {
                let _ = tx.send(AcpEvent::UserText(text.text)).await;
            }
        }
        SessionUpdate::ToolCall(tc) => {
            let mut map = tool_calls.lock().await;
            map.insert(tc.tool_call_id.clone(), tc.clone());
            let _ = tx.send(AcpEvent::ToolCall(Box::new(tc))).await;
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let mut map = tool_calls.lock().await;
            if let Some(existing) = map.get_mut(&update.tool_call_id) {
                apply_tool_call_update(existing, update.fields);
                let merged = existing.clone();
                drop(map);
                let _ = tx.send(AcpEvent::ToolCall(Box::new(merged))).await;
            }
            // If no prior ToolCall was received for this id, silently ignore.
            // This can only happen when history is replayed out of order, which
            // ACP does not do in practice.
        }
        // AgentThoughtChunk and other variants are intentionally ignored in v1.
        _ => {}
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        ContentChunk, TextContent, ToolCall, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };

    /// Build a fresh shared tool-call map for use in tests.
    fn empty_map() -> Arc<Mutex<HashMap<ToolCallId, ToolCall>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    // Helper: send one SessionUpdate through forward_update and return whatever
    // AcpEvent was produced (if any).  Uses a fresh empty map.
    async fn forwarded(update: SessionUpdate) -> Option<AcpEvent> {
        forwarded_with_map(update, &empty_map()).await
    }

    // Helper: send one SessionUpdate through forward_update with a given map.
    async fn forwarded_with_map(
        update: SessionUpdate,
        map: &Arc<Mutex<HashMap<ToolCallId, ToolCall>>>,
    ) -> Option<AcpEvent> {
        let (tx, mut rx) = mpsc::channel(4);
        forward_update(update, &tx, map).await;
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn forward_agent_text_chunk_produces_agent_text_event() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello")));
        let event = forwarded(SessionUpdate::AgentMessageChunk(chunk)).await;
        assert!(matches!(event, Some(AcpEvent::AgentText(t)) if t == "hello"));
    }

    #[tokio::test]
    async fn forward_user_text_chunk_produces_user_text_event() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("world")));
        let event = forwarded(SessionUpdate::UserMessageChunk(chunk)).await;
        assert!(matches!(event, Some(AcpEvent::UserText(t)) if t == "world"));
    }

    #[tokio::test]
    async fn forward_tool_call_emits_tool_call_event_with_title() {
        let tool = ToolCall::new(ToolCallId::new("tc-1"), "my_tool");
        let event = forwarded(SessionUpdate::ToolCall(tool)).await;
        assert!(
            matches!(&event, Some(AcpEvent::ToolCall(tc)) if tc.title == "my_tool"),
            "expected ToolCall event with title 'my_tool', got {event:?}"
        );
    }

    #[tokio::test]
    async fn forward_tool_call_update_emits_merged_snapshot() {
        let map = empty_map();
        let tool = ToolCall::new(ToolCallId::new("tc-2"), "list_dir");
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        // Now send an update with raw_output — the merged snapshot must carry
        // the original title AND the new output.
        let fields =
            ToolCallUpdateFields::new().raw_output(serde_json::json!({"entries": ["a", "b"]}));
        let update = ToolCallUpdate::new(ToolCallId::new("tc-2"), fields);
        let event = forwarded_with_map(SessionUpdate::ToolCallUpdate(update), &map).await;

        match event {
            Some(AcpEvent::ToolCall(tc)) => {
                assert_eq!(
                    tc.title, "list_dir",
                    "title must be preserved from creation"
                );
                assert!(
                    tc.raw_output.is_some(),
                    "raw_output must be set from update"
                );
            }
            other => panic!("expected merged ToolCall event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forward_tool_call_update_without_prior_call_produces_no_event() {
        // An orphaned update (no prior ToolCall) is silently dropped.
        let fields = ToolCallUpdateFields::new().raw_output(serde_json::json!({"result": "ok"}));
        let update = ToolCallUpdate::new(ToolCallId::new("orphan"), fields);
        let event = forwarded(SessionUpdate::ToolCallUpdate(update)).await;
        assert!(event.is_none(), "orphaned update must not emit an event");
    }

    #[tokio::test]
    async fn forward_unknown_variant_produces_no_event() {
        // SessionInfoUpdate is one of the variants intentionally ignored in v1.
        use agent_client_protocol::schema::SessionInfoUpdate;
        let update = SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new());
        let event = forwarded(update).await;
        assert!(event.is_none());
    }

    /// Demonstrates the drain mechanism used in `drain_history_replay`.
    ///
    /// After `session/load` completes the ACP SDK guarantees every history
    /// notification is already queued in the session channel.  A zero-duration
    /// timeout acts like `try_recv`: it returns a buffered message immediately
    /// and fires `Elapsed` the instant the channel is empty, so the drain
    /// loop terminates without waiting.
    #[tokio::test]
    async fn zero_duration_timeout_drains_buffered_channel_then_stops() {
        let (tx, mut rx) = mpsc::channel::<u32>(16);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();

        let mut received: Vec<u32> = Vec::new();
        while let Ok(Some(v)) = tokio::time::timeout(Duration::ZERO, rx.recv()).await {
            received.push(v);
        }
        assert_eq!(received, [1, 2, 3]);
    }

    /// An empty channel (no history) must terminate immediately — no hang.
    #[tokio::test]
    async fn zero_duration_timeout_terminates_immediately_on_empty_channel() {
        let (_tx, mut rx) = mpsc::channel::<u32>(16);
        // Don't drop tx so the channel isn't closed — mirrors the real case
        // where the session channel stays open after history drain.
        let result = tokio::time::timeout(Duration::ZERO, rx.recv()).await;
        assert!(
            result.is_err(),
            "should time out immediately on empty channel"
        );
    }

    // ToolCall JSON serialisation must use camelCase field names because
    // `ToolCall` is `#[serde(rename_all = "camelCase")]`.  The Leptos reader
    // in gander-chat uses `js_sys::Reflect::get` with these exact key strings,
    // so a regression here silently breaks all tool-call cards.
    #[test]
    fn tool_call_serialises_with_camel_case_keys() {
        let mut tc = ToolCall::new(ToolCallId::new("tc-serde-test"), "echo");
        let fields = ToolCallUpdateFields::new()
            .raw_input(serde_json::json!({"text": "hi"}))
            .raw_output(serde_json::json!({"result": "ok"}));
        apply_tool_call_update(&mut tc, fields);

        let json = serde_json::to_string(&tc).expect("ToolCall serializes to JSON");
        let v: serde_json::Value =
            serde_json::from_str(&json).expect("serialized ToolCall is valid JSON");

        assert_eq!(
            v["toolCallId"], "tc-serde-test",
            "tool_call_id must serialise as 'toolCallId'"
        );
        assert!(
            !v["rawInput"].is_null(),
            "raw_input must serialise as 'rawInput'"
        );
        assert!(
            !v["rawOutput"].is_null(),
            "raw_output must serialise as 'rawOutput'"
        );
        // Confirm snake_case keys are absent — if these exist the renaming is broken.
        assert!(
            v.get("tool_call_id").is_none(),
            "snake_case 'tool_call_id' must not appear in JSON"
        );
        assert!(
            v.get("raw_input").is_none(),
            "snake_case 'raw_input' must not appear in JSON"
        );
        assert!(
            v.get("raw_output").is_none(),
            "snake_case 'raw_output' must not appear in JSON"
        );
    }
}
