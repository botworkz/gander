// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code)]

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptRequest, PromptResponse, ProtocolVersion, SessionId, SessionNotification, SessionUpdate,
    StopReason, ToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use tokio_stream::{Stream, wrappers::UnboundedReceiverStream};

use crate::supervisor::{self, ChildState, Supervisor};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Event {
    TextChunk(String),
    ToolCall(ToolCall),
    Complete(StopReason),
    Error(Error),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Supervisor(#[from] supervisor::Error),
    #[error("acp transport closed for profile `{profile}`")]
    TransportClosed { profile: String },
    #[error("acp transport write failed for profile `{profile}`: {message}")]
    TransportWrite { profile: String, message: String },
    #[error("invalid acp message: {0}")]
    InvalidMessage(String),
    #[error("agent returned json-rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("failed to decode acp payload: {0}")]
    Decode(String),
    #[error("failed to resolve current working directory: {0}")]
    CurrentDirectory(String),
    #[error("child for profile `{profile}` failed: {message}")]
    ChildFailed { profile: String, message: String },
    #[error("timed out waiting for acp response for profile `{profile}`")]
    Timeout { profile: String },
}

pub struct Client<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    profile: String,
    supervisor: Supervisor<F>,
    session_id: SessionId,
    request_id: Arc<AtomicU64>,
    prompt_lock: Arc<Mutex<()>>,
}

impl<F> Client<F>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    pub async fn new(profile: impl Into<String>, spawn: F, grace_period: Duration) -> Result<Self> {
        let profile = profile.into();
        let supervisor = Supervisor::new(spawn, grace_period);
        supervisor.ensure_running(&profile).await?;
        let stdio = wait_for_stdio(&supervisor, &profile).await?;
        let mut rx = stdio.subscribe();
        let request_id = Arc::new(AtomicU64::new(1));

        let init_id = request_id.fetch_add(1, Ordering::Relaxed);
        let _: InitializeResponse = send_request(
            &stdio,
            &mut rx,
            init_id,
            "initialize",
            InitializeRequest::new(ProtocolVersion::V1),
            &profile,
        )
        .await?;

        let session_id = request_id.fetch_add(1, Ordering::Relaxed);
        let session_response: NewSessionResponse = send_request(
            &stdio,
            &mut rx,
            session_id,
            "session/new",
            NewSessionRequest::new(
                std::env::current_dir().map_err(|error| Error::CurrentDirectory(error.to_string()))?,
            ),
            &profile,
        )
        .await?;

        Ok(Self {
            profile,
            supervisor,
            session_id: session_response.session_id,
            request_id,
            prompt_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Sends a single prompt to the ACP child and returns an async stream of events.
    ///
    /// The stream yields typed response events (`TextChunk`, `ToolCall`, `Complete`, or `Error`)
    /// for this prompt and then terminates.
    pub fn send_prompt(&self, text: &str) -> impl Stream<Item = Event> {
        let profile = self.profile.clone();
        let supervisor = self.supervisor.clone();
        let session_id = self.session_id.clone();
        let request_id = Arc::clone(&self.request_id);
        let prompt_lock = Arc::clone(&self.prompt_lock);
        let text = text.to_owned();
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let _guard = prompt_lock.lock().await;
            let stdio = match wait_for_stdio(&supervisor, &profile).await {
                Ok(stdio) => stdio,
                Err(error) => {
                    let _ = tx.send(Event::Error(error));
                    return;
                }
            };
            let mut lines = stdio.subscribe();
            let prompt_id = request_id.fetch_add(1, Ordering::Relaxed);
            let request = PromptRequest::new(session_id, vec![ContentBlock::from(text)]);
            let payload = match request_payload(prompt_id, "session/prompt", request) {
                Ok(payload) => payload,
                Err(error) => {
                    let _ = tx.send(Event::Error(error));
                    return;
                }
            };
            if let Err(error) = stdio.send_line(payload) {
                let _ = tx.send(Event::Error(Error::TransportWrite {
                    profile: profile.clone(),
                    message: error.to_string(),
                }));
                return;
            }

            loop {
                let line = match timeout(RESPONSE_TIMEOUT, lines.recv()).await {
                    Ok(Ok(line)) => line,
                    Ok(Err(_)) => {
                        let message = snapshot_message(&supervisor, &profile);
                        let _ = tx.send(Event::Error(Error::ChildFailed { profile, message }));
                        return;
                    }
                    Err(_) => {
                        let message = snapshot_message(&supervisor, &profile);
                        let error = match supervisor.snapshot(&profile).map(|snapshot| snapshot.state) {
                            Some(ChildState::Failed(_)) | Some(ChildState::Exited(_)) => {
                                Error::ChildFailed { profile, message }
                            }
                            _ => Error::Timeout { profile },
                        };
                        let _ = tx.send(Event::Error(error));
                        return;
                    }
                };

                let incoming = match parse_incoming(&line) {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        let _ = tx.send(Event::Error(error));
                        return;
                    }
                };

                if incoming.method.as_deref() == Some("session/update") {
                    let Some(params) = incoming.params else {
                        let _ = tx.send(Event::Error(Error::InvalidMessage(
                            "missing params in session/update".to_owned(),
                        )));
                        return;
                    };
                    let notification: SessionNotification = match serde_json::from_value(params) {
                        Ok(notification) => notification,
                        Err(error) => {
                            let _ = tx.send(Event::Error(Error::Decode(error.to_string())));
                            return;
                        }
                    };
                    match notification.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                let _ = tx.send(Event::TextChunk(text.text));
                            }
                        }
                        SessionUpdate::ToolCall(tool_call) => {
                            let _ = tx.send(Event::ToolCall(tool_call));
                        }
                        _ => {}
                    }
                    continue;
                }

