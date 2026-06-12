// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code)]

use std::sync::Arc;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, ProtocolVersion, SessionNotification, SessionUpdate,
    StopReason, ToolCall,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{AcpAgent, SessionMessage};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::{Stream, wrappers::UnboundedReceiverStream};

#[allow(clippy::large_enum_variant)]
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
    Protocol(#[from] agent_client_protocol::Error),
    #[error("acp initialization failed: {0}")]
    Initialization(String),
    #[error("acp worker is not available")]
    WorkerClosed,
}

pub struct Client {
    prompt_tx: mpsc::UnboundedSender<Prompt>,
}

struct Prompt {
    text: String,
    events_tx: mpsc::UnboundedSender<Event>,
}

impl Client {
    pub async fn new(goose_binary: impl AsRef<str>) -> Result<Self> {
        let agent = AcpAgent::from_args([goose_binary.as_ref(), "acp"])?;
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();

        tokio::spawn(async move {
            let _ = run_worker(agent, prompt_rx, ready_tx).await;
        });

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self { prompt_tx }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::WorkerClosed),
        }
    }

    /// Sends a single prompt and returns an async stream of events for that turn.
    pub fn send_prompt(&self, text: &str) -> impl Stream<Item = Event> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let fallback_tx = events_tx.clone();

        if self
            .prompt_tx
            .send(Prompt {
                text: text.to_owned(),
                events_tx,
            })
            .is_err()
        {
            let _ = fallback_tx.send(Event::Error(Error::WorkerClosed));
        }

        UnboundedReceiverStream::new(events_rx)
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

async fn run_worker(
    agent: AcpAgent,
    mut prompt_rx: mpsc::UnboundedReceiver<Prompt>,
    ready_tx: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let ready = Arc::new(Mutex::new(Some(ready_tx)));
    let ready_in_connect = Arc::clone(&ready);

    let run = agent_client_protocol::Client
        .builder()
        .name("gander")
        .connect_with(agent, async move |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let mut session = cx.build_session_cwd()?.block_task().start_session().await?;
            if let Some(tx) = ready_in_connect.lock().await.take() {
                let _ = tx.send(Ok(()));
            }

            while let Some(prompt) = prompt_rx.recv().await {
                if let Err(error) = session.send_prompt(&prompt.text) {
                    let _ = prompt.events_tx.send(Event::Error(Error::Protocol(error)));
                    continue;
                }

                loop {
                    let update = match session.read_update().await {
                        Ok(update) => update,
                        Err(error) => {
                            let _ = prompt.events_tx.send(Event::Error(Error::Protocol(error)));
                            break;
                        }
                    };

                    match update {
                        SessionMessage::SessionMessage(dispatch) => {
                            let events_tx = prompt.events_tx.clone();
                            let handled = MatchDispatch::new(dispatch)
                                .if_notification(async move |notification: SessionNotification| {
                                    match notification.update {
                                        SessionUpdate::AgentMessageChunk(chunk) => {
                                            if let ContentBlock::Text(text) = chunk.content {
                                                let _ = events_tx.send(Event::TextChunk(text.text));
                                            }
                                        }
                                        SessionUpdate::ToolCall(tool_call) => {
                                            let _ = events_tx.send(Event::ToolCall(tool_call));
                                        }
                                        _ => {}
                                    }
                                    Ok(())
                                })
                                .await
                                .otherwise_ignore();

                            if let Err(error) = handled {
                                let _ = prompt.events_tx.send(Event::Error(Error::Protocol(error)));
                                break;
                            }
                        }
                        SessionMessage::StopReason(stop_reason) => {
                            let _ = prompt.events_tx.send(Event::Complete(stop_reason));
                            break;
                        }
                        _ => {}
                    }
                }
            }

            Ok(())
        })
        .await;

    match run {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(tx) = ready.lock().await.take() {
                let _ = tx.send(Err(Error::Initialization(error.to_string())));
            }
            Err(Error::Protocol(error))
        }
    }
}
