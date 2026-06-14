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

use std::time::Duration;
use std::{collections::HashMap, env, path::PathBuf, sync::Arc};

use agent_client_protocol::{
    ActiveSession, Agent, ByteStreams, SessionMessage, UntypedMessage,
    schema::{
        ContentBlock, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
        NewSessionResponse, ProtocolVersion, SessionId, SessionNotification, SessionUpdate,
        StopReason, ToolCall, ToolCallId, ToolCallStatus,
    },
    util::MatchDispatch,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::debug;

/// goose-ext: queue of (tool_call_id, resource_uri, extension_name) tuples
/// pending a `_goose/unstable/resources/read` RPC.
type PendingFetches = Arc<Mutex<Vec<(ToolCallId, String, String)>>>;

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
    /// HTML panel fetched for a goose MCP App tool call.
    ///
    /// Emitted by `process_pending_fetches` after a successful
    /// `_goose/unstable/resources/read` RPC triggered by a `Completed`
    /// `ToolCallUpdate` that carried both `rawOutput.resourceUri` and
    /// `_meta.goose.toolCall.extensionName`.  The UI should render the HTML
    /// in a sandboxed iframe alongside the tool-call card.
    // goose-ext: emitted after fetching the MCP App HTML via _goose/unstable/resources/read
    ToolResource { tool_call_id: String, html: String },
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

    // goose-ext: queue of (tool_call_id, resource_uri, extension_name) tuples
    // pushed by forward_update on Completed tool calls with rawOutput.resourceUri.
    // run_worker drains after each session update batch and calls
    // _goose/unstable/resources/read to fetch the HTML.
    let pending_fetches: PendingFetches = Arc::new(Mutex::new(Vec::new()));

    let result = agent_client_protocol::Client
        .builder()
        .name("gander")
        .connect_with(transport, async move |cx| {
            let init_req = InitializeRequest::new(ProtocolVersion::V1);
            debug!(
                target: "gander::wire",
                direction = "send",
                method = "initialize",
                payload = %serde_json::to_string(&init_req).unwrap_or_default(),
                "INIT_REQUEST"
            );
            let init_resp = cx.send_request(init_req)
                .block_task()
                .await?;
            debug!(
                target: "gander::wire",
                direction = "recv",
                method = "initialize",
                payload = %serde_json::to_string(&init_resp).unwrap_or_default(),
                "INIT_RESPONSE"
            );

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
                            &pending_fetches,
                        )
                        .await;
                        process_pending_fetches(
                            &cx,
                            &evt_tx_clone,
                            &pending_fetches,
                            current_session.session_id().to_string(),
                        )
                        .await;
                    }

                    AcpCommand::SessionSelect(id) => {
                        let sid = SessionId::new(id.clone());
                        let load_req = LoadSessionRequest::new(sid.clone(), cwd.clone());
                        debug!(
                            target: "gander::wire",
                            direction = "send",
                            method = "session/load",
                            payload = %serde_json::to_string(&load_req).unwrap_or_default(),
                            "SESSION_LOAD_REQUEST"
                        );
                        match cx
                            .send_request(load_req)
                            .block_task()
                            .await
                        {
                            Ok(resp) => {
                                debug!(
                                    target: "gander::wire",
                                    direction = "recv",
                                    method = "session/load",
                                    session_id = %id,
                                    cwd = %cwd.to_string_lossy(),
                                    payload = %serde_json::to_string(&resp).unwrap_or_default(),
                                    "SESSION_LOAD_RESPONSE"
                                );
                                match cx.attach_session(NewSessionResponse::new(sid), vec![]) {
                                    Ok(s) => {
                                        current_session = s;
                                        active_id = id;
                                        // Clear stale tool-call state before replaying the
                                        // new session's history.
                                        tool_calls.lock().await.clear();
                                        // goose-ext: drop any fetches queued from the prior session.
                                        pending_fetches.lock().await.clear();
                                        let _ = evt_tx_clone
                                            .send(AcpEvent::SessionLoadStart)
                                            .await;
                                        drain_history_replay(
                                            &mut current_session,
                                            &evt_tx_clone,
                                            &tool_calls,
                                            &pending_fetches,
                                        )
                                        .await;
                                        // goose-ext: discard fetches queued during history
                                        // replay — we don't auto-fetch resources for historical
                                        // tool calls (would re-trigger on every reconnect).
                                        pending_fetches.lock().await.clear();
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
                        debug!(
                            target: "gander::wire",
                            direction = "send",
                            method = "session/new",
                            "SESSION_NEW_REQUEST"
                        );
                        match cx.build_session_cwd()?.block_task().start_session().await {
                            Ok(s) => {
                                active_id = s.session_id().to_string();
                                debug!(
                                    target: "gander::wire",
                                    direction = "recv",
                                    method = "session/new",
                                    session_id = %active_id,
                                    "SESSION_NEW_RESPONSE"
                                );
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
    pending_fetches: &PendingFetches,
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
                let pf_arc = Arc::clone(pending_fetches);
                let handled = MatchDispatch::new(dispatch)
                    .if_notification(async move |n: SessionNotification| {
                        forward_update(n.update, &tx, &tc_arc, &pf_arc).await;
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
    pending_fetches: &PendingFetches,
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
                let pf_arc = Arc::clone(pending_fetches);
                let handled = MatchDispatch::new(dispatch)
                    .if_notification(async move |n: SessionNotification| {
                        forward_update(n.update, &tx, &tc_arc, &pf_arc).await;
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
    pending_fetches: &PendingFetches,
) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = chunk.content {
                let event = AcpEvent::AgentText(text.text);
                debug!(target: "gander::wire", direction = "emit", event_kind = ?event, "ACP_EVENT_EMIT");
                let _ = tx.send(event).await;
            }
        }
        SessionUpdate::UserMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = chunk.content {
                let event = AcpEvent::UserText(text.text);
                debug!(target: "gander::wire", direction = "emit", event_kind = ?event, "ACP_EVENT_EMIT");
                let _ = tx.send(event).await;
            }
        }
        SessionUpdate::ToolCall(tc) => {
            debug!(
                target: "gander::wire",
                direction = "recv",
                method = "session/update",
                update_kind = "tool_call",
                stage = "create",
                snapshot = %serde_json::to_string(&tc).unwrap_or_default(),
                "TOOL_CALL"
            );
            let mut map = tool_calls.lock().await;
            map.insert(tc.tool_call_id.clone(), tc.clone());
            let event = AcpEvent::ToolCall(Box::new(tc));
            debug!(target: "gander::wire", direction = "emit", event_kind = ?event, "ACP_EVENT_EMIT");
            let _ = tx.send(event).await;
        }
        SessionUpdate::ToolCallUpdate(update) => {
            debug!(
                target: "gander::wire",
                direction = "recv",
                method = "session/update",
                update_kind = "tool_call_update",
                stage = "delta",
                delta = %serde_json::to_string(&update).unwrap_or_default(),
                "TOOL_CALL_UPDATE_DELTA"
            );
            let mut map = tool_calls.lock().await;
            if let Some(existing) = map.get_mut(&update.tool_call_id) {
                // Propagate _meta from the update onto the merged ToolCall so
                // the meta isolator log below sees it and the UI gets it too.
                // goose-ext: top-level merge — goose ships goose.toolCall on
                // the create and a narrower goose.{created,messageId} on the
                // update; a naive replace clobbers toolCall (and any future
                // mcpApp payload that arrives in a different update).
                if let Some(update_meta) = update.meta {
                    if let Some(existing_meta) = existing.meta.as_mut() {
                        for (k, v) in update_meta {
                            existing_meta.insert(k, v);
                        }
                    } else {
                        existing.meta = Some(update_meta);
                    }
                }
                apply_tool_call_update(existing, update.fields);
                let merged = existing.clone();
                drop(map);
                debug!(
                    target: "gander::wire",
                    direction = "recv",
                    method = "session/update",
                    update_kind = "tool_call_update",
                    stage = "merged",
                    snapshot = %serde_json::to_string(&merged).unwrap_or_default(),
                    "TOOL_CALL_UPDATE_MERGED"
                );
                if let Some(meta) = merged.meta.as_ref() {
                    debug!(
                        target: "gander::wire",
                        update_kind = "tool_call_update",
                        tool_call_id = %merged.tool_call_id,
                        meta = %serde_json::to_string(meta).unwrap_or_default(),
                        "META_PAYLOAD"
                    );
                }
                let event = AcpEvent::ToolCall(Box::new(merged.clone()));
                debug!(target: "gander::wire", direction = "emit", event_kind = ?event, "ACP_EVENT_EMIT");
                let _ = tx.send(event).await;
                // goose-ext: on Completed, queue a _goose/unstable/resources/read
                // using rawOutput.resourceUri and _meta.goose.toolCall.extensionName.
                // The actual RPC is made by process_pending_fetches() once we have cx.
                if merged.status == ToolCallStatus::Completed {
                    let resource_uri = merged
                        .raw_output
                        .as_ref()
                        .and_then(|o| o.get("resourceUri"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let extension_name = merged
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("goose"))
                        .and_then(|g| g.get("toolCall"))
                        .and_then(|t| t.get("extensionName"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    if let (Some(uri), Some(ext)) = (resource_uri, extension_name) {
                        debug!(
                            target: "gander::wire",
                            tool_call_id = %merged.tool_call_id,
                            resource_uri = %uri,
                            extension_name = %ext,
                            "QUEUE_READ_RESOURCE"
                        );
                        pending_fetches
                            .lock()
                            .await
                            .push((merged.tool_call_id.clone(), uri, ext));
                    }
                }
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
// goose-ext: explicit resource fetch via _goose/unstable/resources/read
// ---------------------------------------------------------------------------

/// Extract the first `text/html` item from a `_goose/unstable/resources/read` response.
///
/// Response shape (goose ≥ 1.37.0 `acp/server/resources.rs`):
///   `{ result: { contents: [{ uri, mimeType, text }] } }`
///
/// Returns `None` when:
/// - the `result` or `contents` keys are absent
/// - no item carries a `text/html` (or `text/html;…`) mime type
// goose-ext: navigates goose-private _goose/unstable/resources/read response shape
fn extract_html_from_read_resource_response(response: &Value) -> Option<String> {
    let items = response
        .get("result")
        .and_then(|r| r.get("contents"))
        .and_then(|c| c.as_array())?;
    items.iter().find_map(|item| {
        let mime = item.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
        if mime.starts_with("text/html") {
            item.get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// Drain `pending_fetches` and call `_goose/unstable/resources/read` for each.
///
/// On a `text/html` response, emits `AcpEvent::ToolResource`.  Errors are
/// logged and skipped — the tool-call card remains visible without an iframe.
async fn process_pending_fetches<R>(
    cx: &agent_client_protocol::ConnectionTo<R>,
    tx: &mpsc::Sender<AcpEvent>,
    pending_fetches: &PendingFetches,
    session_id: String,
) where
    R: agent_client_protocol::role::Role,
    R: agent_client_protocol::role::HasPeer<R>,
{
    let fetches: Vec<(ToolCallId, String, String)> = {
        let mut guard = pending_fetches.lock().await;
        std::mem::take(&mut *guard)
    };

    for (tool_call_id, uri, extension_name) in fetches {
        let params = serde_json::json!({
            "sessionId": session_id,
            "uri": uri,
            "extensionName": extension_name,
        });
        let msg = match UntypedMessage::new("_goose/unstable/resources/read", params.clone()) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(%err, "failed to build _goose/unstable/resources/read request");
                continue;
            }
        };
        debug!(
            target: "gander::wire",
            direction = "send",
            method = "_goose/unstable/resources/read",
            payload = %params,
            "READ_RESOURCE_REQUEST"
        );
        let response: Value = match cx.send_request(msg).block_task().await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(%err, uri = %uri, "_goose/unstable/resources/read failed");
                continue;
            }
        };
        debug!(
            target: "gander::wire",
            direction = "recv",
            method = "_goose/unstable/resources/read",
            payload = %serde_json::to_string(&response).unwrap_or_default(),
            "READ_RESOURCE_RESPONSE"
        );
        if let Some(html) = extract_html_from_read_resource_response(&response) {
            let _ = tx
                .send(AcpEvent::ToolResource {
                    tool_call_id: tool_call_id.to_string(),
                    html,
                })
                .await;
        } else {
            tracing::warn!(uri = %uri, "_goose/unstable/resources/read returned no text/html content");
        }
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
        ContentChunk, TextContent, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields,
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
        let pf = Arc::new(Mutex::new(Vec::new()));
        forward_update(update, &tx, map, &pf).await;
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

    /// Smoke test: every `forward_update` path emits at least one log line
    /// with `target = "gander::wire"`.
    ///
    /// Uses `tracing_subscriber` with a captured writer so the assertions run
    /// in-process without relying on env-var filtering.  If the target string
    /// is mistyped at any log site the captured output will lack it and this
    /// test will fail.
    #[tokio::test]
    async fn wire_target_appears_in_forward_update_logs() {
        use std::sync::{Arc as StdArc, Mutex as StdMutex};
        use tracing_subscriber::fmt::MakeWriter;

        // Shared buffer that the subscriber writes into.
        let buf: StdArc<StdMutex<Vec<u8>>> = StdArc::new(StdMutex::new(Vec::new()));
        let buf_clone = StdArc::clone(&buf);

        // A MakeWriter that hands out a clone of the Arc<Mutex<Vec<u8>>>.
        #[derive(Clone)]
        struct BufWriter(StdArc<StdMutex<Vec<u8>>>);
        impl std::io::Write for BufWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf_clone))
            .with_target(true)
            // Accept all levels so even a mistaken `trace!` site would be caught.
            .with_max_level(tracing::Level::TRACE)
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        // Exercise a ToolCall path (the richest path: creates a ToolCall,
        // then sends an update so both TOOL_CALL and TOOL_CALL_UPDATE_* fire).
        let map = empty_map();
        let tc = ToolCall::new(ToolCallId::new("tc-wire-test"), "smoke");
        forwarded_with_map(SessionUpdate::ToolCall(tc), &map).await;

        let fields = ToolCallUpdateFields::new().raw_output(serde_json::json!({"ok": true}));
        let update = ToolCallUpdate::new(ToolCallId::new("tc-wire-test"), fields);
        forwarded_with_map(SessionUpdate::ToolCallUpdate(update), &map).await;

        let output = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(
            output.contains("gander::wire"),
            "expected 'gander::wire' in tracing output; got:\n{output}"
        );
    }

    // ── queue+fetch path ──────────────────────────────────────────────────

    // Helper: send one SessionUpdate through forward_update with a given map
    // and return what ended up in pending_fetches.
    async fn queued_fetches_after(
        update: SessionUpdate,
        map: &Arc<Mutex<HashMap<ToolCallId, ToolCall>>>,
    ) -> Vec<(ToolCallId, String, String)> {
        let (tx, _rx) = mpsc::channel(8);
        let pf: PendingFetches = Arc::new(Mutex::new(Vec::new()));
        forward_update(update, &tx, map, &pf).await;
        pf.lock().await.clone()
    }

    #[tokio::test]
    async fn completed_tool_without_resource_uri_does_not_queue() {
        let map = empty_map();
        let tool = ToolCall::new(ToolCallId::new("tc-plain"), "plain_tool");
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        let update = ToolCallUpdate::new(ToolCallId::new("tc-plain"), fields);
        let fetches = queued_fetches_after(SessionUpdate::ToolCallUpdate(update), &map).await;

        assert!(
            fetches.is_empty(),
            "no resourceUri means nothing queued; got {fetches:?}"
        );
    }

    #[tokio::test]
    async fn completed_tool_with_resource_uri_but_no_extension_does_not_queue() {
        let map = empty_map();
        let tool = ToolCall::new(ToolCallId::new("tc-no-ext"), "mcp_tool");
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        // rawOutput has resourceUri but _meta has no extensionName
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://ext/resource"}));
        let update = ToolCallUpdate::new(ToolCallId::new("tc-no-ext"), fields);
        let fetches = queued_fetches_after(SessionUpdate::ToolCallUpdate(update), &map).await;

        assert!(
            fetches.is_empty(),
            "missing extensionName means nothing queued; got {fetches:?}"
        );
    }

    #[tokio::test]
    async fn completed_tool_with_resource_uri_and_extension_queues_fetch() {
        let map = empty_map();
        // goose-ext: extensionName arrives on ToolCall creation in _meta.goose.toolCall
        let meta_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "goose": { "toolCall": { "extensionName": "my_ext" } }
            }))
            .unwrap();
        let tool = ToolCall::new(ToolCallId::new("tc-queue"), "mcp_tool").meta(meta_map);
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        // Completed update merges rawOutput.resourceUri into the snapshot.
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://my_ext/panel"}));
        let update = ToolCallUpdate::new(ToolCallId::new("tc-queue"), fields);
        let fetches = queued_fetches_after(SessionUpdate::ToolCallUpdate(update), &map).await;

        assert_eq!(fetches.len(), 1, "expected exactly one queued fetch");
        let (tc_id, uri, ext) = &fetches[0];
        assert_eq!(tc_id.to_string(), "tc-queue");
        assert_eq!(uri, "mcp://my_ext/panel");
        assert_eq!(ext, "my_ext");
    }

    #[tokio::test]
    async fn in_progress_tool_with_resource_uri_does_not_queue() {
        let map = empty_map();
        let meta_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "goose": { "toolCall": { "extensionName": "my_ext" } }
            }))
            .unwrap();
        let tool = ToolCall::new(ToolCallId::new("tc-inprog"), "mcp_tool").meta(meta_map);
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        // Status is InProgress — queue gate must not fire.
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .raw_output(serde_json::json!({"resourceUri": "mcp://my_ext/panel"}));
        let update = ToolCallUpdate::new(ToolCallId::new("tc-inprog"), fields);
        let fetches = queued_fetches_after(SessionUpdate::ToolCallUpdate(update), &map).await;

        assert!(
            fetches.is_empty(),
            "InProgress must not queue; got {fetches:?}"
        );
    }

    // goose-ext: meta merge test — extensionName is delivered on ToolCall creation
    // and must survive a later ToolCallUpdate that carries rawOutput but no meta.
    #[tokio::test]
    async fn meta_merge_preserves_extension_name_across_updates() {
        let map = empty_map();
        let meta_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "goose": { "toolCall": { "extensionName": "keep_me" } }
            }))
            .unwrap();
        let tool = ToolCall::new(ToolCallId::new("tc-merge"), "mcp_tool").meta(meta_map);
        forwarded_with_map(SessionUpdate::ToolCall(tool), &map).await;

        // First update: no meta, no status change — extensionName must still be in map.
        let fields1 = ToolCallUpdateFields::new().raw_output(serde_json::json!({"partial": true}));
        let upd1 = ToolCallUpdate::new(ToolCallId::new("tc-merge"), fields1);
        forwarded_with_map(SessionUpdate::ToolCallUpdate(upd1), &map).await;

        // Second update: Completed + resourceUri — should queue because meta was preserved.
        let fields2 = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://keep_me/x"}));
        let upd2 = ToolCallUpdate::new(ToolCallId::new("tc-merge"), fields2);
        let fetches = queued_fetches_after(SessionUpdate::ToolCallUpdate(upd2), &map).await;

        assert_eq!(fetches.len(), 1, "extensionName must survive meta merge");
        assert_eq!(fetches[0].2, "keep_me");
    }

    // ── extract_html_from_read_resource_response ──────────────────────────

    #[test]
    fn extract_html_returns_text_for_text_html_mime() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "text/html", "text": "<p>hi</p>", "uri": "x" }]
            }
        });
        assert_eq!(
            extract_html_from_read_resource_response(&resp),
            Some("<p>hi</p>".to_string())
        );
    }

    #[test]
    fn extract_html_accepts_text_html_profile_variant() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "text/html;profile=mcp-app", "text": "<div/>", "uri": "x" }]
            }
        });
        assert_eq!(
            extract_html_from_read_resource_response(&resp),
            Some("<div/>".to_string())
        );
    }

    #[test]
    fn extract_html_returns_none_for_non_html_mime() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "application/json", "text": "{}", "uri": "x" }]
            }
        });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }

    #[test]
    fn extract_html_returns_none_when_contents_empty() {
        let resp = serde_json::json!({ "result": { "contents": [] } });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }

    #[test]
    fn extract_html_returns_none_when_result_key_missing() {
        let resp = serde_json::json!({ "other": {} });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }
}