                if incoming.id != Some(prompt_id) {
                    continue;
                }

                if let Some(error) = incoming.error {
                    let _ = tx.send(Event::Error(Error::Rpc {
                        code: error.code,
                        message: error.message,
                    }));
                    return;
                }

                let Some(result) = incoming.result else {
                    let _ = tx.send(Event::Error(Error::InvalidMessage(
                        "response missing result".to_owned(),
                    )));
                    return;
                };

                let response: PromptResponse = match serde_json::from_value(result) {
                    Ok(response) => response,
                    Err(error) => {
                        let _ = tx.send(Event::Error(Error::Decode(error.to_string())));
                        return;
                    }
                };
                let _ = tx.send(Event::Complete(response.stop_reason));
                return;
            }
        });

        UnboundedReceiverStream::new(rx)
    }

    /// Returns the latest supervisor snapshot for this client's profile.
    ///
    /// Returns `None` when no child entry exists for the profile.
    pub fn snapshot(&self) -> Option<supervisor::ChildSnapshot> {
        self.supervisor.snapshot(&self.profile)
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

async fn wait_for_stdio<F>(supervisor: &Supervisor<F>, profile: &str) -> Result<supervisor::ChildStdio>
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    loop {
        if let Some(stdio) = supervisor.stdio(profile) {
            return Ok(stdio);
        }
        match supervisor.snapshot(profile).map(|snapshot| snapshot.state) {
            Some(ChildState::Failed(message)) => {
                return Err(Error::ChildFailed {
                    profile: profile.to_owned(),
                    message,
                });
            }
            Some(ChildState::Exited(code)) => {
                return Err(Error::ChildFailed {
                    profile: profile.to_owned(),
                    message: format!("exited with status {code}"),
                });
            }
            _ => tokio::task::yield_now().await,
        }
    }
}

async fn send_request<T: Serialize, R: for<'de> Deserialize<'de>>(
    stdio: &supervisor::ChildStdio,
    lines: &mut tokio::sync::broadcast::Receiver<String>,
    id: u64,
    method: &str,
    params: T,
    profile: &str,
) -> Result<R> {
    let payload = request_payload(id, method, params)?;
    stdio.send_line(payload).map_err(|error| Error::TransportWrite {
        profile: profile.to_owned(),
        message: error.to_string(),
    })?;

    loop {
        let line = timeout(RESPONSE_TIMEOUT, lines.recv())
            .await
            .map_err(|_| Error::Timeout {
                profile: profile.to_owned(),
            })?
            .map_err(|_| Error::TransportClosed {
                profile: profile.to_owned(),
            })?;
        let incoming = parse_incoming(&line)?;
        if incoming.id != Some(id) {
            continue;
        }

        if let Some(error) = incoming.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        let Some(result) = incoming.result else {
            return Err(Error::InvalidMessage("response missing result".to_owned()));
        };
        return serde_json::from_value(result).map_err(|error| Error::Decode(error.to_string()));
    }
}

fn request_payload<T: Serialize>(id: u64, method: &str, params: T) -> Result<String> {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|error| Error::Decode(error.to_string()))
}

fn snapshot_message<F>(supervisor: &Supervisor<F>, profile: &str) -> String
where
    F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
{
    match supervisor.snapshot(profile).map(|snapshot| snapshot.state) {
        Some(ChildState::Failed(message)) => message,
        Some(ChildState::Exited(code)) => format!("exited with status {code}"),
        Some(ChildState::Ready) => "transport closed while child still marked ready".to_owned(),
        Some(ChildState::Starting) => "transport closed while child was starting".to_owned(),
        None => "missing child snapshot".to_owned(),
    }
}

fn parse_incoming(line: &str) -> Result<IncomingMessage> {
    serde_json::from_str(line).map_err(|error| Error::InvalidMessage(error.to_string()))
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, time::Instant};

    use super::*;
    use tokio::process::Command;
    use tokio_stream::StreamExt;

    fn reserve_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("bind port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    fn stub_command(mode: &'static str) -> impl Fn(&str) -> io::Result<Command> + Send + Sync {
        move |_profile| {
            let mut command = Command::new("python3");
            command.arg("-c").arg(
                r#"
import json
import os
import sys

port = os.environ["SUPERVISOR_PORT"]
mode = os.environ.get("STUB_MODE", "happy")
session_id = "stub-session"
print(port, flush=True)

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if method == "initialize":
        print(json.dumps({
            "jsonrpc": "2.0",
            "id": identifier,
            "result": {
                "protocolVersion": "1.0",
                "agentCapabilities": {}
            }
        }), flush=True)
    elif method == "session/new":
        print(json.dumps({
            "jsonrpc": "2.0",
            "id": identifier,
            "result": {
                "sessionId": session_id
            }
        }), flush=True)
    elif method == "session/prompt":
        prompt_blocks = message.get("params", {}).get("prompt", [])
        prompt_text = prompt_blocks[0].get("text", "") if prompt_blocks else ""
        print(json.dumps({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": f"echo:{prompt_text}"
                    }
                }
            }
        }), flush=True)
        if mode == "crash":
            os._exit(23)
        print(json.dumps({
            "jsonrpc": "2.0",
            "id": identifier,
            "result": {
                "stopReason": "end_turn"
            }
        }), flush=True)
"#,
            );
            command.env("SUPERVISOR_PORT", reserve_port().to_string());
            command.env("STUB_MODE", mode);
            Ok(command)
        }
    }

    async fn wait_for_failed<F>(client: &Client<F>)
    where
        F: Fn(&str) -> io::Result<Command> + Send + Sync + 'static,
    {
        let start = Instant::now();
        loop {
            if matches!(
                client.snapshot().map(|snapshot| snapshot.state),
                Some(ChildState::Failed(_))
            ) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_prompt_happy_path_streams_events() {
        let client = Client::new("alpha", stub_command("happy"), Duration::from_millis(100))
            .await
            .unwrap();

        let events = client.send_prompt("hello").collect::<Vec<_>>().await;

        assert!(matches!(events.first(), Some(Event::TextChunk(chunk)) if chunk == "echo:hello"));
        assert!(matches!(
            events.last(),
            Some(Event::Complete(StopReason::EndTurn))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crash_mid_stream_surfaces_error_and_supervisor_failure() {
        let client = Client::new("alpha", stub_command("crash"), Duration::from_millis(100))
            .await
            .unwrap();

        let events = client.send_prompt("boom").collect::<Vec<_>>().await;

        assert!(matches!(events.first(), Some(Event::TextChunk(chunk)) if chunk == "echo:boom"));
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Error(Error::ChildFailed { .. }))));

        wait_for_failed(&client).await;
        assert!(matches!(
            client.snapshot().map(|snapshot| snapshot.state),
            Some(ChildState::Failed(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clients_operate_independently() {
        let alpha = Client::new("alpha", stub_command("happy"), Duration::from_millis(100))
            .await
            .unwrap();
        let beta = Client::new("beta", stub_command("happy"), Duration::from_millis(100))
            .await
            .unwrap();

        let (alpha_events, beta_events) = tokio::join!(
            alpha.send_prompt("one").collect::<Vec<_>>(),
            beta.send_prompt("two").collect::<Vec<_>>()
        );

        assert!(alpha_events
            .iter()
            .any(|event| matches!(event, Event::TextChunk(chunk) if chunk == "echo:one")));
        assert!(beta_events
            .iter()
            .any(|event| matches!(event, Event::TextChunk(chunk) if chunk == "echo:two")));
        assert!(matches!(
            alpha_events.last(),
            Some(Event::Complete(StopReason::EndTurn))
        ));
        assert!(matches!(
            beta_events.last(),
            Some(Event::Complete(StopReason::EndTurn))
        ));
    }
}
